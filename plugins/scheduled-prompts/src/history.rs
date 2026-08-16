//! What happened, kept on disk, bounded forever.
//!
//! Three layers, because they answer different questions and have very
//! different cost:
//!
//! * **The last N runs per job**, in full — when, how long, what happened, how
//!   many tokens, where it was delivered, and the first line of any error. N is
//!   `history_per_job`, capped at [`MAX_HISTORY_PER_JOB`].
//! * **Lifetime totals per job** — counts and sums that never age out, so "has
//!   this ever succeeded?" survives long after the run that answers it has
//!   aged out of the detailed list.
//! * **Skips, as counters plus the most recent one.** A half-hourly job with an
//!   overnight window is skipped 32 times a day by design. Writing each of
//!   those into the detailed list would push every real run out of it within a
//!   day, so a skip increments a counter keyed by its reason and replaces
//!   `last_skip` — the detailed history is for attempts that actually spent
//!   time on this machine.
//!
//! So the file cannot grow without bound however often a job fires or is
//! skipped, and nothing interesting is lost to noise.
//!
//! # What is deliberately not stored
//!
//! **The model's output.** Not the completion, not a preview of it. A run
//! record carries how many characters were produced and where they were
//! delivered, and that is all. Output is the part most likely to be sensitive
//! and the part most likely to be large, and the sink is where the operator
//! already decided it should live.
//!
//! **A pause.** [`JobState::pause`] is `#[serde(skip)]`, so pausing a job does
//! not survive a restart. The jobs file is the only durable statement of what
//! this machine has agreed to run; a pause is a runtime override on top of it.
//! Failure backoff *is* persisted, because that is a measurement rather than an
//! intent — a plugin that forgot it on every restart would retry a broken job
//! hot every time the host came back.

use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::jobs::MAX_HISTORY_PER_JOB;

/// Bumped only for a change an older reader could misinterpret.
pub const STATE_VERSION: u32 = 1;
pub const STATE_FILE: &str = "runs.json";

/// Longest error detail kept in a record.
///
/// Long enough for a real message from an endpoint or a filesystem, short
/// enough that an HTML error page cannot become the history file.
pub const MAX_DETAIL_CHARS: usize = 300;

/// Upper bound on distinct skip-reason counters held for one job.
///
/// The set of reasons is fixed and small; the cap exists so that a future
/// reason, or a hand-edited state file, cannot grow the map without bound.
pub const MAX_SKIP_REASONS: usize = 16;

/// How a run ended.
///
/// There is no `Skipped` variant: a skip did not run, so it is counted rather
/// than recorded. See the module documentation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The completion came back and the sink accepted it.
    Success,
    /// The completion failed, or the sink refused it. Both are failures of the
    /// job, because from the operator's side nothing was delivered.
    Failed,
}

impl Outcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failed => "failed",
        }
    }
}

/// One attempt that actually spent time.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RunRecord {
    pub started_ms: i64,
    /// Wall-clock milliseconds from the start of the attempt to its end.
    pub duration_ms: i64,
    pub outcome: Outcome,
    /// Stable machine-readable code. Callers key on this; the prose beside it
    /// may be reworded.
    pub code: String,
    /// `scheduled`, `catch_up`, or `manual`.
    pub trigger: String,
    pub model: String,
    /// Characters of completion text produced. The text itself is not stored.
    pub output_chars: Option<usize>,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    /// Redacted sink label — never a webhook URL in full.
    pub sink: String,
    /// One truncated, redacted line about what went wrong.
    pub detail: Option<String>,
}

/// The most recent occurrence that came due and was not run.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SkipRecord {
    pub at_ms: i64,
    pub code: String,
    pub detail: String,
}

