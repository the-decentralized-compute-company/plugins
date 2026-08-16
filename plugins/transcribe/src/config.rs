//! Where `transcribe` gets its settings, and why it is not `[plugin.settings]`.
//!
//! `[plugin.settings]` never reaches a plugin process. The host stores those
//! values and the console renders them, but there is no settings field in the
//! launch contract or the initialize handshake — only a web UI bundle can read
//! them back. This plugin has no web UI, so declaring a `config_schema` would
//! draw console controls that could not move a single byte of audio.
//!
//! Everything therefore arrives the two ways a plugin process can actually
//! receive configuration: `[[plugin]].args` and the environment of the `tdcc`
//! process. `[[plugin]].url` is forwarded by the host as `TDCC_PLUGIN_URL` and
//! is accepted as the backend URL, which is the idiomatic use of that field.
//!
//! **The API key is environment-only, deliberately.** `args` is written into
//! `~/.tdcc/config.toml`, echoed back by `tdcc plugins info`, and visible in a
//! process listing; a credential belongs in none of those.
//!
//! Two settings decide this plugin's blast radius and both start closed:
//! `--root` (no roots means no readable file) and `--backend-url` (no backend
//! means no outbound request).

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use reqwest::Url;

/// Values read from the process environment, as a map so the parser stays a
/// pure function that tests can drive without touching real environment state.
pub type EnvMap = BTreeMap<String, String>;

pub const PLUGIN_NAME: &str = "transcribe";
pub const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The product token this plugin identifies itself with in `User-Agent`.
pub const PRODUCT_TOKEN: &str = "tdcc-transcribe";
pub const PRODUCT_URL: &str = "https://github.com/the-decentralized-compute-company/tdcc-plugins";

/// Appended to a backend URL that names only an origin or an OpenAI-style
/// prefix. See [`resolve_endpoint`] for exactly when.
pub const TRANSCRIPTIONS_PATH: &str = "/v1/audio/transcriptions";

/// OpenAI requires a `model` field and rejects the request without one.
/// whisper.cpp's server accepts the field and serves whatever model it was
/// started with, so this default is harmless there and required here.
pub const DEFAULT_MODEL: &str = "whisper-1";

pub const DEFAULT_TIMEOUT_SECS: u64 = 300;
pub const MIN_TIMEOUT_SECS: u64 = 5;
pub const MAX_TIMEOUT_SECS: u64 = 3_600;

/// Refuse to read a file bigger than this at all. Not a transcription limit —
/// a "do not slurp a disk image into memory" limit.
pub const DEFAULT_MAX_FILE_BYTES: u64 = 256 * 1_024 * 1_024;
pub const MIN_MAX_FILE_BYTES: u64 = 1_024;
pub const MAX_MAX_FILE_BYTES: u64 = 4 * 1_024 * 1_024 * 1_024;

/// Ceiling on one request body sent to the backend.
///
/// Under the 25 MB limit OpenAI documents for this endpoint, which is the
/// smaller of the two common backends' limits. Written in decimal on purpose:
/// 24 MiB is 25,165,824 bytes, which is *over* a decimal 25 MB, so the obvious
/// binary choice would have been a default that the documented limit rejects.
pub const DEFAULT_MAX_UPLOAD_BYTES: u64 = 24_000_000;
pub const MIN_MAX_UPLOAD_BYTES: u64 = 16 * 1_024;
pub const MAX_MAX_UPLOAD_BYTES: u64 = 2 * 1_024 * 1_024 * 1_024;

pub const DEFAULT_CHUNK_SECONDS: u64 = 300;
pub const MIN_CHUNK_SECONDS: u64 = 10;
pub const MAX_CHUNK_SECONDS: u64 = 1_800;

/// Overlap between neighbouring chunks. A sentence that spans a boundary is
/// spoken in full inside at least one chunk, and the stitcher then cuts at the
/// middle of the overlap so it is transcribed exactly once.
pub const DEFAULT_OVERLAP_SECONDS: f64 = 5.0;
pub const MAX_OVERLAP_SECONDS: f64 = 60.0;

/// A bound on total work per call: at the defaults this is a little over five
/// hours of audio. Exceeding it is an error naming both settings, never a
/// silent truncation of somebody's recording.
pub const DEFAULT_MAX_CHUNKS: u64 = 64;
pub const MIN_MAX_CHUNKS: u64 = 1;
pub const MAX_MAX_CHUNKS: u64 = 512;

pub const DEFAULT_MAX_LIST_ENTRIES: u64 = 500;
pub const MIN_MAX_LIST_ENTRIES: u64 = 1;
pub const MAX_MAX_LIST_ENTRIES: u64 = 10_000;

/// Directory depth `list_audio` will descend. Fixed rather than configurable:
/// nobody has asked for more, and an unbounded walk on somebody else's disk is
/// not a default worth offering.
pub const MAX_LIST_DEPTH: usize = 12;

