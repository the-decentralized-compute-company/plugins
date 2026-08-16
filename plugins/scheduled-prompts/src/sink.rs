//! Where a completion goes: a file under the output root, or one POST to a
//! webhook the operator configured.
//!
//! # Blast radius
//!
//! **Filesystem.** Writes happen only beneath the output directory
//! (`--output-dir`, `<state-dir>/out` by default), and only to paths built from
//! plain names by [`crate::jobs::validate_relative_path`]. That is confinement
//! by construction; this module adds the second layer, canonicalizing the
//! resolved parent and refusing anything that is not inside the canonical root.
//! Nothing outside that tree is read, written, or created.
//!
//! **Network.** One POST per run, to the URL held in the environment variable
//! the job named. No redirects are followed: a webhook that answers 302 is a
//! misconfiguration, and following it would post the model's output somewhere
//! the operator never named.
//!
//! **Growth.** A file sink is capped at [`MAX_SINK_FILE_BYTES`]. When the next
//! record would cross the cap the current file is rotated to `<name>.1`,
//! replacing any previous `.1`. Disk use per sink is therefore bounded at
//! roughly twice the cap, forever, with no cron job and no operator action.
//!
//! # One attempt per run
//!
//! Delivery is not retried inside a run. A failed delivery fails the run, the
//! job backs off, and the next attempt is the next scheduled occurrence — see
//! [`crate::decide`]. Retrying inside a run would hold a concurrency permit
//! while sleeping, and the job-level backoff already covers the case the retry
//! would have.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::{Client, Url};
use serde_json::{Value, json};

use crate::clock::format_utc;
use crate::jobs::{FileFormat, Sink, redact_url};

/// Cap on one sink file before it is rotated.
pub const MAX_SINK_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// How long one webhook POST may take.
pub const WEBHOOK_TIMEOUT: Duration = Duration::from_secs(30);

/// Longest snippet of a failing webhook response quoted back.
pub const MAX_WEBHOOK_BODY_CHARS: usize = 200;

/// What one run produced, in the form both sinks render from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunPayload {
    pub job_id: String,
    pub trigger: String,
    /// The model the job asked for.
    pub model: String,
    /// The model the endpoint says answered, when it says.
    pub answered_by: Option<String>,
    pub started_ms: i64,
    pub duration_ms: i64,
    pub text: String,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
}

/// What a successful delivery did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delivered {
    /// Redacted description of the destination, safe for a tool response.
    pub target: String,
    pub bytes_written: usize,
    /// True when the file sink rotated to make room for this record.
    pub rotated: bool,
    pub http_status: Option<u16>,
}

/// The JSON object a webhook receives and a `jsonl` file records.
///
/// One shape for both, so a webhook payload and a line in a file can be read by
/// the same code.
pub fn render_json(payload: &RunPayload) -> Value {
    json!({
        "plugin": crate::config::PLUGIN_NAME,
        "job": payload.job_id,
        "trigger": payload.trigger,
        "model": payload.model,
        "answered_by": payload.answered_by,
        "started_ms": payload.started_ms,
        "started_utc": format_utc(payload.started_ms),
        "duration_ms": payload.duration_ms,
        "prompt_tokens": payload.prompt_tokens,
        "completion_tokens": payload.completion_tokens,
        "output_chars": payload.text.chars().count(),
        "output": payload.text,
    })
}

/// The block appended to a `text` file sink.
///
/// A header that says which job, when, and with what — so a file holding a
/// month of a daily digest is readable without a second tool.
pub fn render_text(payload: &RunPayload) -> String {
    let answered_by = match &payload.answered_by {
        Some(model) if *model != payload.model => format!("{} (asked {})", model, payload.model),
        _ => payload.model.clone(),
    };
    format!(
        "## {} — {} ({}, {})\n\n{}\n\n",
        format_utc(payload.started_ms),
        payload.job_id,
        payload.trigger,
        answered_by,
        payload.text.trim_end()
    )
}

