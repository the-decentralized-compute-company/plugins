//! Where `describe-image` gets its settings, and why it is not
//! `[plugin.settings]`.
//!
//! `[plugin.settings]` never reaches a plugin process. The host stores those
//! values and the console renders them, but there is no settings field in the
//! launch contract or the initialize handshake — only a web UI bundle can read
//! them back. This plugin has no web UI, so declaring a `config_schema` would
//! draw console controls whose values could not change a single request.
//!
//! Everything therefore arrives the two ways a plugin process can actually
//! receive configuration: `[[plugin]].args` and the environment of the `tdcc`
//! process. `[[plugin]].url` is forwarded by the host as `TDCC_PLUGIN_URL` and
//! is accepted as the OpenAI-compatible base URL, which is the idiomatic use of
//! that field.
//!
//! **The API key is environment-only, deliberately.** `args` is written into
//! `~/.tdcc/config.toml`, echoed back by `tdcc plugins info`, and visible in a
//! process listing; a credential belongs in none of those.
//!
//! Every guard here defaults to the narrowest useful setting: no filesystem
//! root is readable, remote image URLs are refused, private addresses are
//! refused, and the API base has to be on loopback. Widening any of them is a
//! deliberate act by the operator whose hardware this runs on.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::Url;

/// Values read from the process environment, as a map so the parser stays a
/// pure function that tests can drive without touching real environment state.
pub type EnvMap = BTreeMap<String, String>;

pub const PLUGIN_NAME: &str = "describe-image";
pub const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The node's own OpenAI-compatible frontend. Pooling the mesh's own GPUs is
/// the entire point of this plugin, so the default points at the node and
/// nothing has to be configured for the ordinary case.
pub const DEFAULT_API_BASE: &str = "http://127.0.0.1:9337/v1";

/// Longest edge, in pixels, of what is actually sent to the model.
///
/// Vision encoders tile an image into a fixed patch grid; past roughly this
/// size a photo costs steadily more tokens without giving the encoder anything
/// it can resolve. 1024 keeps ordinary screenshot text legible while holding a
/// single image to a few hundred KiB of base64.
pub const DEFAULT_MAX_DIMENSION: u32 = 1_024;

/// Cap on the *source* bytes of one image, before decoding.
pub const DEFAULT_MAX_IMAGE_BYTES: u64 = 8 * 1_024 * 1_024;

/// Cap on the decoded pixel count of one image.
///
/// This is the decompression-bomb guard: a 40 KiB PNG can declare 60000x60000,
/// which is 14 GiB of RGBA once decoded. 50 MP is comfortably above any real
/// camera and far below anything that would take the node down.
pub const DEFAULT_MAX_PIXELS: u64 = 50_000_000;

pub const DEFAULT_MAX_IMAGES: u32 = 4;
pub const MAX_MAX_IMAGES: u32 = 8;

pub const DEFAULT_MAX_TOKENS: u32 = 512;

/// Vision inference on a contributed GPU is slow — a first call also pays for
/// the mmproj load — so the default is generous compared with a text plugin's.
pub const DEFAULT_TIMEOUT_SECS: u64 = 120;

pub const DEFAULT_JPEG_QUALITY: u8 = 82;

