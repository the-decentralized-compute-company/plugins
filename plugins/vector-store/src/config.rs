//! Effective configuration for the `vector-store` process.
//!
//! Every knob here comes from `[[plugin]].args`, `[[plugin]].url`, or the
//! process environment — never from `[plugin.settings]`. That is not a style
//! preference. The host contract is explicit that settings are stored and
//! validated by the host and *never delivered to the plugin process*, so a
//! `config_schema` here would render a chunk-size control in the console that
//! this process could not read. An operator would move the slider, ingest a
//! corpus, and get chunks of the old size. Rather than ship that, the plugin
//! declares no settings schema and documents the launch contract.
//!
//! Precedence, highest first: **command-line flag → environment variable →
//! `TDCC_PLUGIN_URL` (endpoint only) → built-in default**.
//!
//! An unknown flag or an out-of-range value is a **startup error**, never a
//! warning. A typo in `--chunk-overlap-chars` that was quietly ignored leaves
//! an operator believing they tuned retrieval when they did not, and the
//! evidence arrives weeks later as bad answers.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use url::{Host, Url};

use crate::chunk::ChunkOptions;
use crate::store::{DEFAULT_MAX_CHUNKS_PER_COLLECTION, StoreLimits};

pub const PLUGIN_NAME: &str = "vector-store";
pub const PLUGIN_VERSION: &str = "0.1.0";

/// The node's own OpenAI-compatible API. As of the SDK this crate is built
/// against, that frontend's router declares `/v1/models`,
/// `/v1/chat/completions`, `/v1/completions` and `/v1/responses` — it does
/// **not** implement `/v1/embeddings`. The default points at the node anyway,
/// so that the day it grows the route this plugin works with no configuration,
/// and until then every failure says so in plain language rather than looking
/// like an empty index.
pub const DEFAULT_EMBEDDINGS_BASE_URL: &str = "http://127.0.0.1:9337/v1";

/// Roughly 300 tokens of English prose. Big enough to hold a complete idea
/// with its context, small enough that a hit points at a passage rather than a
/// page. Characters, not tokens — there is no tokenizer here.
pub const DEFAULT_CHUNK_CHARS: usize = 1_200;

/// Carried from the end of one chunk into the start of the next, rounded to
/// whole blocks. A fact that straddles a boundary then appears intact in one
/// of them. Roughly a paragraph.
pub const DEFAULT_CHUNK_OVERLAP_CHARS: usize = 200;

/// The hard ceiling on one chunk. Twice the target leaves room for a long
/// paragraph to stay whole rather than being cut for the sake of tidiness.
/// Every embedding model has an input limit; keep this well inside yours.
pub const DEFAULT_MAX_CHUNK_CHARS: usize = 2_400;

/// Namespaces are cheap, but not free: each is a file and an in-memory map.
pub const DEFAULT_MAX_COLLECTIONS: usize = 64;

/// Estimated payload bytes across every collection. At 768 dimensions a chunk
/// costs roughly 4 KB in memory, so this is about 130 000 chunks — well past
/// the per-collection cap, which is the bound that actually bites first.
pub const DEFAULT_MAX_STORE_BYTES: u64 = 512 * 1024 * 1024;

/// Largest single document accepted by one `upsert`.
pub const DEFAULT_MAX_DOCUMENT_BYTES: usize = 1024 * 1024;

/// Documents per `upsert` call.
pub const DEFAULT_MAX_DOCUMENTS_PER_CALL: usize = 64;

/// Default `top_k` when a caller does not ask for one.
pub const DEFAULT_TOP_K: usize = 8;

/// Ceiling on `top_k`, whatever a caller asks for. A retriever that returns
/// 500 passages has not retrieved anything.
pub const MAX_TOP_K: usize = 100;

/// Texts per embeddings request. Large enough to matter for a big ingest,
/// small enough that one failure does not lose much work.
pub const DEFAULT_EMBED_BATCH_SIZE: usize = 32;

/// Embedding calls sit in the latency path of every query, and an ingest can
/// send a large batch, so this is longer than a chat timeout but still finite.
pub const DEFAULT_REQUEST_TIMEOUT_SECONDS: u64 = 30;

