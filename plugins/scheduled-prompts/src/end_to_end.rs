//! The whole path, against a real socket and a real filesystem.
//!
//! The unit tests around this crate pin each piece: the cron search, the
//! decision table, the confinement rules, the redaction. What they cannot show
//! is that a due job actually turns into an HTTP request and a file on disk, or
//! that a second tick during a slow run really does refuse to start a second
//! copy. That is what this module is for.
//!
//! A stub OpenAI-compatible endpoint runs on loopback and counts the requests it
//! serves, so "did it run?" and "did it run twice?" have answers rather than
//! arguments. Nothing here races the clock: the stub **holds** its response
//! until the test releases it, so "while a run is in flight" is a state the test
//! controls rather than a sleep it hopes is long enough. No real model, no
//! network beyond `127.0.0.1`, and every file lands in a per-test scratch
//! directory that is removed at the end.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

use crate::clock::now_ms;
use crate::config::{Config, EnvMap};
use crate::history::Store;
use crate::jobs::parse_jobs;
use crate::scheduler::{JobsSource, Scheduler};

/// Longest any poll in this module waits before giving up.
const PATIENCE: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(10);

/// A stub endpoint: answers every request the same way, counts them, and only
/// answers when the test says so.
struct Endpoint {
    port: u16,
    served: Arc<AtomicUsize>,
    gate: Arc<Semaphore>,
}

impl Endpoint {
    /// A stub that answers immediately.
    async fn open(status: u16, body: &'static str) -> Self {
        Self::start(status, body, Semaphore::MAX_PERMITS).await
    }

    /// A stub that holds every response until [`Endpoint::release`] is called.
    async fn gated(status: u16, body: &'static str) -> Self {
        Self::start(status, body, 0).await
    }

    async fn start(status: u16, body: &'static str, permits: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let served = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(Semaphore::new(permits));

        let counter = Arc::clone(&served);
        let held = Arc::clone(&gate);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let counter = Arc::clone(&counter);
                let held = Arc::clone(&held);
                tokio::spawn(async move {
                    let mut scratch = vec![0u8; 65_536];
                    let _ = socket.read(&mut scratch).await;
                    // Counted as soon as the request arrives, so "has the run
                    // reached the endpoint?" is answerable before it finishes.
                    counter.fetch_add(1, Ordering::SeqCst);

                    let Ok(permit) = held.acquire().await else {
                        return;
                    };
                    permit.forget();

                    let response = format!(
                        "HTTP/1.1 {status} Stub\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });

        Self { port, served, gate }
    }

    fn served(&self) -> usize {
        self.served.load(Ordering::SeqCst)
    }

    /// Let `count` held requests be answered.
    fn release(&self, count: usize) {
        self.gate.add_permits(count);
    }

    /// Wait until at least `count` requests have arrived.
    async fn wait_served(&self, count: usize) -> bool {
        wait_until(|| self.served() >= count).await
    }
}

const COMPLETION: &str = r#"{"model":"stub-model","choices":[{"message":{"role":"assistant",
    "content":"The node was busy overnight."},"finish_reason":"stop"}],
    "usage":{"prompt_tokens":11,"completion_tokens":7}}"#;

