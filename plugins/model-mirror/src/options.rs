//! Operator-facing configuration, read from `[[plugin]].args` and the
//! environment.
//!
//! **Why not `config_schema`?** Because `[plugin.settings]` never reaches the
//! plugin process. The host stores those values and the console renders them,
//! but there is no settings field in the launch contract or the initialize
//! handshake — a plugin only *declares* the schema. Every limit in this file
//! has to be enforced inside this process, so declaring it as a console
//! setting would produce a control that looks like it caps disk usage and does
//! not. The plugins guide says as much: "If the process itself needs a value,
//! pass it through `[[plugin]].args`, `[[plugin]].url`, or the plugin's own
//! state."
//!
//! So: `args` first, environment second, conservative default last.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::policy::{
    DEFAULT_REVERIFY_AFTER_SECS, DEFAULT_SERVE_BYTES_PER_MINUTE, MAX_CHUNK_BYTES_CEILING,
};

pub const ENV_PREFIX: &str = "TDCC_MODEL_MIRROR_";

/// Fully resolved operator configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MirrorOptions {
    /// Where this mirror keeps its own copy of every artifact it holds.
    pub cache_dir: PathBuf,
    /// Directories `mirror.import` is allowed to read from. Nothing outside
    /// these is importable, however the path is spelled.
    pub import_roots: Vec<PathBuf>,
    /// Total bytes of artifact data this mirror may hold. `0` means it holds
    /// nothing — the safe default, and the one that makes an unconfigured
    /// mirror inert instead of surprising.
    pub max_cache_bytes: u64,
    /// Largest single transfer chunk, clamped to [`MAX_CHUNK_BYTES_CEILING`].
    pub max_chunk_bytes: u64,
    /// Outbound artifact bytes per minute. `0` means unlimited, and is an
    /// explicit operator opt-in.
    pub serve_bytes_per_minute: u64,
    /// Re-digest an artifact before serving it if the last full verification
    /// is older than this.
    pub reverify_after_secs: u64,
    /// Announce this node's inventory over the mesh channel.
    pub advertise: bool,
}

impl MirrorOptions {
    /// True when the mirror has been given disk to work with.
    pub fn holds_artifacts(&self) -> bool {
        self.max_cache_bytes > 0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OptionsError {
    UnknownArgument(String),
    MissingValue(&'static str),
    InvalidNumber { flag: String, value: String },
    InvalidSize { flag: String, value: String },
    RelativePath { flag: &'static str, value: String },
    NoDefaultCacheDir,
}

impl fmt::Display for OptionsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownArgument(argument) => write!(
                formatter,
                "unknown option '{argument}'; run the binary with --help for the supported flags"
            ),
            Self::MissingValue(flag) => write!(formatter, "{flag} needs a value"),
            Self::InvalidNumber { flag, value } => {
                write!(formatter, "{flag} expects a whole number, got '{value}'")
            }
            Self::InvalidSize { flag, value } => write!(
                formatter,
                "{flag} expects a byte size like 250GB, 40GiB, or 0, got '{value}'"
            ),
            Self::RelativePath { flag, value } => {
                write!(formatter, "{flag} must be an absolute path, got '{value}'")
            }
            Self::NoDefaultCacheDir => formatter.write_str(
                "no cache directory could be derived from the environment; pass --cache-dir",
            ),
        }
    }
}

impl std::error::Error for OptionsError {}

/// Which default directory layout to use.
///
/// Passed in rather than read from `cfg!` inside the resolver so the layout
/// for every platform is testable from any platform.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Windows,
    MacOs,
    Unix,
}

impl Platform {
    pub fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::MacOs
        } else {
            Self::Unix
        }
    }
}