pub const ENV_API_BASE: &str = "TDCC_DESCRIBE_IMAGE_API_BASE";
pub const ENV_API_KEY: &str = "TDCC_DESCRIBE_IMAGE_API_KEY";
pub const ENV_MODEL: &str = "TDCC_DESCRIBE_IMAGE_MODEL";
pub const ENV_ROOTS: &str = "TDCC_DESCRIBE_IMAGE_ROOTS";
pub const ENV_ALLOW_REMOTE_API: &str = "TDCC_DESCRIBE_IMAGE_ALLOW_REMOTE_API";
pub const ENV_ALLOW_REMOTE_IMAGES: &str = "TDCC_DESCRIBE_IMAGE_ALLOW_REMOTE_IMAGES";
pub const ENV_ALLOW_PRIVATE_NETWORK: &str = "TDCC_DESCRIBE_IMAGE_ALLOW_PRIVATE_NETWORK";
pub const ENV_MAX_DIMENSION: &str = "TDCC_DESCRIBE_IMAGE_MAX_DIMENSION";
pub const ENV_MAX_IMAGE_BYTES: &str = "TDCC_DESCRIBE_IMAGE_MAX_IMAGE_BYTES";
pub const ENV_MAX_PIXELS: &str = "TDCC_DESCRIBE_IMAGE_MAX_PIXELS";
pub const ENV_MAX_IMAGES: &str = "TDCC_DESCRIBE_IMAGE_MAX_IMAGES";
pub const ENV_MAX_TOKENS: &str = "TDCC_DESCRIBE_IMAGE_MAX_TOKENS";
pub const ENV_TIMEOUT_SECS: &str = "TDCC_DESCRIBE_IMAGE_TIMEOUT_SECS";
pub const ENV_JPEG_QUALITY: &str = "TDCC_DESCRIBE_IMAGE_JPEG_QUALITY";
pub const ENV_IMAGE_FORMAT: &str = "TDCC_DESCRIBE_IMAGE_IMAGE_FORMAT";
/// Set by the host from `[[plugin]].url`; accepted as the API base URL.
pub const ENV_PLUGIN_URL: &str = "TDCC_PLUGIN_URL";

const BOOL_FLAGS: &[&str] = &[
    "--allow-private-network",
    "--allow-remote-api",
    "--allow-remote-images",
];

/// The only flag that may appear more than once. Every other repeat is a
/// mistake worth failing on: two `--max-dimension` values means one of them is
/// silently doing nothing.
const REPEATABLE_FLAG: &str = "--root";

const VALUE_FLAGS: &[&str] = &[
    "--api-base",
    "--image-format",
    "--jpeg-quality",
    "--max-dimension",
    "--max-image-bytes",
    "--max-images",
    "--max-pixels",
    "--max-tokens",
    "--model",
    "--root",
    "--timeout-secs",
];

/// What the resized image is re-encoded as before it is base64'd into the
/// request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageFormat {
    /// JPEG for photographs, PNG for small lossless sources. See
    /// [`crate::render::choose_encoding`] for the exact rule.
    Auto,
    Jpeg,
    Png,
}

impl ImageFormat {
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Jpeg => "jpeg",
            Self::Png => "png",
        }
    }
}

/// An API key that cannot be printed by accident.
///
/// `Debug` is written by hand because the natural thing to do while debugging a
/// startup problem is to log the whole config struct.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey(String);

impl ApiKey {
    pub fn as_header_value(&self) -> String {
        format!("Bearer {}", self.0)
    }

    /// The raw secret, for scrubbing it out of a transport error before that
    /// error is handed back to a caller. Nothing else should call this.
    pub fn expose_for_redaction(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ApiKey(<redacted>)")
    }
}

/// Bounds applied to every image, in both directions: what may come in, and
/// what may go out to the model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_dimension: u32,
    pub max_image_bytes: u64,
    pub max_pixels: u64,
    pub max_images: u32,
    pub max_tokens: u32,
    pub jpeg_quality: u8,
    pub image_format: ImageFormat,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Fully-resolved base, always with a trailing path such as `/v1`, never a
    /// complete endpoint: `chat/completions` and `models` hang off it.
    pub api_base: Url,
    pub api_key: Option<ApiKey>,
    /// Pin a model id instead of discovering one from `/v1/models`.
    pub model: Option<String>,
    /// Canonical directories a local image path may resolve inside. Empty means
    /// local paths are refused outright, which is the default.
    pub roots: Vec<PathBuf>,
    pub allow_remote_api: bool,
    pub allow_remote_images: bool,
    pub allow_private_network: bool,
    pub request_timeout: Duration,
    pub limits: Limits,
}