/// Read from the environment only. Never a command-line flag: `[[plugin]].args`
/// is written into `~/.tdcc/config.toml` in plaintext and shows up in process
/// listings.
pub const API_KEY_ENV: &str = "TDCC_VECTOR_STORE_API_KEY";

/// Prefix for every environment-variable form of a flag.
pub const ENV_PREFIX: &str = "TDCC_VECTOR_STORE_";

/// Set by the host from `[[plugin]].url`.
pub const PLUGIN_URL_ENV: &str = "TDCC_PLUGIN_URL";

/// One tunable knob, in both of its spellings.
struct Knob {
    flag: &'static str,
    env: &'static str,
    /// A boolean knob is a bare flag on the command line
    /// (`--allow-remote-embeddings`) and a `true`/`false` string in the
    /// environment.
    boolean: bool,
}

const KNOBS: &[Knob] = &[
    Knob {
        flag: "--data-dir",
        env: "DATA_DIR",
        boolean: false,
    },
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
        flag: "--chunk-chars",
        env: "CHUNK_CHARS",
        boolean: false,
    },
    Knob {
        flag: "--chunk-overlap-chars",
        env: "CHUNK_OVERLAP_CHARS",
        boolean: false,
    },
    Knob {
        flag: "--max-chunk-chars",
        env: "MAX_CHUNK_CHARS",
        boolean: false,
    },
    Knob {
        flag: "--max-collections",
        env: "MAX_COLLECTIONS",
        boolean: false,
    },
    Knob {
        flag: "--max-chunks-per-collection",
        env: "MAX_CHUNKS_PER_COLLECTION",
        boolean: false,
    },
    Knob {
        flag: "--max-store-bytes",
        env: "MAX_STORE_BYTES",
        boolean: false,
    },
    Knob {
        flag: "--max-document-bytes",
        env: "MAX_DOCUMENT_BYTES",
        boolean: false,
    },
    Knob {
        flag: "--max-documents-per-call",
        env: "MAX_DOCUMENTS_PER_CALL",
        boolean: false,
    },
    Knob {
        flag: "--default-top-k",
        env: "DEFAULT_TOP_K",
        boolean: false,
    },
    Knob {
        flag: "--embed-batch-size",
        env: "EMBED_BATCH_SIZE",
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

fn known_options() -> String {
    KNOBS
        .iter()
        .map(|knob| knob.flag)
        .collect::<Vec<_>>()
        .join(", ")
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
    /// Where collection logs live. The **only** directory this plugin ever
    /// writes to.
    pub data_dir: PathBuf,
    /// Fully-resolved POST target, e.g. `http://127.0.0.1:11434/v1/embeddings`.
    pub embeddings_url: Url,
    /// Sent as the `model` field, and pinned into every collection. Empty
    /// means "omit the field"; most servers require it.
    pub embedding_model: String,
    pub chunk_chars: usize,
    pub chunk_overlap_chars: usize,
    pub max_chunk_chars: usize,
    pub max_collections: usize,
    pub max_chunks_per_collection: usize,
    pub max_store_bytes: u64,
    pub max_document_bytes: usize,
    pub max_documents_per_call: usize,
    pub default_top_k: usize,
    pub embed_batch_size: usize,
    pub request_timeout: Duration,
    pub allow_remote_embeddings: bool,
    pub api_key: Option<ApiKey>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(&BTreeMap::new()),
            // Infallible: a literal the `defaults_are_valid` test parses.
            embeddings_url: resolve_embeddings_url(DEFAULT_EMBEDDINGS_BASE_URL)
                .expect("the built-in default endpoint is a valid URL"),
            embedding_model: String::new(),
            chunk_chars: DEFAULT_CHUNK_CHARS,
            chunk_overlap_chars: DEFAULT_CHUNK_OVERLAP_CHARS,
            max_chunk_chars: DEFAULT_MAX_CHUNK_CHARS,
            max_collections: DEFAULT_MAX_COLLECTIONS,
            max_chunks_per_collection: DEFAULT_MAX_CHUNKS_PER_COLLECTION,
            max_store_bytes: DEFAULT_MAX_STORE_BYTES,
            max_document_bytes: DEFAULT_MAX_DOCUMENT_BYTES,
            max_documents_per_call: DEFAULT_MAX_DOCUMENTS_PER_CALL,
            default_top_k: DEFAULT_TOP_K,
            embed_batch_size: DEFAULT_EMBED_BATCH_SIZE,
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
        let mut config = Self {
            data_dir: default_data_dir(environment),
            ..Self::default()
        };

        let lookup = |name: &str| -> Option<String> {
            let knob = knob(name)?;
            flags.get(name).cloned().or_else(|| {
                environment
                    .get(&format!("{ENV_PREFIX}{}", knob.env))
                    .cloned()
            })
        };

        if let Some(raw) = lookup("--data-dir") {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return fail("--data-dir must not be empty");
            }
            config.data_dir = PathBuf::from(trimmed);
        }

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
        if let Some(raw) = lookup("--chunk-chars") {
            config.chunk_chars = parse_usize("--chunk-chars", &raw)?;
        }
        if let Some(raw) = lookup("--chunk-overlap-chars") {
            config.chunk_overlap_chars = parse_usize("--chunk-overlap-chars", &raw)?;
        }
        if let Some(raw) = lookup("--max-chunk-chars") {
            config.max_chunk_chars = parse_usize("--max-chunk-chars", &raw)?;
        }
        if let Some(raw) = lookup("--max-collections") {
            config.max_collections = parse_at_least("--max-collections", &raw, 1)?;
        }
        if let Some(raw) = lookup("--max-chunks-per-collection") {
            config.max_chunks_per_collection =
                parse_at_least("--max-chunks-per-collection", &raw, 1)?;
        }
        if let Some(raw) = lookup("--max-store-bytes") {
            let value = parse_u64("--max-store-bytes", &raw)?;
            if value < 1024 * 1024 {
                return fail("--max-store-bytes must be at least 1048576 (1 MiB)");
            }
            config.max_store_bytes = value;
        }
        if let Some(raw) = lookup("--max-document-bytes") {
            config.max_document_bytes = parse_at_least("--max-document-bytes", &raw, 1_024)?;
        }
        if let Some(raw) = lookup("--max-documents-per-call") {
            config.max_documents_per_call = parse_at_least("--max-documents-per-call", &raw, 1)?;
        }
        if let Some(raw) = lookup("--default-top-k") {
            let value = parse_at_least("--default-top-k", &raw, 1)?;
            if value > MAX_TOP_K {
                return fail(format!("--default-top-k must not exceed {MAX_TOP_K}"));
            }
            config.default_top_k = value;
        }
        if let Some(raw) = lookup("--embed-batch-size") {
            let value = parse_at_least("--embed-batch-size", &raw, 1)?;
            if value > 512 {
                return fail("--embed-batch-size must not exceed 512");
            }
            config.embed_batch_size = value;
        }
        if let Some(raw) = lookup("--request-timeout-seconds") {
            let value = parse_u64("--request-timeout-seconds", &raw)?;
            if !(1..=600).contains(&value) {
                return fail("--request-timeout-seconds must be between 1 and 600");
            }
            config.request_timeout = Duration::from_secs(value);
        }
        if let Some(raw) = lookup("--allow-remote-embeddings") {
            config.allow_remote_embeddings = parse_bool("--allow-remote-embeddings", &raw)?;
        }

        // Chunking bounds have to agree with each other or the splitter cannot
        // terminate. Caught here so it is a startup error, never a surprise on
        // the first ingest.
        config.chunk_options().validate().map_err(|error| {
            ConfigError(format!(
                "{error} (see --chunk-chars, --chunk-overlap-chars, --max-chunk-chars)"
            ))
        })?;

        if config.max_document_bytes < config.max_chunk_chars {
            return fail(format!(
                "--max-document-bytes ({}) is below --max-chunk-chars ({}), so no document \
                 could ever fill one chunk",
                config.max_document_bytes, config.max_chunk_chars
            ));
        }

        endpoint_reach(&config.embeddings_url, config.allow_remote_embeddings)?;

        config.api_key = environment
            .get(API_KEY_ENV)
            .map(|key| key.trim().to_string())
            .filter(|key| !key.is_empty())
            .map(ApiKey);

        Ok(config)
    }

    pub fn chunk_options(&self) -> ChunkOptions {
        ChunkOptions {
            target_chars: self.chunk_chars,
            overlap_chars: self.chunk_overlap_chars,
            max_chars: self.max_chunk_chars,
        }
    }

    pub fn store_limits(&self) -> StoreLimits {
        StoreLimits {
            max_collections: self.max_collections,
            max_chunks_per_collection: self.max_chunks_per_collection,
            max_store_bytes: self.max_store_bytes,
        }
    }

    /// Clamp a caller's `top_k` into the range the operator allows.
    pub fn effective_top_k(&self, requested: Option<u32>) -> usize {
        match requested {
            Some(0) | None => self.default_top_k,
            Some(value) => (value as usize).min(MAX_TOP_K),
        }
    }

    /// Warnings worth putting in the host's log at startup. Deliberately not
    /// errors: they are choices an operator is allowed to make, just not
    /// quietly.
    pub fn advisories(&self) -> Vec<String> {
        let mut advisories = Vec::new();
        if self.embedding_model.is_empty() {
            advisories.push(
                "no --embedding-model set; the `model` field is omitted from embedding \
                 requests, which most servers reject, and collections created now pin to \
                 the identity \"<unset>\""
                    .to_string(),
            );
        }
        if self.allow_remote_embeddings && !is_loopback(&self.embeddings_url) {
            advisories.push(format!(
                "every document and every query is sent to the non-loopback endpoint {}",
                self.embeddings_url
            ));
        }
        if self.chunk_overlap_chars == 0 {
            advisories.push(
                "chunk overlap is 0; a fact that straddles a chunk boundary will be \
                 retrievable from neither side"
                    .to_string(),
            );
        }
        if self.max_chunks_per_collection > DEFAULT_MAX_CHUNKS_PER_COLLECTION {
            advisories.push(format!(
                "--max-chunks-per-collection is {}; search is an exact brute-force cosine \
                 scan, so query latency grows linearly past roughly {} chunks",
                self.max_chunks_per_collection, DEFAULT_MAX_CHUNKS_PER_COLLECTION
            ));
        }
        advisories
    }

    /// A one-line startup summary. Contains no secrets by construction — the
    /// API key is not a field here.
    pub fn startup_summary(&self) -> String {
        format!(
            "data_dir={} endpoint={} model={} chunk_chars={} overlap={} max_chunk_chars={} \
             max_collections={} max_chunks_per_collection={} max_store_bytes={} \
             default_top_k={} batch={} timeout_s={} api_key={}",
            crate::names::display_path(&self.data_dir),
            self.embeddings_url,
            if self.embedding_model.is_empty() {
                "<unset>"
            } else {
                &self.embedding_model
            },
            self.chunk_chars,
            self.chunk_overlap_chars,
            self.max_chunk_chars,
            self.max_collections,
            self.max_chunks_per_collection,
            self.max_store_bytes,
            self.default_top_k,
            self.embed_batch_size,
            self.request_timeout.as_secs(),
            if self.api_key.is_some() {
                "set"
            } else {
                "unset"
            },
        )
    }
}