/// Parse `[[plugin]].args`, falling back to `TDCC_MODEL_MIRROR_*` and then to
/// defaults.
///
/// `env` is injected so the whole resolver is testable without mutating the
/// process environment, which is a data race in a threaded test binary.
pub fn parse_options<F>(
    args: &[String],
    env: F,
    platform: Platform,
) -> Result<MirrorOptions, OptionsError>
where
    F: Fn(&str) -> Option<String>,
{
    let mut cache_dir: Option<PathBuf> = None;
    let mut import_roots: Vec<PathBuf> = Vec::new();
    let mut max_cache_bytes: Option<u64> = None;
    let mut max_chunk_bytes: Option<u64> = None;
    let mut serve_bytes_per_minute: Option<u64> = None;
    let mut reverify_after_secs: Option<u64> = None;
    let mut advertise = true;

    let mut index = 0;
    while index < args.len() {
        // `--flag=value` and `--flag value` are both accepted; `value_of`
        // consumes the following argument only when there is no inline one.
        let (flag, inline) = match args[index].split_once('=') {
            Some((flag, value)) => (flag.to_string(), Some(value.to_string())),
            None => (args[index].clone(), None),
        };

        match flag.as_str() {
            "--cache-dir" => {
                cache_dir = Some(PathBuf::from(value_of(
                    args,
                    &mut index,
                    inline,
                    "--cache-dir",
                )?));
            }
            "--import-root" => {
                import_roots.push(PathBuf::from(value_of(
                    args,
                    &mut index,
                    inline,
                    "--import-root",
                )?));
            }
            "--max-cache-bytes" => {
                let value = value_of(args, &mut index, inline, "--max-cache-bytes")?;
                max_cache_bytes = Some(size_or_error("--max-cache-bytes", value)?);
            }
            "--max-chunk-bytes" => {
                let value = value_of(args, &mut index, inline, "--max-chunk-bytes")?;
                max_chunk_bytes = Some(size_or_error("--max-chunk-bytes", value)?);
            }
            "--serve-bytes-per-minute" => {
                let value = value_of(args, &mut index, inline, "--serve-bytes-per-minute")?;
                serve_bytes_per_minute = Some(size_or_error("--serve-bytes-per-minute", value)?);
            }
            "--reverify-after-secs" => {
                let value = value_of(args, &mut index, inline, "--reverify-after-secs")?;
                reverify_after_secs =
                    Some(
                        value
                            .trim()
                            .parse::<u64>()
                            .map_err(|_| OptionsError::InvalidNumber {
                                flag: "--reverify-after-secs".to_string(),
                                value,
                            })?,
                    );
            }
            "--no-advertise" => advertise = false,
            other => return Err(OptionsError::UnknownArgument(other.to_string())),
        }
        index += 1;
    }

    let cache_dir = match cache_dir {
        Some(path) => path,
        None => match env_path(&env, "CACHE_DIR") {
            Some(path) => path,
            None => default_cache_dir(&env, platform).ok_or(OptionsError::NoDefaultCacheDir)?,
        },
    };
    require_absolute("--cache-dir", &cache_dir)?;

    if import_roots.is_empty() {
        import_roots = match env(&format!("{ENV_PREFIX}IMPORT_ROOTS")) {
            Some(joined) => split_path_list(&joined),
            None => huggingface_hub_cache_dir(&env).into_iter().collect(),
        };
    }
    for root in &import_roots {
        require_absolute("--import-root", root)?;
    }

    let max_cache_bytes = resolve_size(
        max_cache_bytes,
        &env,
        "MAX_CACHE_BYTES",
        "--max-cache-bytes",
        0,
    )?;
    let max_chunk_bytes = resolve_size(
        max_chunk_bytes,
        &env,
        "MAX_CHUNK_BYTES",
        "--max-chunk-bytes",
        MAX_CHUNK_BYTES_CEILING,
    )?
    .clamp(1, MAX_CHUNK_BYTES_CEILING);
    let serve_bytes_per_minute = resolve_size(
        serve_bytes_per_minute,
        &env,
        "SERVE_BYTES_PER_MINUTE",
        "--serve-bytes-per-minute",
        DEFAULT_SERVE_BYTES_PER_MINUTE,
    )?;
    let reverify_after_secs = match reverify_after_secs {
        Some(value) => value,
        None => match env(&format!("{ENV_PREFIX}REVERIFY_AFTER_SECS")) {
            Some(raw) => raw
                .trim()
                .parse::<u64>()
                .map_err(|_| OptionsError::InvalidNumber {
                    flag: "--reverify-after-secs".to_string(),
                    value: raw,
                })?,
            None => DEFAULT_REVERIFY_AFTER_SECS,
        },
    };

    Ok(MirrorOptions {
        cache_dir,
        import_roots,
        max_cache_bytes,
        max_chunk_bytes,
        serve_bytes_per_minute,
        reverify_after_secs,
        advertise,
    })
}

