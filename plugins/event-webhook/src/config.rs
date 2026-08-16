//! Operator configuration, and the rules that keep the webhook URL out of
//! logs, process listings, and the console.
//!
//! Two constraints shape this module.
//!
//! * **The webhook URL is a credential.** Anyone holding a Slack or Discord
//!   incoming-webhook URL can post into that channel as you. It is therefore
//!   read *only* from the environment — never from `[[plugin]].args`, which is
//!   visible in `ps` and in the console, and never from `[plugin.settings]`,
//!   which the host stores in `config.toml` and hands to browser code through
//!   `host.config.visible.settings`.
//! * **Everything else is a plain operating knob**, so it may come from either
//!   `[[plugin]].args` or the environment, with args winning.
//!
//! `[plugin.settings]` is not used at all: those values never reach the plugin
//! process (see the plugins README, "Host owns / plugin owns"), so declaring a
//! config schema here would render controls in the console that this process
//! could never read.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use anyhow::{Result, bail};
use reqwest::Url;
use serde_json::{Value, json};

use crate::event::{EventKind, SUBSCRIBABLE};

pub const WEBHOOK_URL_ENV: &str = "TDCC_EVENT_WEBHOOK_URL";
pub const PLUGIN_URL_ENV: &str = "TDCC_PLUGIN_URL";
pub const FORMAT_ENV: &str = "TDCC_EVENT_WEBHOOK_FORMAT";
pub const EVENTS_ENV: &str = "TDCC_EVENT_WEBHOOK_EVENTS";
pub const QUEUE_ENV: &str = "TDCC_EVENT_WEBHOOK_QUEUE_CAPACITY";
pub const COALESCE_ENV: &str = "TDCC_EVENT_WEBHOOK_COALESCE_SECS";
pub const TIMEOUT_ENV: &str = "TDCC_EVENT_WEBHOOK_TIMEOUT_SECS";
pub const ATTEMPTS_ENV: &str = "TDCC_EVENT_WEBHOOK_MAX_ATTEMPTS";
pub const INSECURE_ENV: &str = "TDCC_EVENT_WEBHOOK_ALLOW_INSECURE_URL";

/// Every environment variable this plugin reads. `main` copies exactly these
/// out of the process environment so nothing else can leak into a snapshot.
pub const ENV_VARS: [&str; 9] = [
    WEBHOOK_URL_ENV,
    PLUGIN_URL_ENV,
    FORMAT_ENV,
    EVENTS_ENV,
    QUEUE_ENV,
    COALESCE_ENV,
    TIMEOUT_ENV,
    ATTEMPTS_ENV,
    INSECURE_ENV,
];

const DEFAULT_QUEUE_CAPACITY: usize = 512;
const DEFAULT_COALESCE_SECS: u64 = 15;
const DEFAULT_TIMEOUT_SECS: u64 = 10;
const DEFAULT_MAX_ATTEMPTS: u32 = 5;

/// Backoff bounds are fixed rather than configurable: they only matter relative
/// to the retry count, and one more knob would not buy an operator anything.
pub const INITIAL_BACKOFF: Duration = Duration::from_millis(500);
pub const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Longest list rendered into a payload before it is elided. Discord rejects
/// embeds over ~6000 characters, so an unbounded model list is a delivery bug.
pub const MAX_LIST_ITEMS: usize = 12;

/// Upper bound on peers whose serving-model set is remembered for derivation.
pub const MAX_TRACKED_PEERS: usize = 1024;

/// Upper bound on distinct coalescing keys held in memory.
pub const MAX_COALESCE_KEYS: usize = 4096;

const USAGE: &str = "\
event-webhook options (all optional; the webhook URL is NOT an option):
  --format <json|slack|discord>   payload shape                (env TDCC_EVENT_WEBHOOK_FORMAT)
  --events <all|a,b,c>            event filter                 (env TDCC_EVENT_WEBHOOK_EVENTS)
  --queue-capacity <n>            bounded queue depth          (env TDCC_EVENT_WEBHOOK_QUEUE_CAPACITY)
  --coalesce-secs <n>             flood window, 0 disables     (env TDCC_EVENT_WEBHOOK_COALESCE_SECS)
  --timeout-secs <n>              per-request timeout          (env TDCC_EVENT_WEBHOOK_TIMEOUT_SECS)
  --max-attempts <n>              tries per event              (env TDCC_EVENT_WEBHOOK_MAX_ATTEMPTS)
  --allow-insecure-url            permit non-loopback http://  (env TDCC_EVENT_WEBHOOK_ALLOW_INSECURE_URL)
  --print-package-manifest        emit plugin-manifest.json and exit