/// Counters that never age out.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Totals {
    /// Runs attempted: successes plus failures. Skips are counted separately.
    pub attempts: u64,
    pub succeeded: u64,
    pub failed: u64,
    pub skipped: u64,
    /// Occurrences that came due while the node was off, asleep, or busy,
    /// whether or not a catch-up run followed.
    pub missed_occurrences: u64,
    pub total_duration_ms: i64,
    pub completion_tokens: u64,
    pub last_success_ms: Option<i64>,
    pub last_failure_ms: Option<i64>,
}

/// Why a job is not running even though the jobs file enables it.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PauseReason {
    /// Somebody called the `pause` tool or its HTTP route.
    Requested,
    /// The job failed `quarantine_after_failures` times in a row and parked
    /// itself rather than spending more GPU time on a broken prompt.
    Quarantined,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Pause {
    pub reason: PauseReason,
    pub at_ms: i64,
    pub note: Option<String>,
}

/// Everything the scheduler remembers about one job.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct JobState {
    /// The occurrence this job is waiting for. `None` until the scheduler has
    /// seen the job once.
    pub next_due_ms: Option<i64>,
    /// The last occurrence the scheduler acted on, whether it ran or skipped.
    pub last_fire_ms: Option<i64>,
    pub consecutive_failures: u32,
    /// Instant before which the job will not be attempted again.
    pub backoff_until_ms: Option<i64>,
    pub totals: Totals,
    /// Skip counts keyed by reason code.
    #[serde(default)]
    pub skips: BTreeMap<String, u64>,
    #[serde(default)]
    pub last_skip: Option<SkipRecord>,
    /// Runs, newest last.
    #[serde(default)]
    pub history: VecDeque<RunRecord>,
    /// Runtime only: never written to disk, so a restart restores the jobs
    /// file's intent. See the module documentation.
    #[serde(skip)]
    pub pause: Option<Pause>,
}

impl JobState {
    pub fn last_run(&self) -> Option<&RunRecord> {
        self.history.back()
    }

    /// Record an attempt and roll it into the lifetime totals.
    pub fn record_run(&mut self, record: RunRecord, limit: usize) {
        self.totals.attempts += 1;
        self.totals.total_duration_ms = self
            .totals
            .total_duration_ms
            .saturating_add(record.duration_ms.max(0));
        match record.outcome {
            Outcome::Success => {
                self.totals.succeeded += 1;
                self.totals.last_success_ms = Some(record.started_ms);
                self.totals.completion_tokens = self
                    .totals
                    .completion_tokens
                    .saturating_add(record.completion_tokens.unwrap_or(0));
            }
            Outcome::Failed => {
                self.totals.failed += 1;
                self.totals.last_failure_ms = Some(record.started_ms);
            }
        }

        self.history.push_back(record);
        let limit = limit.clamp(1, MAX_HISTORY_PER_JOB);
        while self.history.len() > limit {
            self.history.pop_front();
        }
    }

    /// Count one occurrence that came due and was not run.
    pub fn record_skip(&mut self, at_ms: i64, code: &str, detail: &str) {
        self.totals.skipped += 1;
        if self.skips.len() < MAX_SKIP_REASONS || self.skips.contains_key(code) {
            *self.skips.entry(code.to_string()).or_insert(0) += 1;
        }
        self.last_skip = Some(SkipRecord {
            at_ms,
            code: code.to_string(),
            detail: truncate_detail(detail),
        });
    }
}

/// Trim a message for storage: one line, bounded, on a character boundary.
pub fn truncate_detail(detail: &str) -> String {
    let single_line = detail.replace(['\n', '\r'], " ");
    let trimmed = single_line.trim();
    if trimmed.chars().count() <= MAX_DETAIL_CHARS {
        return trimmed.to_string();
    }
    let kept: String = trimmed.chars().take(MAX_DETAIL_CHARS).collect();
    format!("{kept}…")
}

/// The persisted document.
#[derive(Debug, Deserialize, Serialize)]
struct StateDocument {
    v: u32,
    jobs: BTreeMap<String, JobState>,
}

