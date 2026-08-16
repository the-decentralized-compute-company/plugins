//! Launch-time configuration: which databases exist, and how big an answer is
//! allowed to get.
//!
//! Everything here is parsed once, from `[[plugin]].args` and the process
//! environment, before the runtime connects to the host. It deliberately does
//! *not* come from `[plugin.settings]`: host-owned settings are stored and
//! rendered by the console but are never delivered to the plugin process, so a
//! security boundary must not be built on them.
//!
//! The parser is a plain function over an argument list and an environment
//! lookup so the whole surface is unit-testable without touching the real
//! environment.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

/// Rows a single result set may carry before it is reported as truncated.
pub const DEFAULT_MAX_ROWS: usize = 200;
/// Approximate serialized size a single result set may reach.
pub const DEFAULT_MAX_BYTES: usize = 256 * 1024;
/// Wall-clock budget for one statement, enforced with `sqlite3_interrupt`.
pub const DEFAULT_TIMEOUT_MS: u64 = 5_000;
/// Size at which one cell is shortened, so a single BLOB or document column
/// cannot consume the whole byte budget on row one.
pub const DEFAULT_MAX_CELL_BYTES: usize = 2_048;

/// Ceilings an operator cannot raise past. These exist because the limits are
/// the only thing standing between a model writing `SELECT * FROM events` and
/// a multi-gigabyte response, and a typo in `args` should not remove them.
const MAX_ROWS_CEILING: usize = 10_000;
const MAX_BYTES_CEILING: usize = 8 * 1024 * 1024;
const TIMEOUT_MS_CEILING: u64 = 120_000;
const MAX_CELL_BYTES_CEILING: usize = 1024 * 1024;

/// Longest alias accepted. Aliases are typed by a model, so they stay short.
const MAX_ALIAS_LEN: usize = 64;

/// Separator between `alias=path` entries inside one environment variable.
///
/// `;` is used on every platform. A POSIX path may legally contain `;`, so the
/// README tells operators with such a path to use `--db` instead.
const ENV_DB_SEPARATOR: char = ';';

pub const ENV_DB: &str = "TDCC_SQLITE_QUERY_DB";
pub const ENV_DB_RW: &str = "TDCC_SQLITE_QUERY_DB_RW";
pub const ENV_MAX_ROWS: &str = "TDCC_SQLITE_QUERY_MAX_ROWS";
pub const ENV_MAX_BYTES: &str = "TDCC_SQLITE_QUERY_MAX_BYTES";
pub const ENV_TIMEOUT_MS: &str = "TDCC_SQLITE_QUERY_TIMEOUT_MS";
pub const ENV_MAX_CELL_BYTES: &str = "TDCC_SQLITE_QUERY_MAX_CELL_BYTES";

/// One database the operator has decided this plugin may open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatabaseSpec {
    /// The name a caller uses to select this database. No caller ever supplies
    /// a path.
    pub alias: String,
    pub path: PathBuf,
    /// True only for `--db-rw`. Read-only is not a mode the caller can leave.
    pub writable: bool,
}

/// The bounds every statement runs under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_rows: usize,
    pub max_bytes: usize,
    pub max_cell_bytes: usize,
    pub timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_rows: DEFAULT_MAX_ROWS,
            max_bytes: DEFAULT_MAX_BYTES,
            max_cell_bytes: DEFAULT_MAX_CELL_BYTES,
            timeout: Duration::from_millis(DEFAULT_TIMEOUT_MS),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub databases: Vec<DatabaseSpec>,
    pub limits: Limits,
}

