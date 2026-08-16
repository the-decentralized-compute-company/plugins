//! Effective configuration for the `semantic-cache` process.
//!
//! Every knob here comes from `[[plugin]].args`, `[[plugin]].url`, or the
//! process environment — never from `[plugin.settings]`. That is not a style
//! preference. The host contract is explicit that settings are stored and
//! validated by the host and *never delivered to the plugin process*, so a
//! `config_schema` here would render a similarity-threshold control in the
//! console that this process could not read. An operator would move the slider
//! and the cache would keep using the old threshold. Rather than ship that,
//! the plugin declares no settings schema and documents the launch contract.
//!
//! Precedence, highest first: command-line flag, environment variable,
//! `TDCC_PLUGIN_URL` (endpoint only), built-in default.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use url::{Host, Url};

/// Conservative by design. A false hit returns a confidently wrong answer,
/// which is far worse than paying for the model call again. Sentence
/// embeddings put negations and near-antonyms ("how do I enable X" vs "how do
/// I disable X") very close together, so anything in the 0.85–0.90 range that
/// looks good on a paraphrase set will also merge pairs that mean the
/// opposite. Raise it if you measure false hits; lower it only with a
/// `best_similarity` histogram from real traffic in front of you.
pub const DEFAULT_MIN_SIMILARITY: f64 = 0.95;

/// One hour. A cache that answers "what is deployed right now" from last week
/// is a correctness bug, not a saving. Raise it for stable reference-style
/// workloads; keep it short (or zero the entry TTL per store call) for
/// anything that reads current state.
pub const DEFAULT_TTL_SECONDS: u64 = 3_600;

/// Entry-count bound. With a 1536-dimension embedder this is roughly 6 MiB of
/// vectors plus the stored prompt and completion text.
pub const DEFAULT_MAX_ENTRIES: usize = 1_000;

/// Payload-byte bound, applied alongside `max_entries` — one 30 MiB completion
/// should not be allowed to occupy the whole cache just because it is one row.
pub const DEFAULT_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Requests hotter than this are never stored and never served. Temperature is
/// part of the cache key, so a `t=0.7` request can only ever match another
/// `t=0.7` request — but past a point the caller is explicitly asking for
/// variance, and handing back a frozen sample stops being a cache and starts
/// being a lie. 1.0 is the OpenAI default; above it, output spread is wide
/// enough that reuse is not defensible.
pub const DEFAULT_MAX_TEMPERATURE: f64 = 1.0;

/// A paraphrase is roughly the same length as its original. When the nearest
/// neighbour is more than this many times longer (or shorter) than the query,
/// the high cosine score is usually topic overlap rather than equivalence.
pub const DEFAULT_MAX_LENGTH_RATIO: f64 = 2.0;

/// Embedding calls sit in the latency path of every miss, so the timeout is
/// short on purpose: a slow embedder should surface as an error, not as a
/// request that hangs longer than the model call it was meant to avoid.
pub const DEFAULT_REQUEST_TIMEOUT_SECONDS: u64 = 10;

/// The node's own OpenAI-compatible API. See README.md: as of the SDK this
/// crate is built against, that frontend serves `/v1/models`,
/// `/v1/chat/completions`, `/v1/completions` and `/v1/responses` — it does
/// **not** implement `/v1/embeddings`. The default points at the node anyway
/// so that the day it grows one this plugin works with no configuration, and
/// until then `status` reports the failure in plain language instead of the
/// cache silently never hitting.
pub const DEFAULT_EMBEDDINGS_BASE_URL: &str = "http://127.0.0.1:9337/v1";

/// Read from the environment only. Never a command-line flag: `[[plugin]].args`
/// is written into `~/.tdcc/config.toml` in plaintext and shows up in process
/// listings.
pub const API_KEY_ENV: &str = "TDCC_SEMANTIC_CACHE_API_KEY";

/// Prefix for every environment-variable form of a flag.
pub const ENV_PREFIX: &str = "TDCC_SEMANTIC_CACHE_";