pub const ENV_BACKEND_URL: &str = "TDCC_TRANSCRIBE_BACKEND_URL";
pub const ENV_API_KEY: &str = "TDCC_TRANSCRIBE_API_KEY";
pub const ENV_MODEL: &str = "TDCC_TRANSCRIBE_MODEL";
pub const ENV_ROOTS: &str = "TDCC_TRANSCRIBE_ROOTS";
pub const ENV_LANGUAGE: &str = "TDCC_TRANSCRIBE_LANGUAGE";
pub const ENV_TIMEOUT_SECS: &str = "TDCC_TRANSCRIBE_TIMEOUT_SECS";
pub const ENV_MAX_FILE_BYTES: &str = "TDCC_TRANSCRIBE_MAX_FILE_BYTES";
pub const ENV_MAX_UPLOAD_BYTES: &str = "TDCC_TRANSCRIBE_MAX_UPLOAD_BYTES";
pub const ENV_CHUNK_SECONDS: &str = "TDCC_TRANSCRIBE_CHUNK_SECONDS";
pub const ENV_OVERLAP_SECONDS: &str = "TDCC_TRANSCRIBE_OVERLAP_SECONDS";
pub const ENV_MAX_CHUNKS: &str = "TDCC_TRANSCRIBE_MAX_CHUNKS";
pub const ENV_MAX_LIST_ENTRIES: &str = "TDCC_TRANSCRIBE_MAX_LIST_ENTRIES";
pub const ENV_INCLUDE_HIDDEN: &str = "TDCC_TRANSCRIBE_INCLUDE_HIDDEN";
pub const ENV_NO_GRANULARITY_FIELD: &str = "TDCC_TRANSCRIBE_NO_GRANULARITY_FIELD";
/// Set by the host from `[[plugin]].url`; accepted as the backend URL.
pub const ENV_PLUGIN_URL: &str = "TDCC_PLUGIN_URL";

const BOOL_FLAGS: &[&str] = &["--include-hidden", "--no-granularity-field"];
const VALUE_FLAGS: &[&str] = &[
    "--backend-url",
    "--chunk-seconds",
    "--language",
    "--max-chunks",
    "--max-file-bytes",
    "--max-list-entries",
    "--max-upload-bytes",
    "--model",
    "--overlap-seconds",
    "--timeout-secs",
];
/// `--root` is the one repeatable flag, so it is parsed separately from the
/// single-valued table above.
const ROOT_FLAG: &str = "--root";

pub const USAGE: &str = "\
transcribe — turn an audio file into text and timestamped segments, using a
Whisper-compatible backend you configure.

The host launches this binary; it is not meant to be run by hand. Configure it
through [[plugin]].args in ~/.tdcc/config.toml.

  --root <dir>              Directory the plugin may read audio from. Repeatable.
                            With none configured, `transcribe` refuses every path.
  --backend-url <url>       Whisper-compatible transcription endpoint. Also
                            accepted as [[plugin]].url.
  --model <name>            Model field sent with each request (default whisper-1).
  --language <code>         Default ISO-639-1 language hint; a tool argument wins.
  --chunk-seconds <10-1800> Chunk length for long WAV audio (default 300).
  --overlap-seconds <n>     Overlap between chunks, 0-60 (default 5).
  --max-chunks <1-512>      Refuse a file needing more chunks than this (default 64).
  --max-file-bytes <n>      Refuse to read a file larger than this (default 268435456).
  --max-upload-bytes <n>    Ceiling on one request body (default 25165824).
  --max-list-entries <n>    Cap on list_audio results (default 500).
  --timeout-secs <5-3600>   Per-request backend timeout (default 300).
  --include-hidden          Let list_audio descend into dot-directories.
  --no-granularity-field    Omit timestamp_granularities[] for backends that
                            reject unknown multipart fields.
  --print-package-manifest  Emit plugin-manifest.json and exit.
  --help                    Show this text.

The API key is read only from TDCC_TRANSCRIBE_API_KEY in the environment of the
tdcc process. It is never accepted as an argument, never logged, and redacted
from every error this plugin returns.";

/// A directory this plugin may read, and the label a caller names it by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootSpec {
    /// Stable, caller-facing name for the root. Derived from the directory's
    /// own final component, deduplicated across roots.
    pub label: String,
    /// The path exactly as the operator wrote it. Canonicalization happens in
    /// [`crate::roots`], which is where the filesystem is allowed to be
    /// touched.
    pub path: PathBuf,
}

/// A resolved backend.
///
/// `Debug` is hand-written so an accidental `{:?}` — in a log line, a panic
/// message, an error context — can never print the API key.
#[derive(Clone, PartialEq, Eq)]
pub struct Backend {
    pub endpoint: Url,
    pub api_key: Option<String>,
    pub model: String,
}

impl std::fmt::Debug for Backend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Backend")
            .field("endpoint", &self.endpoint.as_str())
            .field(
                "api_key",
                &self
                    .api_key
                    .as_ref()
                    .map(|_| "<redacted>")
                    .unwrap_or("none"),
            )
            .field("model", &self.model)
            .finish()
    }
}

