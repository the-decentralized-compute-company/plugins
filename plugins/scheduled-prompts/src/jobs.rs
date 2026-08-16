//! The jobs file: the only place a job can come from.
//!
//! A job pairs a schedule with a prompt, a model, and a destination. All four
//! are written by the operator, in a file they own, and **no tool in this
//! plugin can add, edit, or delete one**. The reasoning is in README.md under
//! "Why a model cannot create a job"; the enforcement is that there is simply
//! no code path from a tool call to this module's output.
//!
//! Everything here is a pure function of the file text and the environment, so
//! every rule below is covered by a test that never touches a real machine.
//!
//! # What is rejected, and why
//!
//! * **Unknown keys.** `scheduel = "0 3 * * *"` stops the file loading with a
//!   line number. A typo that silently disabled a job would be indistinguishable
//!   from a node that was simply never busy.
//! * **A schedule that can never fire inside its window.** `0 12 * * *` with
//!   `window = "22:00-06:00"` is a contradiction; finding it when the file is
//!   read beats discovering it after a week of nothing happening.
//! * **A file sink that could escape the output root.** Paths are relative,
//!   `/`-separated, and built from plain names only. There is no input that
//!   reaches an absolute path, a parent directory, a drive letter, a UNC share,
//!   or a Windows device name.
//! * **A webhook URL written into the file.** The file names an *environment
//!   variable*; the URL itself is read from the environment of the `tdcc`
//!   process. A Slack or Discord webhook URL is a bearer credential, and this
//!   file is the kind of thing people paste into an issue.

use std::collections::BTreeSet;

use reqwest::Url;
use serde::Deserialize;

use crate::clock::{HourWindow, Zone, first_occurrence_inside};
use crate::config::EnvMap;
use crate::cron::Schedule;

/// The only file version this build understands.
pub const SUPPORTED_VERSION: u32 = 1;

pub const MAX_JOBS: usize = 64;
pub const MAX_ID_CHARS: usize = 64;
pub const MAX_PROMPT_CHARS: usize = 32_768;
pub const MAX_SYSTEM_CHARS: usize = 8_192;
pub const MAX_DESCRIPTION_CHARS: usize = 240;

pub const DEFAULT_TIMEOUT_SECS: u64 = 300;
pub const MIN_TIMEOUT_SECS: u64 = 5;
pub const MAX_TIMEOUT_SECS: u64 = 3_600;

pub const DEFAULT_MAX_CONCURRENT_RUNS: u32 = 1;
pub const MAX_MAX_CONCURRENT_RUNS: u32 = 8;

pub const DEFAULT_HISTORY_PER_JOB: usize = 20;
pub const MAX_HISTORY_PER_JOB: usize = 200;

pub const DEFAULT_CATCH_UP_GRACE_SECS: u64 = 3_600;
pub const MAX_CATCH_UP_GRACE_SECS: u64 = 86_400;

pub const DEFAULT_QUARANTINE_AFTER_FAILURES: u32 = 10;
pub const MAX_QUARANTINE_AFTER_FAILURES: u32 = 10_000;

pub const MAX_OUTPUT_TOKENS_CEILING: u32 = 131_072;

/// Depth and length limits on a file sink's relative path.
pub const MAX_PATH_COMPONENTS: usize = 8;
pub const MAX_PATH_COMPONENT_CHARS: usize = 64;

/// What to do about occurrences that were missed while the node was asleep,
/// switched off, or busy.
///
/// There is deliberately **no** "run every missed occurrence" option. See
/// README.md > "Misfire policy"; the short version is that a laptop that wakes
/// after a week owes an `@hourly` job 168 runs, and delivering them is a way to
/// melt a machine somebody lent you.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Misfire {
    /// Run once on the next tick, if the missed occurrence is still fresh
    /// enough to be worth running. The default.
    RunOnce,
    /// Do not catch up at all; wait for the next scheduled occurrence.
    Skip,
}

impl Misfire {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "run_once" | "coalesce" => Ok(Self::RunOnce),
            "skip" => Ok(Self::Skip),
            other => Err(format!(
                "unknown misfire policy \"{other}\"; this plugin understands \"run_once\" (one \
                 catch-up run, if it is still fresh) and \"skip\" (wait for the next \
                 occurrence). There is no \"run_all\": see README.md > Misfire policy."
            )),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RunOnce => "run_once",
            Self::Skip => "skip",
        }
    }
}

/// How a file sink records a run.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileFormat {
    /// A dated header and the completion text. Meant to be read by a person.
    Text,
    /// One JSON object per line: job id, timestamps, model, usage, and the
    /// completion. Meant to be read by something else.
    Jsonl,
}