/// Set by the host from `[[plugin]].url`.
pub const PLUGIN_URL_ENV: &str = "TDCC_PLUGIN_URL";

/// One tunable knob, in both of its spellings.
struct Knob {
    flag: &'static str,
    env: &'static str,
    /// A boolean knob is a bare flag on the command line (`--allow-remote`)
    /// and a `true`/`false` string in the environment.
    boolean: bool,
}

const KNOBS: &[Knob] = &[
    Knob {
        flag: "--embeddings-url",
        env: "EMBEDDINGS_URL",
        boolean: false,
    },
    Knob {
        flag: "--embedding-model",
        env: "EMBEDDING_MODEL",
        boolean: false,
    },
    Knob {
        flag: "--min-similarity",
        env: "MIN_SIMILARITY",
        boolean: false,
    },
    Knob {
        flag: "--ttl-seconds",
        env: "TTL_SECONDS",
        boolean: false,
    },
    Knob {
        flag: "--max-entries",
        env: "MAX_ENTRIES",
        boolean: false,
    },
    Knob {
        flag: "--max-bytes",
        env: "MAX_BYTES",
        boolean: false,
    },
    Knob {
        flag: "--max-temperature",
        env: "MAX_TEMPERATURE",
        boolean: false,
    },
    Knob {
        flag: "--max-length-ratio",
        env: "MAX_LENGTH_RATIO",
        boolean: false,
    },
    Knob {
        flag: "--request-timeout-seconds",
        env: "REQUEST_TIMEOUT_SECONDS",
        boolean: false,
    },
    Knob {
        flag: "--allow-remote-embeddings",
        env: "ALLOW_REMOTE_EMBEDDINGS",
        boolean: true,
    },
];

fn knob(flag: &str) -> Option<&'static Knob> {
    KNOBS.iter().find(|knob| knob.flag == flag)
}

/// An API key that cannot be printed by accident.
///
/// `Debug` is implemented by hand because the natural thing to do while
/// debugging a startup problem is to log the whole config struct.
#[derive(Clone)]
pub struct ApiKey(String);

impl ApiKey {
    pub fn as_header_value(&self) -> String {
        format!("Bearer {}", self.0)
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ApiKey(<redacted>)")
    }
}

#[derive(Debug)]
pub struct ConfigError(pub String);

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

fn fail<T>(message: impl Into<String>) -> Result<T, ConfigError> {
    Err(ConfigError(message.into()))
}

#[derive(Clone, Debug)]
pub struct Config {
    /// Fully-resolved POST target, e.g. `http://127.0.0.1:9337/v1/embeddings`.
    pub embeddings_url: Url,
    /// Sent as the `model` field. Empty means "omit the field"; most servers
    /// require it, but llama.cpp's `--embeddings` server ignores it.
    pub embedding_model: String,
    pub min_similarity: f64,
    /// Zero means entries never expire. Allowed, warned about, never default.
    pub ttl_seconds: u64,
    pub max_entries: usize,
    pub max_bytes: u64,
    pub max_temperature: f64,
    pub max_length_ratio: f64,
    pub request_timeout: Duration,
    pub allow_remote_embeddings: bool,
    pub api_key: Option<ApiKey>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // Infallible: a literal that the `defaults_are_valid` test parses.
            embeddings_url: resolve_embeddings_url(DEFAULT_EMBEDDINGS_BASE_URL)
                .expect("the built-in default endpoint is a valid URL"),
            embedding_model: String::new(),
            min_similarity: DEFAULT_MIN_SIMILARITY,
            ttl_seconds: DEFAULT_TTL_SECONDS,
            max_entries: DEFAULT_MAX_ENTRIES,
            max_bytes: DEFAULT_MAX_BYTES,
            max_temperature: DEFAULT_MAX_TEMPERATURE,
            max_length_ratio: DEFAULT_MAX_LENGTH_RATIO,
            request_timeout: Duration::from_secs(DEFAULT_REQUEST_TIMEOUT_SECONDS),
            allow_remote_embeddings: false,
            api_key: None,
        }
    }
}