/// Whether this plugin can reach a backend at all.
///
/// A missing backend is not a startup failure. `status` and `list_audio` stay
/// useful without one, so the plugin starts, prints the reason once to stderr,
/// and returns that same message — naming the missing setting — from every
/// call that would need the network.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackendSetup {
    Configured(Box<Backend>),
    Unconfigured(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Chunking {
    pub chunk: Duration,
    pub overlap: Duration,
    pub max_chunks: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_file_bytes: u64,
    pub max_upload_bytes: u64,
    pub max_list_entries: u64,
    pub request_timeout: Duration,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub roots: Vec<RootSpec>,
    pub backend: BackendSetup,
    pub chunking: Chunking,
    pub limits: Limits,
    /// Applied when a `transcribe` call passes no `language` of its own.
    pub default_language: Option<String>,
    pub include_hidden: bool,
    pub send_granularity_field: bool,
    pub user_agent: String,
}

impl Config {
    /// Parse `[[plugin]].args` and the process environment into a config.
    ///
    /// Returns `Err` only for input the operator clearly got wrong — an unknown
    /// flag, an unparseable number, a malformed URL, a credential in the wrong
    /// channel — because silently ignoring those is how a security setting ends
    /// up not applied. A *missing* backend is not an error here; it lands in
    /// [`BackendSetup::Unconfigured`], and missing roots leave `roots` empty so
    /// every path is refused.
    pub fn parse(args: &[String], env: &EnvMap) -> Result<Self, String> {
        let (flags, root_args) = parse_flags(args)?;

        let chunk = number(
            &flags,
            env,
            "--chunk-seconds",
            ENV_CHUNK_SECONDS,
            DEFAULT_CHUNK_SECONDS,
            MIN_CHUNK_SECONDS,
            MAX_CHUNK_SECONDS,
        )?;
        let overlap = seconds(
            &flags,
            env,
            "--overlap-seconds",
            ENV_OVERLAP_SECONDS,
            DEFAULT_OVERLAP_SECONDS,
            0.0,
            MAX_OVERLAP_SECONDS,
        )?;
        // An overlap at or past half the chunk length makes the stitcher's
        // midpoint cut meaningless and can make a chunk plan that never
        // advances. Caught here so it is a startup error rather than a
        // surprise on somebody's three-hour recording.
        if overlap >= chunk as f64 / 2.0 {
            return Err(format!(
                "`--overlap-seconds` is {overlap}, which is not less than half of \
                 `--chunk-seconds` ({chunk}). Lower the overlap or raise the chunk length."
            ));
        }

        let max_upload_bytes = number(
            &flags,
            env,
            "--max-upload-bytes",
            ENV_MAX_UPLOAD_BYTES,
            DEFAULT_MAX_UPLOAD_BYTES,
            MIN_MAX_UPLOAD_BYTES,
            MAX_MAX_UPLOAD_BYTES,
        )?;
        let max_file_bytes = number(
            &flags,
            env,
            "--max-file-bytes",
            ENV_MAX_FILE_BYTES,
            DEFAULT_MAX_FILE_BYTES,
            MIN_MAX_FILE_BYTES,
            MAX_MAX_FILE_BYTES,
        )?;

        Ok(Self {
            roots: resolve_roots(&root_args, env)?,
            backend: resolve_backend(&flags, env)?,
            chunking: Chunking {
                chunk: Duration::from_secs(chunk),
                overlap: Duration::from_secs_f64(overlap),
                max_chunks: number(
                    &flags,
                    env,
                    "--max-chunks",
                    ENV_MAX_CHUNKS,
                    DEFAULT_MAX_CHUNKS,
                    MIN_MAX_CHUNKS,
                    MAX_MAX_CHUNKS,
                )?,
            },
            limits: Limits {
                max_file_bytes,
                max_upload_bytes,
                max_list_entries: number(
                    &flags,
                    env,
                    "--max-list-entries",
                    ENV_MAX_LIST_ENTRIES,
                    DEFAULT_MAX_LIST_ENTRIES,
                    MIN_MAX_LIST_ENTRIES,
                    MAX_MAX_LIST_ENTRIES,
                )?,
                request_timeout: Duration::from_secs(number(
                    &flags,
                    env,
                    "--timeout-secs",
                    ENV_TIMEOUT_SECS,
                    DEFAULT_TIMEOUT_SECS,
                    MIN_TIMEOUT_SECS,
                    MAX_TIMEOUT_SECS,
                )?),
            },
            default_language: match value(&flags, env, "--language", ENV_LANGUAGE) {
                Some((raw, source)) => Some(normalize_language(&raw, &source)?),
                None => None,
            },
            include_hidden: toggle(&flags, env, "--include-hidden", ENV_INCLUDE_HIDDEN)?,
            send_granularity_field: !toggle(
                &flags,
                env,
                "--no-granularity-field",
                ENV_NO_GRANULARITY_FIELD,
            )?,
            user_agent: format!("{PRODUCT_TOKEN}/{PLUGIN_VERSION} (+{PRODUCT_URL})"),
        })
    }

    /// Read the real process arguments and environment.
    pub fn from_process() -> Result<Self, String> {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let env: EnvMap = std::env::vars().collect();
        Self::parse(&args, &env)
    }

    /// The message returned by every tool that needs a file, when the operator
    /// configured no root. Naming both channels here keeps the wording in one
    /// place instead of drifting between the three tools that use it.
    pub fn no_roots_message() -> String {
        format!(
            "transcribe has no audio root configured, so there is no file it is allowed to read. \
             Add `--root <dir>` to [[plugin]].args (repeat it for more than one directory) or set \
             {ENV_ROOTS} in the environment of the tdcc process to a list of directories separated \
             by the platform path separator."
        )
    }
}

/// A language hint the backend will accept, or an explanation of why not.
///
/// Whisper's own language codes are ISO-639-1 two-letter codes, and `auto`
/// means "detect it". Anything else is rejected here rather than forwarded,
/// because a backend's reply to a nonsense code is usually a 400 with no
/// indication that the language field was the problem.
pub fn normalize_language(raw: &str, source: &str) -> Result<String, String> {
    let trimmed = raw.trim().to_ascii_lowercase();
    if trimmed == "auto" {
        return Ok(trimmed);
    }
    if trimmed.len() == 2
        && trimmed
            .chars()
            .all(|character| character.is_ascii_alphabetic())
    {
        return Ok(trimmed);
    }
    Err(format!(
        "{source} is `{raw}`, which is not a language hint this plugin will forward. Use a \
         two-letter ISO-639-1 code such as `en`, `de`, or `ja`, or `auto` to let the backend \
         detect it."
    ))
}

/// Turn the operator's `--root` list into labelled roots.
///
/// The label is what a caller writes in a `path` argument, so it has to be
/// stable and readable: it comes from the directory's own final component, with
/// a numeric suffix when two roots would otherwise share one.
fn resolve_roots(root_args: &[String], env: &EnvMap) -> Result<Vec<RootSpec>, String> {
    let mut raw: Vec<String> = root_args.to_vec();
    if raw.is_empty()
        && let Some((value, _)) = env_value(env, ENV_ROOTS)
    {
        // `split_paths` is the platform's own rule for a PATH-shaped list —
        // `;` on Windows, `:` elsewhere — so an operator's muscle memory works.
        raw = std::env::split_paths(&OsString::from(value))
            .map(|path| path.to_string_lossy().into_owned())
            .filter(|path| !path.trim().is_empty())
            .collect();
    }

    let mut roots: Vec<RootSpec> = Vec::new();
    for entry in raw {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            return Err(format!("`{ROOT_FLAG}` was given an empty path"));
        }
        let path = PathBuf::from(trimmed);
        let base = label_for(&path);
        let mut label = base.clone();
        let mut suffix = 2;
        while roots.iter().any(|root| root.label == label) {
            label = format!("{base}-{suffix}");
            suffix += 1;
        }
        roots.push(RootSpec { label, path });
    }
    Ok(roots)
}