impl FileFormat {
    fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "text" | "markdown" | "md" => Ok(Self::Text),
            "jsonl" | "json" | "ndjson" => Ok(Self::Jsonl),
            other => Err(format!(
                "unknown file format \"{other}\"; expected \"text\" or \"jsonl\""
            )),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Jsonl => "jsonl",
        }
    }
}

/// Where a run's output goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Sink {
    File {
        /// Validated relative path, `/`-separated, always resolved beneath the
        /// configured output root.
        relative: String,
        format: FileFormat,
    },
    Webhook {
        url: Url,
        /// Name of the environment variable the URL came from. Reported instead
        /// of the URL wherever a human or a model can see it.
        url_env: String,
    },
}

impl Sink {
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::File { .. } => "file",
            Self::Webhook { .. } => "webhook",
        }
    }

    /// A description safe to put in a tool response, a log line, or an error.
    ///
    /// A webhook URL is a bearer credential — anyone holding a Slack or Discord
    /// URL can post as the integration — so it is never rendered in full.
    pub fn label(&self) -> String {
        match self {
            Self::File { relative, format } => format!("file:{relative} ({})", format.as_str()),
            Self::Webhook { url, url_env } => {
                format!("webhook:{} via {url_env}", redact_url(url))
            }
        }
    }
}

/// Reduce a URL to scheme, host, port, and a marker — never a path, query, or
/// fragment, which is where webhook secrets live.
pub fn redact_url(url: &Url) -> String {
    let host = url.host_str().unwrap_or("<no host>");
    let authority = match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    };
    let has_secret_bearing_tail = url.path().trim_matches('/').is_empty()
        && url.query().is_none()
        && url.fragment().is_none();
    if has_secret_bearing_tail {
        format!("{}://{authority}/", url.scheme())
    } else {
        format!("{}://{authority}/[redacted]", url.scheme())
    }
}

/// One scheduled job, fully validated.
#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    pub id: String,
    pub description: Option<String>,
    pub schedule: Schedule,
    pub model: String,
    pub prompt: String,
    pub system: Option<String>,
    /// The file's own statement about whether this job runs. A tool can pause a
    /// job at runtime but can never turn this from `false` to `true`.
    pub enabled: bool,
    pub window: Option<HourWindow>,
    pub misfire: Misfire,
    pub catch_up_grace_ms: i64,
    pub timeout_secs: u64,
    pub max_output_tokens: Option<u32>,
    pub temperature: Option<f64>,
    /// Whether `run_now` may run this job outside its window. Off by default,
    /// and settable only here — not through a tool argument.
    pub manual_ignores_window: bool,
    /// Consecutive failures after which the job parks itself. 0 disables it.
    pub quarantine_after_failures: u32,
    pub sink: Sink,
}

/// A loaded jobs file.
#[derive(Clone, Debug, PartialEq)]
pub struct JobsFile {
    pub zone: Zone,
    pub max_concurrent_runs: u32,
    pub history_per_job: usize,
    pub jobs: Vec<Job>,
}

impl JobsFile {
    /// An empty file's worth of defaults, used when no jobs file exists.
    pub fn empty() -> Self {
        Self {
            zone: Zone::Local,
            max_concurrent_runs: DEFAULT_MAX_CONCURRENT_RUNS,
            history_per_job: DEFAULT_HISTORY_PER_JOB,
            jobs: Vec::new(),
        }
    }

    pub fn job(&self, id: &str) -> Option<&Job> {
        self.jobs.iter().find(|job| job.id == id)
    }
}