/// `~/.tdcc/vector-store`, or a relative fallback when the home directory is
/// not discoverable.
///
/// Takes the environment as an argument rather than reading it, so the choice
/// is testable on both platforms.
pub fn default_data_dir(environment: &BTreeMap<String, String>) -> PathBuf {
    let home = environment
        .get("HOME")
        .or_else(|| environment.get("USERPROFILE"))
        .map(PathBuf::from)
        .or_else(|| std::env::var("HOME").ok().map(PathBuf::from))
        .or_else(|| std::env::var("USERPROFILE").ok().map(PathBuf::from));

    match home {
        Some(home) => home.join(".tdcc").join(PLUGIN_NAME),
        // Better a working relative directory than a panic at startup. Named
        // in the startup summary either way, so an operator can see which one
        // was chosen.
        None => PathBuf::from(".tdcc").join(PLUGIN_NAME),
    }
}

/// Create the data directory if needed and return its canonical form.
///
/// Canonical, because every later containment check compares against it — see
/// [`crate::names::collection_path`].
pub fn prepare_data_dir(path: &Path) -> Result<PathBuf, ConfigError> {
    if let Err(error) = std::fs::create_dir_all(path) {
        return fail(format!(
            "could not create the data directory {}: {error}",
            crate::names::display_path(path)
        ));
    }
    std::fs::canonicalize(path).map_err(|error| {
        ConfigError(format!(
            "could not resolve the data directory {}: {error}",
            crate::names::display_path(path)
        ))
    })
}