impl Config {
    /// Parse `[[plugin]].args` and the process environment into a config.
    ///
    /// Returns `Err` for anything the operator clearly got wrong — an unknown
    /// flag, an out-of-range number, a root that does not exist, a non-loopback
    /// API base without the opt-in. Silently ignoring those is how a guard ends
    /// up not applied while an operator believes it is.
    pub fn parse(args: &[String], env: &EnvMap) -> Result<Self, String> {
        let flags = parse_flags(args)?;

        let raw_base = value(&flags, env, "--api-base", ENV_API_BASE)
            .or_else(|| env_value(env, ENV_PLUGIN_URL).map(|(v, n)| (v, format!("`{n}`"))))
            .map(|(value, _)| value)
            .unwrap_or_else(|| DEFAULT_API_BASE.to_string());
        let api_base = parse_api_base(&raw_base)?;

        let allow_remote_api = toggle(&flags, env, "--allow-remote-api", ENV_ALLOW_REMOTE_API)?;
        if !is_loopback(&api_base) && !allow_remote_api {
            return Err(format!(
                "refusing to send images to the non-loopback endpoint {api_base}. Every call \
                 uploads picture bytes from this machine, so leaving the mesh is an explicit \
                 choice: pass `--allow-remote-api` in [[plugin]].args or set \
                 {ENV_ALLOW_REMOTE_API}=true."
            ));
        }

        let roots = resolve_roots(&flags, env)?;

        let limits = Limits {
            max_dimension: number(
                &flags,
                env,
                "--max-dimension",
                ENV_MAX_DIMENSION,
                u64::from(DEFAULT_MAX_DIMENSION),
                64,
                4_096,
            )? as u32,
            max_image_bytes: number(
                &flags,
                env,
                "--max-image-bytes",
                ENV_MAX_IMAGE_BYTES,
                DEFAULT_MAX_IMAGE_BYTES,
                4_096,
                128 * 1_024 * 1_024,
            )?,
            max_pixels: number(
                &flags,
                env,
                "--max-pixels",
                ENV_MAX_PIXELS,
                DEFAULT_MAX_PIXELS,
                65_536,
                400_000_000,
            )?,
            max_images: number(
                &flags,
                env,
                "--max-images",
                ENV_MAX_IMAGES,
                u64::from(DEFAULT_MAX_IMAGES),
                1,
                u64::from(MAX_MAX_IMAGES),
            )? as u32,
            max_tokens: number(
                &flags,
                env,
                "--max-tokens",
                ENV_MAX_TOKENS,
                u64::from(DEFAULT_MAX_TOKENS),
                16,
                8_192,
            )? as u32,
            jpeg_quality: number(
                &flags,
                env,
                "--jpeg-quality",
                ENV_JPEG_QUALITY,
                u64::from(DEFAULT_JPEG_QUALITY),
                40,
                95,
            )? as u8,
            image_format: parse_image_format(&flags, env)?,
        };

        Ok(Self {
            api_base,
            api_key: env_value(env, ENV_API_KEY).map(|(value, _)| ApiKey(value)),
            model: value(&flags, env, "--model", ENV_MODEL)
                .map(|(value, _)| value.trim().to_string())
                .filter(|model| !model.is_empty()),
            roots,
            allow_remote_api,
            allow_remote_images: toggle(
                &flags,
                env,
                "--allow-remote-images",
                ENV_ALLOW_REMOTE_IMAGES,
            )?,
            allow_private_network: toggle(
                &flags,
                env,
                "--allow-private-network",
                ENV_ALLOW_PRIVATE_NETWORK,
            )?,
            request_timeout: Duration::from_secs(number(
                &flags,
                env,
                "--timeout-secs",
                ENV_TIMEOUT_SECS,
                DEFAULT_TIMEOUT_SECS,
                5,
                900,
            )?),
            limits,
        })
    }

    /// Read the real process arguments and environment.
    pub fn from_process() -> Result<Self, String> {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let env: EnvMap = std::env::vars().collect();
        Self::parse(&args, &env)
    }

    /// The POST target for chat completions.
    pub fn chat_completions_url(&self) -> Url {
        join_path(&self.api_base, "chat/completions")
    }