/// Parse `[[plugin]].args` with environment variables as a fallback.
///
/// `args` must not include the executable name. `env` is injected rather than
/// read directly so tests can drive it. Arguments win over the environment for
/// the scalar limits; database lists from both sources are merged, and a
/// duplicate alias is an error rather than a silent last-one-wins.
pub fn parse_settings<A, E>(args: A, env: E) -> Result<Settings, String>
where
    A: IntoIterator<Item = String>,
    E: Fn(&str) -> Option<String>,
{
    let mut databases: Vec<DatabaseSpec> = Vec::new();
    let mut max_rows: Option<usize> = None;
    let mut max_bytes: Option<usize> = None;
    let mut max_cell_bytes: Option<usize> = None;
    let mut timeout_ms: Option<u64> = None;

    let mut args = args.into_iter().peekable();
    while let Some(argument) = args.next() {
        // Accept both `--flag value` and `--flag=value`; operators write both.
        let (flag, inline) = match argument.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => {
                (flag.to_string(), Some(value.to_string()))
            }
            _ => (argument.clone(), None),
        };
        let mut take_value = |flag: &str| -> Result<String, String> {
            match inline.clone() {
                Some(value) => Ok(value),
                None => args
                    .next()
                    .ok_or_else(|| format!("{flag} requires a value")),
            }
        };

        match flag.as_str() {
            "--db" => databases.push(parse_database_entry(&take_value("--db")?, false)?),
            "--db-rw" => databases.push(parse_database_entry(&take_value("--db-rw")?, true)?),
            "--max-rows" => max_rows = Some(parse_usize("--max-rows", &take_value("--max-rows")?)?),
            "--max-bytes" => {
                max_bytes = Some(parse_usize("--max-bytes", &take_value("--max-bytes")?)?)
            }
            "--max-cell-bytes" => {
                max_cell_bytes = Some(parse_usize(
                    "--max-cell-bytes",
                    &take_value("--max-cell-bytes")?,
                )?)
            }
            "--timeout-ms" => {
                timeout_ms = Some(parse_u64("--timeout-ms", &take_value("--timeout-ms")?)?)
            }
            other => {
                return Err(format!(
                    "unknown argument {other:?}; supported: --db, --db-rw, --max-rows, \
                     --max-bytes, --max-cell-bytes, --timeout-ms"
                ));
            }
        }
    }

    // `--db` beats `TDCC_SQLITE_QUERY_DB` only in the sense that both are kept;
    // a collision between them is a configuration mistake worth reporting.
    if let Some(raw) = env(ENV_DB) {
        databases.extend(parse_database_list(&raw, false)?);
    }
    if let Some(raw) = env(ENV_DB_RW) {
        databases.extend(parse_database_list(&raw, true)?);
    }

    let max_rows = resolve_usize(max_rows, &env, ENV_MAX_ROWS, DEFAULT_MAX_ROWS)?;
    let max_bytes = resolve_usize(max_bytes, &env, ENV_MAX_BYTES, DEFAULT_MAX_BYTES)?;
    let max_cell_bytes = resolve_usize(
        max_cell_bytes,
        &env,
        ENV_MAX_CELL_BYTES,
        DEFAULT_MAX_CELL_BYTES,
    )?;
    let timeout_ms = match timeout_ms {
        Some(value) => value,
        None => match env(ENV_TIMEOUT_MS) {
            Some(raw) => parse_u64(ENV_TIMEOUT_MS, &raw)?,
            None => DEFAULT_TIMEOUT_MS,
        },
    };

    let limits = Limits {
        max_rows: bounded_usize("max_rows", max_rows, 1, MAX_ROWS_CEILING)?,
        max_bytes: bounded_usize("max_bytes", max_bytes, 1_024, MAX_BYTES_CEILING)?,
        max_cell_bytes: bounded_usize(
            "max_cell_bytes",
            max_cell_bytes,
            16,
            MAX_CELL_BYTES_CEILING,
        )?,
        timeout: Duration::from_millis(bounded_u64(
            "timeout_ms",
            timeout_ms,
            1,
            TIMEOUT_MS_CEILING,
        )?),
    };

    let mut seen = BTreeSet::new();
    for database in &databases {
        if !seen.insert(database.alias.clone()) {
            return Err(format!(
                "database alias {:?} is configured more than once",
                database.alias
            ));
        }
    }

    Ok(Settings { databases, limits })
}

fn parse_database_list(raw: &str, writable: bool) -> Result<Vec<DatabaseSpec>, String> {
    raw.split(ENV_DB_SEPARATOR)
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| parse_database_entry(entry, writable))
        .collect()
}

