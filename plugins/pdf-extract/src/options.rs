//! Startup options for the plugin process.
//!
//! `[plugin.settings]` never reaches a plugin process: the host stores those
//! values, the console renders them, and only a web UI bundle reads them back.
//! There is no settings field in the launch contract or the initialize
//! handshake. Everything this plugin needs before it can answer a single call —
//! above all the roots it is confined to — therefore arrives through
//! `[[plugin]].args` or the environment, which are the two channels that do
//! reach the process.
//!
//! Every limit here exists because a PDF is a parser-hostile format that
//! arrives from somewhere else. The defaults are the narrow ones; widening any
//! of them is a deliberate act by the operator whose machine this runs on.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

pub const PLUGIN_NAME: &str = "pdf-extract";
pub const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");

pub const ROOTS_ENV: &str = "TDCC_PDF_EXTRACT_ROOTS";
pub const MAX_FILE_BYTES_ENV: &str = "TDCC_PDF_EXTRACT_MAX_FILE_BYTES";
pub const MAX_PAGES_ENV: &str = "TDCC_PDF_EXTRACT_MAX_PAGES";
pub const MAX_CHARS_ENV: &str = "TDCC_PDF_EXTRACT_MAX_CHARS";
pub const TIMEOUT_SECS_ENV: &str = "TDCC_PDF_EXTRACT_TIMEOUT_SECS";
pub const MAX_DECOMPRESSED_BYTES_ENV: &str = "TDCC_PDF_EXTRACT_MAX_DECOMPRESSED_BYTES";

/// Files larger than this are refused without being opened. 32 MiB is a large
/// text PDF and a small scanned one; the point is that the ceiling is reached
/// by a `stat`, not by a parse.
pub const DEFAULT_MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
pub const MIN_MAX_FILE_BYTES: u64 = 1024;
pub const MAX_MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;

/// Pages parsed in one call. A caller asking for more gets the first
/// `max_pages` and a `truncated` flag, never a silently shortened answer.
pub const DEFAULT_MAX_PAGES: u32 = 200;
pub const MIN_MAX_PAGES: u32 = 1;
pub const MAX_MAX_PAGES: u32 = 10_000;

/// Characters of extracted text returned by one call.
pub const DEFAULT_MAX_CHARS: u64 = 200_000;
pub const MIN_MAX_CHARS: u64 = 1_000;
pub const MAX_MAX_CHARS: u64 = 20_000_000;

/// Wall-clock budget for one call, checked between pages and inside the
/// content-stream walk.
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
pub const MIN_TIMEOUT_SECS: u64 = 1;
pub const MAX_TIMEOUT_SECS: u64 = 600;

/// Ceiling on what any single compressed stream inside the PDF may inflate to.
/// This is the decompression-bomb guard: a few kilobytes of deflate can expand
/// to gigabytes, and object streams are decoded while the document loads.
pub const DEFAULT_MAX_DECOMPRESSED_BYTES: u64 = 128 * 1024 * 1024;
pub const MIN_MAX_DECOMPRESSED_BYTES: u64 = 1024 * 1024;
pub const MAX_MAX_DECOMPRESSED_BYTES: u64 = 2 * 1024 * 1024 * 1024;

pub const USAGE: &str = "\
pdf-extract — read text, metadata, and tables out of PDF files, confined to
configured roots.

The host launches this binary; it is not meant to be run by hand. Configure it
through [[plugin]].args in ~/.tdcc/config.toml.

  --root <dir>                    Directory the plugin may read. Repeatable.
  --root <label>=<dir>            Same, with an explicit label for callers.
  --max-file-bytes <n>            Refuse PDFs larger than this (default 33554432).
  --max-pages <n>                 Pages parsed per call (default 200).
  --max-chars <n>                 Characters returned per call (default 200000).
  --timeout-secs <n>              Wall-clock budget per call (default 30).
  --max-decompressed-bytes <n>    Per-stream inflate ceiling (default 134217728).
  --print-package-manifest        Emit plugin-manifest.json and exit.
  --help                          Show this text.

At least one --root is required. Callers address files as `<label>/<path>`;
`status` and `list_documents` report the labels in use.