/// The caller-facing name for a root directory.
///
/// Path separators and `:` are stripped rather than escaped, because the label
/// becomes the first segment of a `path` argument and a separator inside it
/// would make that argument ambiguous.
fn label_for(path: &std::path::Path) -> String {
    let candidate = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let cleaned: String = candidate
        .chars()
        .filter(|character| !matches!(character, '/' | '\\' | ':'))
        .collect();
    let cleaned = cleaned.trim().trim_matches('.').to_string();
    if cleaned.is_empty() {
        "root".to_string()
    } else {
        cleaned
    }
}

fn resolve_backend(flags: &Flags, env: &EnvMap) -> Result<BackendSetup, String> {
    let model = match value(flags, env, "--model", ENV_MODEL) {
        Some((raw, source)) => {
            let trimmed = raw.trim().to_string();
            if trimmed.is_empty() {
                return Err(format!("{source} is empty; give a model name or omit it."));
            }
            trimmed
        }
        None => DEFAULT_MODEL.to_string(),
    };

    let configured = value(flags, env, "--backend-url", ENV_BACKEND_URL).or_else(|| {
        env_value(env, ENV_PLUGIN_URL).map(|(value, name)| (value, format!("`{name}`")))
    });

    let Some((raw, source)) = configured else {
        return Ok(BackendSetup::Unconfigured(format!(
            "transcribe has no backend configured, so it cannot turn audio into text. A TDCC node \
             does not serve {TRANSCRIPTIONS_PATH} itself — point this at something that does. Set \
             `--backend-url <url>` in [[plugin]].args, {ENV_BACKEND_URL} in the environment, or \
             [[plugin]].url in config.toml. A local whisper.cpp server \
             (`whisper-server --host 127.0.0.1 --port 8080 -m ggml-base.en.bin`) and any \
             OpenAI-compatible transcription endpoint both work."
        )));
    };

    // A credential passed where an argument is stored on disk is a mistake
    // worth refusing loudly rather than quietly honouring.
    let api_key = env_value(env, ENV_API_KEY).map(|(value, _)| value);
    if let Some(key) = flags.get("--api-key").or_else(|| flags.get("--key")) {
        let _ = key;
        return Err(format!(
            "an API key must not be passed as an argument: [[plugin]].args is written into \
             config.toml, echoed back by `tdcc plugins info`, and visible in a process listing. \
             Export {ENV_API_KEY} in the environment of the tdcc process instead."
        ));
    }

    Ok(BackendSetup::Configured(Box::new(Backend {
        endpoint: resolve_endpoint(&raw, &source)?,
        api_key,
        model,
    })))
}