/// Deliver one run's output to its sink.
pub async fn deliver(
    client: &Client,
    sink: &Sink,
    root: &Path,
    payload: &RunPayload,
) -> Result<Delivered, String> {
    match sink {
        Sink::File { relative, format } => write_file(root, relative, *format, payload),
        Sink::Webhook { url, url_env } => post_webhook(client, url, url_env, payload).await,
    }
}

/// Append one record to a file beneath `root`, rotating at the size cap.
pub fn write_file(
    root: &Path,
    relative: &str,
    format: FileFormat,
    payload: &RunPayload,
) -> Result<Delivered, String> {
    let path = confined_path(root, relative)?;

    let body = match format {
        FileFormat::Text => render_text(payload),
        FileFormat::Jsonl => {
            let mut line = serde_json::to_string(&render_json(payload))
                .map_err(|error| format!("rendering the run as JSON failed: {error}"))?;
            line.push('\n');
            line
        }
    };

    let existing = fs::metadata(&path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let rotated = existing > 0 && existing.saturating_add(body.len() as u64) > MAX_SINK_FILE_BYTES;
    if rotated {
        let rolled = rotated_path(&path);
        fs::rename(&path, &rolled).map_err(|error| {
            format!(
                "rotating {} to {} failed: {error}",
                path.display(),
                rolled.display()
            )
        })?;
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| format!("opening {} failed: {error}", path.display()))?;
    file.write_all(body.as_bytes())
        .map_err(|error| format!("writing {} failed: {error}", path.display()))?;
    file.sync_data()
        .map_err(|error| format!("syncing {} failed: {error}", path.display()))?;

    Ok(Delivered {
        target: format!("file:{relative}"),
        bytes_written: body.len(),
        rotated,
        http_status: None,
    })
}

fn rotated_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".1");
    path.with_file_name(name)
}

/// Resolve a relative sink path beneath `root`, and prove it stayed there.
///
/// The path was already built from plain names by
/// [`crate::jobs::validate_relative_path`], so neither check here normally
/// fires. They exist because confinement enforced in one place is a rule, and
/// confinement enforced independently is a property: a future edit to the
/// validator cannot quietly widen what reaches the filesystem.
///
/// Two checks, in this order, and the lexical one comes first so that a path
/// which would escape never causes a directory to be created outside the root:
///
/// 1. **Lexical.** Every segment must be a plain name — no empty segment (a
///    leading `/`), no `.` or `..`, no backslash, no `:` (a Windows drive or
///    stream), because `Path::push` of an absolute component silently replaces
///    everything before it.
/// 2. **Canonical.** The resolved parent directory must still be inside the
///    canonicalized root, which is what catches a symlink pointing out of it.
pub fn confined_path(root: &Path, relative: &str) -> Result<PathBuf, String> {
    fs::create_dir_all(root).map_err(|error| {
        format!(
            "creating output directory {} failed: {error}",
            root.display()
        )
    })?;
    let canonical_root = fs::canonicalize(root).map_err(|error| {
        format!(
            "resolving output directory {} failed: {error}",
            root.display()
        )
    })?;

    let mut path = canonical_root.clone();
    for component in relative.split('/') {
        if component.is_empty()
            || component == "."
            || component == ".."
            || component.contains('\\')
            || component.contains(':')
        {
            return Err(format!(
                "sink path {relative} contains the segment {component:?}, which could leave the \
                 output directory; refusing to write there"
            ));
        }
        path.push(component);
    }
    let parent = path
        .parent()
        .ok_or_else(|| format!("sink path {relative} has no parent directory"))?
        .to_path_buf();
    fs::create_dir_all(&parent)
        .map_err(|error| format!("creating {} failed: {error}", parent.display()))?;

    let canonical_parent = fs::canonicalize(&parent)
        .map_err(|error| format!("resolving {} failed: {error}", parent.display()))?;
    if !canonical_parent.starts_with(&canonical_root) {
        return Err(format!(
            "sink path {relative} resolves to {} which is outside the output directory {}; \
             refusing to write there",
            canonical_parent.display(),
            canonical_root.display()
        ));
    }

    let file_name = path
        .file_name()
        .ok_or_else(|| format!("sink path {relative} does not name a file"))?
        .to_os_string();
    Ok(canonical_parent.join(file_name))
}