Every flag has an environment fallback, used only when the flag is absent:
TDCC_PDF_EXTRACT_ROOTS (path-separator-separated, entries may be label=dir),
TDCC_PDF_EXTRACT_MAX_FILE_BYTES, TDCC_PDF_EXTRACT_MAX_PAGES,
TDCC_PDF_EXTRACT_MAX_CHARS, TDCC_PDF_EXTRACT_TIMEOUT_SECS,
TDCC_PDF_EXTRACT_MAX_DECOMPRESSED_BYTES.";

/// One root as the operator wrote it: a label a caller uses, and a directory
/// that has not been canonicalized yet. [`crate::paths::Roots::open`] does that.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootSpec {
    pub label: String,
    pub directory: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_file_bytes: u64,
    pub max_pages: u32,
    pub max_chars: u64,
    pub timeout: Duration,
    pub max_decompressed_bytes: u64,
}

impl Limits {
    /// The inflate ceiling as `lopdf` wants it. On a 32-bit target a
    /// configured value above `usize::MAX` saturates rather than wrapping to a
    /// small number, which would turn the guard into a denial of service of its
    /// own.
    pub fn max_decompressed_usize(&self) -> usize {
        usize::try_from(self.max_decompressed_bytes).unwrap_or(usize::MAX)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    pub roots: Vec<RootSpec>,
    pub limits: Limits,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OptionsError {
    NoRoots,
    MissingValue(String),
    InvalidValue {
        name: String,
        value: String,
        expected: String,
    },
    DuplicateRoot(String),
    UnknownArgument(String),
}

impl fmt::Display for OptionsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoRoots => write!(
                formatter,
                "no root configured: pass --root <dir> in [[plugin]].args or set {ROOTS_ENV}"
            ),
            Self::MissingValue(name) => write!(formatter, "{name} needs a value"),
            Self::InvalidValue {
                name,
                value,
                expected,
            } => write!(formatter, "{name} got {value:?}, expected {expected}"),
            Self::DuplicateRoot(label) => write!(
                formatter,
                "two roots were given the same label {label:?}; pass --root <label>=<dir> to \
                 name them apart"
            ),
            Self::UnknownArgument(argument) => {
                write!(formatter, "unknown argument {argument:?}\n\n{USAGE}")
            }
        }
    }
}

impl std::error::Error for OptionsError {}

/// Parse the process arguments, falling back to the environment for anything
/// not passed as a flag.
///
/// `lookup` is injected rather than read from `std::env` so the precedence
/// rules below are testable without mutating process state.
pub fn parse<L>(arguments: &[String], lookup: L) -> Result<Options, OptionsError>
where
    L: Fn(&str) -> Option<String>,
{
    let mut raw_roots: Vec<String> = Vec::new();
    let mut max_file_bytes: Option<u64> = None;
    let mut max_pages: Option<u64> = None;
    let mut max_chars: Option<u64> = None;
    let mut timeout_secs: Option<u64> = None;
    let mut max_decompressed_bytes: Option<u64> = None;

    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index].as_str();
        // Accept both `--flag value` and `--flag=value`; the host passes
        // whatever the operator wrote in [[plugin]].args through verbatim.
        let (name, inline_value) = match argument.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (argument, None),
        };
        let mut take_value = |name: &str| -> Result<String, OptionsError> {
            match inline_value.clone() {
                Some(value) => Ok(value),
                None => {
                    index += 1;
                    arguments
                        .get(index)
                        .cloned()
                        .ok_or_else(|| OptionsError::MissingValue(name.to_string()))
                }
            }
        };

        match name {
            "--root" => raw_roots.push(take_value("--root")?),
            "--max-file-bytes" => {
                let value = take_value("--max-file-bytes")?;
                max_file_bytes = Some(parse_u64("--max-file-bytes", &value)?);
            }
            "--max-pages" => {
                let value = take_value("--max-pages")?;
                max_pages = Some(parse_u64("--max-pages", &value)?);
            }
            "--max-chars" => {
                let value = take_value("--max-chars")?;
                max_chars = Some(parse_u64("--max-chars", &value)?);
            }
            "--timeout-secs" => {
                let value = take_value("--timeout-secs")?;
                timeout_secs = Some(parse_u64("--timeout-secs", &value)?);
            }
            "--max-decompressed-bytes" => {
                let value = take_value("--max-decompressed-bytes")?;
                max_decompressed_bytes = Some(parse_u64("--max-decompressed-bytes", &value)?);
            }
            other => return Err(OptionsError::UnknownArgument(other.to_string())),
        }
        index += 1;
    }

    if raw_roots.is_empty()
        && let Some(value) = lookup(ROOTS_ENV)
    {
        raw_roots.extend(
            std::env::split_paths(&value)
                .map(|entry| entry.to_string_lossy().into_owned())
                .filter(|entry| !entry.trim().is_empty()),
        );
    }
    let roots = label_roots(&raw_roots)?;
    if roots.is_empty() {
        return Err(OptionsError::NoRoots);
    }

    let limits = Limits {
        max_file_bytes: bounded(
            "--max-file-bytes",
            MAX_FILE_BYTES_ENV,
            max_file_bytes,
            &lookup,
            DEFAULT_MAX_FILE_BYTES,
            MIN_MAX_FILE_BYTES,
            MAX_MAX_FILE_BYTES,
        )?,
        max_pages: bounded(
            "--max-pages",
            MAX_PAGES_ENV,
            max_pages,
            &lookup,
            u64::from(DEFAULT_MAX_PAGES),
            u64::from(MIN_MAX_PAGES),
            u64::from(MAX_MAX_PAGES),
        )? as u32,
        max_chars: bounded(
            "--max-chars",
            MAX_CHARS_ENV,
            max_chars,
            &lookup,
            DEFAULT_MAX_CHARS,
            MIN_MAX_CHARS,
            MAX_MAX_CHARS,
        )?,
        timeout: Duration::from_secs(bounded(
            "--timeout-secs",
            TIMEOUT_SECS_ENV,
            timeout_secs,
            &lookup,
            DEFAULT_TIMEOUT_SECS,
            MIN_TIMEOUT_SECS,
            MAX_TIMEOUT_SECS,
        )?),
        max_decompressed_bytes: bounded(
            "--max-decompressed-bytes",
            MAX_DECOMPRESSED_BYTES_ENV,
            max_decompressed_bytes,
            &lookup,
            DEFAULT_MAX_DECOMPRESSED_BYTES,
            MIN_MAX_DECOMPRESSED_BYTES,
            MAX_MAX_DECOMPRESSED_BYTES,
        )?,
    };

    Ok(Options { roots, limits })
}