The webhook URL is read from TDCC_EVENT_WEBHOOK_URL, falling back to
TDCC_PLUGIN_URL ([[plugin]].url). It is never accepted as a command-line
argument, because arguments are visible to every process on the machine.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PayloadFormat {
    /// A generic JSON envelope for your own receiver.
    Json,
    /// `text` + a coloured attachment, for a Slack incoming webhook.
    Slack,
    /// A single embed, for a Discord channel webhook.
    Discord,
}

impl PayloadFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            PayloadFormat::Json => "json",
            PayloadFormat::Slack => "slack",
            PayloadFormat::Discord => "discord",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "json" | "generic" => Ok(PayloadFormat::Json),
            "slack" => Ok(PayloadFormat::Slack),
            "discord" => Ok(PayloadFormat::Discord),
            other => bail!("unknown payload format '{other}'; expected json, slack, or discord"),
        }
    }
}

/// Which events leave the node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EventFilter {
    All,
    Only(BTreeSet<EventKind>),
}

impl EventFilter {
    pub fn allows(&self, kind: EventKind) -> bool {
        match self {
            EventFilter::All => true,
            EventFilter::Only(kinds) => kinds.contains(&kind),
        }
    }

    /// Rejects unknown names loudly. A typo in a filter that silently means
    /// "send nothing" is the worst possible failure for an alerting plugin.
    pub fn parse(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            bail!("event filter is empty; use 'all' or a comma-separated list");
        }
        if trimmed.eq_ignore_ascii_case("all") || trimmed == "*" {
            return Ok(EventFilter::All);
        }
        let mut kinds = BTreeSet::new();
        for token in trimmed.split(',') {
            let token = token.trim();
            if token.is_empty() {
                continue;
            }
            match EventKind::parse(token) {
                Some(kind) if SUBSCRIBABLE.contains(&kind) => {
                    kinds.insert(kind);
                }
                _ => bail!(
                    "unknown event '{token}'; known events: {}",
                    known_event_names().join(", ")
                ),
            }
        }
        if kinds.is_empty() {
            bail!("event filter selected no events; use 'all' or a comma-separated list");
        }
        Ok(EventFilter::Only(kinds))
    }

    pub fn to_json(&self) -> Value {
        match self {
            EventFilter::All => json!("all"),
            EventFilter::Only(kinds) => {
                json!(kinds.iter().map(|kind| kind.as_str()).collect::<Vec<_>>())
            }
        }
    }
}

pub fn known_event_names() -> Vec<&'static str> {
    SUBSCRIBABLE.iter().map(|kind| kind.as_str()).collect()
}

/// A validated webhook destination plus where it came from, so `status` can say
/// which variable to edit without ever showing the value.
#[derive(Clone, Debug)]
pub struct Target {
    pub url: Url,
    pub source: &'static str,
}

impl Target {
    pub fn redacted(&self) -> String {
        redact(&self.url)
    }
}

#[derive(Clone, Debug)]
pub struct Settings {
    /// `None` means no destination is configured. The plugin still starts, so
    /// `status` and `health` can tell the operator exactly what is missing.
    pub target: Option<Target>,
    pub format: PayloadFormat,
    pub filter: EventFilter,
    pub queue_capacity: usize,
    pub coalesce_window: Duration,
    pub request_timeout: Duration,
    pub max_attempts: u32,
    pub allow_insecure_url: bool,
}

impl Settings {
    pub fn to_json(&self) -> Value {
        json!({
            "configured": self.target.is_some(),
            "target": self.target.as_ref().map(Target::redacted),
            "target_source": self.target.as_ref().map(|target| target.source),
            "format": self.format.as_str(),
            "events": self.filter.to_json(),
            "queue_capacity": self.queue_capacity,
            "coalesce_window_secs": self.coalesce_window.as_secs(),
            "request_timeout_secs": self.request_timeout.as_secs(),
            "max_attempts": self.max_attempts,
            "allow_insecure_url": self.allow_insecure_url,
        })
    }
}