// ---------------------------------------------------------------------------
// The document as written
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    version: u32,
    timezone: Option<String>,
    window: Option<String>,
    max_concurrent_runs: Option<u32>,
    misfire: Option<String>,
    catch_up_grace_secs: Option<u64>,
    history_per_job: Option<usize>,
    #[serde(default)]
    job: Vec<RawJob>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawJob {
    id: String,
    schedule: String,
    model: String,
    prompt: String,
    description: Option<String>,
    system: Option<String>,
    enabled: Option<bool>,
    window: Option<String>,
    misfire: Option<String>,
    catch_up_grace_secs: Option<u64>,
    timeout_secs: Option<u64>,
    max_output_tokens: Option<u32>,
    temperature: Option<f64>,
    manual_ignores_window: Option<bool>,
    quarantine_after_failures: Option<u32>,
    sink: RawSink,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSink {
    kind: String,
    path: Option<String>,
    format: Option<String>,
    url_env: Option<String>,
}

/// Parse and validate a jobs file.
///
/// `env` supplies webhook URLs, and `now_ms` anchors the "can this schedule
/// ever fire inside its window?" check. Both are arguments rather than global
/// reads so the whole of this module is testable.
pub fn parse_jobs(text: &str, env: &EnvMap, now_ms: i64) -> Result<JobsFile, String> {
    let raw: RawFile = toml::from_str(text).map_err(|error| error.to_string())?;

    if raw.version != SUPPORTED_VERSION {
        return Err(format!(
            "jobs file declares version {} but this build of scheduled-prompts understands \
             version {SUPPORTED_VERSION}",
            raw.version
        ));
    }

    let zone = match raw.timezone.as_deref() {
        Some(value) => Zone::parse(value)?,
        None => Zone::Local,
    };
    let default_window = match raw.window.as_deref() {
        Some(value) => Some(HourWindow::parse(value)?),
        None => None,
    };
    let default_misfire = match raw.misfire.as_deref() {
        Some(value) => Misfire::parse(value)?,
        None => Misfire::RunOnce,
    };
    let default_grace = bounded_u64(
        raw.catch_up_grace_secs,
        DEFAULT_CATCH_UP_GRACE_SECS,
        0,
        MAX_CATCH_UP_GRACE_SECS,
        "catch_up_grace_secs",
    )?;
    let max_concurrent_runs = bounded_u32(
        raw.max_concurrent_runs,
        DEFAULT_MAX_CONCURRENT_RUNS,
        1,
        MAX_MAX_CONCURRENT_RUNS,
        "max_concurrent_runs",
    )?;
    let history_per_job = bounded_usize(
        raw.history_per_job,
        DEFAULT_HISTORY_PER_JOB,
        1,
        MAX_HISTORY_PER_JOB,
        "history_per_job",
    )?;

    if raw.job.len() > MAX_JOBS {
        return Err(format!(
            "jobs file declares {} jobs; the limit is {MAX_JOBS}. Every job is a standing claim \
             on this machine's GPU.",
            raw.job.len()
        ));
    }

    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut jobs = Vec::with_capacity(raw.job.len());
    for raw_job in raw.job {
        let job = validate_job(
            raw_job,
            JobDefaults {
                zone,
                window: default_window,
                misfire: default_misfire,
                grace_secs: default_grace,
            },
            env,
            now_ms,
        )?;
        if !seen.insert(job.id.clone()) {
            return Err(format!(
                "two jobs share the id \"{}\"; ids identify a job in every tool response and in \
                 the run history, so they have to be unique",
                job.id
            ));
        }
        jobs.push(job);
    }

    Ok(JobsFile {
        zone,
        max_concurrent_runs,
        history_per_job,
        jobs,
    })
}

#[derive(Clone, Copy)]
struct JobDefaults {
    zone: Zone,
    window: Option<HourWindow>,
    misfire: Misfire,
    grace_secs: u64,
}

fn validate_job(
    raw: RawJob,
    defaults: JobDefaults,
    env: &EnvMap,
    now_ms: i64,
) -> Result<Job, String> {
    let id = validate_id(&raw.id)?;
    let context = |message: String| format!("job \"{id}\": {message}");

    let schedule = Schedule::parse(&raw.schedule).map_err(context)?;
    let window = match raw.window.as_deref() {
        Some(value) => Some(HourWindow::parse(value).map_err(context)?),
        None => defaults.window,
    };
    let misfire = match raw.misfire.as_deref() {
        Some(value) => Misfire::parse(value).map_err(context)?,
        None => defaults.misfire,
    };

    let model = raw.model.trim().to_string();
    if model.is_empty() {
        return Err(context(
            "model is empty; name the model this prompt should be answered by, exactly as the \
             node's /v1/models lists it"
                .into(),
        ));
    }
    let prompt = raw.prompt.trim().to_string();
    if prompt.is_empty() {
        return Err(context("prompt is empty".into()));
    }
    if prompt.chars().count() > MAX_PROMPT_CHARS {
        return Err(context(format!(
            "prompt is {} characters; the limit is {MAX_PROMPT_CHARS}",
            prompt.chars().count()
        )));
    }
    let system = match raw
        .system
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(system) if system.chars().count() > MAX_SYSTEM_CHARS => {
            return Err(context(format!(
                "system message is {} characters; the limit is {MAX_SYSTEM_CHARS}",
                system.chars().count()
            )));
        }
        Some(system) => Some(system.to_string()),
        None => None,
    };
    let description = match raw
        .description
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(description) if description.chars().count() > MAX_DESCRIPTION_CHARS => {
            return Err(context(format!(
                "description is longer than {MAX_DESCRIPTION_CHARS} characters"
            )));
        }
        Some(description) => Some(description.to_string()),
        None => None,
    };

    let timeout_secs = bounded_u64(
        raw.timeout_secs,
        DEFAULT_TIMEOUT_SECS,
        MIN_TIMEOUT_SECS,
        MAX_TIMEOUT_SECS,
        "timeout_secs",
    )
    .map_err(context)?;
    let catch_up_grace_secs = bounded_u64(
        raw.catch_up_grace_secs,
        defaults.grace_secs,
        0,
        MAX_CATCH_UP_GRACE_SECS,
        "catch_up_grace_secs",
    )
    .map_err(context)?;
    let quarantine_after_failures = bounded_u32(
        raw.quarantine_after_failures,
        DEFAULT_QUARANTINE_AFTER_FAILURES,
        0,
        MAX_QUARANTINE_AFTER_FAILURES,
        "quarantine_after_failures",
    )
    .map_err(context)?;

    if let Some(tokens) = raw.max_output_tokens
        && (tokens == 0 || tokens > MAX_OUTPUT_TOKENS_CEILING)
    {
        return Err(context(format!(
            "max_output_tokens must be between 1 and {MAX_OUTPUT_TOKENS_CEILING}, got {tokens}"
        )));
    }
    if let Some(temperature) = raw.temperature
        && !(0.0..=2.0).contains(&temperature)
    {
        return Err(context(format!(
            "temperature must be between 0.0 and 2.0, got {temperature}"
        )));
    }

    let sink = validate_sink(&raw.sink, env).map_err(context)?;

    // A schedule and a window that never coincide is a contradiction the
    // operator wrote by accident. Catch it here, not after a silent week.
    if first_occurrence_inside(&schedule, defaults.zone, window.as_ref(), now_ms).is_none() {
        return Err(context(match window {
            Some(window) => format!(
                "schedule \"{schedule}\" never falls inside window \"{window}\", so this job \
                 could never run. Widen the window or change the schedule."
            ),
            None => format!("schedule \"{schedule}\" has no next occurrence, so it can never run"),
        }));
    }

    Ok(Job {
        id,
        description,
        schedule,
        model,
        prompt,
        system,
        enabled: raw.enabled.unwrap_or(true),
        window,
        misfire,
        catch_up_grace_ms: (catch_up_grace_secs as i64).saturating_mul(1_000),
        timeout_secs,
        max_output_tokens: raw.max_output_tokens,
        temperature: raw.temperature,
        manual_ignores_window: raw.manual_ignores_window.unwrap_or(false),
        quarantine_after_failures,
        sink,
    })
}