/// The state directory, and the two operations performed on it.
///
/// Holds no file handles: every write goes to a sibling temp file and is
/// renamed over the target, so an interrupted write leaves the previous state
/// intact rather than a half-parsed file.
#[derive(Clone, Debug)]
pub struct Store {
    dir: PathBuf,
}

/// What loading the state file found, including what it could not read.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LoadedState {
    pub jobs: BTreeMap<String, JobState>,
    /// Set when a state file existed but could not be read. Surfaced by
    /// `status` rather than swallowed: history vanishing silently is how an
    /// operator ends up trusting a number that was reset yesterday.
    pub error: Option<String>,
    /// States dropped because the jobs file no longer declares that id.
    pub dropped_unknown_jobs: usize,
}

impl Store {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join(STATE_FILE)
    }

    /// Read the state file, keeping only states for ids the jobs file declares.
    ///
    /// A missing file is not an error — it is a node that has not run anything
    /// yet. An unreadable one is reported and then treated as empty, because
    /// refusing to start would let a corrupt history file take a node's
    /// automation offline permanently.
    pub fn load(&self, known_ids: &[String], history_limit: usize) -> LoadedState {
        let text = match fs::read_to_string(self.path()) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return LoadedState::default();
            }
            Err(error) => {
                return LoadedState {
                    error: Some(format!("reading {}: {error}", self.path().display())),
                    ..LoadedState::default()
                };
            }
        };

        let document: StateDocument = match serde_json::from_str(&text) {
            Ok(document) => document,
            Err(error) => {
                return LoadedState {
                    error: Some(format!(
                        "{} is not readable run history ({error}); starting from empty history. \
                         Totals and backoff state are lost, jobs are not.",
                        self.path().display()
                    )),
                    ..LoadedState::default()
                };
            }
        };
        if document.v != STATE_VERSION {
            return LoadedState {
                error: Some(format!(
                    "{} declares state version {} and this build writes {STATE_VERSION}; \
                     starting from empty history",
                    self.path().display(),
                    document.v
                )),
                ..LoadedState::default()
            };
        }

        let before = document.jobs.len();
        let mut jobs = document.jobs;
        jobs.retain(|id, _| known_ids.iter().any(|known| known == id));
        let limit = history_limit.clamp(1, MAX_HISTORY_PER_JOB);
        for state in jobs.values_mut() {
            while state.history.len() > limit {
                state.history.pop_front();
            }
            // A pause never persists. Make that true even for a file somebody
            // wrote by hand.
            state.pause = None;
        }

        LoadedState {
            dropped_unknown_jobs: before - jobs.len(),
            jobs,
            error: None,
        }
    }

    /// Write the state file atomically.
    pub fn save(&self, jobs: &BTreeMap<String, JobState>) -> Result<()> {
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating state directory {}", self.dir.display()))?;
        let document = StateDocument {
            v: STATE_VERSION,
            jobs: jobs.clone(),
        };
        let body = serde_json::to_vec_pretty(&document).context("serializing run history")?;

        let path = self.path();
        let temp = path.with_extension("json.tmp");
        {
            let mut file =
                File::create(&temp).with_context(|| format!("creating {}", temp.display()))?;
            file.write_all(&body)
                .with_context(|| format!("writing {}", temp.display()))?;
            file.sync_all()
                .with_context(|| format!("syncing {}", temp.display()))?;
        }
        fs::rename(&temp, &path)
            .with_context(|| format!("replacing {} with {}", path.display(), temp.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(started_ms: i64, outcome: Outcome) -> RunRecord {
        RunRecord {
            started_ms,
            duration_ms: 1_500,
            outcome,
            code: "ok".into(),
            trigger: "scheduled".into(),
            model: "qwen3:8b".into(),
            output_chars: Some(420),
            prompt_tokens: Some(30),
            completion_tokens: Some(120),
            sink: "file:daily.md (text)".into(),
            detail: None,
        }
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tdcc-scheduled-prompts-{tag}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn history_is_bounded_and_keeps_the_newest_runs() {
        let mut state = JobState::default();

        for index in 0..50 {
            state.record_run(record(index, Outcome::Success), 5);
        }

        assert_eq!(state.history.len(), 5, "the detailed list is bounded");
        assert_eq!(state.history.front().expect("front").started_ms, 45);
        assert_eq!(state.last_run().expect("back").started_ms, 49);
        // The rollup remembers everything the detailed list dropped.
        assert_eq!(state.totals.attempts, 50);
        assert_eq!(state.totals.succeeded, 50);
        assert_eq!(state.totals.completion_tokens, 50 * 120);
        assert_eq!(state.totals.total_duration_ms, 50 * 1_500);
    }

    #[test]
    fn the_history_limit_is_clamped_to_something_sane() {
        let mut state = JobState::default();
        for index in 0..10 {
            // A caller asking for zero, or for a million, gets neither.
            state.record_run(record(index, Outcome::Success), 0);
        }
        assert_eq!(state.history.len(), 1);

        let mut generous = JobState::default();
        for index in 0..(MAX_HISTORY_PER_JOB + 20) {
            generous.record_run(record(index as i64, Outcome::Success), usize::MAX);
        }
        assert_eq!(generous.history.len(), MAX_HISTORY_PER_JOB);
    }

    #[test]
    fn outcomes_land_in_the_right_counters() {
        let mut state = JobState::default();

        state.record_run(record(10, Outcome::Success), 10);
        state.record_run(record(20, Outcome::Failed), 10);

        assert_eq!(state.totals.succeeded, 1);
        assert_eq!(state.totals.failed, 1);
        assert_eq!(state.totals.last_success_ms, Some(10));
        assert_eq!(state.totals.last_failure_ms, Some(20));
        // A failed run contributes no tokens to the rollup.
        assert_eq!(state.totals.completion_tokens, 120);
    }

    #[test]
    fn skips_are_counted_by_reason_and_never_crowd_out_the_run_history() {
        let mut state = JobState::default();
        state.record_run(record(1, Outcome::Success), 3);

        // A half-hourly job with an overnight window, one ordinary day.
        for index in 0..32 {
            state.record_skip(1_000 + index, "skipped_window", "outside 22:00-06:00");
        }

        assert_eq!(state.history.len(), 1, "the real run is still there");
        assert_eq!(state.totals.skipped, 32);
        assert_eq!(state.skips.get("skipped_window"), Some(&32));
        let last = state.last_skip.expect("the most recent skip is kept");
        assert_eq!(last.code, "skipped_window");
        assert_eq!(last.at_ms, 1_031);
    }

    #[test]
    fn the_skip_reason_map_is_bounded() {
        let mut state = JobState::default();

        for index in 0..(MAX_SKIP_REASONS * 3) {
            state.record_skip(index as i64, &format!("reason_{index}"), "detail");
        }

        assert_eq!(state.skips.len(), MAX_SKIP_REASONS);
        assert_eq!(
            state.totals.skipped,
            (MAX_SKIP_REASONS * 3) as u64,
            "the total still counts every skip"
        );
    }

    #[test]
    fn details_are_one_line_and_bounded_on_a_character_boundary() {
        assert_eq!(truncate_detail("  boom \n at line 2  "), "boom   at line 2");

        let long = "é".repeat(1_000);
        let truncated = truncate_detail(&long);
        assert_eq!(truncated.chars().count(), MAX_DETAIL_CHARS + 1);
        assert!(truncated.ends_with('…'));
    }

    #[test]
    fn state_round_trips_through_a_real_directory() {
        let dir = scratch("store");
        let store = Store::new(&dir);
        let ids = vec!["digest".to_string()];

        assert_eq!(store.load(&ids, 20), LoadedState::default());

        let mut state = JobState {
            next_due_ms: Some(1_700_000_000_000),
            consecutive_failures: 3,
            backoff_until_ms: Some(1_700_000_060_000),
            ..JobState::default()
        };
        state.record_run(record(1, Outcome::Success), 20);
        state.record_skip(2, "skipped_window", "outside 22:00-06:00");
        let mut jobs = BTreeMap::new();
        jobs.insert("digest".to_string(), state);
        store.save(&jobs).expect("state writes");

        let loaded = store.load(&ids, 20);
        assert_eq!(loaded.error, None);
        let restored = loaded.jobs.get("digest").expect("present");
        assert_eq!(
            restored.consecutive_failures, 3,
            "backoff is a measurement and survives a restart"
        );
        assert_eq!(restored.history.len(), 1);
        assert_eq!(restored.skips.get("skipped_window"), Some(&1));

        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn a_pause_does_not_survive_a_write_and_read_cycle() {
        let dir = scratch("pause");
        let store = Store::new(&dir);
        let mut jobs = BTreeMap::new();
        jobs.insert(
            "digest".to_string(),
            JobState {
                pause: Some(Pause {
                    reason: PauseReason::Requested,
                    at_ms: 42,
                    note: Some("by hand".into()),
                }),
                ..JobState::default()
            },
        );

        store.save(&jobs).expect("state writes");
        let loaded = store.load(&["digest".to_string()], 20);

        assert_eq!(
            loaded.jobs.get("digest").expect("present").pause,
            None,
            "the jobs file is the only durable statement of what runs"
        );
        let raw = fs::read_to_string(store.path()).expect("readable");
        assert!(
            !raw.contains("pause"),
            "a pause must not reach the disk: {raw}"
        );

        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn state_for_a_job_the_file_no_longer_declares_is_dropped_and_counted() {
        let dir = scratch("prune");
        let store = Store::new(&dir);
        let mut jobs = BTreeMap::new();
        jobs.insert("kept".to_string(), JobState::default());
        jobs.insert("removed".to_string(), JobState::default());
        store.save(&jobs).expect("state writes");

        let loaded = store.load(&["kept".to_string()], 20);

        assert_eq!(loaded.jobs.len(), 1);
        assert_eq!(loaded.dropped_unknown_jobs, 1);

        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn an_unreadable_state_file_is_reported_rather_than_hidden_or_fatal() {
        let dir = scratch("corrupt");
        fs::create_dir_all(&dir).expect("mkdir");
        let store = Store::new(&dir);
        fs::write(store.path(), "{ not json").expect("write");

        let loaded = store.load(&["digest".to_string()], 20);

        assert!(loaded.jobs.is_empty());
        let error = loaded.error.expect("the problem is surfaced");
        assert!(error.contains("run history"), "{error}");

        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn a_state_file_from_a_future_version_is_refused_rather_than_misread() {
        let dir = scratch("version");
        fs::create_dir_all(&dir).expect("mkdir");
        let store = Store::new(&dir);
        fs::write(store.path(), r#"{"v":99,"jobs":{}}"#).expect("write");

        let loaded = store.load(&[], 20);

        assert!(loaded.error.expect("reported").contains("state version 99"));

        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn a_long_history_is_trimmed_on_load_when_the_limit_shrinks() {
        let dir = scratch("trim");
        let store = Store::new(&dir);
        let mut state = JobState::default();
        for index in 0..30 {
            state.record_run(record(index, Outcome::Success), 30);
        }
        let mut jobs = BTreeMap::new();
        jobs.insert("digest".to_string(), state);
        store.save(&jobs).expect("state writes");

        let loaded = store.load(&["digest".to_string()], 5);

        assert_eq!(loaded.jobs["digest"].history.len(), 5);
        assert_eq!(
            loaded.jobs["digest"].totals.attempts, 30,
            "trimming the detail must not touch the rollup"
        );

        fs::remove_dir_all(&dir).expect("cleanup");
    }
}