/// POST one run's output to a webhook. One attempt; see the module docs.
pub async fn post_webhook(
    client: &Client,
    url: &Url,
    url_env: &str,
    payload: &RunPayload,
) -> Result<Delivered, String> {
    let target = format!("webhook:{} via {url_env}", redact_url(url));

    let response = client
        .post(url.clone())
        .timeout(WEBHOOK_TIMEOUT)
        .json(&render_json(payload))
        .send()
        .await
        .map_err(|error| {
            // `without_url` strips the URL reqwest embeds in its Display
            // output; the webhook URL is a bearer credential.
            format!("{target} could not be reached: {}", error.without_url())
        })?;

    let status = response.status();
    if status.is_success() {
        return Ok(Delivered {
            target,
            bytes_written: 0,
            rotated: false,
            http_status: Some(status.as_u16()),
        });
    }

    let body = response.text().await.unwrap_or_default();
    Err(format!(
        "{target} answered HTTP {}{}",
        status.as_u16(),
        match truncate_body(&scrub(&body, url)) {
            body if body.is_empty() => String::new(),
            body => format!(": {body}"),
        }
    ))
}

/// Remove anything URL-shaped from text that came back from the endpoint.
///
/// A webhook URL is a bearer credential and its **path** is usually the secret
/// part — Slack answers `invalid_token for /services/T0/B0/…`, quoting the
/// token straight back at you. So the full URL, the full path, the query, and
/// every path segment long enough to be a token are replaced before the body is
/// quoted anywhere.
pub fn scrub(text: &str, url: &Url) -> String {
    /// Below this, a segment is a word like `hooks` rather than a credential,
    /// and replacing it would mangle the message without protecting anything.
    const MIN_SECRET_SEGMENT: usize = 8;

    let mut scrubbed = text.replace(url.as_str(), &redact_url(url));
    let path = url.path();
    if path.len() > 1 {
        scrubbed = scrubbed.replace(path, "/[redacted]");
    }
    if let Some(query) = url.query()
        && !query.is_empty()
    {
        scrubbed = scrubbed.replace(query, "[redacted]");
    }
    for segment in path.split('/') {
        if segment.len() >= MIN_SECRET_SEGMENT {
            scrubbed = scrubbed.replace(segment, "[redacted]");
        }
    }
    scrubbed
}

fn truncate_body(body: &str) -> String {
    let trimmed = body.trim().replace(['\n', '\r'], " ");
    if trimmed.chars().count() <= MAX_WEBHOOK_BODY_CHARS {
        return trimmed;
    }
    trimmed.chars().take(MAX_WEBHOOK_BODY_CHARS).collect()
}

