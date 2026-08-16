//! Startup options for the plugin process.
//!
//! `[plugin.settings]` never reaches a plugin process: the host stores those
//! values, the console renders them, and a web UI bundle reads them back. There
//! is no settings field in the launch contract or the initialize handshake.
//!
//! Everything this plugin needs before it can serve a single request — above
//! all the root it is confined to — therefore arrives through
//! `[[plugin]].args` or the environment, which are the two channels that do
//! reach the process.

use std::fmt;
use std::path::PathBuf;

pub const ROOT_ENV: &str = "CODE_CONTEXT_ROOT";
pub const MAX_FILE_BYTES_ENV: &str = "CODE_CONTEXT_MAX_FILE_BYTES";
pub const REFRESH_SECS_ENV: &str = "CODE_CONTEXT_REFRESH_SECS";
pub const INCLUDE_HIDDEN_ENV: &str = "CODE_CONTEXT_INCLUDE_HIDDEN";
pub const INCLUDE_VENDORED_ENV: &str = "CODE_CONTEXT_INCLUDE_VENDORED";

/// Files larger than this are listed in the skip counters and never read.
/// A model that receives a 2 MB minified bundle has lost its context window
/// for nothing.
pub const DEFAULT_MAX_FILE_BYTES: u64 = 1024 * 1024;
/// Hard ceiling on `--max-file-bytes`, so a typo cannot turn the index into a
/// whole-disk read.
pub const MAX_FILE_BYTES_CEILING: u64 = 16 * 1024 * 1024;
/// Minimum age of the index before a tool call triggers an incremental
/// refresh. `0` refreshes on every call.
pub const DEFAULT_REFRESH_SECS: u64 = 5;
pub const MAX_REFRESH_SECS: u64 = 3600;

pub const USAGE: &str = "\
code-context — index a local codebase and expose search, read, and tree as MCP tools.

The host launches this binary; it is not meant to be run by hand. Configure it
through [[plugin]].args in ~/.tdcc/config.toml.

  --root <dir>              Directory the plugin is confined to. Required.
  --max-file-bytes <n>      Skip files larger than this (default 1048576).
  --refresh-secs <n>        Minimum age before a tool call reindexes (default 5).
  --include-hidden          Index dot-files and dot-directories too.
  --include-vendored        Index node_modules, target, dist, and friends too.
  --print-package-manifest  Emit plugin-manifest.json and exit.
  --help                    Show this text.

Every flag has an environment fallback, used only when the flag is absent:
CODE_CONTEXT_ROOT, CODE_CONTEXT_MAX_FILE_BYTES, CODE_CONTEXT_REFRESH_SECS,
CODE_CONTEXT_INCLUDE_HIDDEN, CODE_CONTEXT_INCLUDE_VENDORED.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// The one directory this plugin may read. Canonicalized by
    /// [`crate::workspace::Workspace::open`] before any path is resolved
    /// against it.
    pub root: PathBuf,
    pub max_file_bytes: u64,
    pub refresh_secs: u64,
    pub include_hidden: bool,
    pub include_vendored: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OptionsError {
    MissingRoot,
    MissingValue(String),
    InvalidValue {
        name: String,
        value: String,
        expected: String,
    },
    UnknownArgument(String),
}