fn validate_id(raw: &str) -> Result<String, String> {
    let id = raw.trim();
    if id.is_empty() {
        return Err("a job has an empty id".into());
    }
    if id.chars().count() > MAX_ID_CHARS {
        return Err(format!(
            "job id \"{id}\" is longer than {MAX_ID_CHARS} characters"
        ));
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(format!(
            "job id \"{id}\" contains characters outside A-Z a-z 0-9 - _ . — an id appears in \
             tool arguments, file names, and log lines, so it stays boring on purpose"
        ));
    }
    Ok(id.to_string())
}

fn validate_sink(raw: &RawSink, env: &EnvMap) -> Result<Sink, String> {
    match raw.kind.trim().to_ascii_lowercase().as_str() {
        "file" => {
            if raw.url_env.is_some() {
                return Err(
                    "sink kind is \"file\" but url_env is set; url_env belongs to a \
                            webhook sink"
                        .into(),
                );
            }
            let Some(path) = raw.path.as_deref() else {
                return Err("sink kind is \"file\" but no path was given".into());
            };
            let format = match raw.format.as_deref() {
                Some(value) => FileFormat::parse(value)?,
                None => FileFormat::Text,
            };
            Ok(Sink::File {
                relative: validate_relative_path(path)?,
                format,
            })
        }
        "webhook" => {
            if raw.path.is_some() || raw.format.is_some() {
                return Err(
                    "sink kind is \"webhook\"; path and format belong to a file sink. A \
                            webhook always receives one JSON object."
                        .into(),
                );
            }
            let Some(url_env) = raw.url_env.as_deref().map(str::trim) else {
                return Err(format!(
                    "sink kind is \"webhook\" but no url_env was given. Name an environment \
                     variable holding the URL — for example url_env = \
                     \"{}\" — never the URL itself, which is a bearer credential.",
                    "TDCC_SCHEDULED_PROMPTS_WEBHOOK_DIGEST"
                ));
            };
            Ok(Sink::Webhook {
                url: resolve_webhook_url(url_env, env)?,
                url_env: url_env.to_string(),
            })
        }
        other => Err(format!(
            "unknown sink kind \"{other}\"; expected \"file\" or \"webhook\""
        )),
    }
}