    /// The GET target for the model list.
    pub fn models_url(&self) -> Url {
        join_path(&self.api_base, "models")
    }

    /// A one-line startup summary. Contains no secrets by construction — the
    /// key is reported as set or unset and never as a value.
    pub fn startup_summary(&self) -> String {
        format!(
            "api_base={} model={} api_key={} roots={} remote_images={} private_network={} \
             max_dimension={} max_images={} max_image_bytes={} max_pixels={} max_tokens={} \
             format={} timeout_secs={}",
            self.api_base,
            self.model.as_deref().unwrap_or("<discovered>"),
            if self.api_key.is_some() {
                "set"
            } else {
                "unset"
            },
            self.roots.len(),
            self.allow_remote_images,
            self.allow_private_network,
            self.limits.max_dimension,
            self.limits.max_images,
            self.limits.max_image_bytes,
            self.limits.max_pixels,
            self.limits.max_tokens,
            self.limits.image_format.label(),
            self.request_timeout.as_secs(),
        )
    }

    /// Choices an operator is allowed to make, but not quietly. These go to
    /// stderr once at startup, where the host's log picks them up.
    pub fn advisories(&self) -> Vec<String> {
        let mut advisories = Vec::new();
        if self.roots.is_empty() {
            advisories.push(format!(
                "no --root configured: local file paths are refused, so only data: URIs{} can be \
                 described",
                if self.allow_remote_images {
                    " and remote URLs"
                } else {
                    ""
                }
            ));
        }
        if !is_loopback(&self.api_base) {
            advisories.push(format!(
                "image bytes are uploaded to the non-loopback endpoint {}",
                self.api_base
            ));
        }
        if self.allow_remote_images && self.allow_private_network {
            advisories.push(
                "--allow-remote-images and --allow-private-network are both on: a tool argument \
                 can make this node fetch a URL inside your own network"
                    .to_string(),
            );
        }
        advisories
    }
}

/// Render a canonical path for a human to read.
///
/// Windows canonicalization returns verbatim paths (`\\?\C:\photos`). The
/// prefix is meaningful to the OS and noise to an operator reading a startup
/// log, so it is stripped here and nowhere else — containment comparisons keep
/// using the real canonical path.
pub fn display_path(path: &Path) -> String {
    let rendered = path.display().to_string();
    rendered
        .strip_prefix(r"\\?\")
        .unwrap_or(rendered.as_str())
        .to_string()
}

/// Turn a configured base URL into a normalized base with a trailing slash.
///
/// Accepts `http://host:9337`, `http://host:9337/v1`, and
/// `http://host:9337/v1/` alike, so an operator reusing the `[[plugin]].url`
/// convention from the other plugins does not have to think about it. A bare
/// origin gains `/v1`, because every OpenAI-compatible server this could point
/// at serves the API there.
pub fn parse_api_base(raw: &str) -> Result<Url, String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err("the API base URL is empty".to_string());
    }

    let mut url = Url::parse(trimmed)
        .map_err(|error| format!("the API base URL `{raw}` is not a URL: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "the API base URL must be http or https, not `{}`.",
            url.scheme()
        ));
    }
    // Credentials in the URL would be written into ~/.tdcc/config.toml in
    // plaintext. Point the operator at the environment variable instead.
    if !url.username().is_empty() || url.password().is_some() {
        return Err(format!(
            "the API base URL must not carry credentials; set {ENV_API_KEY} in the environment of \
             the tdcc process instead."
        ));
    }
    if url.host_str().is_none() {
        return Err(format!("the API base URL `{raw}` has no host."));
    }
    if url.path() == "/" || url.path().is_empty() {
        url.set_path("/v1");
    }
    // A trailing slash is what makes `Url::join("models")` append rather than
    // replace the last segment.
    let with_slash = format!("{}/", url.as_str().trim_end_matches('/'));
    Url::parse(&with_slash)
        .map_err(|error| format!("the API base URL `{raw}` is unusable: {error}"))
}