impl Config {
    /// Build the effective config from parsed flags and a filtered environment.
    ///
    /// `environment` is a map of already-filtered variables (see
    /// [`collect_environment`]); passing it in rather than reading the process
    /// environment is what makes every branch below testable.
    pub fn resolve(
        argv: &[String],
        environment: &BTreeMap<String, String>,
    ) -> Result<Self, ConfigError> {
        let flags = parse_args(argv)?;
        let mut config = Self::default();

        let lookup = |name: &str| -> Option<String> {
            let knob = knob(name)?;
            flags.get(name).cloned().or_else(|| {
                environment
                    .get(&format!("{ENV_PREFIX}{}", knob.env))
                    .cloned()
            })
        };

        // The endpoint has a third source: the host passes `[[plugin]].url`
        // through as TDCC_PLUGIN_URL, which is the idiomatic place to put an
        // endpoint address for a plugin that attaches to one.
        let raw_url = lookup("--embeddings-url")
            .or_else(|| environment.get(PLUGIN_URL_ENV).cloned())
            .unwrap_or_else(|| DEFAULT_EMBEDDINGS_BASE_URL.to_string());
        config.embeddings_url = resolve_embeddings_url(&raw_url)?;

        if let Some(model) = lookup("--embedding-model") {
            config.embedding_model = model.trim().to_string();
        }
        if let Some(raw) = lookup("--min-similarity") {
            config.min_similarity = parse_unit_interval("--min-similarity", &raw)?;
        }
        if let Some(raw) = lookup("--ttl-seconds") {
            config.ttl_seconds = parse_u64("--ttl-seconds", &raw)?;
        }
        if let Some(raw) = lookup("--max-entries") {
            let value = parse_u64("--max-entries", &raw)?;
            if value == 0 {
                return fail("--max-entries must be at least 1");
            }
            config.max_entries = usize::try_from(value)
                .map_err(|_| ConfigError("--max-entries does not fit in a usize".into()))?;
        }
        if let Some(raw) = lookup("--max-bytes") {
            let value = parse_u64("--max-bytes", &raw)?;
            if value < 64 * 1024 {
                return fail("--max-bytes must be at least 65536 (64 KiB)");
            }
            config.max_bytes = value;
        }
        if let Some(raw) = lookup("--max-temperature") {
            let value = parse_finite_f64("--max-temperature", &raw)?;
            if !(0.0..=2.0).contains(&value) {
                return fail("--max-temperature must be between 0.0 and 2.0");
            }
            config.max_temperature = value;
        }
        if let Some(raw) = lookup("--max-length-ratio") {
            let value = parse_finite_f64("--max-length-ratio", &raw)?;
            if value < 1.0 {
                return fail("--max-length-ratio must be at least 1.0");
            }
            config.max_length_ratio = value;
        }
        if let Some(raw) = lookup("--request-timeout-seconds") {
            let value = parse_u64("--request-timeout-seconds", &raw)?;
            if !(1..=300).contains(&value) {
                return fail("--request-timeout-seconds must be between 1 and 300");
            }
            config.request_timeout = Duration::from_secs(value);
        }
        if let Some(raw) = lookup("--allow-remote-embeddings") {
            config.allow_remote_embeddings = parse_bool("--allow-remote-embeddings", &raw)?;
        }

        endpoint_reach(&config.embeddings_url, config.allow_remote_embeddings)?;

        config.api_key = environment
            .get(API_KEY_ENV)
            .map(|key| key.trim().to_string())
            .filter(|key| !key.is_empty())
            .map(ApiKey);

        Ok(config)
    }