/// Look a webhook URL up in the environment, refusing a URL written in place.
fn resolve_webhook_url(url_env: &str, env: &EnvMap) -> Result<Url, String> {
    if url_env.contains("://") || url_env.contains('/') {
        return Err(format!(
            "url_env is \"{url_env}\", which looks like a URL rather than the name of an \
             environment variable. A webhook URL is a bearer credential and must not be written \
             into the jobs file; export it in the environment of the tdcc process and name the \
             variable here."
        ));
    }
    if url_env.is_empty()
        || !url_env
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        || url_env.starts_with(|c: char| c.is_ascii_digit())
    {
        return Err(format!(
            "url_env \"{url_env}\" is not a valid environment variable name; use SCREAMING_SNAKE \
             such as TDCC_SCHEDULED_PROMPTS_WEBHOOK_DIGEST"
        ));
    }

    let raw = env
        .get(url_env)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            format!(
                "webhook sink names {url_env}, which is not set in the environment of the tdcc \
                 process. Export it there and restart the node; the plugin refuses to start with \
                 a delivery target it cannot resolve rather than dropping every run silently."
            )
        })?;

    let url = Url::parse(raw).map_err(|error| {
        // The value is a credential, so the parse error is reported without it.
        format!("the URL in {url_env} is not a valid URL ({error})")
    })?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "the URL in {url_env} uses scheme `{}`; only http and https are delivered to",
            url.scheme()
        ));
    }
    if url.host_str().is_none() {
        return Err(format!("the URL in {url_env} has no host"));
    }
    Ok(url)
}

/// Windows treats these as devices whatever directory they appear in, and a
/// file named after one is a mistake on every platform.
const RESERVED_NAMES: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// Validate a file sink's path.
///
/// The result is a relative, `/`-separated path built from plain names. This is
/// confinement by construction rather than by inspection: there is no input
/// that produces an absolute path, a `..`, a drive letter, a UNC prefix, or a
/// device name, so the join in [`Sink::resolved_path`] cannot leave the root.
/// [`crate::sink`] re-checks containment after canonicalizing, because two
/// independent layers is the standard this catalog holds paths to.
pub fn validate_relative_path(raw: &str) -> Result<String, String> {
    let path = raw.trim();
    if path.is_empty() {
        return Err("sink path is empty".into());
    }
    if path.contains('\\') {
        return Err(format!(
            "sink path \"{path}\" contains a backslash; write it with `/` separators, which work \
             on every platform this plugin runs on"
        ));
    }
    if path.starts_with('/') {
        return Err(format!(
            "sink path \"{path}\" is absolute; every file sink is written beneath the output \
             directory, so paths are relative to it"
        ));
    }

    let components: Vec<&str> = path.split('/').collect();
    if components.len() > MAX_PATH_COMPONENTS {
        return Err(format!(
            "sink path \"{path}\" is {} levels deep; the limit is {MAX_PATH_COMPONENTS}",
            components.len()
        ));
    }

    for component in &components {
        if component.is_empty() {
            return Err(format!(
                "sink path \"{path}\" has an empty segment; write `reports/daily.md`, not \
                 `reports//daily.md`"
            ));
        }
        if *component == "." || *component == ".." {
            return Err(format!(
                "sink path \"{path}\" contains `{component}`; a file sink cannot name a directory \
                 outside the output root"
            ));
        }
        if component.chars().count() > MAX_PATH_COMPONENT_CHARS {
            return Err(format!(
                "sink path segment \"{component}\" is longer than {MAX_PATH_COMPONENT_CHARS} \
                 characters"
            ));
        }
        if !component
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err(format!(
                "sink path segment \"{component}\" contains characters outside A-Z a-z 0-9 - _ . \
                 — the set that means the same thing on every filesystem"
            ));
        }
        if component.ends_with('.') {
            return Err(format!(
                "sink path segment \"{component}\" ends with a dot, which Windows silently strips"
            ));
        }
        let stem = component.split('.').next().unwrap_or(component);
        if RESERVED_NAMES.contains(&stem.to_ascii_lowercase().as_str()) {
            return Err(format!(
                "sink path segment \"{component}\" is a reserved device name on Windows"
            ));
        }
    }

    Ok(components.join("/"))
}

fn bounded_u64(
    value: Option<u64>,
    default: u64,
    min: u64,
    max: u64,
    name: &str,
) -> Result<u64, String> {
    match value {
        None => Ok(default),
        Some(value) if (min..=max).contains(&value) => Ok(value),
        Some(value) => Err(format!(
            "{name} must be between {min} and {max}, got {value}"
        )),
    }
}

fn bounded_u32(
    value: Option<u32>,
    default: u32,
    min: u32,
    max: u32,
    name: &str,
) -> Result<u32, String> {
    match value {
        None => Ok(default),
        Some(value) if (min..=max).contains(&value) => Ok(value),
        Some(value) => Err(format!(
            "{name} must be between {min} and {max}, got {value}"
        )),
    }
}