/// Copies the variables this plugin knows about out of the process
/// environment. The plugin process inherits the environment of `tdcc`, which is
/// what lets an operator keep the webhook URL in a systemd unit, a launchd
/// plist, or a shell profile instead of in `config.toml`.
pub fn read_env() -> BTreeMap<String, String> {
    ENV_VARS
        .iter()
        .filter_map(|name| {
            std::env::var(name)
                .ok()
                .map(|value| ((*name).to_string(), value))
        })
        .collect()
}

/// Parses `[[plugin]].args` over the environment. Pure, so the whole precedence
/// table is unit-testable without touching the real environment.
pub fn parse(args: &[String], env: &BTreeMap<String, String>) -> Result<Settings> {
    let mut format = env.get(FORMAT_ENV).map(String::as_str);
    let mut events = env.get(EVENTS_ENV).map(String::as_str);
    let mut queue = env.get(QUEUE_ENV).map(String::as_str);
    let mut coalesce = env.get(COALESCE_ENV).map(String::as_str);
    let mut timeout = env.get(TIMEOUT_ENV).map(String::as_str);
    let mut attempts = env.get(ATTEMPTS_ENV).map(String::as_str);
    let mut allow_insecure = match env.get(INSECURE_ENV) {
        Some(raw) => parse_bool(INSECURE_ENV, raw)?,
        None => false,
    };

    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        match flag {
            "--url" | "--webhook-url" | "--webhook" => bail!(
                "{flag} is refused on purpose: command-line arguments are visible to other \
                 processes and are recorded in the console. Set {WEBHOOK_URL_ENV} in the \
                 environment of the tdcc process instead."
            ),
            "--allow-insecure-url" => {
                allow_insecure = true;
                index += 1;
                continue;
            }
            "--help" | "-h" => bail!("{USAGE}"),
            _ => {}
        }

        let Some(value) = args.get(index + 1) else {
            bail!("{flag} requires a value\n\n{USAGE}");
        };
        match flag {
            "--format" => format = Some(value.as_str()),
            "--events" => events = Some(value.as_str()),
            "--queue-capacity" => queue = Some(value.as_str()),
            "--coalesce-secs" => coalesce = Some(value.as_str()),
            "--timeout-secs" => timeout = Some(value.as_str()),
            "--max-attempts" => attempts = Some(value.as_str()),
            other => bail!("unknown option '{other}'\n\n{USAGE}"),
        }
        index += 2;
    }

    let target = resolve_target(env, allow_insecure)?;

    Ok(Settings {
        target,
        format: match format {
            Some(raw) => PayloadFormat::parse(raw)?,
            None => PayloadFormat::Json,
        },
        filter: match events {
            Some(raw) => EventFilter::parse(raw)?,
            None => EventFilter::All,
        },
        queue_capacity: parse_bounded_usize(
            "queue capacity",
            queue,
            DEFAULT_QUEUE_CAPACITY,
            1,
            100_000,
        )?,
        coalesce_window: Duration::from_secs(parse_bounded_u64(
            "coalesce window",
            coalesce,
            DEFAULT_COALESCE_SECS,
            0,
            3_600,
        )?),
        request_timeout: Duration::from_secs(parse_bounded_u64(
            "request timeout",
            timeout,
            DEFAULT_TIMEOUT_SECS,
            1,
            120,
        )?),
        max_attempts: parse_bounded_u64(
            "max attempts",
            attempts,
            DEFAULT_MAX_ATTEMPTS as u64,
            1,
            10,
        )? as u32,
        allow_insecure_url: allow_insecure,
    })
}

fn resolve_target(env: &BTreeMap<String, String>, allow_insecure: bool) -> Result<Option<Target>> {
    for name in [WEBHOOK_URL_ENV, PLUGIN_URL_ENV] {
        let Some(raw) = env.get(name) else { continue };
        if raw.trim().is_empty() {
            continue;
        }
        return Ok(Some(parse_target(raw, name, allow_insecure)?));
    }
    Ok(None)
}