/// Copy just the variables this plugin reads out of the process environment.
///
/// Filtering rather than cloning the whole environment keeps unrelated secrets
/// out of a struct that gets passed around.
pub fn collect_environment() -> BTreeMap<String, String> {
    std::env::vars()
        .filter(|(name, _)| {
            name.starts_with(ENV_PREFIX)
                || name == PLUGIN_URL_ENV
                || name == "HOME"
                || name == "USERPROFILE"
        })
        .collect()
}

/// Parse `--flag value`, `--flag=value`, and bare boolean flags.
///
/// An unknown flag is a hard error. Silently ignoring a mistyped
/// `--chunk-overlap-char` would leave the operator believing they had tuned
/// retrieval when they had not.
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
                known_options()
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

/// Refuse to ship document text off-box unless the operator asked for it.
///
/// This plugin runs on hardware people contributed to a mesh. Every `upsert`
/// sends whole documents to the embeddings endpoint and every `query` sends
/// the question, so the default has to be "loopback only" and going wider has
/// to be a deliberate, visible act.
pub fn endpoint_reach(url: &Url, allow_remote: bool) -> Result<(), ConfigError> {
    if is_loopback(url) || allow_remote {
        return Ok(());
    }
    fail(format!(
        "refusing to send document text to the non-loopback embeddings endpoint {url}: \
         pass --allow-remote-embeddings to allow it"
    ))
}