fn value_of(
    args: &[String],
    index: &mut usize,
    inline: Option<String>,
    flag: &'static str,
) -> Result<String, OptionsError> {
    if let Some(value) = inline {
        return Ok(value);
    }
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or(OptionsError::MissingValue(flag))
}

fn size_or_error(flag: &'static str, value: String) -> Result<u64, OptionsError> {
    parse_byte_size(&value).map_err(|_| OptionsError::InvalidSize {
        flag: flag.to_string(),
        value,
    })
}

fn resolve_size<F>(
    parsed: Option<u64>,
    env: &F,
    env_suffix: &str,
    flag: &str,
    fallback: u64,
) -> Result<u64, OptionsError>
where
    F: Fn(&str) -> Option<String>,
{
    if let Some(value) = parsed {
        return Ok(value);
    }
    match env(&format!("{ENV_PREFIX}{env_suffix}")) {
        Some(raw) => parse_byte_size(&raw).map_err(|_| OptionsError::InvalidSize {
            flag: flag.to_string(),
            value: raw,
        }),
        None => Ok(fallback),
    }
}

fn require_absolute(flag: &'static str, path: &Path) -> Result<(), OptionsError> {
    if path.is_absolute() {
        return Ok(());
    }
    Err(OptionsError::RelativePath {
        flag,
        value: path.display().to_string(),
    })
}

fn env_path<F>(env: &F, suffix: &str) -> Option<PathBuf>
where
    F: Fn(&str) -> Option<String>,
{
    let value = env(&format!("{ENV_PREFIX}{suffix}"))?;
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| PathBuf::from(trimmed))
}