/// Validates a destination without ever putting it in an error message.
///
/// `https` is always allowed. Plain `http` is allowed only to a loopback host,
/// or when the operator explicitly opts in: mesh topology, node ids, and model
/// names should not cross a network in cleartext by accident.
pub fn parse_target(raw: &str, source: &'static str, allow_insecure: bool) -> Result<Target> {
    let trimmed = raw.trim();
    let Ok(url) = Url::parse(trimmed) else {
        bail!("{source} is not a valid absolute URL (value withheld from logs)");
    };
    let Some(host) = url.host_str().map(str::to_string) else {
        bail!("{source} has no host component (value withheld from logs)");
    };
    match url.scheme() {
        "https" => {}
        "http" => {
            if !allow_insecure && !is_loopback_host(&host) {
                bail!(
                    "{source} points at http://{host}/…, which would send mesh events in \
                     cleartext. Use https, a loopback address, or pass --allow-insecure-url \
                     if you know the hop is already private."
                );
            }
        }
        scheme => bail!("{source} uses unsupported scheme '{scheme}'; expected http or https"),
    }
    Ok(Target { url, source })
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host == "::1"
        || host == "[::1]"
        || host.strip_prefix("127.").is_some_and(|rest| {
            !rest.is_empty() && rest.chars().all(|ch| ch.is_ascii_digit() || ch == '.')
        })
}

/// Everything a human or a log line is allowed to see of the destination.
///
/// Slack and Discord put the entire secret in the path, so only the scheme,
/// host, and port survive. Userinfo is dropped because `host_str` excludes it.
pub fn redact(url: &Url) -> String {
    let scheme = url.scheme();
    let host = url.host_str().unwrap_or("unknown-host");
    let port = url
        .port()
        .map(|port| format!(":{port}"))
        .unwrap_or_default();
    let has_secret_tail = url.path() != "/" && !url.path().is_empty() || url.query().is_some();
    let tail = if has_secret_tail { "/[redacted]" } else { "/" };
    format!("{scheme}://{host}{port}{tail}")
}

/// Any substring of the destination at least this long is assumed to be the
/// secret part. Deliberately biased towards over-redacting: a mangled error
/// message costs an operator a minute, a leaked webhook URL costs them the
/// channel.
const SECRET_SUBSTRING_LEN: usize = 8;

/// Removes any trace of the destination from third-party text.
///
/// `reqwest::Error` embeds the request URL in its `Display` output. `without_url`
/// handles the common case; this is the belt-and-braces pass for error text that
/// arrived from anywhere else — most importantly a response body that echoes
/// back part of the path, which is where Slack and Discord keep the token.
pub fn scrub(text: &str, url: &Url) -> String {
    let mut out = text.replace(url.as_str(), &redact(url));

    let path = url.path();
    if path.len() > 1 {
        out = out.replace(path, "/[redacted]");
    }
    if let Some(query) = url.query()
        && !query.is_empty()
    {
        out = out.replace(query, "[redacted]");
    }

    // Individual path segments and query values, for text that quotes only a
    // fragment of the URL.
    for segment in path.split('/') {
        if segment.len() >= SECRET_SUBSTRING_LEN {
            out = out.replace(segment, "[redacted]");
        }
    }
    for (_, value) in url.query_pairs() {
        if value.len() >= SECRET_SUBSTRING_LEN {
            out = out.replace(value.as_ref(), "[redacted]");
        }
    }
    out
}

fn parse_bool(name: &str, raw: &str) -> Result<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" | "" => Ok(false),
        other => bail!("{name} expects a boolean, got '{other}'"),
    }
}

fn parse_bounded_u64(
    name: &str,
    raw: Option<&str>,
    default: u64,
    min: u64,
    max: u64,
) -> Result<u64> {
    let Some(raw) = raw else { return Ok(default) };
    let trimmed = raw.trim();
    let Ok(value) = trimmed.parse::<u64>() else {
        bail!("{name} expects a whole number, got '{trimmed}'");
    };
    if value < min || value > max {
        bail!("{name} must be between {min} and {max}, got {value}");
    }
    Ok(value)
}