fn parse_u64(flag: &str, raw: &str) -> Result<u64, ConfigError> {
    raw.trim()
        .parse()
        .map_err(|_| ConfigError(format!("{flag} expects a whole number, got {raw:?}")))
}

fn parse_usize(flag: &str, raw: &str) -> Result<usize, ConfigError> {
    let value = parse_u64(flag, raw)?;
    usize::try_from(value).map_err(|_| ConfigError(format!("{flag} does not fit in a usize")))
}

fn parse_at_least(flag: &str, raw: &str, minimum: usize) -> Result<usize, ConfigError> {
    let value = parse_usize(flag, raw)?;
    if value < minimum {
        return fail(format!("{flag} must be at least {minimum}"));
    }
    Ok(value)
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
    use crate::chunk::MIN_TARGET_CHARS;

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
            "http://127.0.0.1:9337/v1/embeddings",
            "the default is the node, which does not serve this route yet — see README"
        );
        assert!(
            !config.allow_remote_embeddings,
            "sending documents off-box by default is not acceptable"
        );
        assert!(config.api_key.is_none());
        assert!(config.chunk_overlap_chars > 0, "overlap is on by default");
        config
            .chunk_options()
            .validate()
            .expect("the built-in chunk bounds must agree with each other");
        assert!(config.default_top_k <= MAX_TOP_K);
    }

    #[test]
    fn flags_accept_both_spellings() {
        let parsed = parse_args(&args(&[
            "--chunk-chars=800",
            "--chunk-overlap-chars",
            "100",
            "--allow-remote-embeddings",
        ]))
        .expect("valid flags");
        assert_eq!(parsed.get("--chunk-chars").map(String::as_str), Some("800"));
        assert_eq!(
            parsed.get("--chunk-overlap-chars").map(String::as_str),
            Some("100")
        );
        assert_eq!(
            parsed.get("--allow-remote-embeddings").map(String::as_str),
            Some("true")
        );
    }

    #[test]
    fn a_mistyped_flag_is_an_error_rather_than_a_silent_default() {
        let error = parse_args(&args(&["--chunk-char", "800"])).expect_err("typo must fail");
        assert!(error.0.contains("unknown option"), "{}", error.0);
        assert!(error.0.contains("--chunk-chars"), "{}", error.0);
    }

    #[test]
    fn a_repeated_flag_is_an_error() {
        let error = parse_args(&args(&["--chunk-chars", "800", "--chunk-chars", "900"]))
            .expect_err("ambiguous config must fail");
        assert!(error.0.contains("more than once"), "{}", error.0);
    }

    #[test]
    fn a_value_flag_without_a_value_is_an_error() {
        let error = parse_args(&args(&["--chunk-chars"])).expect_err("missing value must fail");
        assert!(error.0.contains("requires a value"), "{}", error.0);
    }

    #[test]
    fn flags_win_over_environment_which_wins_over_plugin_url() {
        let full = environment(&[
            (
                "TDCC_VECTOR_STORE_EMBEDDINGS_URL",
                "http://127.0.0.1:2222/v1",
            ),
            ("TDCC_PLUGIN_URL", "http://127.0.0.1:3333/v1"),
        ]);

        let config = Config::resolve(
            &args(&["--embeddings-url", "http://127.0.0.1:1111/v1"]),
            &full,
        )
        .expect("valid config");
        assert_eq!(
            config.embeddings_url.as_str(),
            "http://127.0.0.1:1111/v1/embeddings"
        );

        let config = Config::resolve(&args(&[]), &full).expect("valid config");
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
    fn every_knob_is_settable_from_the_environment() {
        let config = Config::resolve(
            &args(&[]),
            &environment(&[
                ("TDCC_VECTOR_STORE_CHUNK_CHARS", "900"),
                ("TDCC_VECTOR_STORE_CHUNK_OVERLAP_CHARS", "120"),
                ("TDCC_VECTOR_STORE_MAX_CHUNK_CHARS", "1800"),
                ("TDCC_VECTOR_STORE_DEFAULT_TOP_K", "5"),
                ("TDCC_VECTOR_STORE_EMBED_BATCH_SIZE", "4"),
                ("TDCC_VECTOR_STORE_EMBEDDING_MODEL", "nomic-embed-text"),
            ]),
        )
        .expect("valid config");

        assert_eq!(config.chunk_chars, 900);
        assert_eq!(config.chunk_overlap_chars, 120);
        assert_eq!(config.max_chunk_chars, 1_800);
        assert_eq!(config.default_top_k, 5);
        assert_eq!(config.embed_batch_size, 4);
        assert_eq!(config.embedding_model, "nomic-embed-text");
    }

    #[test]
    fn the_endpoint_path_is_appended_only_once() {
        for raw in [
            "http://127.0.0.1:11434/v1",
            "http://127.0.0.1:11434/v1/",
            "http://127.0.0.1:11434/v1/embeddings",
        ] {
            let url = resolve_embeddings_url(raw).expect("valid URL");
            assert_eq!(
                url.as_str(),
                "http://127.0.0.1:11434/v1/embeddings",
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
        assert!(!error.0.contains("secret"), "the error leaked the password");
    }

    #[test]
    fn userinfo_cannot_disguise_a_remote_host_as_loopback() {
        // `http://127.0.0.1@evil.example/v1` has host `evil.example`. Matching
        // on the string "127.0.0.1" instead of parsing would wave it through.
        let error = resolve_embeddings_url("http://127.0.0.1@evil.example/v1")
            .expect_err("userinfo is credentials-shaped and rejected outright");
        assert!(error.0.contains(API_KEY_ENV), "{}", error.0);

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
    fn chunk_bounds_that_cannot_terminate_are_a_startup_error() {
        // Overlap at or above the target would make every chunk repeat its
        // predecessor. Caught at startup rather than on the first ingest.
        let error = Config::resolve(
            &args(&["--chunk-chars=400", "--chunk-overlap-chars=400"]),
            &environment(&[]),
        )
        .expect_err("must be refused");
        assert!(error.0.contains("overlap"), "{}", error.0);
        assert!(error.0.contains("--chunk-overlap-chars"), "{}", error.0);
    }

    #[test]
    fn a_ceiling_below_the_target_is_a_startup_error() {
        let error = Config::resolve(
            &args(&["--chunk-chars=2000", "--max-chunk-chars=1000"]),
            &environment(&[]),
        )
        .expect_err("must be refused");
        assert!(error.0.contains("maximum chunk size"), "{}", error.0);
    }

    #[test]
    fn a_document_limit_below_a_chunk_ceiling_is_refused() {
        let error = Config::resolve(
            &args(&["--max-document-bytes=2048", "--max-chunk-chars=4000"]),
            &environment(&[]),
        )
        .expect_err("no document could ever fill a chunk");
        assert!(error.0.contains("--max-document-bytes"), "{}", error.0);
    }

    #[test]
    fn out_of_range_values_are_rejected() {
        for flag in [
            "--chunk-chars=10",
            "--chunk-chars=abc",
            "--max-collections=0",
            "--max-chunks-per-collection=0",
            "--max-store-bytes=1024",
            "--max-document-bytes=16",
            "--max-documents-per-call=0",
            "--default-top-k=0",
            "--default-top-k=1000",
            "--embed-batch-size=0",
            "--embed-batch-size=9999",
            "--request-timeout-seconds=0",
            "--request-timeout-seconds=99999",
            "--allow-remote-embeddings=maybe",
            "--data-dir=",
        ] {
            assert!(
                Config::resolve(&args(&[flag]), &environment(&[])).is_err(),
                "{flag} must be rejected"
            );
        }
    }

    #[test]
    fn top_k_is_clamped_to_the_ceiling_and_defaults_when_unset() {
        let config = Config::default();
        assert_eq!(config.effective_top_k(None), config.default_top_k);
        assert_eq!(config.effective_top_k(Some(0)), config.default_top_k);
        assert_eq!(config.effective_top_k(Some(3)), 3);
        assert_eq!(
            config.effective_top_k(Some(100_000)),
            MAX_TOP_K,
            "a retriever that returns 100 000 passages has not retrieved anything"
        );
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
    fn there_is_no_flag_that_takes_a_key() {
        // A key on the command line is written into config.toml and visible in
        // a process listing. The only way in is the environment.
        assert!(
            !KNOBS
                .iter()
                .any(|knob| knob.flag.contains("key") || knob.flag.contains("token")),
            "no flag may carry a credential: {}",
            known_options()
        );
    }

    #[test]
    fn advisories_name_the_choices_that_cost_something() {
        let config = Config::resolve(
            &args(&[
                "--embeddings-url=https://api.example.com/v1",
                "--allow-remote-embeddings",
                "--chunk-overlap-chars=0",
                "--max-chunks-per-collection=500000",
            ]),
            &environment(&[]),
        )
        .expect("valid config");
        let advisories = config.advisories().join("\n");
        assert!(advisories.contains("non-loopback"), "{advisories}");
        assert!(advisories.contains("overlap is 0"), "{advisories}");
        assert!(advisories.contains("brute-force"), "{advisories}");
        assert!(advisories.contains("no --embedding-model"), "{advisories}");
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

    #[test]
    fn the_data_directory_comes_from_the_home_directory() {
        let unix = default_data_dir(&environment(&[("HOME", "/home/contributor")]));
        assert_eq!(unix, PathBuf::from("/home/contributor/.tdcc/vector-store"));

        let windows = default_data_dir(&environment(&[("USERPROFILE", r"C:\Users\me")]));
        assert_eq!(
            windows,
            PathBuf::from(r"C:\Users\me")
                .join(".tdcc")
                .join("vector-store")
        );
    }

    #[test]
    fn an_explicit_data_dir_wins() {
        let config = Config::resolve(
            &args(&["--data-dir", "/srv/vectors"]),
            &environment(&[("HOME", "/home/contributor")]),
        )
        .expect("valid config");
        assert_eq!(config.data_dir, PathBuf::from("/srv/vectors"));
    }

    #[test]
    fn the_startup_summary_reports_the_bounds_actually_in_force() {
        let config = Config::resolve(&args(&["--chunk-chars=777"]), &environment(&[]))
            .expect("valid config");
        let summary = config.startup_summary();
        assert!(summary.contains("chunk_chars=777"), "{summary}");
        assert!(summary.contains("model=<unset>"), "{summary}");
    }

    #[test]
    fn store_limits_and_chunk_options_mirror_the_configuration() {
        let config = Config::resolve(
            &args(&[
                "--max-collections=3",
                "--chunk-chars=500",
                "--max-chunk-chars=900",
            ]),
            &environment(&[]),
        )
        .expect("valid config");
        assert_eq!(config.store_limits().max_collections, 3);
        assert_eq!(config.chunk_options().target_chars, 500);
        assert_eq!(config.chunk_options().max_chars, 900);
        assert!(config.chunk_options().target_chars >= MIN_TARGET_CHARS);
    }
}