fn join_path(base: &Url, suffix: &str) -> Url {
    base.join(suffix)
        .expect("a base ending in `/` always joins a relative segment")
}

/// True when the URL's host is, syntactically, this machine.
///
/// Syntactic on purpose: this is a guard against an operator pasting a public
/// endpoint by accident, not a defence against a hostile DNS server, and the
/// README says so. The guard that *does* resolve names is [`crate::net`], and
/// it applies to caller-supplied image URLs rather than to operator config.
pub fn is_loopback(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    // `host_str` keeps the brackets on an IPv6 literal.
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(address) = bare.parse::<std::net::IpAddr>() {
        return address.is_loopback();
    }
    let domain = host.to_ascii_lowercase();
    domain == "localhost" || domain.ends_with(".localhost")
}

fn resolve_roots(flags: &Flags, env: &EnvMap) -> Result<Vec<PathBuf>, String> {
    let mut raw: Vec<String> = flags.get(REPEATABLE_FLAG).cloned().unwrap_or_default();
    if raw.is_empty()
        && let Some((joined, _)) = env_value(env, ENV_ROOTS)
    {
        // Platform path-list syntax: `;` on Windows, `:` elsewhere. Using
        // `split_paths` rather than a hand-rolled split is what keeps
        // `C:\photos;D:\scans` working.
        raw = std::env::split_paths(&joined)
            .map(|path| path.to_string_lossy().into_owned())
            .filter(|path| !path.trim().is_empty())
            .collect();
    }

    let mut roots = Vec::new();
    for entry in raw {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let canonical = std::fs::canonicalize(entry).map_err(|error| {
            format!(
                "the configured root `{entry}` could not be resolved ({}). A root that does not \
                 exist would silently make every local path unreadable, so this is a startup \
                 error rather than a warning.",
                error.kind()
            )
        })?;
        if !canonical.is_dir() {
            return Err(format!("the configured root `{entry}` is not a directory."));
        }
        if !roots.contains(&canonical) {
            roots.push(canonical);
        }
    }
    Ok(roots)
}

fn parse_image_format(flags: &Flags, env: &EnvMap) -> Result<ImageFormat, String> {
    match value(flags, env, "--image-format", ENV_IMAGE_FORMAT) {
        None => Ok(ImageFormat::Auto),
        Some((raw, source)) => match raw.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(ImageFormat::Auto),
            "jpeg" | "jpg" => Ok(ImageFormat::Jpeg),
            "png" => Ok(ImageFormat::Png),
            other => Err(format!(
                "{source} is `{other}`, which is not a known encoding. Use `auto`, `jpeg`, or \
                 `png`."
            )),
        },
    }
}

/// Parsed flags. The value is a list because `--root` may repeat; every other
/// flag rejects a repeat outright.
type Flags = BTreeMap<String, Vec<String>>;