/// Split one `alias=path` entry. The path keeps every character after the first
/// `=`, so a Windows path like `db=C:\data\app.db` and a path containing `=`
/// both survive.
fn parse_database_entry(entry: &str, writable: bool) -> Result<DatabaseSpec, String> {
    let (alias, path) = entry
        .split_once('=')
        .ok_or_else(|| format!("expected alias=path, got {entry:?}"))?;
    let alias = alias.trim();
    let path = path.trim();
    validate_alias(alias)?;
    if path.is_empty() {
        return Err(format!("database {alias:?} has an empty path"));
    }
    Ok(DatabaseSpec {
        alias: alias.to_string(),
        path: PathBuf::from(path),
        writable,
    })
}

/// Aliases appear in tool arguments and in error messages, so they are kept to
/// an obviously safe character set. This is not a path check — no caller ever
/// supplies a path — it just keeps identifiers readable and unambiguous.
pub fn validate_alias(alias: &str) -> Result<(), String> {
    if alias.is_empty() {
        return Err("database alias must not be empty".to_string());
    }
    if alias.len() > MAX_ALIAS_LEN {
        return Err(format!(
            "database alias {alias:?} is longer than {MAX_ALIAS_LEN} characters"
        ));
    }
    if !alias
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return Err(format!(
            "database alias {alias:?} may only contain ASCII letters, digits, '_' and '-'"
        ));
    }
    if !alias
        .chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_alphanumeric())
    {
        return Err(format!(
            "database alias {alias:?} must start with an ASCII letter or digit"
        ));
    }
    Ok(())
}

fn resolve_usize<E>(
    from_args: Option<usize>,
    env: &E,
    key: &str,
    fallback: usize,
) -> Result<usize, String>
where
    E: Fn(&str) -> Option<String>,
{
    match from_args {
        Some(value) => Ok(value),
        None => match env(key) {
            Some(raw) => parse_usize(key, &raw),
            None => Ok(fallback),
        },
    }
}

fn parse_usize(what: &str, raw: &str) -> Result<usize, String> {
    raw.trim()
        .parse::<usize>()
        .map_err(|_| format!("{what} expects a non-negative integer, got {raw:?}"))
}

fn parse_u64(what: &str, raw: &str) -> Result<u64, String> {
    raw.trim()
        .parse::<u64>()
        .map_err(|_| format!("{what} expects a non-negative integer, got {raw:?}"))
}

fn bounded_usize(what: &str, value: usize, min: usize, max: usize) -> Result<usize, String> {
    if value < min || value > max {
        return Err(format!(
            "{what} must be between {min} and {max}, got {value}"
        ));
    }
    Ok(value)
}