impl fmt::Display for OptionsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingRoot => write!(
                formatter,
                "no root configured: pass --root <dir> in [[plugin]].args or set {ROOT_ENV}"
            ),
            Self::MissingValue(name) => write!(formatter, "{name} needs a value"),
            Self::InvalidValue {
                name,
                value,
                expected,
            } => write!(formatter, "{name} got {value:?}, expected {expected}"),
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
/// `lookup` is injected rather than read from `std::env` directly so the
/// precedence rules below are testable without mutating process state.
pub fn parse<L>(arguments: &[String], lookup: L) -> Result<Options, OptionsError>
where
    L: Fn(&str) -> Option<String>,
{
    let mut root: Option<String> = None;
    let mut max_file_bytes: Option<u64> = None;
    let mut refresh_secs: Option<u64> = None;
    let mut include_hidden: Option<bool> = None;
    let mut include_vendored: Option<bool> = None;

    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index].as_str();
        // Accept both `--flag value` and `--flag=value`; the host passes
        // whatever the operator wrote in [[plugin]].args verbatim.
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
            "--root" => root = Some(take_value("--root")?),
            "--max-file-bytes" => {
                let value = take_value("--max-file-bytes")?;
                max_file_bytes = Some(parse_u64("--max-file-bytes", &value)?);
            }
            "--refresh-secs" => {
                let value = take_value("--refresh-secs")?;
                refresh_secs = Some(parse_u64("--refresh-secs", &value)?);
            }
            "--include-hidden" => include_hidden = Some(true),
            "--include-vendored" => include_vendored = Some(true),
            other => return Err(OptionsError::UnknownArgument(other.to_string())),
        }
        index += 1;
    }

    let root = root
        .or_else(|| lookup(ROOT_ENV))
        .filter(|value| !value.trim().is_empty())
        .ok_or(OptionsError::MissingRoot)?;

    let max_file_bytes = match max_file_bytes {
        Some(value) => value,
        None => match lookup(MAX_FILE_BYTES_ENV) {
            Some(value) => parse_u64(MAX_FILE_BYTES_ENV, &value)?,
            None => DEFAULT_MAX_FILE_BYTES,
        },
    };
    if max_file_bytes == 0 || max_file_bytes > MAX_FILE_BYTES_CEILING {
        return Err(OptionsError::InvalidValue {
            name: "--max-file-bytes".to_string(),
            value: max_file_bytes.to_string(),
            expected: format!("1..={MAX_FILE_BYTES_CEILING}"),
        });
    }

    let refresh_secs = match refresh_secs {
        Some(value) => value,
        None => match lookup(REFRESH_SECS_ENV) {
            Some(value) => parse_u64(REFRESH_SECS_ENV, &value)?,
            None => DEFAULT_REFRESH_SECS,
        },
    };
    if refresh_secs > MAX_REFRESH_SECS {
        return Err(OptionsError::InvalidValue {
            name: "--refresh-secs".to_string(),
            value: refresh_secs.to_string(),
            expected: format!("0..={MAX_REFRESH_SECS}"),
        });
    }

    let include_hidden = resolve_flag(include_hidden, INCLUDE_HIDDEN_ENV, &lookup)?;
    let include_vendored = resolve_flag(include_vendored, INCLUDE_VENDORED_ENV, &lookup)?;

    Ok(Options {
        root: PathBuf::from(root),
        max_file_bytes,
        refresh_secs,
        include_hidden,
        include_vendored,
    })
}