    /// Warnings worth putting in the host's log at startup. Deliberately not
    /// errors: they are choices an operator is allowed to make, just not
    /// quietly.
    pub fn advisories(&self) -> Vec<String> {
        let mut advisories = Vec::new();
        if self.min_similarity < 0.85 {
            advisories.push(format!(
                "min_similarity is {:.3}; below ~0.85 reworded-but-opposite prompts \
                 (\"enable\" vs \"disable\") start matching each other",
                self.min_similarity
            ));
        }
        if self.ttl_seconds == 0 {
            advisories.push(
                "ttl_seconds is 0: entries never expire, so any question about current \
                 state will be answered from whenever it was first asked"
                    .to_string(),
            );
        }
        if self.allow_remote_embeddings && !is_loopback(&self.embeddings_url) {
            advisories.push(format!(
                "every cached prompt is sent to the non-loopback endpoint {}",
                self.embeddings_url
            ));
        }
        if self.embedding_model.is_empty() {
            advisories.push(
                "no --embedding-model set; the `model` field is omitted from embedding \
                 requests, which most servers reject"
                    .to_string(),
            );
        }
        advisories
    }

    /// A one-line startup summary. Contains no secrets by construction — the
    /// API key is not a field here.
    pub fn startup_summary(&self) -> String {
        format!(
            "endpoint={} model={} min_similarity={:.3} ttl_seconds={} max_entries={} \
             max_bytes={} max_temperature={:.2} max_length_ratio={:.2} api_key={}",
            self.embeddings_url,
            if self.embedding_model.is_empty() {
                "<unset>"
            } else {
                &self.embedding_model
            },
            self.min_similarity,
            self.ttl_seconds,
            self.max_entries,
            self.max_bytes,
            self.max_temperature,
            self.max_length_ratio,
            if self.api_key.is_some() {
                "set"
            } else {
                "unset"
            },
        )
    }
}

/// Copy just the variables this plugin reads out of the process environment.
///
/// Filtering rather than cloning the whole environment keeps unrelated secrets
/// out of a struct that gets passed around.
pub fn collect_environment() -> BTreeMap<String, String> {
    std::env::vars()
        .filter(|(name, _)| name.starts_with(ENV_PREFIX) || name == PLUGIN_URL_ENV)
        .collect()
}

/// Parse `--flag value`, `--flag=value`, and bare boolean flags.
///
/// An unknown flag is a hard error. Silently ignoring a mistyped
/// `--min-similarty` would leave the operator believing they had loosened or
/// tightened the cache when they had not.
pub fn parse_args(argv: &[String]) -> Result<BTreeMap<String, String>, ConfigError> {
    let mut flags = BTreeMap::new();
    let mut index = 0;
    while index < argv.len() {
        let argument = argv[index].as_str();
        let (name, inline_value) = match argument.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (argument, None),
        };

        let Some(knob) = knob(name) else {
            return fail(format!(
                "unknown option: {argument} (known options: {})",
                KNOBS.iter().map(|k| k.flag).collect::<Vec<_>>().join(", ")
            ));
        };

        let value = match (knob.boolean, inline_value) {
            // `--allow-remote-embeddings` with no value means "yes".
            (true, None) => "true".to_string(),
            (true, Some(value)) | (false, Some(value)) => value,
            (false, None) => {
                index += 1;
                match argv.get(index) {
                    Some(value) => value.clone(),
                    None => return fail(format!("{name} requires a value")),
                }
            }
        };

        if flags.insert(name.to_string(), value).is_some() {
            return fail(format!("{name} was given more than once"));
        }
        index += 1;
    }
    Ok(flags)
}

/// Turn a configured base URL into the exact POST target.
///
/// Accepts both `http://host/v1` and `http://host/v1/embeddings` so that an
/// operator who reuses the `[[plugin]].url` convention from `openai-endpoint`
/// does not have to think about it.
pub fn resolve_embeddings_url(raw: &str) -> Result<Url, ConfigError> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return fail("embeddings URL is empty");
    }
    let joined = if trimmed.ends_with("/embeddings") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/embeddings")
    };

    let url = Url::parse(&joined)
        .map_err(|error| ConfigError(format!("embeddings URL {raw:?} is not a URL: {error}")))?;

    if !matches!(url.scheme(), "http" | "https") {
        return fail(format!(
            "embeddings URL scheme must be http or https, got {:?}",
            url.scheme()
        ));
    }
    // Credentials in the URL would be written into ~/.tdcc/config.toml in
    // plaintext. Point the operator at the environment variable instead.
    if !url.username().is_empty() || url.password().is_some() {
        return fail(format!(
            "embeddings URL must not carry credentials; set {API_KEY_ENV} instead"
        ));
    }
    if url.host().is_none() {
        return fail("embeddings URL has no host");
    }
    Ok(url)
}