fn bounded_usize(
    value: Option<usize>,
    default: usize,
    min: usize,
    max: usize,
    name: &str,
) -> Result<usize, String> {
    match value {
        None => Ok(default),
        Some(value) if (min..=max).contains(&value) => Ok(value),
        Some(value) => Err(format!(
            "{name} must be between {min} and {max}, got {value}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-03-01T00:00:00Z, so window checks have a fixed anchor.
    const NOW: i64 = 1_772_323_200_000;

    fn env(pairs: &[(&str, &str)]) -> EnvMap {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    fn minimal(extra: &str) -> String {
        format!(
            "version = 1\n\
             timezone = \"utc\"\n\
             \n\
             [[job]]\n\
             id = \"digest\"\n\
             schedule = \"0 3 * * *\"\n\
             model = \"qwen3:8b\"\n\
             prompt = \"Summarise the day.\"\n\
             sink = {{ kind = \"file\", path = \"digests/daily.md\" }}\n\
             {extra}"
        )
    }

    fn load(text: &str) -> Result<JobsFile, String> {
        parse_jobs(text, &env(&[]), NOW)
    }

    #[test]
    fn a_minimal_file_loads_with_documented_defaults() {
        let file = load(&minimal("")).expect("minimal file loads");

        assert_eq!(file.zone, Zone::Utc);
        assert_eq!(file.max_concurrent_runs, DEFAULT_MAX_CONCURRENT_RUNS);
        assert_eq!(file.history_per_job, DEFAULT_HISTORY_PER_JOB);

        let job = file.job("digest").expect("the job is there");
        assert!(
            job.enabled,
            "a job is enabled unless the file says otherwise"
        );
        assert_eq!(job.misfire, Misfire::RunOnce);
        assert_eq!(job.timeout_secs, DEFAULT_TIMEOUT_SECS);
        assert_eq!(job.window, None);
        assert!(
            !job.manual_ignores_window,
            "run_now must honour the window until the file says otherwise"
        );
        assert_eq!(
            job.sink,
            Sink::File {
                relative: "digests/daily.md".into(),
                format: FileFormat::Text
            }
        );
    }

    #[test]
    fn file_level_settings_become_job_defaults_and_a_job_can_override_them() {
        let text = "version = 1\n\
             timezone = \"utc\"\n\
             window = \"22:00-06:00\"\n\
             misfire = \"skip\"\n\
             max_concurrent_runs = 3\n\
             \n\
             [[job]]\n\
             id = \"inherits\"\n\
             schedule = \"0 23 * * *\"\n\
             model = \"m\"\n\
             prompt = \"p\"\n\
             sink = { kind = \"file\", path = \"a.md\" }\n\
             \n\
             [[job]]\n\
             id = \"overrides\"\n\
             schedule = \"30 13 * * *\"\n\
             window = \"09:00-17:00\"\n\
             misfire = \"run_once\"\n\
             model = \"m\"\n\
             prompt = \"p\"\n\
             sink = { kind = \"file\", path = \"b.md\" }\n";

        let file = load(text).expect("loads");

        assert_eq!(file.max_concurrent_runs, 3);
        let inherits = file.job("inherits").expect("present");
        assert_eq!(
            inherits.window.map(|w| w.to_string()).as_deref(),
            Some("22:00-06:00")
        );
        assert_eq!(inherits.misfire, Misfire::Skip);
        let overrides = file.job("overrides").expect("present");
        assert_eq!(
            overrides.window.map(|w| w.to_string()).as_deref(),
            Some("09:00-17:00")
        );
        assert_eq!(overrides.misfire, Misfire::RunOnce);
    }

    #[test]
    fn an_unknown_key_stops_the_file_loading() {
        let error = load(&minimal("scheduel = \"0 3 * * *\"\n")).expect_err("typo is refused");

        assert!(error.contains("scheduel"), "{error}");
    }

    #[test]
    fn an_unsupported_version_is_named_rather_than_guessed_at() {
        let error = load("version = 2\n").expect_err("version 2 is refused");

        assert!(error.contains("version 1"), "{error}");
    }

    #[test]
    fn a_schedule_that_can_never_run_inside_its_window_is_refused_at_load() {
        let text = "version = 1\n\
             timezone = \"utc\"\n\
             \n\
             [[job]]\n\
             id = \"noon\"\n\
             schedule = \"0 12 * * *\"\n\
             window = \"22:00-06:00\"\n\
             model = \"m\"\n\
             prompt = \"p\"\n\
             sink = { kind = \"file\", path = \"a.md\" }\n";

        let error = load(text).expect_err("a contradiction is refused");

        assert!(error.contains("never falls inside window"), "{error}");
        assert!(error.contains("noon"), "{error}");
    }

    #[test]
    fn duplicate_ids_are_refused() {
        let text = "version = 1\n\
             [[job]]\n\
             id = \"same\"\n\
             schedule = \"@daily\"\n\
             model = \"m\"\n\
             prompt = \"p\"\n\
             sink = { kind = \"file\", path = \"a.md\" }\n\
             [[job]]\n\
             id = \"same\"\n\
             schedule = \"@daily\"\n\
             model = \"m\"\n\
             prompt = \"p\"\n\
             sink = { kind = \"file\", path = \"b.md\" }\n";

        let error = load(text).expect_err("duplicate ids are refused");

        assert!(error.contains("share the id"), "{error}");
    }

    #[test]
    fn out_of_range_numbers_name_the_setting_and_the_bounds() {
        let cases = [
            ("timeout_secs = 1\n", "timeout_secs"),
            ("timeout_secs = 99999\n", "timeout_secs"),
            ("max_output_tokens = 0\n", "max_output_tokens"),
            ("temperature = 5.0\n", "temperature"),
            (
                "quarantine_after_failures = 99999\n",
                "quarantine_after_failures",
            ),
        ];
        for (extra, expected) in cases {
            let error = load(&minimal(extra)).expect_err("must be refused");
            assert!(error.contains(expected), "{extra} -> {error}");
        }

        let error = load("version = 1\nmax_concurrent_runs = 99\n")
            .expect_err("a huge concurrency cap is refused");
        assert!(error.contains("max_concurrent_runs"), "{error}");
    }

    #[test]
    fn a_webhook_url_written_into_the_file_is_refused_by_name() {
        let text = "version = 1\n\
             [[job]]\n\
             id = \"w\"\n\
             schedule = \"@daily\"\n\
             model = \"m\"\n\
             prompt = \"p\"\n\
             sink = { kind = \"webhook\", url_env = \"https://hooks.slack.com/services/T0/B0/x\" }\n";

        let error = load(text).expect_err("a URL in place of a variable name is refused");

        assert!(error.contains("bearer credential"), "{error}");
        assert!(error.contains("environment"), "{error}");
    }

    #[test]
    fn a_webhook_variable_that_is_not_set_fails_the_load_loudly() {
        let text = "version = 1\n\
             [[job]]\n\
             id = \"w\"\n\
             schedule = \"@daily\"\n\
             model = \"m\"\n\
             prompt = \"p\"\n\
             sink = { kind = \"webhook\", url_env = \"TDCC_SCHEDULED_PROMPTS_WEBHOOK_X\" }\n";

        let error = parse_jobs(text, &env(&[]), NOW).expect_err("an unset variable is refused");

        assert!(
            error.contains("TDCC_SCHEDULED_PROMPTS_WEBHOOK_X"),
            "{error}"
        );
        assert!(error.contains("not set in the environment"), "{error}");
    }

    #[test]
    fn a_resolved_webhook_is_never_rendered_in_full() {
        let text = "version = 1\n\
             [[job]]\n\
             id = \"w\"\n\
             schedule = \"@daily\"\n\
             model = \"m\"\n\
             prompt = \"p\"\n\
             sink = { kind = \"webhook\", url_env = \"TDCC_SCHEDULED_PROMPTS_WEBHOOK_X\" }\n";
        let env = env(&[(
            "TDCC_SCHEDULED_PROMPTS_WEBHOOK_X",
            "https://hooks.slack.com/services/T0/B0/XXXXsecretXXXX",
        )]);

        let file = parse_jobs(text, &env, NOW).expect("loads");
        let label = file.job("w").expect("present").sink.label();

        assert!(!label.contains("XXXXsecretXXXX"), "{label}");
        assert!(label.contains("hooks.slack.com"), "{label}");
        assert!(label.contains("[redacted]"), "{label}");
        assert!(
            label.contains("TDCC_SCHEDULED_PROMPTS_WEBHOOK_X"),
            "{label}"
        );
    }

    #[test]
    fn sink_kinds_do_not_borrow_each_others_fields() {
        let file_with_url_env = "version = 1\n\
             [[job]]\n\
             id = \"j\"\n\
             schedule = \"@daily\"\n\
             model = \"m\"\n\
             prompt = \"p\"\n\
             sink = { kind = \"file\", path = \"a.md\", url_env = \"X\" }\n";
        assert!(load(file_with_url_env).is_err());

        let webhook_with_path = "version = 1\n\
             [[job]]\n\
             id = \"j\"\n\
             schedule = \"@daily\"\n\
             model = \"m\"\n\
             prompt = \"p\"\n\
             sink = { kind = \"webhook\", url_env = \"X_URL\", path = \"a.md\" }\n";
        assert!(load(webhook_with_path).is_err());
    }

    #[test]
    fn a_file_sink_path_cannot_escape_the_output_root() {
        let escapes = [
            "../secrets.md",
            "/etc/passwd",
            "reports/../../etc/passwd",
            "C:/Windows/system32/config",
            "reports\\daily.md",
            "reports//daily.md",
            "./daily.md",
            "nul",
            "com1.md",
            "trailing.",
            "a/b/c/d/e/f/g/h/i/j.md",
            "spaces are out.md",
            "unicodé.md",
            "",
        ];
        for path in escapes {
            assert!(
                validate_relative_path(path).is_err(),
                "{path:?} should be refused"
            );
        }

        for path in ["daily.md", "reports/2026/daily.md", "a_b-c.2026.jsonl"] {
            assert!(
                validate_relative_path(path).is_ok(),
                "{path:?} should be accepted"
            );
        }
    }

    #[test]
    fn a_validated_path_keeps_its_separators_and_nothing_else() {
        // What comes out is what `crate::sink` joins onto the output root, so
        // it has to be exactly the segments that went in.
        assert_eq!(
            validate_relative_path("  reports/2026/daily.md  ").expect("valid"),
            "reports/2026/daily.md"
        );
        assert_eq!(
            validate_relative_path("daily.md").expect("valid"),
            "daily.md"
        );
    }

    #[test]
    fn ids_stay_boring() {
        for bad in ["", "   ", "has space", "has/slash", "emoji-🙂"] {
            let text = format!(
                "version = 1\n[[job]]\nid = \"{bad}\"\nschedule = \"@daily\"\nmodel = \"m\"\n\
                 prompt = \"p\"\nsink = {{ kind = \"file\", path = \"a.md\" }}\n"
            );
            assert!(load(&text).is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn misfire_run_all_does_not_exist_and_the_error_says_where_to_read_why() {
        let error = Misfire::parse("run_all").expect_err("run_all is not a policy");

        assert!(error.contains("run_once"), "{error}");
        assert!(error.contains("Misfire policy"), "{error}");
    }

    #[test]
    fn too_many_jobs_are_refused_rather_than_scheduled() {
        let mut text = String::from("version = 1\n");
        for index in 0..(MAX_JOBS + 1) {
            text.push_str(&format!(
                "[[job]]\nid = \"j{index}\"\nschedule = \"@daily\"\nmodel = \"m\"\nprompt = \"p\"\n\
                 sink = {{ kind = \"file\", path = \"j{index}.md\" }}\n"
            ));
        }

        let error = load(&text).expect_err("the job count is bounded");

        assert!(error.contains(&MAX_JOBS.to_string()), "{error}");
    }

    #[test]
    fn an_empty_file_is_valid_and_holds_no_jobs() {
        let file = load("version = 1\n").expect("a jobs file with no jobs is legal");

        assert!(file.jobs.is_empty());
        assert_eq!(file.zone, Zone::Local);
    }

    /// The jobs file the README shows, parsed by the code that reads a real one.
    ///
    /// The example in a README is the first thing anybody copies. Pinning it
    /// here means a rename, a new bound, or a stricter rule cannot leave the
    /// documentation quietly wrong.
    #[test]
    fn the_example_in_the_readme_is_a_file_this_build_accepts() {
        const README: &str = include_str!("../README.md");

        let example = README
            .split("```toml")
            .map(|block| block.split("```").next().unwrap_or_default())
            .find(|block| block.contains("[[job]]"))
            .expect("the README shows a jobs file");

        let file = parse_jobs(
            example,
            &env(&[(
                "TDCC_SCHEDULED_PROMPTS_WEBHOOK_ALERT",
                "https://hooks.slack.com/services/T0/B0/example",
            )]),
            NOW,
        )
        .unwrap_or_else(|error| panic!("the README's example must load: {error}\n\n{example}"));

        assert_eq!(file.jobs.len(), 2, "both example jobs load");
        let digest = file.job("nightly-digest").expect("the first example job");
        assert_eq!(digest.schedule.spec(), "0 3 * * *");
        assert!(
            digest
                .prompt
                .starts_with("Summarise, in five bullet points"),
            "the multi-line prompt in the README must not swallow a comment: {:?}",
            digest.prompt
        );
        let alert = file.job("hourly-alert").expect("the second example job");
        assert_eq!(alert.misfire, Misfire::Skip);
        assert_eq!(alert.quarantine_after_failures, 5);
        assert!(matches!(alert.sink, Sink::Webhook { .. }));
    }
}