/// Poll a condition rather than sleeping a guessed interval, so a loaded
/// machine makes a test slower and never makes it wrong.
async fn wait_until(mut condition: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + PATIENCE;
    while std::time::Instant::now() < deadline {
        if condition() {
            return true;
        }
        tokio::time::sleep(POLL).await;
    }
    condition()
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tdcc-scheduled-prompts-e2e-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn build(dir: &Path, port: u16, jobs_text: &str) -> Arc<Scheduler> {
    let config = Config::parse(
        &[
            "--endpoint".to_string(),
            format!("http://127.0.0.1:{port}/v1"),
            "--state-dir".to_string(),
            dir.display().to_string(),
            "--output-dir".to_string(),
            dir.join("out").display().to_string(),
        ],
        &EnvMap::from([("HOME".to_string(), dir.display().to_string())]),
    )
    .expect("config parses");

    let file = parse_jobs(jobs_text, &EnvMap::new(), now_ms()).expect("jobs load");
    Arc::new(
        Scheduler::new(
            config,
            JobsSource {
                file,
                error: None,
                present: true,
            },
            Store::new(dir),
        )
        .expect("scheduler builds"),
    )
}

/// Every minute, into a text file. No window, so it is due whenever it is due.
fn minutely(extra: &str) -> String {
    format!(
        "version = 1\n\
         timezone = \"utc\"\n\
         \n\
         [[job]]\n\
         id = \"digest\"\n\
         schedule = \"* * * * *\"\n\
         model = \"stub-model\"\n\
         prompt = \"Summarise the last hour.\"\n\
         timeout_secs = 30\n\
         sink = {{ kind = \"file\", path = \"reports/digest.md\" }}\n\
         {extra}"
    )
}

/// Give the jobs a cursor and hand back the instant job `index` is next due.
fn arm(scheduler: &Arc<Scheduler>, index: usize) -> i64 {
    Arc::clone(scheduler).tick(now_ms());
    scheduler.list()["jobs"][index]["next_due_ms"]
        .as_i64()
        .expect("the first tick sets a cursor")
}

fn attempts(scheduler: &Arc<Scheduler>, job_id: &str) -> u64 {
    scheduler.list()["jobs"]
        .as_array()
        .expect("jobs")
        .iter()
        .find(|job| job["id"] == job_id)
        .expect("the job is listed")["totals"]["attempts"]
        .as_u64()
        .unwrap_or(0)
}

#[tokio::test]
async fn a_due_job_calls_the_endpoint_and_writes_its_answer_to_the_sink() {
    let dir = scratch("run");
    let endpoint = Endpoint::open(200, COMPLETION).await;
    let scheduler = build(&dir, endpoint.port, &minutely(""));

    // The first tick establishes the cursor and runs nothing.
    let due = arm(&scheduler, 0);
    assert_eq!(endpoint.served(), 0, "establishing a cursor must not run");

    // The second tick lands on the occurrence.
    Arc::clone(&scheduler).tick(due);
    assert!(
        wait_until(|| attempts(&scheduler, "digest") >= 1).await,
        "the run never finished"
    );

    assert_eq!(endpoint.served(), 1, "exactly one completion was requested");
    let written = std::fs::read_to_string(dir.join("out").join("reports").join("digest.md"))
        .expect("the sink file exists");
    assert!(
        written.contains("The node was busy overnight."),
        "{written}"
    );
    assert!(
        written.contains("digest (scheduled, stub-model)"),
        "{written}"
    );

    let history = scheduler.history(Some("digest"), Some(5)).expect("answers");
    let run = &history["jobs"][0]["runs"][0];
    assert_eq!(run["outcome"], "success");
    assert_eq!(run["code"], "ok");
    assert_eq!(run["trigger"], "scheduled");
    assert_eq!(run["completion_tokens"], 7);
    assert_eq!(run["output_chars"], 28);
    assert_eq!(run["sink"], "file:reports/digest.md");
    assert_eq!(history["jobs"][0]["totals"]["succeeded"], 1);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_job_never_runs_twice_at_once_however_often_the_scheduler_wakes() {
    let dir = scratch("overlap");
    // The endpoint holds its answer, so the first run stays in flight for
    // exactly as long as this test wants it to.
    let endpoint = Endpoint::gated(200, COMPLETION).await;
    let scheduler = build(&dir, endpoint.port, &minutely(""));

    let due = arm(&scheduler, 0);
    Arc::clone(&scheduler).tick(due);
    assert!(
        endpoint.wait_served(1).await,
        "the first run never reached the endpoint"
    );

    // Four more occurrences come due while that run is stuck.
    for step in 1..=4 {
        Arc::clone(&scheduler).tick(due + step * 60_000);
    }
    let listed = scheduler.list();
    assert_eq!(listed["jobs"][0]["running"], true);
    assert_eq!(
        listed["jobs"][0]["skips_by_reason"]["skipped_overlap"], 4,
        "occurrences arriving during a run are skipped, not queued"
    );
    assert_eq!(
        endpoint.served(),
        1,
        "a second copy of a running job must never start"
    );

    // Let the run finish, and confirm it was the only one.
    endpoint.release(1);
    assert!(
        wait_until(|| attempts(&scheduler, "digest") >= 1).await,
        "the run never finished"
    );
    assert_eq!(endpoint.served(), 1);
    let listed = scheduler.list();
    assert_eq!(listed["jobs"][0]["totals"]["attempts"], 1);
    assert_eq!(listed["jobs"][0]["totals"]["skipped"], 4);
    assert_eq!(listed["jobs"][0]["running"], false);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn run_now_runs_a_declared_job_and_reports_what_happened() {
    let dir = scratch("manual");
    let endpoint = Endpoint::open(200, COMPLETION).await;
    let scheduler = build(&dir, endpoint.port, &minutely(""));

    let answer = scheduler.run_now("digest").await.expect("the job runs");

    assert_eq!(answer["status"], "finished");
    assert_eq!(answer["outcome"], "success");
    assert_eq!(answer["completion_tokens"], 7);
    assert_eq!(endpoint.served(), 1);
    assert!(
        dir.join("out").join("reports").join("digest.md").exists(),
        "a manual run delivers to the same sink"
    );
    let history = scheduler.history(Some("digest"), Some(5)).expect("answers");
    assert_eq!(history["jobs"][0]["runs"][0]["trigger"], "manual");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_endpoint_failure_is_a_failed_run_that_backs_off_and_writes_nothing() {
    let dir = scratch("failure");
    let endpoint = Endpoint::open(503, r#"{"error":{"message":"model not loaded"}}"#).await;
    let scheduler = build(&dir, endpoint.port, &minutely(""));

    let answer = scheduler.run_now("digest").await.expect("the tool answers");

    assert_eq!(answer["status"], "finished");
    assert_eq!(answer["outcome"], "failed");
    assert_eq!(answer["code"], "endpoint_error");
    assert!(
        !dir.join("out").join("reports").join("digest.md").exists(),
        "a failed completion must not produce an output file"
    );

    let listed = scheduler.list();
    assert_eq!(listed["jobs"][0]["consecutive_failures"], 1);
    assert!(
        listed["jobs"][0]["backoff_until_utc"].is_string(),
        "a failure must schedule a delay rather than retrying on the next tick"
    );
    let detail = listed["jobs"][0]["last_run"]["detail"]
        .as_str()
        .expect("the failure names a cause");
    assert!(detail.contains("503"), "{detail}");
    assert!(detail.contains("stub-model"), "{detail}");

    // The backoff is honoured rather than decorative: the next attempt is
    // refused, and the endpoint that just failed is not touched again. (The
    // scheduled path through the same gate is covered, without any clock
    // racing, by `decide::tests::a_backing_off_job_is_skipped_until_the_delay_expires`.)
    let refused = scheduler
        .run_now("digest")
        .await
        .expect_err("a backing-off job must not be re-run on demand")
        .to_string();
    assert!(refused.contains("backing off"), "{refused}");
    assert!(refused.contains("failed 1 time"), "{refused}");
    assert_eq!(
        endpoint.served(),
        1,
        "a failing job must back off, not retry hot"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_concurrency_cap_bounds_how_much_of_the_machine_a_schedule_can_take() {
    let dir = scratch("cap");
    let endpoint = Endpoint::gated(200, COMPLETION).await;
    // Three jobs, all due at the same minute, and one slot between them.
    let mut text = String::from("version = 1\ntimezone = \"utc\"\nmax_concurrent_runs = 1\n");
    for index in 0..3 {
        text.push_str(&format!(
            "[[job]]\nid = \"j{index}\"\nschedule = \"* * * * *\"\nmodel = \"stub-model\"\n\
             prompt = \"p\"\ntimeout_secs = 30\n\
             sink = {{ kind = \"file\", path = \"j{index}.md\" }}\n"
        ));
    }
    let scheduler = build(&dir, endpoint.port, &text);

    let due = arm(&scheduler, 0);
    Arc::clone(&scheduler).tick(due);
    assert!(
        endpoint.wait_served(1).await,
        "the first run never reached the endpoint"
    );

    // One job took the only slot; the other two were shed, not queued.
    assert_eq!(endpoint.served(), 1, "the cap is one concurrent run");
    let listed = scheduler.list();
    let shed: u64 = (0..3)
        .map(|index| {
            listed["jobs"][index]["skips_by_reason"]["skipped_busy"]
                .as_u64()
                .unwrap_or(0)
        })
        .sum();
    assert_eq!(shed, 2, "two occurrences were shed rather than queued");

    endpoint.release(1);
    assert!(
        wait_until(|| (0..3).any(|index| attempts(&scheduler, &format!("j{index}")) >= 1)).await,
        "the one permitted run never finished"
    );
    let listed = scheduler.list();
    let total_attempts: u64 = (0..3)
        .map(|index| {
            listed["jobs"][index]["totals"]["attempts"]
                .as_u64()
                .unwrap_or(0)
        })
        .sum();
    assert_eq!(total_attempts, 1, "one occurrence, one run");

    let _ = std::fs::remove_dir_all(&dir);
}