/// Accepts `--flag value`, `--flag=value`, and bare boolean flags.
///
/// An unknown flag is a hard error: a typo in `--allow-remote-images` that was
/// quietly ignored would leave the operator believing a guard was off when it
/// was on, or the reverse.
fn parse_flags(args: &[String]) -> Result<Flags, String> {
    let mut flags = Flags::new();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (arg, None),
        };

        let value = if BOOL_FLAGS.contains(&name) {
            index += 1;
            match inline {
                Some(value) => parse_bool(&value)
                    .ok_or_else(|| format!("`{name}` expects true or false, got `{value}`"))?
                    .to_string(),
                None => "true".to_string(),
            }
        } else if VALUE_FLAGS.contains(&name) {
            match inline {
                Some(value) => {
                    index += 1;
                    value
                }
                None => {
                    index += 1;
                    let value = args
                        .get(index)
                        .cloned()
                        .ok_or_else(|| format!("`{name}` expects a value"))?;
                    index += 1;
                    value
                }
            }
        } else {
            return Err(format!(
                "unknown option `{arg}`. Supported: {}, {}.",
                VALUE_FLAGS.join(", "),
                BOOL_FLAGS.join(", ")
            ));
        };

        let entry = flags.entry(name.to_string()).or_default();
        if !entry.is_empty() && name != REPEATABLE_FLAG {
            return Err(format!(
                "`{name}` was given more than once; only `{REPEATABLE_FLAG}` may repeat."
            ));
        }
        entry.push(value);
    }
    Ok(flags)
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
/// from so an error can point at the thing the operator actually wrote.
fn value(flags: &Flags, env: &EnvMap, flag: &str, var: &str) -> Option<(String, String)> {
    flags
        .get(flag)
        .and_then(|values| values.last())
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

/// A guard that is off by default; the flag and the environment variable both
/// turn it on.
fn toggle(flags: &Flags, env: &EnvMap, flag: &str, var: &str) -> Result<bool, String> {
    if let Some(raw) = flags.get(flag).and_then(|values| values.last()) {
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

    #[test]
    fn defaults_point_at_the_node_and_open_nothing() {
        let config = Config::parse(&[], &env(&[])).expect("defaults parse");

        assert_eq!(config.api_base.as_str(), "http://127.0.0.1:9337/v1/");
        assert_eq!(
            config.chat_completions_url().as_str(),
            "http://127.0.0.1:9337/v1/chat/completions"
        );
        assert_eq!(
            config.models_url().as_str(),
            "http://127.0.0.1:9337/v1/models"
        );
        assert!(config.roots.is_empty(), "no filesystem access by default");
        assert!(!config.allow_remote_images);
        assert!(!config.allow_private_network);
        assert!(!config.allow_remote_api);
        assert!(config.model.is_none(), "the model is discovered by default");
        assert!(config.api_key.is_none());
    }

    #[test]
    fn the_api_base_accepts_every_spelling_an_operator_might_write() {
        for raw in [
            "http://127.0.0.1:9337",
            "http://127.0.0.1:9337/",
            "http://127.0.0.1:9337/v1",
            "http://127.0.0.1:9337/v1/",
        ] {
            assert_eq!(
                parse_api_base(raw).expect("parses").as_str(),
                "http://127.0.0.1:9337/v1/",
                "input {raw}"
            );
        }
        // A non-standard prefix is preserved rather than rewritten to /v1.
        assert_eq!(
            parse_api_base("http://127.0.0.1:8080/openai/v1")
                .expect("parses")
                .as_str(),
            "http://127.0.0.1:8080/openai/v1/"
        );
    }

    #[test]
    fn the_hosts_plugin_url_is_accepted_as_the_api_base() {
        let config =
            Config::parse(&[], &env(&[(ENV_PLUGIN_URL, "http://localhost:8080/v1")])).expect("ok");
        assert_eq!(config.api_base.as_str(), "http://localhost:8080/v1/");
    }

    #[test]
    fn a_flag_beats_the_environment_which_beats_the_plugin_url() {
        let config = Config::parse(
            &args(&["--api-base", "http://127.0.0.1:1111"]),
            &env(&[
                (ENV_API_BASE, "http://127.0.0.1:2222"),
                (ENV_PLUGIN_URL, "http://127.0.0.1:3333"),
            ]),
        )
        .expect("ok");
        assert_eq!(config.api_base.as_str(), "http://127.0.0.1:1111/v1/");

        let config = Config::parse(
            &[],
            &env(&[
                (ENV_API_BASE, "http://127.0.0.1:2222"),
                (ENV_PLUGIN_URL, "http://127.0.0.1:3333"),
            ]),
        )
        .expect("ok");
        assert_eq!(config.api_base.as_str(), "http://127.0.0.1:2222/v1/");

        let config =
            Config::parse(&[], &env(&[(ENV_PLUGIN_URL, "http://127.0.0.1:3333")])).expect("ok");
        assert_eq!(config.api_base.as_str(), "http://127.0.0.1:3333/v1/");
    }

    #[test]
    fn a_non_loopback_api_base_needs_an_explicit_opt_in() {
        let error = Config::parse(
            &args(&["--api-base", "https://api.example.com/v1"]),
            &env(&[]),
        )
        .expect_err("remote endpoints are refused by default");
        assert!(error.contains("--allow-remote-api"), "{error}");

        let config = Config::parse(
            &args(&[
                "--api-base",
                "https://api.example.com/v1",
                "--allow-remote-api",
            ]),
            &env(&[]),
        )
        .expect("the opt-in allows it");
        assert!(!is_loopback(&config.api_base));
    }

    #[test]
    fn userinfo_cannot_disguise_a_remote_host_as_loopback() {
        // `http://127.0.0.1@evil.example/v1` has host `evil.example`. Matching
        // the string "127.0.0.1" instead of parsing would wave it through.
        let error = parse_api_base("http://127.0.0.1@evil.example/v1")
            .expect_err("userinfo is credentials-shaped and refused outright");
        assert!(error.contains(ENV_API_KEY), "{error}");

        let suffixed = parse_api_base("http://127.0.0.1.evil.example/v1").expect("parses");
        assert!(
            !is_loopback(&suffixed),
            "a suffixed hostname is not loopback"
        );
    }

    #[test]
    fn loopback_spellings_are_all_recognised() {
        for raw in [
            "http://127.0.0.1:9337/v1",
            "http://127.5.5.5:9337/v1",
            "http://localhost:11434/v1",
            "http://[::1]:8080/v1",
        ] {
            assert!(
                is_loopback(&parse_api_base(raw).expect("parses")),
                "{raw} should be loopback"
            );
        }
    }

    #[test]
    fn a_non_http_api_base_is_refused() {
        for raw in ["file:///etc/passwd", "ftp://example.com/v1"] {
            assert!(parse_api_base(raw).is_err(), "{raw} must be refused");
        }
    }

    #[test]
    fn the_api_key_comes_from_the_environment_and_never_prints() {
        let config = Config::parse(&[], &env(&[(ENV_API_KEY, "sk-super-secret")])).expect("ok");

        let key = config.api_key.as_ref().expect("key is read");
        assert_eq!(key.as_header_value(), "Bearer sk-super-secret");
        assert_eq!(format!("{key:?}"), "ApiKey(<redacted>)");
        assert!(!format!("{config:?}").contains("sk-super-secret"));
        assert!(!config.startup_summary().contains("sk-super-secret"));
        assert!(config.startup_summary().contains("api_key=set"));
    }

    #[test]
    fn an_empty_api_key_is_treated_as_unset() {
        let config = Config::parse(&[], &env(&[(ENV_API_KEY, "   ")])).expect("ok");
        assert!(config.api_key.is_none());
    }

    #[test]
    fn there_is_no_flag_that_could_put_a_key_in_config_toml() {
        assert!(
            !VALUE_FLAGS.iter().any(|flag| flag.contains("key")),
            "args are written into ~/.tdcc/config.toml; a key flag would leak"
        );
    }

    #[test]
    fn a_misspelled_option_is_an_error_rather_than_a_silently_ignored_guard() {
        let error = Config::parse(&args(&["--allow-remote-imgaes"]), &env(&[]))
            .expect_err("unknown flags are refused");
        assert!(error.contains("unknown option"), "{error}");
    }

    #[test]
    fn a_repeated_flag_is_an_error_except_for_root() {
        let error = Config::parse(
            &args(&["--max-dimension", "512", "--max-dimension", "800"]),
            &env(&[]),
        )
        .expect_err("an ambiguous limit must fail");
        assert!(error.contains("more than once"), "{error}");
    }

    #[test]
    fn out_of_range_and_unparseable_numbers_are_rejected_with_the_source_named() {
        let error = Config::parse(&args(&["--max-dimension", "8"]), &env(&[]))
            .expect_err("below the floor");
        assert!(error.contains("--max-dimension"), "{error}");

        let error = Config::parse(&[], &env(&[(ENV_MAX_IMAGES, "99")])).expect_err("above the cap");
        assert!(error.contains(ENV_MAX_IMAGES), "{error}");

        let error =
            Config::parse(&args(&["--max-tokens=lots"]), &env(&[])).expect_err("not a number");
        assert!(error.contains("--max-tokens"), "{error}");
    }

    #[test]
    fn both_flag_spellings_parse() {
        let config = Config::parse(
            &args(&["--max-dimension=640", "--max-tokens", "256"]),
            &env(&[]),
        )
        .expect("ok");
        assert_eq!(config.limits.max_dimension, 640);
        assert_eq!(config.limits.max_tokens, 256);
    }

    #[test]
    fn the_encoding_choice_is_validated() {
        assert_eq!(
            Config::parse(&args(&["--image-format", "png"]), &env(&[]))
                .expect("ok")
                .limits
                .image_format,
            ImageFormat::Png
        );
        assert_eq!(
            Config::parse(&args(&["--image-format", "JPG"]), &env(&[]))
                .expect("ok")
                .limits
                .image_format,
            ImageFormat::Jpeg
        );
        let error = Config::parse(&args(&["--image-format", "avif"]), &env(&[]))
            .expect_err("unknown encodings are refused");
        assert!(error.contains("avif"), "{error}");
    }

    #[test]
    fn guards_can_be_widened_from_either_channel() {
        let config = Config::parse(
            &args(&["--allow-remote-images", "--allow-private-network"]),
            &env(&[]),
        )
        .expect("ok");
        assert!(config.allow_remote_images);
        assert!(config.allow_private_network);

        let config = Config::parse(&[], &env(&[(ENV_ALLOW_REMOTE_IMAGES, "true")])).expect("ok");
        assert!(config.allow_remote_images);

        let config = Config::parse(&[], &env(&[(ENV_ALLOW_REMOTE_IMAGES, "false")])).expect("ok");
        assert!(!config.allow_remote_images);
    }

    #[test]
    fn a_root_that_does_not_exist_is_a_startup_error() {
        let error = Config::parse(
            &args(&["--root", "/definitely/not/a/real/directory/anywhere"]),
            &env(&[]),
        )
        .expect_err("a bad root must fail loudly");
        assert!(error.contains("could not be resolved"), "{error}");
    }

    #[test]
    fn roots_are_canonicalized_deduplicated_and_repeatable() {
        let temp = std::env::temp_dir();
        let canonical = std::fs::canonicalize(&temp).expect("the temp directory exists");
        let raw = temp.to_string_lossy().into_owned();

        let config = Config::parse(&args(&["--root", &raw, "--root", &raw]), &env(&[]))
            .expect("repeats are allowed for --root");

        assert_eq!(config.roots, vec![canonical]);
    }

    #[test]
    fn the_environment_can_supply_a_platform_path_list_of_roots() {
        let temp = std::fs::canonicalize(std::env::temp_dir()).expect("temp exists");
        let joined = std::env::join_paths([&temp]).expect("joins");

        let config =
            Config::parse(&[], &env(&[(ENV_ROOTS, &joined.to_string_lossy())])).expect("ok");

        assert_eq!(config.roots, vec![temp]);
    }

    #[test]
    fn advisories_name_the_choices_that_widen_the_blast_radius() {
        let config = Config::parse(&[], &env(&[])).expect("ok");
        assert!(
            config
                .advisories()
                .iter()
                .any(|line| line.contains("no --root")),
            "the most common first-run surprise has to be announced"
        );

        let config = Config::parse(
            &args(&[
                "--api-base",
                "https://api.example.com/v1",
                "--allow-remote-api",
                "--allow-remote-images",
                "--allow-private-network",
            ]),
            &env(&[]),
        )
        .expect("ok");
        let advisories = config.advisories().join("\n");
        assert!(advisories.contains("non-loopback"), "{advisories}");
        assert!(advisories.contains("your own network"), "{advisories}");
    }

    #[test]
    fn the_verbatim_prefix_is_stripped_only_for_display() {
        assert_eq!(display_path(Path::new(r"\\?\C:\photos")), r"C:\photos");
        assert_eq!(display_path(Path::new("/srv/photos")), "/srv/photos");
    }
}