/// Turn `--root` values into labelled roots.
///
/// `label=dir` names the label outright. A bare directory takes the label from
/// its final component, sanitized to the character set a caller can type
/// without quoting. A collision between two *derived* labels is resolved by
/// appending a number, because the operator did not choose either name; a
/// collision between two *explicit* labels is an error, because they did.
fn label_roots(raw: &[String]) -> Result<Vec<RootSpec>, OptionsError> {
    let mut roots: Vec<RootSpec> = Vec::new();
    for entry in raw {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let (explicit, directory) = match entry.split_once('=') {
            // A single leading path component before `=` is a label. A Windows
            // path (`C:\docs`) has no `=`, so it never lands here.
            Some((label, directory))
                if !label.trim().is_empty() && !directory.trim().is_empty() =>
            {
                (Some(label.trim().to_string()), directory.trim())
            }
            _ => (None, entry),
        };

        let label = match explicit {
            Some(label) => {
                let sanitized = sanitize_label(&label);
                if roots.iter().any(|root| root.label == sanitized) {
                    return Err(OptionsError::DuplicateRoot(sanitized));
                }
                sanitized
            }
            None => {
                let base = PathBuf::from(directory)
                    .file_name()
                    .map(|name| sanitize_label(&name.to_string_lossy()))
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| "root".to_string());
                unique_label(&base, &roots)
            }
        };

        roots.push(RootSpec {
            label,
            directory: PathBuf::from(directory),
        });
    }
    Ok(roots)
}

/// Reduce a label to `[A-Za-z0-9._-]`, which is what a caller can put in a
/// tool argument without any quoting question arising.
fn sanitize_label(raw: &str) -> String {
    let mut label = String::new();
    for character in raw.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
            label.push(character);
        } else if !label.ends_with('-') {
            label.push('-');
        }
    }
    let trimmed = label.trim_matches(['-', '.']).to_string();
    if trimmed.is_empty() {
        "root".to_string()
    } else {
        trimmed
    }
}

fn unique_label(base: &str, existing: &[RootSpec]) -> String {
    if !existing.iter().any(|root| root.label == base) {
        return base.to_string();
    }
    for suffix in 2..u32::MAX {
        let candidate = format!("{base}-{suffix}");
        if !existing.iter().any(|root| root.label == candidate) {
            return candidate;
        }
    }
    unreachable!("a label collision cannot survive four billion attempts")
}