/// Turn what the operator wrote into the exact URL a request is POSTed to.
///
/// People write all three of these and mean the same thing, so all three are
/// accepted and only the last one is left alone:
///
/// - `http://127.0.0.1:8080` — an origin, gets the full path appended
/// - `http://127.0.0.1:8080/v1` — an OpenAI-style prefix, gets the rest appended
/// - `http://127.0.0.1:8080/inference` — a complete path, used verbatim
///
/// Guessing beyond that would be worse than asking: any other path is taken as
/// deliberate and used exactly as given.
pub fn resolve_endpoint(raw: &str, source: &str) -> Result<Url, String> {
    let trimmed = raw.trim();
    let url = Url::parse(trimmed)
        .map_err(|error| format!("{source} is not a valid URL ({error}): {trimmed}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "{source} must be an http or https URL, not `{}`.",
            url.scheme()
        ));
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err(format!("{source} has no host: {trimmed}"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(format!(
            "{source} embeds credentials in the URL. Put the key in {ENV_API_KEY} instead, where \
             it is redacted from errors and never written to config.toml."
        ));
    }

    let path = url.path().trim_end_matches('/').to_string();
    let completed = match path.as_str() {
        "" => format!("{}{TRANSCRIPTIONS_PATH}", trimmed.trim_end_matches('/')),
        "/v1" => format!("{}/audio/transcriptions", trimmed.trim_end_matches('/')),
        "/v1/audio" => format!("{}/transcriptions", trimmed.trim_end_matches('/')),
        _ => return Ok(url),
    };
    Url::parse(&completed).map_err(|error| {
        format!("{source} could not be completed to a transcriptions URL: {error}")
    })
}

type Flags = BTreeMap<String, String>;

/// Accepts `--flag value`, `--flag=value`, and bare boolean flags, returning
/// the single-valued flags and the repeatable `--root` list separately.
///
/// An unknown flag is a hard error. A typo in `--root` that was quietly ignored
/// would leave an operator believing this plugin could read a directory it
/// cannot, or — worse, if it were the other way round — that it could not read
/// one it can.
fn parse_flags(args: &[String]) -> Result<(Flags, Vec<String>), String> {
    let mut flags = Flags::new();
    let mut roots: Vec<String> = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (arg, None),
        };

        if name == ROOT_FLAG {
            let value = match inline {
                Some(value) => value,
                None => {
                    index += 1;
                    args.get(index)
                        .cloned()
                        .ok_or_else(|| format!("{ROOT_FLAG} expects a directory"))?
                }
            };
            roots.push(value);
            index += 1;
        } else if BOOL_FLAGS.contains(&name) {
            let value = match inline {
                Some(value) => parse_bool(&value)
                    .ok_or_else(|| format!("{name} expects true or false, got `{value}`"))?,
                None => true,
            };
            flags.insert(name.to_string(), value.to_string());
            index += 1;
        } else if VALUE_FLAGS.contains(&name) {
            let value = match inline {
                Some(value) => value,
                None => {
                    index += 1;
                    args.get(index)
                        .cloned()
                        .ok_or_else(|| format!("{name} expects a value"))?
                }
            };
            flags.insert(name.to_string(), value);
            index += 1;
        } else if matches!(name, "--api-key" | "--key") {
            // Recorded rather than rejected here so `resolve_backend` can
            // answer with the sentence that says where the key does belong.
            flags.insert(name.to_string(), String::new());
            index += 1;
            if inline.is_none() {
                index += 1;
            }
        } else {
            return Err(format!("unknown option `{arg}`.\n\n{USAGE}"));
        }
    }
    Ok((flags, roots))
}