fn split_path_list(joined: &str) -> Vec<PathBuf> {
    let separator = if cfg!(windows) { ';' } else { ':' };
    joined
        .split(separator)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Where the mirror keeps its artifacts when the operator does not say.
///
/// Follows the same per-platform convention the host's own caches use
/// (`dirs::cache_dir()` joined with `tdcc`), reimplemented here rather than
/// taking a dependency for four `join` calls.
pub fn default_cache_dir<F>(env: &F, platform: Platform) -> Option<PathBuf>
where
    F: Fn(&str) -> Option<String>,
{
    let base = match platform {
        Platform::Windows => non_empty(env("LOCALAPPDATA")).map(PathBuf::from)?,
        Platform::MacOs => home_dir(env)?.join("Library").join("Caches"),
        Platform::Unix => match non_empty(env("XDG_CACHE_HOME")) {
            Some(value) => PathBuf::from(value),
            None => home_dir(env)?.join(".cache"),
        },
    };
    Some(base.join("tdcc").join("model-mirror"))
}

/// Default import root: the Hugging Face hub cache.
///
/// Same precedence as `model_hf::huggingface_hub_cache_dir`, so `mirror.import`
/// can read exactly the models `tdcc` already downloaded and nothing else.
/// `HOME` is checked before `USERPROFILE`; the host only consults `HOME`, and
/// the extra fallback exists because `HOME` is usually unset on Windows.
pub fn huggingface_hub_cache_dir<F>(env: &F) -> Option<PathBuf>
where
    F: Fn(&str) -> Option<String>,
{
    if let Some(path) = non_empty(env("HF_HUB_CACHE")) {
        return Some(PathBuf::from(path));
    }
    if let Some(path) = non_empty(env("HUGGINGFACE_HUB_CACHE")) {
        return Some(PathBuf::from(path));
    }
    if let Some(path) = non_empty(env("HF_HOME")) {
        return Some(PathBuf::from(path).join("hub"));
    }
    if let Some(path) = non_empty(env("XDG_CACHE_HOME")) {
        return Some(PathBuf::from(path).join("huggingface").join("hub"));
    }
    Some(
        home_dir(env)?
            .join(".cache")
            .join("huggingface")
            .join("hub"),
    )
}

fn home_dir<F>(env: &F) -> Option<PathBuf>
where
    F: Fn(&str) -> Option<String>,
{
    non_empty(env("HOME"))
        .or_else(|| non_empty(env("USERPROFILE")))
        .map(PathBuf::from)
}

fn non_empty(value: Option<String>) -> Option<String> {
    let value = value?;
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Which configured root contains `resolved`, if any.
///
/// Both sides must already be canonicalized by the caller. Canonicalization is
/// what makes this a real boundary rather than a string check: a symlink inside
/// an import root that points at `/etc` resolves to `/etc`, matches no root,
/// and is refused.
pub fn containing_root<'a>(roots: &'a [PathBuf], resolved: &Path) -> Option<&'a Path> {
    roots
        .iter()
        .find(|root| resolved.starts_with(root))
        .map(PathBuf::as_path)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByteSizeError;

/// Parse `1024`, `40GiB`, `250 GB`, `0`.
///
/// Binary suffixes (`KiB`, `MiB`, `GiB`, `TiB`) and decimal ones (`KB`, `MB`,
/// `GB`, `TB`) both work and mean what they say; disk vendors and operating
/// systems disagree about this, so the mirror refuses to guess.
pub fn parse_byte_size(value: &str) -> Result<u64, ByteSizeError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(ByteSizeError);
    }
    let split = trimmed
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(trimmed.len());
    let (digits, suffix) = trimmed.split_at(split);
    if digits.is_empty() {
        return Err(ByteSizeError);
    }
    let number: u64 = digits.parse().map_err(|_| ByteSizeError)?;
    let multiplier: u64 = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "kb" => 1_000,
        "mb" => 1_000_000,
        "gb" => 1_000_000_000,
        "tb" => 1_000_000_000_000,
        "k" | "kib" => 1024,
        "m" | "mib" => 1024 * 1024,
        "g" | "gib" => 1024 * 1024 * 1024,
        "t" | "tib" => 1024_u64 * 1024 * 1024 * 1024,
        _ => return Err(ByteSizeError),
    };
    number.checked_mul(multiplier).ok_or(ByteSizeError)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[cfg(windows)]
    const ABSOLUTE_ROOT: &str = r"C:\mirror";
    #[cfg(not(windows))]
    const ABSOLUTE_ROOT: &str = "/mirror";

    #[test]
    fn byte_sizes_accept_binary_and_decimal_suffixes() {
        assert_eq!(parse_byte_size("0"), Ok(0));
        assert_eq!(parse_byte_size("1024"), Ok(1024));
        assert_eq!(parse_byte_size("1KiB"), Ok(1024));
        assert_eq!(parse_byte_size("1kb"), Ok(1_000));
        assert_eq!(parse_byte_size(" 250 GB "), Ok(250_000_000_000));
        assert_eq!(parse_byte_size("40GiB"), Ok(40 * 1024 * 1024 * 1024));
    }

    #[test]
    fn byte_sizes_reject_nonsense_instead_of_guessing() {
        assert_eq!(parse_byte_size(""), Err(ByteSizeError));
        assert_eq!(parse_byte_size("GB"), Err(ByteSizeError));
        assert_eq!(parse_byte_size("-1"), Err(ByteSizeError));
        assert_eq!(parse_byte_size("1.5GB"), Err(ByteSizeError));
        assert_eq!(parse_byte_size("12 parsecs"), Err(ByteSizeError));
        assert_eq!(
            parse_byte_size("99999999999999999999TiB"),
            Err(ByteSizeError)
        );
    }

    #[test]
    fn an_unconfigured_mirror_holds_nothing() {
        let options = parse_options(
            &args(&["--cache-dir", ABSOLUTE_ROOT]),
            env_from(&[]),
            Platform::Unix,
        )
        .expect("defaults resolve");

        assert_eq!(options.max_cache_bytes, 0);
        assert!(!options.holds_artifacts());
        assert_eq!(
            options.serve_bytes_per_minute,
            DEFAULT_SERVE_BYTES_PER_MINUTE
        );
        assert_eq!(options.reverify_after_secs, DEFAULT_REVERIFY_AFTER_SECS);
        assert!(options.advertise);
    }

    #[test]
    fn args_accept_both_inline_and_separated_values() {
        let inline = parse_options(
            &args(&[
                &format!("--cache-dir={ABSOLUTE_ROOT}"),
                "--max-cache-bytes=40GiB",
                "--no-advertise",
            ]),
            env_from(&[]),
            Platform::Unix,
        )
        .expect("inline values parse");
        let separated = parse_options(
            &args(&[
                "--cache-dir",
                ABSOLUTE_ROOT,
                "--max-cache-bytes",
                "40GiB",
                "--no-advertise",
            ]),
            env_from(&[]),
            Platform::Unix,
        )
        .expect("separated values parse");

        assert_eq!(inline, separated);
        assert_eq!(inline.max_cache_bytes, 40 * 1024 * 1024 * 1024);
        assert!(!inline.advertise);
    }

    #[test]
    fn args_win_over_environment() {
        let options = parse_options(
            &args(&["--cache-dir", ABSOLUTE_ROOT, "--max-cache-bytes", "1MiB"]),
            env_from(&[("TDCC_MODEL_MIRROR_MAX_CACHE_BYTES", "999GB")]),
            Platform::Unix,
        )
        .expect("args win");

        assert_eq!(options.max_cache_bytes, 1024 * 1024);
    }

    #[test]
    fn environment_fills_in_what_args_omit() {
        let options = parse_options(
            &args(&[]),
            env_from(&[
                ("TDCC_MODEL_MIRROR_CACHE_DIR", ABSOLUTE_ROOT),
                ("TDCC_MODEL_MIRROR_MAX_CACHE_BYTES", "2MiB"),
                ("TDCC_MODEL_MIRROR_SERVE_BYTES_PER_MINUTE", "0"),
                ("TDCC_MODEL_MIRROR_REVERIFY_AFTER_SECS", "60"),
            ]),
            Platform::Unix,
        )
        .expect("environment resolves");

        assert_eq!(options.cache_dir, PathBuf::from(ABSOLUTE_ROOT));
        assert_eq!(options.max_cache_bytes, 2 * 1024 * 1024);
        assert_eq!(options.serve_bytes_per_minute, 0);
        assert_eq!(options.reverify_after_secs, 60);
    }

    #[test]
    fn chunk_size_is_clamped_to_the_ceiling() {
        let options = parse_options(
            &args(&["--cache-dir", ABSOLUTE_ROOT, "--max-chunk-bytes", "1GiB"]),
            env_from(&[]),
            Platform::Unix,
        )
        .expect("clamped");

        assert_eq!(options.max_chunk_bytes, MAX_CHUNK_BYTES_CEILING);
    }

    #[test]
    fn bad_arguments_are_named_not_swallowed() {
        assert_eq!(
            parse_options(&args(&["--nope"]), env_from(&[]), Platform::Unix),
            Err(OptionsError::UnknownArgument("--nope".to_string()))
        );
        assert_eq!(
            parse_options(&args(&["--cache-dir"]), env_from(&[]), Platform::Unix),
            Err(OptionsError::MissingValue("--cache-dir"))
        );
        assert_eq!(
            parse_options(
                &args(&["--cache-dir", ABSOLUTE_ROOT, "--max-cache-bytes", "lots"]),
                env_from(&[]),
                Platform::Unix
            ),
            Err(OptionsError::InvalidSize {
                flag: "--max-cache-bytes".to_string(),
                value: "lots".to_string()
            })
        );
    }

    #[test]
    fn relative_paths_are_refused_so_the_root_cannot_follow_the_working_directory() {
        assert_eq!(
            parse_options(
                &args(&["--cache-dir", "relative/mirror"]),
                env_from(&[]),
                Platform::Unix
            ),
            Err(OptionsError::RelativePath {
                flag: "--cache-dir",
                value: "relative/mirror".to_string()
            })
        );
    }

    #[test]
    fn default_import_root_follows_the_hugging_face_cache_precedence() {
        let explicit = huggingface_hub_cache_dir(&env_from(&[
            ("HF_HUB_CACHE", "/explicit/hub"),
            ("HF_HOME", "/ignored"),
        ]))
        .expect("explicit");
        assert_eq!(explicit, PathBuf::from("/explicit/hub"));

        let from_home =
            huggingface_hub_cache_dir(&env_from(&[("HF_HOME", "/hf")])).expect("hf home");
        assert_eq!(from_home, PathBuf::from("/hf").join("hub"));

        let fallback =
            huggingface_hub_cache_dir(&env_from(&[("HOME", "/home/dev")])).expect("home fallback");
        assert_eq!(
            fallback,
            PathBuf::from("/home/dev")
                .join(".cache")
                .join("huggingface")
                .join("hub")
        );

        assert!(huggingface_hub_cache_dir(&env_from(&[])).is_none());
    }

    #[test]
    fn default_cache_dir_matches_the_host_cache_convention_per_platform() {
        assert_eq!(
            default_cache_dir(
                &env_from(&[("LOCALAPPDATA", "C:/Users/dev/AppData/Local")]),
                Platform::Windows
            ),
            Some(
                PathBuf::from("C:/Users/dev/AppData/Local")
                    .join("tdcc")
                    .join("model-mirror")
            )
        );
        assert_eq!(
            default_cache_dir(&env_from(&[("HOME", "/Users/dev")]), Platform::MacOs),
            Some(
                PathBuf::from("/Users/dev")
                    .join("Library")
                    .join("Caches")
                    .join("tdcc")
                    .join("model-mirror")
            )
        );
        assert_eq!(
            default_cache_dir(&env_from(&[("XDG_CACHE_HOME", "/cache")]), Platform::Unix),
            Some(PathBuf::from("/cache").join("tdcc").join("model-mirror"))
        );
        assert_eq!(
            default_cache_dir(&env_from(&[]), Platform::Unix),
            None,
            "an environment with no home should ask for --cache-dir rather than guess"
        );
    }

    #[test]
    fn containing_root_only_matches_a_configured_root() {
        let roots = vec![PathBuf::from("/models/hub"), PathBuf::from("/extra")];

        assert_eq!(
            containing_root(&roots, Path::new("/models/hub/models--org--repo/x.gguf")),
            Some(Path::new("/models/hub"))
        );
        assert_eq!(containing_root(&roots, Path::new("/etc/passwd")), None);
        // `starts_with` is component-wise, so a sibling directory whose name
        // merely begins with a root's name is not inside it.
        assert_eq!(
            containing_root(&roots, Path::new("/models/hub-evil/x")),
            None
        );
    }
}