/// True when the URL's host resolves, syntactically, to this machine.
///
/// This is a syntactic check on purpose: it is a guard against an operator
/// pasting a public endpoint by accident, not a defence against a hostile DNS
/// server. It is stated that way in the README so nobody mistakes it for one.
pub fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        Some(Host::Domain(domain)) => {
            let domain = domain.to_ascii_lowercase();
            domain == "localhost" || domain.ends_with(".localhost")
        }
        None => false,
    }
}

/// Refuse to ship prompts off-box unless the operator asked for it.
///
/// This plugin runs on hardware people contributed to a mesh. Every lookup and
/// every store sends the user's prompt text to the embeddings endpoint, so the
/// default has to be "loopback only" and going wider has to be a deliberate,
/// visible act.
pub fn endpoint_reach(url: &Url, allow_remote: bool) -> Result<(), ConfigError> {
    if is_loopback(url) || allow_remote {
        return Ok(());
    }
    fail(format!(
        "refusing to send prompts to the non-loopback embeddings endpoint {url}: \
         pass --allow-remote-embeddings to allow it"
    ))
}

fn parse_finite_f64(flag: &str, raw: &str) -> Result<f64, ConfigError> {
    let value: f64 = raw
        .trim()
        .parse()
        .map_err(|_| ConfigError(format!("{flag} expects a number, got {raw:?}")))?;
    if !value.is_finite() {
        return fail(format!("{flag} must be finite, got {raw:?}"));
    }
    Ok(value)
}

fn parse_unit_interval(flag: &str, raw: &str) -> Result<f64, ConfigError> {
    let value = parse_finite_f64(flag, raw)?;
    if !(0.0..=1.0).contains(&value) {
        return fail(format!(
            "{flag} is a cosine similarity and must be between 0.0 and 1.0, got {value}"
        ));
    }
    Ok(value)
}

fn parse_u64(flag: &str, raw: &str) -> Result<u64, ConfigError> {
    raw.trim()
        .parse()
        .map_err(|_| ConfigError(format!("{flag} expects a whole number, got {raw:?}")))
}