fn parse_bool(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn env_value(env: &EnvMap, name: &str) -> Option<(String, String)> {
    env.get(name)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(|value| (value, name.to_string()))
}

/// Resolve one setting, returning its value and a label naming *where* it came
/// from, so an error message can point at the thing the operator actually
/// wrote.
fn value(flags: &Flags, env: &EnvMap, flag: &str, var: &str) -> Option<(String, String)> {
    flags
        .get(flag)
        .map(|value| (value.clone(), format!("`{flag}`")))
        .or_else(|| env_value(env, var).map(|(value, name)| (value, format!("`{name}`"))))
}

fn number(
    flags: &Flags,
    env: &EnvMap,
    flag: &str,
    var: &str,
    default: u64,
    min: u64,
    max: u64,
) -> Result<u64, String> {
    let Some((raw, source)) = value(flags, env, flag, var) else {
        return Ok(default);
    };
    let parsed: u64 = raw
        .trim()
        .parse()
        .map_err(|_| format!("{source} must be a whole number, got `{raw}`"))?;
    if parsed < min || parsed > max {
        return Err(format!(
            "{source} must be between {min} and {max}, got {parsed}"
        ));
    }
    Ok(parsed)
}

fn seconds(
    flags: &Flags,
    env: &EnvMap,
    flag: &str,
    var: &str,
    default: f64,
    min: f64,
    max: f64,
) -> Result<f64, String> {
    let Some((raw, source)) = value(flags, env, flag, var) else {
        return Ok(default);
    };
    let parsed: f64 = raw
        .trim()
        .parse()
        .map_err(|_| format!("{source} must be a number of seconds, got `{raw}`"))?;
    if !parsed.is_finite() || parsed < min || parsed > max {
        return Err(format!(
            "{source} must be between {min} and {max}, got `{raw}`"
        ));
    }
    Ok(parsed)
}

fn toggle(flags: &Flags, env: &EnvMap, flag: &str, var: &str) -> Result<bool, String> {
    if let Some(raw) = flags.get(flag) {
        return parse_bool(raw).ok_or_else(|| format!("`{flag}` expects true or false: {raw}"));
    }
    match env_value(env, var) {
        Some((raw, name)) => {
            parse_bool(&raw).ok_or_else(|| format!("`{name}` expects true or false: {raw}"))
        }
        None => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> EnvMap {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    fn parsed(values: &[&str]) -> Config {
        Config::parse(&args(values), &env(&[])).expect("parses")
    }

    #[test]
    fn nothing_configured_leaves_both_capabilities_closed() {
        let config = Config::parse(&[], &env(&[])).expect("defaults parse");

        assert!(config.roots.is_empty(), "no root means no readable file");
        let BackendSetup::Unconfigured(message) = &config.backend else {
            panic!("expected an unconfigured backend");
        };
        assert!(message.contains(ENV_BACKEND_URL), "{message}");
        assert!(message.contains("[[plugin]].url"), "{message}");
    }

    #[test]
    fn the_missing_root_message_names_both_channels_it_could_come_from() {
        let message = Config::no_roots_message();
        assert!(message.contains("--root"), "{message}");
        assert!(message.contains(ENV_ROOTS), "{message}");
    }

    #[test]
    fn the_unconfigured_backend_message_says_a_node_does_not_serve_this_itself() {
        let config = Config::parse(&[], &env(&[])).expect("parses");
        let BackendSetup::Unconfigured(message) = &config.backend else {
            panic!("expected an unconfigured backend");
        };
        // The single most expensive wrong assumption an operator can make.
        assert!(message.contains(TRANSCRIPTIONS_PATH), "{message}");
        assert!(message.contains("does not serve"), "{message}");
    }

    #[test]
    fn roots_are_labelled_by_their_final_component() {
        let config = parsed(&["--root", "/srv/podcasts", "--root", "/mnt/interviews"]);

        assert_eq!(
            config
                .roots
                .iter()
                .map(|root| root.label.as_str())
                .collect::<Vec<_>>(),
            ["podcasts", "interviews"]
        );
        assert_eq!(config.roots[0].path, PathBuf::from("/srv/podcasts"));
    }

    #[test]
    fn two_roots_with_the_same_basename_get_distinct_labels() {
        let config = parsed(&[
            "--root", "/a/audio", "--root", "/b/audio", "--root", "/c/audio",
        ]);

        assert_eq!(
            config
                .roots
                .iter()
                .map(|root| root.label.as_str())
                .collect::<Vec<_>>(),
            ["audio", "audio-2", "audio-3"]
        );
    }

    #[test]
    fn a_root_whose_label_would_be_empty_falls_back_to_a_usable_name() {
        // A drive root or `/` has no final component to name it by.
        let config = parsed(&["--root", "/"]);
        assert_eq!(config.roots[0].label, "root");
    }

    #[test]
    fn the_environment_supplies_roots_as_a_platform_path_list() {
        let joined = std::env::join_paths(["/srv/one", "/srv/two"])
            .expect("join")
            .to_string_lossy()
            .into_owned();
        let config = Config::parse(&[], &env(&[(ENV_ROOTS, joined.as_str())])).expect("parses");

        assert_eq!(
            config
                .roots
                .iter()
                .map(|root| root.label.as_str())
                .collect::<Vec<_>>(),
            ["one", "two"]
        );
    }

    #[test]
    fn root_arguments_win_over_the_environment() {
        let config = Config::parse(
            &args(&["--root", "/from/args"]),
            &env(&[(ENV_ROOTS, "/from/env")]),
        )
        .expect("parses");

        assert_eq!(config.roots.len(), 1);
        assert_eq!(config.roots[0].path, PathBuf::from("/from/args"));
    }

    #[test]
    fn an_origin_gains_the_openai_transcriptions_path() {
        let config = parsed(&["--backend-url", "http://127.0.0.1:8080"]);
        let BackendSetup::Configured(backend) = &config.backend else {
            panic!("expected a configured backend");
        };
        assert_eq!(
            backend.endpoint.as_str(),
            "http://127.0.0.1:8080/v1/audio/transcriptions"
        );
    }

    #[test]
    fn an_openai_style_prefix_is_completed_rather_than_doubled() {
        for (given, expected) in [
            (
                "https://api.example.com/v1",
                "https://api.example.com/v1/audio/transcriptions",
            ),
            (
                "https://api.example.com/v1/",
                "https://api.example.com/v1/audio/transcriptions",
            ),
            (
                "https://api.example.com/v1/audio",
                "https://api.example.com/v1/audio/transcriptions",
            ),
        ] {
            let config = parsed(&["--backend-url", given]);
            let BackendSetup::Configured(backend) = &config.backend else {
                panic!("expected a configured backend for {given}");
            };
            assert_eq!(backend.endpoint.as_str(), expected, "given {given}");
        }
    }

    #[test]
    fn a_complete_path_is_used_verbatim() {
        // whisper.cpp's server serves `/inference`, not the OpenAI path.
        let config = parsed(&["--backend-url", "http://127.0.0.1:8080/inference"]);
        let BackendSetup::Configured(backend) = &config.backend else {
            panic!("expected a configured backend");
        };
        assert_eq!(backend.endpoint.as_str(), "http://127.0.0.1:8080/inference");
    }

    #[test]
    fn the_hosts_plugin_url_is_accepted_as_the_backend() {
        let config =
            Config::parse(&[], &env(&[(ENV_PLUGIN_URL, "http://127.0.0.1:8080")])).expect("parses");

        let BackendSetup::Configured(backend) = &config.backend else {
            panic!("expected a configured backend");
        };
        assert_eq!(
            backend.endpoint.as_str(),
            "http://127.0.0.1:8080/v1/audio/transcriptions"
        );
    }

    #[test]
    fn an_explicit_backend_url_beats_the_hosts_plugin_url() {
        let config = Config::parse(
            &args(&["--backend-url", "http://flag.example/inference"]),
            &env(&[(ENV_PLUGIN_URL, "http://url.example")]),
        )
        .expect("parses");

        let BackendSetup::Configured(backend) = &config.backend else {
            panic!("expected a configured backend");
        };
        assert_eq!(backend.endpoint.as_str(), "http://flag.example/inference");
    }

    #[test]
    fn a_backend_url_must_be_http_or_https() {
        let error = Config::parse(&args(&["--backend-url", "file:///etc/passwd"]), &env(&[]))
            .expect_err("non-http backends are rejected");
        assert!(error.contains("http"), "{error}");
    }

    #[test]
    fn a_url_with_embedded_credentials_is_refused_and_names_the_variable() {
        let error = Config::parse(
            &args(&["--backend-url", "https://user:secret@api.example.com/v1"]),
            &env(&[]),
        )
        .expect_err("credentials do not belong in a URL");

        assert!(error.contains(ENV_API_KEY), "{error}");
        assert!(!error.contains("secret"), "{error}");
    }

    #[test]
    fn a_key_passed_as_an_argument_is_refused_and_says_where_it_belongs() {
        let error = Config::parse(
            &args(&[
                "--backend-url",
                "https://api.example.com",
                "--api-key",
                "sk-live-123",
            ]),
            &env(&[]),
        )
        .expect_err("args are stored on disk");

        assert!(error.contains(ENV_API_KEY), "{error}");
        assert!(!error.contains("sk-live-123"), "{error}");
    }

    #[test]
    fn the_api_key_is_read_from_the_environment_and_never_printed_by_debug() {
        let config = Config::parse(
            &args(&["--backend-url", "https://api.example.com"]),
            &env(&[(ENV_API_KEY, "sk-super-secret")]),
        )
        .expect("parses");

        let BackendSetup::Configured(backend) = &config.backend else {
            panic!("expected a configured backend");
        };
        assert_eq!(backend.api_key.as_deref(), Some("sk-super-secret"));

        let rendered = format!("{backend:?}");
        assert!(!rendered.contains("sk-super-secret"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");

        // And the whole config, which is what a panic message would print.
        let whole = format!("{config:?}");
        assert!(!whole.contains("sk-super-secret"), "{whole}");
    }

    #[test]
    fn the_model_defaults_to_the_name_openai_requires() {
        let config = parsed(&["--backend-url", "https://api.example.com"]);
        let BackendSetup::Configured(backend) = &config.backend else {
            panic!("expected a configured backend");
        };
        assert_eq!(backend.model, DEFAULT_MODEL);

        let overridden = parsed(&[
            "--backend-url",
            "https://api.example.com",
            "--model",
            "base.en",
        ]);
        let BackendSetup::Configured(backend) = &overridden.backend else {
            panic!("expected a configured backend");
        };
        assert_eq!(backend.model, "base.en");
    }

    #[test]
    fn chunking_and_limits_have_the_documented_defaults() {
        let config = parsed(&[]);

        assert_eq!(config.chunking.chunk, Duration::from_secs(300));
        assert_eq!(config.chunking.overlap, Duration::from_secs(5));
        assert_eq!(config.chunking.max_chunks, DEFAULT_MAX_CHUNKS);
        assert_eq!(config.limits.max_upload_bytes, DEFAULT_MAX_UPLOAD_BYTES);
        assert_eq!(config.limits.max_file_bytes, DEFAULT_MAX_FILE_BYTES);
        assert_eq!(config.limits.request_timeout, Duration::from_secs(300));
        assert!(!config.include_hidden);
        assert!(config.send_granularity_field);
    }

    /// The trap this default exists to avoid: the obvious binary choice, 24
    /// MiB, is 25,165,824 bytes — over a decimal 25 MB, and therefore over the
    /// limit it was picked to stay under.
    #[test]
    fn the_upload_default_is_under_the_documented_limit_in_either_reading() {
        // "25 MB" is written both ways in the wild, so the default has to clear
        // whichever one the backend meant.
        let strictest = [25_000_000u64, 25 * 1_024 * 1_024]
            .into_iter()
            .min()
            .expect("two readings of the same limit");

        assert!(
            DEFAULT_MAX_UPLOAD_BYTES < strictest,
            "the default upload ceiling is {DEFAULT_MAX_UPLOAD_BYTES}, which is not under {strictest}"
        );
    }

    #[test]
    fn an_overlap_that_would_defeat_the_stitcher_is_a_startup_error() {
        let error = Config::parse(
            &args(&["--chunk-seconds", "60", "--overlap-seconds", "30"]),
            &env(&[]),
        )
        .expect_err("overlap must be under half the chunk");

        assert!(error.contains("--overlap-seconds"), "{error}");
        assert!(error.contains("--chunk-seconds"), "{error}");
    }

    #[test]
    fn an_overlap_just_under_half_the_chunk_is_allowed() {
        let config = parsed(&["--chunk-seconds", "60", "--overlap-seconds", "29.5"]);
        assert_eq!(config.chunking.overlap, Duration::from_secs_f64(29.5));
    }

    #[test]
    fn out_of_range_and_unparseable_numbers_are_rejected_with_the_source_named() {
        let error = Config::parse(&args(&["--chunk-seconds", "2"]), &env(&[]))
            .expect_err("below the floor");
        assert!(error.contains("--chunk-seconds"), "{error}");

        let error = Config::parse(&[], &env(&[(ENV_MAX_CHUNKS, "abc")])).expect_err("not a number");
        assert!(error.contains(ENV_MAX_CHUNKS), "{error}");

        let error = Config::parse(&[], &env(&[(ENV_TIMEOUT_SECS, "99999")]))
            .expect_err("above the ceiling");
        assert!(error.contains(ENV_TIMEOUT_SECS), "{error}");
    }

    #[test]
    fn a_misspelled_option_is_an_error_rather_than_a_silently_ignored_setting() {
        let error = Config::parse(&args(&["--rooot", "/srv/audio"]), &env(&[])).expect_err("typo");
        assert!(error.contains("unknown option"), "{error}");
        // The usage text comes with it, so the operator does not have to guess.
        assert!(error.contains("--root <dir>"), "{error}");
    }

    #[test]
    fn inline_and_separate_value_forms_agree() {
        let inline = parsed(&["--root=/srv/audio", "--chunk-seconds=120"]);
        let separate = parsed(&["--root", "/srv/audio", "--chunk-seconds", "120"]);
        assert_eq!(inline, separate);
    }

    #[test]
    fn a_flag_without_its_value_is_reported_by_name() {
        let error = Config::parse(&args(&["--root"]), &env(&[])).expect_err("no value");
        assert!(error.contains("--root"), "{error}");
    }

    #[test]
    fn language_hints_are_validated_before_they_reach_a_backend() {
        assert_eq!(normalize_language("EN", "`--language`").unwrap(), "en");
        assert_eq!(
            normalize_language(" auto ", "`--language`").unwrap(),
            "auto"
        );

        for bad in ["english", "e", "en-US", "12", ""] {
            let error = normalize_language(bad, "`--language`")
                .expect_err("only ISO-639-1 or auto is forwarded");
            assert!(error.contains("ISO-639-1"), "{bad}: {error}");
        }
    }

    #[test]
    fn a_default_language_is_taken_from_either_channel() {
        assert_eq!(
            parsed(&["--language", "DE"]).default_language.as_deref(),
            Some("de")
        );
        assert_eq!(
            Config::parse(&[], &env(&[(ENV_LANGUAGE, "ja")]))
                .expect("parses")
                .default_language
                .as_deref(),
            Some("ja")
        );
        assert_eq!(parsed(&[]).default_language, None);
    }

    #[test]
    fn the_granularity_field_can_be_suppressed_from_either_channel() {
        assert!(!parsed(&["--no-granularity-field"]).send_granularity_field);
        assert!(
            !Config::parse(&[], &env(&[(ENV_NO_GRANULARITY_FIELD, "true")]))
                .expect("parses")
                .send_granularity_field
        );
        assert!(
            Config::parse(&[], &env(&[(ENV_NO_GRANULARITY_FIELD, "false")]))
                .expect("parses")
                .send_granularity_field
        );
    }

    #[test]
    fn the_user_agent_names_this_software_and_where_to_find_it() {
        let config = parsed(&[]);
        assert!(
            config.user_agent.starts_with(PRODUCT_TOKEN),
            "{}",
            config.user_agent
        );
        assert!(
            config.user_agent.contains(PLUGIN_VERSION),
            "{}",
            config.user_agent
        );
        assert!(
            config.user_agent.contains(PRODUCT_URL),
            "{}",
            config.user_agent
        );
    }
}