fn bounded<L>(
    flag: &str,
    variable: &str,
    from_arguments: Option<u64>,
    lookup: &L,
    default: u64,
    minimum: u64,
    maximum: u64,
) -> Result<u64, OptionsError>
where
    L: Fn(&str) -> Option<String>,
{
    let (value, name) = match from_arguments {
        Some(value) => (value, flag.to_string()),
        None => match lookup(variable) {
            Some(raw) => (parse_u64(variable, &raw)?, variable.to_string()),
            None => return Ok(default),
        },
    };
    if value < minimum || value > maximum {
        return Err(OptionsError::InvalidValue {
            name,
            value: value.to_string(),
            expected: format!("{minimum}..={maximum}"),
        });
    }
    Ok(value)
}

fn parse_u64(name: &str, value: &str) -> Result<u64, OptionsError> {
    value
        .trim()
        .parse::<u64>()
        .map_err(|_| OptionsError::InvalidValue {
            name: name.to_string(),
            value: value.to_string(),
            expected: "a non-negative integer".to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    fn no_environment(_name: &str) -> Option<String> {
        None
    }

    #[test]
    fn at_least_one_root_is_required() {
        let error = parse(&[], no_environment).expect_err("no root anywhere");
        assert_eq!(error, OptionsError::NoRoots);
    }

    #[test]
    fn defaults_apply_when_only_a_root_is_given() {
        let options = parse(&arguments(&["--root", "/srv/docs"]), no_environment).expect("parses");

        assert_eq!(options.roots.len(), 1);
        assert_eq!(options.roots[0].label, "docs");
        assert_eq!(options.roots[0].directory, PathBuf::from("/srv/docs"));
        assert_eq!(options.limits.max_file_bytes, DEFAULT_MAX_FILE_BYTES);
        assert_eq!(options.limits.max_pages, DEFAULT_MAX_PAGES);
        assert_eq!(options.limits.max_chars, DEFAULT_MAX_CHARS);
        assert_eq!(
            options.limits.timeout,
            Duration::from_secs(DEFAULT_TIMEOUT_SECS)
        );
    }

    #[test]
    fn inline_and_separate_value_forms_agree() {
        let inline = parse(&arguments(&["--root=/srv/docs"]), no_environment).expect("parses");
        let separate = parse(&arguments(&["--root", "/srv/docs"]), no_environment).expect("parses");
        assert_eq!(inline, separate);
    }

    #[test]
    fn roots_may_be_repeated_and_take_their_label_from_the_final_component() {
        let options = parse(
            &arguments(&["--root", "/srv/docs", "--root", "/home/me/papers"]),
            no_environment,
        )
        .expect("parses");

        let labels: Vec<&str> = options
            .roots
            .iter()
            .map(|root| root.label.as_str())
            .collect();
        assert_eq!(labels, vec!["docs", "papers"]);
    }

    #[test]
    fn an_explicit_label_overrides_the_derived_one() {
        let options = parse(
            &arguments(&["--root", "invoices=/srv/docs/2024"]),
            no_environment,
        )
        .expect("parses");

        assert_eq!(options.roots[0].label, "invoices");
        assert_eq!(options.roots[0].directory, PathBuf::from("/srv/docs/2024"));
    }

    #[test]
    fn a_windows_path_without_a_label_is_not_split_on_its_drive_letter() {
        let options = parse(&arguments(&[r"--root=C:\Users\me\Docs"]), no_environment)
            .expect("parses a drive-letter path");

        assert_eq!(
            options.roots[0].directory,
            PathBuf::from(r"C:\Users\me\Docs")
        );
    }

    #[test]
    fn colliding_derived_labels_are_numbered_rather_than_rejected() {
        let options = parse(
            &arguments(&["--root", "/a/reports", "--root", "/b/reports"]),
            no_environment,
        )
        .expect("parses");

        let labels: Vec<&str> = options
            .roots
            .iter()
            .map(|root| root.label.as_str())
            .collect();
        assert_eq!(labels, vec!["reports", "reports-2"]);
    }

    #[test]
    fn colliding_explicit_labels_are_an_error_because_the_operator_chose_both() {
        let error = parse(
            &arguments(&["--root", "docs=/a", "--root", "docs=/b"]),
            no_environment,
        )
        .expect_err("two explicit labels collide");

        assert_eq!(error, OptionsError::DuplicateRoot("docs".to_string()));
    }

    #[test]
    fn labels_are_reduced_to_characters_a_caller_can_type() {
        assert_eq!(sanitize_label("My Documents"), "My-Documents");
        assert_eq!(sanitize_label("rapports (2024)"), "rapports-2024");
        assert_eq!(sanitize_label("..."), "root");
        assert_eq!(sanitize_label(""), "root");
    }

    #[test]
    fn arguments_win_over_the_environment() {
        let options = parse(&arguments(&["--root", "/from/args"]), |name| {
            (name == ROOTS_ENV).then(|| "/from/env".to_string())
        })
        .expect("parses");

        assert_eq!(options.roots.len(), 1);
        assert_eq!(options.roots[0].directory, PathBuf::from("/from/args"));
    }

    #[test]
    fn the_environment_can_carry_several_roots_and_explicit_labels() {
        let joined = std::env::join_paths(["papers=/srv/papers", "/srv/invoices"])
            .expect("join")
            .to_string_lossy()
            .into_owned();
        let options = parse(&[], |name| match name {
            ROOTS_ENV => Some(joined.clone()),
            MAX_PAGES_ENV => Some("12".to_string()),
            _ => None,
        })
        .expect("parses");

        let labels: Vec<&str> = options
            .roots
            .iter()
            .map(|root| root.label.as_str())
            .collect();
        assert_eq!(labels, vec!["papers", "invoices"]);
        assert_eq!(options.limits.max_pages, 12);
    }

    #[test]
    fn unknown_arguments_are_rejected_rather_than_ignored() {
        let error = parse(
            &arguments(&["--root", "/srv/docs", "--max-file-byte", "10"]),
            no_environment,
        )
        .expect_err("a typo must not be silently dropped");

        assert_eq!(
            error,
            OptionsError::UnknownArgument("--max-file-byte".to_string())
        );
    }

    #[test]
    fn a_missing_flag_value_is_reported_by_name() {
        let error = parse(&arguments(&["--root"]), no_environment).expect_err("no value");
        assert_eq!(error, OptionsError::MissingValue("--root".to_string()));
    }

    #[test]
    fn every_limit_is_bounded_on_both_ends() {
        for (flag, below, above) in [
            ("--max-file-bytes", "1023", "536870913"),
            ("--max-pages", "0", "10001"),
            ("--max-chars", "999", "20000001"),
            ("--timeout-secs", "0", "601"),
            ("--max-decompressed-bytes", "1048575", "2147483649"),
        ] {
            for value in [below, above] {
                let error = parse(
                    &arguments(&["--root", "/srv/docs", flag, value]),
                    no_environment,
                )
                .expect_err("a limit outside its range must be a startup error");
                assert!(
                    matches!(&error, OptionsError::InvalidValue { name, .. } if name == flag),
                    "{flag}={value} produced {error}"
                );
            }
        }
    }

    #[test]
    fn out_of_range_limits_name_the_flag_and_the_range() {
        let error = parse(
            &arguments(&["--root", "/srv/docs", "--timeout-secs", "0"]),
            no_environment,
        )
        .expect_err("zero timeout is refused");

        assert_eq!(
            error,
            OptionsError::InvalidValue {
                name: "--timeout-secs".to_string(),
                value: "0".to_string(),
                expected: "1..=600".to_string(),
            }
        );
    }

    #[test]
    fn an_out_of_range_environment_value_names_the_variable_not_the_flag() {
        let error = parse(&arguments(&["--root", "/srv/docs"]), |name| {
            (name == MAX_PAGES_ENV).then(|| "100000".to_string())
        })
        .expect_err("out of range");

        assert!(
            matches!(&error, OptionsError::InvalidValue { name, .. } if name == MAX_PAGES_ENV),
            "{error}"
        );
    }

    #[test]
    fn non_numeric_limits_are_rejected() {
        let error = parse(
            &arguments(&["--root", "/srv/docs", "--max-file-bytes", "32MB"]),
            no_environment,
        )
        .expect_err("not a number");

        assert!(
            matches!(error, OptionsError::InvalidValue { .. }),
            "{error}"
        );
    }
}