fn parse_bounded_usize(
    name: &str,
    raw: Option<&str>,
    default: usize,
    min: usize,
    max: usize,
) -> Result<usize> {
    parse_bounded_u64(name, raw, default as u64, min as u64, max as u64).map(|value| value as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn defaults_are_usable_without_any_configuration() {
        let settings = parse(&[], &env(&[])).expect("defaults parse");

        assert!(settings.target.is_none());
        assert_eq!(settings.format, PayloadFormat::Json);
        assert_eq!(settings.filter, EventFilter::All);
        assert_eq!(settings.queue_capacity, DEFAULT_QUEUE_CAPACITY);
        assert_eq!(settings.coalesce_window, Duration::from_secs(15));
        assert_eq!(settings.max_attempts, DEFAULT_MAX_ATTEMPTS);
    }

    #[test]
    fn args_win_over_environment_for_non_secret_knobs() {
        let settings = parse(
            &args(&["--format", "slack", "--max-attempts", "3"]),
            &env(&[(FORMAT_ENV, "discord"), (ATTEMPTS_ENV, "9")]),
        )
        .expect("args override env");

        assert_eq!(settings.format, PayloadFormat::Slack);
        assert_eq!(settings.max_attempts, 3);
    }

    #[test]
    fn the_dedicated_variable_wins_over_the_plugin_url() {
        let settings = parse(
            &[],
            &env(&[
                (WEBHOOK_URL_ENV, "https://hooks.example.com/a/b/c"),
                (PLUGIN_URL_ENV, "https://other.example.com/x"),
            ]),
        )
        .expect("target resolves");

        let target = settings.target.expect("target configured");
        assert_eq!(target.source, WEBHOOK_URL_ENV);
        assert_eq!(target.url.host_str(), Some("hooks.example.com"));
    }

    #[test]
    fn the_plugin_url_is_the_documented_fallback() {
        let settings = parse(
            &[],
            &env(&[(PLUGIN_URL_ENV, "https://other.example.com/x")]),
        )
        .expect("target resolves");
        assert_eq!(settings.target.expect("configured").source, PLUGIN_URL_ENV);
    }

    #[test]
    fn passing_the_url_as_an_argument_is_refused_with_a_pointer_to_the_env_var() {
        let error = parse(&args(&["--url", "https://hooks.example.com/a"]), &env(&[]))
            .expect_err("must be refused");
        let message = error.to_string();
        assert!(message.contains(WEBHOOK_URL_ENV), "{message}");
        assert!(
            !message.contains("hooks.example.com"),
            "the refusal must not echo the value back: {message}"
        );
    }

    #[test]
    fn cleartext_http_is_refused_off_loopback_but_allowed_on_it() {
        assert!(parse_target("http://example.com/hook", WEBHOOK_URL_ENV, false).is_err());
        assert!(parse_target("http://127.0.0.1:9000/hook", WEBHOOK_URL_ENV, false).is_ok());
        assert!(parse_target("http://localhost:9000/hook", WEBHOOK_URL_ENV, false).is_ok());
        assert!(parse_target("http://[::1]:9000/hook", WEBHOOK_URL_ENV, false).is_ok());
        assert!(parse_target("http://example.com/hook", WEBHOOK_URL_ENV, true).is_ok());
        assert!(parse_target("https://example.com/hook", WEBHOOK_URL_ENV, false).is_ok());
    }

    #[test]
    fn non_http_schemes_and_malformed_urls_are_refused_without_echoing_the_value() {
        for raw in [
            "file:///etc/passwd",
            "ftp://example.com/hook",
            "not a url",
            "/relative/only",
        ] {
            let error = parse_target(raw, WEBHOOK_URL_ENV, true).expect_err("must be refused");
            let message = error.to_string();
            assert!(
                !message.contains("passwd") && !message.contains("/relative/only"),
                "refusal leaked the value: {message}"
            );
        }
    }

    #[test]
    fn redaction_keeps_the_host_and_drops_everything_secret() {
        let url = Url::parse("https://hooks.slack.com/services/T000/B111/XXXXsecretXXXX").unwrap();
        let redacted = redact(&url);

        assert_eq!(redacted, "https://hooks.slack.com/[redacted]");
        assert!(!redacted.contains("secret"));

        let with_query = Url::parse("https://example.com/?token=abc123").unwrap();
        assert_eq!(redact(&with_query), "https://example.com/[redacted]");

        let with_port = Url::parse("http://127.0.0.1:9000/").unwrap();
        assert_eq!(redact(&with_port), "http://127.0.0.1:9000/");

        let with_credentials = Url::parse("https://user:pass@example.com/hook").unwrap();
        let redacted = redact(&with_credentials);
        assert!(!redacted.contains("pass"), "{redacted}");
    }

    #[test]
    fn scrubbing_removes_the_url_from_borrowed_error_text() {
        let url = Url::parse("https://hooks.slack.com/services/T000/B111/XXXXsecretXXXX").unwrap();

        let full = scrub(
            "error sending request for url (https://hooks.slack.com/services/T000/B111/XXXXsecretXXXX)",
            &url,
        );
        assert!(!full.contains("XXXXsecretXXXX"), "{full}");

        let path_only = scrub("no route for /services/T000/B111/XXXXsecretXXXX", &url);
        assert!(!path_only.contains("XXXXsecretXXXX"), "{path_only}");

        // A body that quotes only the token, without the surrounding path.
        let fragment = scrub("unknown webhook XXXXsecretXXXX", &url);
        assert!(!fragment.contains("XXXXsecretXXXX"), "{fragment}");

        let query_url = Url::parse("https://example.com/hook?token=s3cr3t-token-value").unwrap();
        let with_query = scrub("rejected token s3cr3t-token-value", &query_url);
        assert!(!with_query.contains("s3cr3t-token-value"), "{with_query}");
    }

    #[test]
    fn a_typo_in_the_event_filter_fails_loudly_instead_of_silently_muting() {
        let error = EventFilter::parse("peer.up,peer.uP2").expect_err("typo must fail");
        assert!(error.to_string().contains("peer.uP2"), "{error}");
        assert!(EventFilter::parse("").is_err());
        assert!(EventFilter::parse(" , , ").is_err());
    }

    #[test]
    fn filters_admit_exactly_what_they_name() {
        let filter = EventFilter::parse("peer.up, model_unloaded").expect("valid filter");

        assert!(filter.allows(EventKind::PeerUp));
        assert!(filter.allows(EventKind::ModelUnloaded));
        assert!(!filter.allows(EventKind::PeerDown));
        assert!(EventFilter::All.allows(EventKind::PeerDown));
        assert_eq!(filter.to_json(), json!(["peer.up", "model.unloaded"]));
    }

    #[test]
    fn numeric_knobs_are_bounded_and_reject_junk() {
        assert!(parse(&args(&["--queue-capacity", "0"]), &env(&[])).is_err());
        assert!(parse(&args(&["--queue-capacity", "999999"]), &env(&[])).is_err());
        assert!(parse(&args(&["--max-attempts", "11"]), &env(&[])).is_err());
        assert!(parse(&args(&["--timeout-secs", "0"]), &env(&[])).is_err());
        assert!(parse(&args(&["--coalesce-secs", "abc"]), &env(&[])).is_err());

        // Zero is the documented "disable coalescing" value, not an error.
        let settings = parse(&args(&["--coalesce-secs", "0"]), &env(&[])).expect("zero is valid");
        assert_eq!(settings.coalesce_window, Duration::ZERO);
    }

    #[test]
    fn unknown_options_and_dangling_values_are_rejected() {
        assert!(parse(&args(&["--nope"]), &env(&[])).is_err());
        assert!(parse(&args(&["--format"]), &env(&[])).is_err());
    }

    #[test]
    fn a_blank_env_value_is_treated_as_unset_rather_than_as_a_broken_url() {
        let settings = parse(
            &[],
            &env(&[
                (WEBHOOK_URL_ENV, "   "),
                (PLUGIN_URL_ENV, "https://ok.example/x"),
            ]),
        )
        .expect("blank falls through");
        assert_eq!(settings.target.expect("configured").source, PLUGIN_URL_ENV);
    }

    #[test]
    fn the_settings_snapshot_never_carries_the_raw_url() {
        let settings = parse(
            &[],
            &env(&[(
                WEBHOOK_URL_ENV,
                "https://hooks.slack.com/services/XXXXsecret",
            )]),
        )
        .expect("parses");
        let rendered = settings.to_json().to_string();

        assert!(rendered.contains("hooks.slack.com"));
        assert!(!rendered.contains("XXXXsecret"), "{rendered}");
    }
}