fn resolve_flag<L>(
    from_arguments: Option<bool>,
    name: &str,
    lookup: &L,
) -> Result<bool, OptionsError>
where
    L: Fn(&str) -> Option<String>,
{
    match from_arguments {
        Some(value) => Ok(value),
        None => match lookup(name) {
            Some(value) => parse_bool(name, &value),
            None => Ok(false),
        },
    }
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

fn parse_bool(name: &str, value: &str) -> Result<bool, OptionsError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(OptionsError::InvalidValue {
            name: name.to_string(),
            value: value.to_string(),
            expected: "one of 1/0, true/false, yes/no, on/off".to_string(),
        }),
    }
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
    fn root_is_required() {
        let error = parse(&[], no_environment).expect_err("no root anywhere");
        assert_eq!(error, OptionsError::MissingRoot);
    }

    #[test]
    fn defaults_apply_when_only_the_root_is_given() {
        let options = parse(&arguments(&["--root", "/srv/repo"]), no_environment).expect("parses");
        assert_eq!(options.root, PathBuf::from("/srv/repo"));
        assert_eq!(options.max_file_bytes, DEFAULT_MAX_FILE_BYTES);
        assert_eq!(options.refresh_secs, DEFAULT_REFRESH_SECS);
        assert!(!options.include_hidden);
        assert!(!options.include_vendored);
    }

    #[test]
    fn inline_and_separate_value_forms_agree() {
        let inline = parse(&arguments(&["--root=/srv/repo"]), no_environment).expect("parses");
        let separate = parse(&arguments(&["--root", "/srv/repo"]), no_environment).expect("parses");
        assert_eq!(inline, separate);
    }

    #[test]
    fn arguments_win_over_the_environment() {
        let options = parse(&arguments(&["--root", "/from/args"]), |name| {
            (name == ROOT_ENV).then(|| "/from/env".to_string())
        })
        .expect("parses");
        assert_eq!(options.root, PathBuf::from("/from/args"));
    }

    #[test]
    fn the_environment_fills_in_what_the_arguments_omit() {
        let options = parse(&[], |name| match name {
            ROOT_ENV => Some("/from/env".to_string()),
            MAX_FILE_BYTES_ENV => Some("2048".to_string()),
            INCLUDE_HIDDEN_ENV => Some("yes".to_string()),
            _ => None,
        })
        .expect("parses");
        assert_eq!(options.root, PathBuf::from("/from/env"));
        assert_eq!(options.max_file_bytes, 2048);
        assert!(options.include_hidden);
        assert!(!options.include_vendored);
    }

    #[test]
    fn an_empty_root_environment_value_is_not_a_root() {
        let error = parse(&[], |name| (name == ROOT_ENV).then(|| "   ".to_string()))
            .expect_err("blank is not a path");
        assert_eq!(error, OptionsError::MissingRoot);
    }

    #[test]
    fn a_missing_flag_value_is_reported_by_name() {
        let error = parse(&arguments(&["--root"]), no_environment).expect_err("no value");
        assert_eq!(error, OptionsError::MissingValue("--root".to_string()));
    }

    #[test]
    fn unknown_arguments_are_rejected_rather_than_ignored() {
        let error = parse(
            &arguments(&["--root", "/srv/repo", "--allow-everything"]),
            no_environment,
        )
        .expect_err("unknown flag");
        assert_eq!(
            error,
            OptionsError::UnknownArgument("--allow-everything".to_string())
        );
    }

    #[test]
    fn max_file_bytes_is_bounded_on_both_ends() {
        for value in ["0", "999999999999"] {
            let error = parse(
                &arguments(&["--root", "/srv/repo", "--max-file-bytes", value]),
                no_environment,
            )
            .expect_err("out of range");
            assert!(
                matches!(error, OptionsError::InvalidValue { .. }),
                "{error}"
            );
        }
    }

    #[test]
    fn non_numeric_sizes_are_rejected() {
        let error = parse(
            &arguments(&["--root", "/srv/repo", "--max-file-bytes", "1MB"]),
            no_environment,
        )
        .expect_err("not a number");
        assert!(
            matches!(error, OptionsError::InvalidValue { .. }),
            "{error}"
        );
    }

    #[test]
    fn boolean_environment_values_accept_the_usual_spellings() {
        for (value, expected) in [
            ("1", true),
            ("true", true),
            ("ON", true),
            ("0", false),
            ("no", false),
        ] {
            let options = parse(&arguments(&["--root", "/srv/repo"]), |name| {
                (name == INCLUDE_VENDORED_ENV).then(|| value.to_string())
            })
            .expect("parses");
            assert_eq!(options.include_vendored, expected, "value {value:?}");
        }

        let error = parse(&arguments(&["--root", "/srv/repo"]), |name| {
            (name == INCLUDE_VENDORED_ENV).then(|| "maybe".to_string())
        })
        .expect_err("not a boolean");
        assert!(
            matches!(error, OptionsError::InvalidValue { .. }),
            "{error}"
        );
    }
}