/// The client used for webhook delivery.
pub fn build_client() -> reqwest::Result<Client> {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!(
            "tdcc-scheduled-prompts/",
            env!("CARGO_PKG_VERSION")
        ))
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn payload(text: &str) -> RunPayload {
        RunPayload {
            job_id: "digest".into(),
            trigger: "scheduled".into(),
            model: "qwen3:8b".into(),
            answered_by: Some("qwen3:8b".into()),
            started_ms: 1_772_334_000_000,
            duration_ms: 4_200,
            text: text.into(),
            prompt_tokens: Some(31),
            completion_tokens: Some(12),
        }
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tdcc-scheduled-prompts-sink-{tag}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn the_text_rendering_says_which_job_when_and_with_what() {
        let rendered = render_text(&payload("Two things happened.\n"));

        assert!(rendered.starts_with("## 2026-03-01T03:00:00Z — digest (scheduled, qwen3:8b)"));
        assert!(rendered.contains("Two things happened."));
        assert!(rendered.ends_with("\n\n"), "records stay separated");
    }

    #[test]
    fn a_model_that_differs_from_the_one_asked_for_is_named() {
        let mut payload = payload("text");
        payload.answered_by = Some("qwen3:8b-q4".into());

        let rendered = render_text(&payload);

        assert!(
            rendered.contains("qwen3:8b-q4 (asked qwen3:8b)"),
            "{rendered}"
        );
    }

    #[test]
    fn the_json_rendering_carries_the_run_and_its_output() {
        let value = render_json(&payload("done"));

        assert_eq!(value["job"], "digest");
        assert_eq!(value["model"], "qwen3:8b");
        assert_eq!(value["output"], "done");
        assert_eq!(value["output_chars"], 4);
        assert_eq!(value["started_utc"], "2026-03-01T03:00:00Z");
        assert_eq!(value["completion_tokens"], 12);
    }

    #[test]
    fn a_text_sink_appends_rather_than_replacing() {
        let root = scratch("append");

        write_file(
            &root,
            "reports/daily.md",
            FileFormat::Text,
            &payload("first"),
        )
        .expect("first write");
        let second = write_file(
            &root,
            "reports/daily.md",
            FileFormat::Text,
            &payload("second"),
        )
        .expect("second write");

        let body = fs::read_to_string(root.join("reports").join("daily.md")).expect("readable");
        assert!(body.contains("first"));
        assert!(body.contains("second"));
        assert_eq!(second.target, "file:reports/daily.md");
        assert!(!second.rotated);

        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn a_jsonl_sink_writes_one_parseable_object_per_line() {
        let root = scratch("jsonl");

        write_file(&root, "runs.jsonl", FileFormat::Jsonl, &payload("one")).expect("write");
        write_file(&root, "runs.jsonl", FileFormat::Jsonl, &payload("two")).expect("write");

        let body = fs::read_to_string(root.join("runs.jsonl")).expect("readable");
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in lines {
            let value: Value = serde_json::from_str(line).expect("each line is JSON");
            assert_eq!(value["job"], "digest");
        }

        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn a_file_sink_rotates_instead_of_growing_without_bound() {
        let root = scratch("rotate");
        let path = root.join("big.md");
        fs::create_dir_all(&root).expect("mkdir");
        // One byte under the cap, so the next record must cross it.
        fs::write(&path, vec![b'x'; MAX_SINK_FILE_BYTES as usize - 1]).expect("seed");

        let delivered =
            write_file(&root, "big.md", FileFormat::Text, &payload("after")).expect("write");

        assert!(delivered.rotated, "the cap must trigger a rotation");
        assert!(root.join("big.md.1").exists(), "the old file is kept once");
        let fresh = fs::read_to_string(&path).expect("readable");
        assert!(fresh.contains("after"));
        assert!(
            fresh.len() < 1_000,
            "the new file starts empty rather than continuing"
        );

        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn a_second_rotation_replaces_the_first_so_disk_use_stays_bounded() {
        let root = scratch("rotate-twice");
        fs::create_dir_all(&root).expect("mkdir");
        let path = root.join("big.md");

        for round in ["first", "second"] {
            fs::write(&path, vec![b'x'; MAX_SINK_FILE_BYTES as usize - 1]).expect("seed");
            write_file(&root, "big.md", FileFormat::Text, &payload(round)).expect("write");
        }

        let entries: Vec<String> = fs::read_dir(&root)
            .expect("readable")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            entries.len(),
            2,
            "one live file and one rotation, never three: {entries:?}"
        );

        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn the_second_confinement_layer_refuses_a_path_that_escapes_the_root() {
        let root = scratch("confine");
        fs::create_dir_all(&root).expect("mkdir");

        // None of these gets past `jobs::validate_relative_path`. This proves
        // the sink refuses them on its own, so the two layers are genuinely
        // independent rather than one check written twice.
        for relative in [
            "../escape.md",
            "sub/../../escape.md",
            "/etc/passwd",
            "C:/Windows/win.ini",
            "sub\\escape.md",
            "./escape.md",
        ] {
            match confined_path(&root, relative) {
                Ok(path) => panic!(
                    "{relative} resolved to {} instead of being refused",
                    path.display()
                ),
                Err(error) => assert!(error.contains("output directory"), "{relative} -> {error}"),
            }
        }
        // And nothing was created outside the root on the way to refusing.
        assert!(!root.parent().expect("a parent").join("escape.md").exists());

        fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn a_legitimate_path_resolves_inside_the_canonical_root() {
        let root = scratch("resolve");

        let path = confined_path(&root, "reports/2026/daily.md").expect("valid");

        let canonical_root = fs::canonicalize(&root).expect("root exists");
        assert!(path.starts_with(&canonical_root), "{path:?}");
        assert!(path.ends_with("daily.md"));

        fs::remove_dir_all(&root).expect("cleanup");
    }

    // -----------------------------------------------------------------------
    // Webhook delivery against a real socket.
    // -----------------------------------------------------------------------

    async fn serve_once(status: u16, body: &'static str) -> (Url, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let handle = tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return String::new();
            };
            let mut buffer = vec![0u8; 65_536];
            let read = socket.read(&mut buffer).await.unwrap_or(0);
            let request = String::from_utf8_lossy(&buffer[..read]).into_owned();
            let response = format!(
                "HTTP/1.1 {status} Scripted\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
            request
        });
        (
            Url::parse(&format!(
                "http://127.0.0.1:{port}/services/T0/B0/XXXXsecretXXXX"
            ))
            .expect("valid url"),
            handle,
        )
    }

    #[tokio::test]
    async fn a_webhook_receives_the_run_as_json() {
        let (url, served) = serve_once(204, "").await;

        let delivered = post_webhook(
            &build_client().expect("client"),
            &url,
            "TDCC_SCHEDULED_PROMPTS_WEBHOOK_X",
            &payload("done"),
        )
        .await
        .expect("204 is a success");

        assert_eq!(delivered.http_status, Some(204));
        let request = served.await.expect("the listener finished");
        assert!(request.contains("POST /services/"), "{request}");
        assert!(request.contains("\"job\":\"digest\""), "{request}");
        assert!(request.contains("\"output\":\"done\""), "{request}");
    }

    #[tokio::test]
    async fn a_webhook_failure_names_the_status_without_the_url() {
        let (url, _served) =
            serve_once(403, "invalid_token for /services/T0/B0/XXXXsecretXXXX").await;

        let error = post_webhook(
            &build_client().expect("client"),
            &url,
            "TDCC_SCHEDULED_PROMPTS_WEBHOOK_X",
            &payload("done"),
        )
        .await
        .expect_err("403 is a failure");

        assert!(error.contains("403"), "{error}");
        assert!(error.contains("[redacted]"), "{error}");
        assert!(
            !error.contains("XXXXsecretXXXX"),
            "the webhook secret leaked: {error}"
        );
        // The useful half of the message survives the scrubbing.
        assert!(error.contains("invalid_token"), "{error}");
    }

    #[test]
    fn scrubbing_removes_the_url_its_path_and_any_token_shaped_segment() {
        let url = Url::parse("https://hooks.slack.com/services/T0123456/B0123456/XXXXsecretXXXX")
            .expect("valid url");

        for body in [
            "invalid_token for /services/T0123456/B0123456/XXXXsecretXXXX",
            "no such hook: https://hooks.slack.com/services/T0123456/B0123456/XXXXsecretXXXX",
            "the id XXXXsecretXXXX was revoked",
        ] {
            let scrubbed = scrub(body, &url);
            assert!(!scrubbed.contains("XXXXsecretXXXX"), "{body} -> {scrubbed}");
            assert!(!scrubbed.contains("T0123456"), "{body} -> {scrubbed}");
        }

        // A short, ordinary word is left alone, so the message stays readable.
        assert_eq!(scrub("rate limited", &url), "rate limited");
    }

    #[tokio::test]
    async fn an_unreachable_webhook_fails_without_printing_the_url() {
        let port = {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            listener.local_addr().expect("addr").port()
        };
        let url = Url::parse(&format!("http://127.0.0.1:{port}/services/XXXXsecretXXXX"))
            .expect("valid url");

        let error = post_webhook(
            &build_client().expect("client"),
            &url,
            "TDCC_SCHEDULED_PROMPTS_WEBHOOK_X",
            &payload("done"),
        )
        .await
        .expect_err("nothing is listening");

        assert!(!error.contains("XXXXsecretXXXX"), "{error}");
        assert!(
            error.contains("TDCC_SCHEDULED_PROMPTS_WEBHOOK_X"),
            "{error}"
        );
    }
}