fn parse_bool(flag: &str, raw: &str) -> Result<bool, ConfigError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        other => fail(format!("{flag} expects a boolean, got {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|value| value.to_string()).collect()
    }

    fn environment(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn defaults_are_valid_and_conservative() {
        let config = Config::default();
        assert_eq!(
            config.embeddings_url.as_str(),
            "http://127.0.0.1:9337/v1/embeddings"
        );
        assert!(
            config.min_similarity >= 0.95,
            "the default must stay conservative"
        );
        assert!(config.ttl_seconds > 0, "entries must expire by default");
        assert!(
            !config.allow_remote_embeddings,
            "off-box by default is not acceptable"
        );
        assert!(config.api_key.is_none());
    }

    #[test]
    fn flags_accept_both_spellings() {
        let parsed = parse_args(&args(&[
            "--min-similarity=0.97",
            "--ttl-seconds",
            "60",
            "--allow-remote-embeddings",
        ]))
        .expect("valid flags");
        assert_eq!(
            parsed.get("--min-similarity").map(String::as_str),
            Some("0.97")
        );
        assert_eq!(parsed.get("--ttl-seconds").map(String::as_str), Some("60"));
        assert_eq!(
            parsed.get("--allow-remote-embeddings").map(String::as_str),
            Some("true")
        );
    }

    #[test]
    fn a_mistyped_flag_is_an_error_rather_than_a_silent_default() {
        let error = parse_args(&args(&["--min-similarty", "0.5"])).expect_err("typo must fail");
        assert!(error.0.contains("unknown option"), "{}", error.0);
    }

    #[test]
    fn a_repeated_flag_is_an_error() {
        let error = parse_args(&args(&["--ttl-seconds", "10", "--ttl-seconds", "20"]))
            .expect_err("ambiguous config must fail");
        assert!(error.0.contains("more than once"), "{}", error.0);
    }

    #[test]
    fn a_value_flag_without_a_value_is_an_error() {
        let error = parse_args(&args(&["--ttl-seconds"])).expect_err("missing value must fail");
        assert!(error.0.contains("requires a value"), "{}", error.0);
    }

    #[test]
    fn flags_win_over_environment_which_wins_over_plugin_url() {
        let config = Config::resolve(
            &args(&["--embeddings-url", "http://127.0.0.1:1111/v1"]),
            &environment(&[
                (
                    "TDCC_SEMANTIC_CACHE_EMBEDDINGS_URL",
                    "http://127.0.0.1:2222/v1",
                ),
                ("TDCC_PLUGIN_URL", "http://127.0.0.1:3333/v1"),
            ]),
        )
        .expect("valid config");
        assert_eq!(
            config.embeddings_url.as_str(),
            "http://127.0.0.1:1111/v1/embeddings"
        );

        let config = Config::resolve(
            &args(&[]),
            &environment(&[
                (
                    "TDCC_SEMANTIC_CACHE_EMBEDDINGS_URL",
                    "http://127.0.0.1:2222/v1",
                ),
                ("TDCC_PLUGIN_URL", "http://127.0.0.1:3333/v1"),
            ]),
        )
        .expect("valid config");
        assert_eq!(
            config.embeddings_url.as_str(),
            "http://127.0.0.1:2222/v1/embeddings"
        );

        let config = Config::resolve(
            &args(&[]),
            &environment(&[("TDCC_PLUGIN_URL", "http://127.0.0.1:3333/v1")]),
        )
        .expect("valid config");
        assert_eq!(
            config.embeddings_url.as_str(),
            "http://127.0.0.1:3333/v1/embeddings"
        );
    }

    #[test]
    fn the_endpoint_path_is_appended_only_once() {
        for raw in [
            "http://127.0.0.1:9337/v1",
            "http://127.0.0.1:9337/v1/",
            "http://127.0.0.1:9337/v1/embeddings",
        ] {
            let url = resolve_embeddings_url(raw).expect("valid URL");
            assert_eq!(
                url.as_str(),
                "http://127.0.0.1:9337/v1/embeddings",
                "input {raw}"
            );
        }
    }

    #[test]
    fn non_http_schemes_are_rejected() {
        for raw in [
            "file:///etc/passwd",
            "ftp://example.com/v1",
            "javascript:alert(1)",
        ] {
            assert!(
                resolve_embeddings_url(raw).is_err(),
                "{raw} must be rejected"
            );
        }
    }

    #[test]
    fn credentials_in_the_url_are_rejected() {
        let error = resolve_embeddings_url("https://user:secret@example.com/v1")
            .expect_err("credentials must be rejected");
        assert!(error.0.contains(API_KEY_ENV), "{}", error.0);
    }

    #[test]
    fn userinfo_cannot_disguise_a_remote_host_as_loopback() {
        // `http://127.0.0.1@evil.example/v1` has host `evil.example`. Matching
        // on the string "127.0.0.1" instead of parsing would wave it through.
        let url = resolve_embeddings_url("http://127.0.0.1@evil.example/v1")
            .expect_err("userinfo is credentials-shaped and rejected outright");
        assert!(url.0.contains(API_KEY_ENV), "{}", url.0);

        let url = Url::parse("http://127.0.0.1.evil.example/v1/embeddings").expect("parses");
        assert!(!is_loopback(&url), "a suffixed hostname is not loopback");
        assert!(endpoint_reach(&url, false).is_err());
    }

    #[test]
    fn loopback_spellings_are_recognised() {
        for raw in [
            "http://127.0.0.1:9337/v1",
            "http://127.5.5.5:9337/v1",
            "http://localhost:11434/v1",
            "http://[::1]:8080/v1",
        ] {
            let url = resolve_embeddings_url(raw).expect("valid URL");
            assert!(is_loopback(&url), "{raw} should be loopback");
            assert!(
                endpoint_reach(&url, false).is_ok(),
                "{raw} should be allowed"
            );
        }
    }

    #[test]
    fn a_remote_endpoint_needs_an_explicit_opt_in() {
        let url = resolve_embeddings_url("https://api.example.com/v1").expect("valid URL");
        let error = endpoint_reach(&url, false).expect_err("remote must be refused by default");
        assert!(error.0.contains("--allow-remote-embeddings"), "{}", error.0);
        assert!(endpoint_reach(&url, true).is_ok());
    }

    #[test]
    fn resolve_refuses_a_remote_endpoint_without_the_flag() {
        let error = Config::resolve(
            &args(&["--embeddings-url", "https://api.example.com/v1"]),
            &environment(&[]),
        )
        .expect_err("remote endpoints are refused by default");
        assert!(error.0.contains("--allow-remote-embeddings"), "{}", error.0);
    }

    #[test]
    fn out_of_range_values_are_rejected() {
        for flag in [
            "--min-similarity=1.5",
            "--min-similarity=-0.1",
            "--min-similarity=abc",
            "--max-entries=0",
            "--max-bytes=1024",
            "--max-length-ratio=0.5",
            "--request-timeout-seconds=0",
            "--request-timeout-seconds=9999",
            "--max-temperature=5",
        ] {
            assert!(
                Config::resolve(&args(&[flag]), &environment(&[])).is_err(),
                "{flag} must be rejected"
            );
        }
    }

    #[test]
    fn the_api_key_comes_from_the_environment_and_never_prints() {
        let config = Config::resolve(
            &args(&[]),
            &environment(&[(API_KEY_ENV, "sk-super-secret-value")]),
        )
        .expect("valid config");
        let key = config.api_key.as_ref().expect("key is read");
        assert_eq!(key.as_header_value(), "Bearer sk-super-secret-value");
        assert_eq!(format!("{key:?}"), "ApiKey(<redacted>)");
        assert!(!format!("{config:?}").contains("sk-super-secret-value"));
        assert!(!config.startup_summary().contains("sk-super-secret-value"));
        assert!(config.startup_summary().contains("api_key=set"));
    }

    #[test]
    fn an_empty_api_key_is_treated_as_unset() {
        let config = Config::resolve(&args(&[]), &environment(&[(API_KEY_ENV, "   ")]))
            .expect("valid config");
        assert!(config.api_key.is_none());
    }

    #[test]
    fn advisories_name_the_dangerous_choices() {
        let config = Config::resolve(
            &args(&[
                "--min-similarity=0.6",
                "--ttl-seconds=0",
                "--embeddings-url=https://api.example.com/v1",
                "--allow-remote-embeddings",
                "--embedding-model=text-embedding-3-small",
            ]),
            &environment(&[]),
        )
        .expect("valid config");
        let advisories = config.advisories().join("\n");
        assert!(advisories.contains("min_similarity"), "{advisories}");
        assert!(advisories.contains("ttl_seconds is 0"), "{advisories}");
        assert!(advisories.contains("non-loopback"), "{advisories}");
        assert!(!advisories.contains("no --embedding-model"), "{advisories}");
    }

    #[test]
    fn a_default_config_still_advises_about_the_missing_embedding_model() {
        let config = Config::resolve(&args(&[]), &environment(&[])).expect("valid config");
        assert!(
            config
                .advisories()
                .iter()
                .any(|line| line.contains("no --embedding-model")),
            "an unset model is the most common first-run failure"
        );
    }
}