fn bounded_u64(what: &str, value: u64, min: u64, max: u64) -> Result<u64, String> {
    if value < min || value > max {
        return Err(format!(
            "{what} must be between {min} and {max}, got {value}"
        ));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn defaults_apply_when_nothing_is_configured() {
        let settings = parse_settings(Vec::new(), no_env).expect("empty config parses");
        assert!(settings.databases.is_empty());
        assert_eq!(settings.limits, Limits::default());
    }

    #[test]
    fn separate_and_inline_flag_values_are_equivalent() {
        let separate = parse_settings(args(&["--db", "app=/srv/app.db"]), no_env).unwrap();
        let inline = parse_settings(args(&["--db=app=/srv/app.db"]), no_env).unwrap();
        assert_eq!(separate, inline);
        assert_eq!(separate.databases[0].alias, "app");
        assert_eq!(separate.databases[0].path, PathBuf::from("/srv/app.db"));
        assert!(!separate.databases[0].writable);
    }

    #[test]
    fn a_windows_path_keeps_its_drive_letter() {
        let settings = parse_settings(args(&["--db", r"app=C:\data\app.db"]), no_env).unwrap();
        assert_eq!(settings.databases[0].path, PathBuf::from(r"C:\data\app.db"));
    }

    #[test]
    fn db_rw_is_the_only_way_to_mark_a_database_writable() {
        let settings =
            parse_settings(args(&["--db", "ro=/a.db", "--db-rw", "rw=/b.db"]), no_env).unwrap();
        assert!(!settings.databases[0].writable);
        assert!(settings.databases[1].writable);
    }

    #[test]
    fn environment_lists_are_split_and_merged() {
        let settings = parse_settings(args(&["--db", "one=/one.db"]), |key| match key {
            ENV_DB => Some("two=/two.db; three=/three.db".to_string()),
            ENV_DB_RW => Some("four=/four.db".to_string()),
            _ => None,
        })
        .unwrap();

        let aliases: Vec<_> = settings
            .databases
            .iter()
            .map(|database| database.alias.as_str())
            .collect();
        assert_eq!(aliases, ["one", "two", "three", "four"]);
        assert!(settings.databases[3].writable);
    }

    #[test]
    fn a_duplicate_alias_is_rejected_rather_than_silently_overridden() {
        let error = parse_settings(args(&["--db", "app=/a.db", "--db", "app=/b.db"]), no_env)
            .expect_err("duplicates must fail");
        assert!(error.contains("more than once"), "{error}");
    }

    #[test]
    fn a_duplicate_across_args_and_environment_is_also_rejected() {
        let error = parse_settings(args(&["--db", "app=/a.db"]), |key| {
            (key == ENV_DB).then(|| "app=/b.db".to_string())
        })
        .expect_err("duplicates must fail");
        assert!(error.contains("more than once"), "{error}");
    }

    #[test]
    fn malformed_database_entries_are_rejected() {
        for entry in ["app", "=/a.db", "app=", "app db=/a.db", "../app=/a.db"] {
            parse_settings(args(&["--db", entry]), no_env)
                .expect_err(&format!("{entry:?} must not parse"));
        }
    }

    #[test]
    fn alias_rules_reject_traversal_shaped_and_leading_punctuation_names() {
        assert!(validate_alias("app").is_ok());
        assert!(validate_alias("app-2_v1").is_ok());
        assert!(validate_alias("..").is_err());
        assert!(validate_alias("-app").is_err());
        assert!(validate_alias("a/b").is_err());
        assert!(validate_alias("").is_err());
        assert!(validate_alias(&"a".repeat(MAX_ALIAS_LEN + 1)).is_err());
    }

    #[test]
    fn limits_come_from_args_then_environment_then_defaults() {
        let from_args = parse_settings(args(&["--max-rows", "10", "--timeout-ms", "250"]), |key| {
            (key == ENV_MAX_ROWS).then(|| "9999".to_string())
        })
        .unwrap();
        assert_eq!(from_args.limits.max_rows, 10);
        assert_eq!(from_args.limits.timeout, Duration::from_millis(250));

        let from_env = parse_settings(Vec::new(), |key| match key {
            ENV_MAX_BYTES => Some("4096".to_string()),
            ENV_MAX_CELL_BYTES => Some("64".to_string()),
            _ => None,
        })
        .unwrap();
        assert_eq!(from_env.limits.max_bytes, 4096);
        assert_eq!(from_env.limits.max_cell_bytes, 64);
        assert_eq!(from_env.limits.max_rows, DEFAULT_MAX_ROWS);
    }

    #[test]
    fn limits_outside_the_ceilings_are_refused() {
        for bad in [
            vec!["--max-rows", "0"],
            vec!["--max-rows", "10001"],
            vec!["--max-bytes", "16"],
            vec!["--max-bytes", "999999999"],
            vec!["--timeout-ms", "0"],
            vec!["--timeout-ms", "600000"],
            vec!["--max-cell-bytes", "1"],
        ] {
            parse_settings(args(&bad), no_env).expect_err(&format!("{bad:?} must be refused"));
        }
    }

    #[test]
    fn a_flag_without_a_value_and_an_unknown_flag_both_fail_loudly() {
        let missing = parse_settings(args(&["--db"]), no_env).expect_err("missing value");
        assert!(missing.contains("requires a value"), "{missing}");

        let unknown =
            parse_settings(args(&["--allow-everything"]), no_env).expect_err("unknown flag");
        assert!(unknown.contains("unknown argument"), "{unknown}");
    }

    #[test]
    fn non_numeric_limits_report_the_offending_value() {
        let error =
            parse_settings(args(&["--max-rows", "lots"]), no_env).expect_err("not a number");
        assert!(error.contains("lots"), "{error}");
    }
}
