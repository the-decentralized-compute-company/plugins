//! Launch-time configuration: which repositories exist, how much output a
//! single answer may produce, and whether file content may leave the process.
//!
//! Everything here is parsed once, from `[[plugin]].args` and the process
//! environment, before the runtime connects to the host. It deliberately does
//! *not* come from `[plugin.settings]`: host-owned settings are stored and
//! rendered by the console but are never delivered to the plugin process, so
//! the list of repositories a model may read must not be built on them.
//!
//! The parser is a plain function over an argument list and an environment
//! lookup, so every precedence rule below is unit-testable without touching
//! real process state.

use std::collections::BTreeSet;
use std::path::PathBuf;

pub const PLUGIN_NAME: &str = "git-tools";
pub const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Commits a single `log` call may return when the caller names no `limit`.
pub const DEFAULT_MAX_COMMITS: usize = 30;
/// Ceiling on the `limit` argument of `log`, settable by the operator.
pub const DEFAULT_COMMIT_CEILING: usize = 200;
/// Commits the revision walk may *examine* before it gives up and reports the
/// answer as truncated. A path filter over a long history reads a tree diff per
/// commit, so this is the bound that keeps a filtered `log` from running for
/// minutes on somebody else's CPU.
pub const DEFAULT_MAX_SCAN_COMMITS: usize = 50_000;
/// Bytes of unified diff text a single response may carry.
pub const DEFAULT_MAX_PATCH_BYTES: usize = 256 * 1024;
/// Files a diff may touch before rename detection is skipped.
///
/// Inexact rename detection compares every removed file against every added
/// one, so it is quadratic. This is not a guess: on a 3065-file release range
/// in the TDCC repository, computing the diff took 19 ms and detecting renames
/// took 12 seconds. `git` has the same limit under the name
/// `diff.renameLimit`.
pub const DEFAULT_RENAME_CANDIDATE_LIMIT: usize = 400;
/// Lines a single `blame` call may attribute.
pub const DEFAULT_MAX_BLAME_LINES: usize = 2_000;
/// Size at which a file is refused for `blame` outright. Blame cost grows with
/// file size *and* history depth, so the cheapest guard is the file itself.
pub const DEFAULT_MAX_BLAME_FILE_BYTES: u64 = 1024 * 1024;

/// Ceilings an operator cannot raise past. A typo in `args` should not be able
/// to remove the only thing standing between a model asking for "the diff
/// between these two releases" and a multi-gigabyte response.
const COMMIT_CEILING_MAX: usize = 5_000;
const SCAN_COMMITS_CEILING: usize = 5_000_000;
const PATCH_BYTES_CEILING: usize = 8 * 1024 * 1024;
const RENAME_CANDIDATE_CEILING: usize = 20_000;
const BLAME_LINES_CEILING: usize = 50_000;
const BLAME_FILE_BYTES_CEILING: u64 = 32 * 1024 * 1024;

/// Hard caps, not settable at all. Every one of these bounds a list a caller
/// can grow just by pointing at a bigger repository.
pub const MAX_FILE_ENTRIES: usize = 1_000;
pub const MAX_REF_ENTRIES: usize = 1_000;
pub const MAX_REFS_SCANNED: usize = 5_000;
pub const MAX_STATUS_ENTRIES: usize = 1_000;
pub const MAX_PATHSPECS: usize = 32;
/// Commit message bytes carried per entry in a `log` response.
pub const MAX_LOG_MESSAGE_BYTES: usize = 8 * 1024;
/// Commit message bytes carried by `show`, which returns exactly one commit.
pub const MAX_SHOW_MESSAGE_BYTES: usize = 64 * 1024;

/// Longest alias accepted. Aliases are typed by a model, so they stay short.
const MAX_ALIAS_LEN: usize = 64;

/// Separator between `alias=path` entries inside one environment variable.
///
/// `;` on every platform. A POSIX path may legally contain `;`, so the README
/// tells operators with such a path to use `--repo` instead.
const ENV_REPO_SEPARATOR: char = ';';

pub const ENV_REPO: &str = "TDCC_GIT_TOOLS_REPO";
pub const ENV_MAX_COMMITS: &str = "TDCC_GIT_TOOLS_MAX_COMMITS";
pub const ENV_MAX_SCAN_COMMITS: &str = "TDCC_GIT_TOOLS_MAX_SCAN_COMMITS";
pub const ENV_MAX_PATCH_BYTES: &str = "TDCC_GIT_TOOLS_MAX_PATCH_BYTES";
pub const ENV_MAX_RENAME_CANDIDATES: &str = "TDCC_GIT_TOOLS_MAX_RENAME_CANDIDATES";
pub const ENV_MAX_BLAME_LINES: &str = "TDCC_GIT_TOOLS_MAX_BLAME_LINES";
pub const ENV_MAX_BLAME_FILE_BYTES: &str = "TDCC_GIT_TOOLS_MAX_BLAME_FILE_BYTES";
pub const ENV_NO_CONTENT: &str = "TDCC_GIT_TOOLS_NO_CONTENT";
pub const ENV_REDACT_EMAILS: &str = "TDCC_GIT_TOOLS_REDACT_EMAILS";

pub const USAGE: &str = "\
git-tools — read the history of repositories an operator listed, as MCP tools.

The host launches this binary; it is not meant to be run by hand. Configure it
through [[plugin]].args in ~/.tdcc/config.toml. Every operation is read-only:
this plugin never commits, checks out, fetches, pushes, or writes git config.

  --repo <alias>=<path>       A repository the plugin may read. Repeatable.
                              At least one is required.
  --max-commits <n>           Ceiling on the `limit` argument of log
                              (default 200, max 5000).
  --max-scan-commits <n>      Commits a walk may examine before reporting the
                              answer truncated (default 50000, max 5000000).
  --max-patch-bytes <n>       Diff text one response may carry
                              (default 262144, max 8388608).
  --max-rename-candidates <n> Files a diff may touch before rename detection
                              is skipped (default 400, max 20000).
  --max-blame-lines <n>       Lines one blame call may attribute
                              (default 2000, max 50000).
  --max-blame-file-bytes <n>  Largest file blame will accept
                              (default 1048576, max 33554432).
  --no-content                Never return file content: no diff hunks, no
                              blame line text. Metadata and statistics only.
  --redact-emails             Replace author and committer email addresses
                              with '<redacted>' in every response.
  --print-package-manifest    Emit plugin-manifest.json and exit.
  --help                      Show this text.

Every flag has an environment fallback, used only when the flag is absent:
TDCC_GIT_TOOLS_REPO (alias=path;alias=path), TDCC_GIT_TOOLS_MAX_COMMITS,
TDCC_GIT_TOOLS_MAX_SCAN_COMMITS, TDCC_GIT_TOOLS_MAX_PATCH_BYTES,
TDCC_GIT_TOOLS_MAX_RENAME_CANDIDATES, TDCC_GIT_TOOLS_MAX_BLAME_LINES,
TDCC_GIT_TOOLS_MAX_BLAME_FILE_BYTES, TDCC_GIT_TOOLS_NO_CONTENT,
TDCC_GIT_TOOLS_REDACT_EMAILS.";

/// One repository the operator has decided this plugin may read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoSpec {
    /// The name a caller uses to select this repository. No caller ever
    /// supplies a path.
    pub alias: String,
    pub path: PathBuf,
}

/// The bounds every call runs under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub commit_ceiling: usize,
    pub max_scan_commits: usize,
    pub max_patch_bytes: usize,
    pub rename_candidate_limit: usize,
    pub max_blame_lines: usize,
    pub max_blame_file_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            commit_ceiling: DEFAULT_COMMIT_CEILING,
            max_scan_commits: DEFAULT_MAX_SCAN_COMMITS,
            max_patch_bytes: DEFAULT_MAX_PATCH_BYTES,
            rename_candidate_limit: DEFAULT_RENAME_CANDIDATE_LIMIT,
            max_blame_lines: DEFAULT_MAX_BLAME_LINES,
            max_blame_file_bytes: DEFAULT_MAX_BLAME_FILE_BYTES,
        }
    }
}

/// What a response is allowed to disclose.
///
/// Both fields default to the *permissive* value, because a history tool that
/// returns no content and no authors is not much of a history tool. They exist
/// so an operator lending a machine to strangers can narrow it in one flag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Disclosure {
    /// When false, `--no-content` is in force: no diff hunks and no blame line
    /// text ever leave the process. Asking for them is an error, not a silent
    /// omission.
    pub content: bool,
    /// When true, every author and committer email is replaced by
    /// `<redacted>`. Names are kept — they are what makes a blame useful.
    pub redact_emails: bool,
}

impl Default for Disclosure {
    fn default() -> Self {
        Self {
            content: true,
            redact_emails: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub repositories: Vec<RepoSpec>,
    pub limits: Limits,
    pub disclosure: Disclosure,
}

/// Parse `[[plugin]].args` with environment variables as a fallback.
///
/// `args` must not include the executable name. `env` is injected rather than
/// read directly so tests can drive it. Arguments win over the environment for
/// the scalar limits and the boolean policies; repository lists from both
/// sources are merged, and a duplicate alias is an error rather than a silent
/// last-one-wins.
///
/// `require_repositories` is false only on the packaging path, where the
/// manifest has to be printable on a machine that has no repositories at all.
pub fn parse_settings<A, E>(args: A, env: E, require_repositories: bool) -> Result<Settings, String>
where
    A: IntoIterator<Item = String>,
    E: Fn(&str) -> Option<String>,
{
    let mut repositories: Vec<RepoSpec> = Vec::new();
    let mut commit_ceiling: Option<usize> = None;
    let mut max_scan_commits: Option<usize> = None;
    let mut max_patch_bytes: Option<usize> = None;
    let mut rename_candidate_limit: Option<usize> = None;
    let mut max_blame_lines: Option<usize> = None;
    let mut max_blame_file_bytes: Option<u64> = None;
    let mut no_content: Option<bool> = None;
    let mut redact_emails: Option<bool> = None;

    let mut args = args.into_iter();
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
            "--repo" => repositories.push(parse_repo_entry(&take_value("--repo")?)?),
            "--max-commits" => {
                commit_ceiling = Some(parse_usize("--max-commits", &take_value("--max-commits")?)?);
            }
            "--max-scan-commits" => {
                max_scan_commits = Some(parse_usize(
                    "--max-scan-commits",
                    &take_value("--max-scan-commits")?,
                )?);
            }
            "--max-patch-bytes" => {
                max_patch_bytes = Some(parse_usize(
                    "--max-patch-bytes",
                    &take_value("--max-patch-bytes")?,
                )?);
            }
            "--max-rename-candidates" => {
                rename_candidate_limit = Some(parse_usize(
                    "--max-rename-candidates",
                    &take_value("--max-rename-candidates")?,
                )?);
            }
            "--max-blame-lines" => {
                max_blame_lines = Some(parse_usize(
                    "--max-blame-lines",
                    &take_value("--max-blame-lines")?,
                )?);
            }
            "--max-blame-file-bytes" => {
                max_blame_file_bytes = Some(parse_u64(
                    "--max-blame-file-bytes",
                    &take_value("--max-blame-file-bytes")?,
                )?);
            }
            "--no-content" => no_content = Some(true),
            "--redact-emails" => redact_emails = Some(true),
            other => {
                return Err(format!(
                    "unknown argument {other:?}; supported: --repo, --max-commits, \
                     --max-scan-commits, --max-patch-bytes, --max-rename-candidates, \
                     --max-blame-lines, --max-blame-file-bytes, --no-content, \
                     --redact-emails\n\n{USAGE}"
                ));
            }
        }
    }

    if let Some(raw) = env(ENV_REPO) {
        repositories.extend(parse_repo_list(&raw)?);
    }

    let mut seen = BTreeSet::new();
    for repository in &repositories {
        if !seen.insert(repository.alias.clone()) {
            return Err(format!(
                "repository alias {:?} is configured more than once",
                repository.alias
            ));
        }
    }
    if require_repositories && repositories.is_empty() {
        return Err(format!(
            "no repository configured: pass --repo <alias>=<path> in [[plugin]].args or set \
             {ENV_REPO}\n\n{USAGE}"
        ));
    }

    let limits = Limits {
        commit_ceiling: bounded_usize(
            "--max-commits",
            resolve_usize(
                commit_ceiling,
                &env,
                ENV_MAX_COMMITS,
                DEFAULT_COMMIT_CEILING,
            )?,
            1,
            COMMIT_CEILING_MAX,
        )?,
        max_scan_commits: bounded_usize(
            "--max-scan-commits",
            resolve_usize(
                max_scan_commits,
                &env,
                ENV_MAX_SCAN_COMMITS,
                DEFAULT_MAX_SCAN_COMMITS,
            )?,
            1,
            SCAN_COMMITS_CEILING,
        )?,
        max_patch_bytes: bounded_usize(
            "--max-patch-bytes",
            resolve_usize(
                max_patch_bytes,
                &env,
                ENV_MAX_PATCH_BYTES,
                DEFAULT_MAX_PATCH_BYTES,
            )?,
            1_024,
            PATCH_BYTES_CEILING,
        )?,
        rename_candidate_limit: bounded_usize(
            "--max-rename-candidates",
            resolve_usize(
                rename_candidate_limit,
                &env,
                ENV_MAX_RENAME_CANDIDATES,
                DEFAULT_RENAME_CANDIDATE_LIMIT,
            )?,
            1,
            RENAME_CANDIDATE_CEILING,
        )?,
        max_blame_lines: bounded_usize(
            "--max-blame-lines",
            resolve_usize(
                max_blame_lines,
                &env,
                ENV_MAX_BLAME_LINES,
                DEFAULT_MAX_BLAME_LINES,
            )?,
            1,
            BLAME_LINES_CEILING,
        )?,
        max_blame_file_bytes: bounded_u64(
            "--max-blame-file-bytes",
            match max_blame_file_bytes {
                Some(value) => value,
                None => match env(ENV_MAX_BLAME_FILE_BYTES) {
                    Some(raw) => parse_u64(ENV_MAX_BLAME_FILE_BYTES, &raw)?,
                    None => DEFAULT_MAX_BLAME_FILE_BYTES,
                },
            },
            1_024,
            BLAME_FILE_BYTES_CEILING,
        )?,
    };

    let disclosure = Disclosure {
        content: !resolve_bool(no_content, &env, ENV_NO_CONTENT, false)?,
        redact_emails: resolve_bool(redact_emails, &env, ENV_REDACT_EMAILS, false)?,
    };

    Ok(Settings {
        repositories,
        limits,
        disclosure,
    })
}

fn parse_repo_list(raw: &str) -> Result<Vec<RepoSpec>, String> {
    raw.split(ENV_REPO_SEPARATOR)
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(parse_repo_entry)
        .collect()
}

/// Split one `alias=path` entry. The path keeps every character after the first
/// `=`, so a Windows path like `mesh=C:\src\tdcc-mesh` and a path containing
/// `=` both survive.
fn parse_repo_entry(entry: &str) -> Result<RepoSpec, String> {
    let (alias, path) = entry
        .split_once('=')
        .ok_or_else(|| format!("expected alias=path, got {entry:?}"))?;
    let alias = alias.trim();
    let path = path.trim();
    validate_alias(alias)?;
    if path.is_empty() {
        return Err(format!("repository {alias:?} has an empty path"));
    }
    Ok(RepoSpec {
        alias: alias.to_string(),
        path: PathBuf::from(path),
    })
}

/// Aliases appear in tool arguments and in error messages, so they are kept to
/// an obviously safe character set. This is not a path check — no caller ever
/// supplies a path — it just keeps identifiers readable and unambiguous, and
/// keeps a traversal-shaped alias out of an error string.
pub fn validate_alias(alias: &str) -> Result<(), String> {
    if alias.is_empty() {
        return Err("repository alias must not be empty".to_string());
    }
    if alias.len() > MAX_ALIAS_LEN {
        return Err(format!(
            "repository alias {alias:?} is longer than {MAX_ALIAS_LEN} characters"
        ));
    }
    if !alias
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || character == '_' || character == '-')
    {
        return Err(format!(
            "repository alias {alias:?} may only contain ASCII letters, digits, '_' and '-'"
        ));
    }
    if !alias
        .chars()
        .next()
        .is_some_and(|character| character.is_ascii_alphanumeric())
    {
        return Err(format!(
            "repository alias {alias:?} must start with an ASCII letter or digit"
        ));
    }
    Ok(())
}

fn resolve_usize<E>(
    from_args: Option<usize>,
    env: &E,
    name: &str,
    default: usize,
) -> Result<usize, String>
where
    E: Fn(&str) -> Option<String>,
{
    match from_args {
        Some(value) => Ok(value),
        None => match env(name) {
            Some(raw) => parse_usize(name, &raw),
            None => Ok(default),
        },
    }
}

fn resolve_bool<E>(
    from_args: Option<bool>,
    env: &E,
    name: &str,
    default: bool,
) -> Result<bool, String>
where
    E: Fn(&str) -> Option<String>,
{
    match from_args {
        Some(value) => Ok(value),
        None => match env(name) {
            Some(raw) => parse_bool(name, &raw),
            None => Ok(default),
        },
    }
}

fn parse_usize(name: &str, raw: &str) -> Result<usize, String> {
    raw.trim()
        .parse::<usize>()
        .map_err(|_| format!("{name} expects a non-negative integer, got {raw:?}"))
}

fn parse_u64(name: &str, raw: &str) -> Result<u64, String> {
    raw.trim()
        .parse::<u64>()
        .map_err(|_| format!("{name} expects a non-negative integer, got {raw:?}"))
}

fn parse_bool(name: &str, raw: &str) -> Result<bool, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(format!(
            "{name} expects one of 1/0, true/false, yes/no, on/off, got {raw:?}"
        )),
    }
}

fn bounded_usize(name: &str, value: usize, low: usize, high: usize) -> Result<usize, String> {
    if value < low || value > high {
        return Err(format!(
            "{name} must be between {low} and {high}, got {value}"
        ));
    }
    Ok(value)
}

fn bounded_u64(name: &str, value: u64, low: u64, high: u64) -> Result<u64, String> {
    if value < low || value > high {
        return Err(format!(
            "{name} must be between {low} and {high}, got {value}"
        ));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    fn no_env(_name: &str) -> Option<String> {
        None
    }

    fn parse(values: &[&str]) -> Result<Settings, String> {
        parse_settings(args(values), no_env, true)
    }

    #[test]
    fn at_least_one_repository_is_required() {
        let error = parse(&[]).expect_err("no repository anywhere");
        assert!(error.contains("no repository configured"), "{error}");
        assert!(error.contains(ENV_REPO), "{error}");
    }

    #[test]
    fn the_packaging_path_does_not_need_a_repository() {
        let settings =
            parse_settings(Vec::<String>::new(), no_env, false).expect("manifest-only parse");
        assert!(settings.repositories.is_empty());
        assert_eq!(settings.limits, Limits::default());
        assert_eq!(settings.disclosure, Disclosure::default());
    }

    #[test]
    fn defaults_apply_when_only_a_repository_is_given() {
        let settings = parse(&["--repo", "mesh=/srv/repos/tdcc-mesh"]).expect("parses");
        assert_eq!(settings.repositories.len(), 1);
        assert_eq!(settings.repositories[0].alias, "mesh");
        assert_eq!(
            settings.repositories[0].path,
            PathBuf::from("/srv/repos/tdcc-mesh")
        );
        assert_eq!(settings.limits, Limits::default());
        assert!(settings.disclosure.content);
        assert!(!settings.disclosure.redact_emails);
    }

    #[test]
    fn inline_and_separate_value_forms_agree() {
        let inline = parse(&["--repo=mesh=/srv/repo", "--max-commits=50"]).expect("parses");
        let separate = parse(&["--repo", "mesh=/srv/repo", "--max-commits", "50"]).expect("parses");
        assert_eq!(inline, separate);
        assert_eq!(inline.limits.commit_ceiling, 50);
    }

    #[test]
    fn a_windows_path_keeps_everything_after_the_first_equals() {
        let settings = parse(&["--repo", r"mesh=C:\src\tdcc-mesh"]).expect("parses");
        assert_eq!(
            settings.repositories[0].path,
            PathBuf::from(r"C:\src\tdcc-mesh")
        );
    }

    #[test]
    fn several_repositories_are_kept_in_order() {
        let settings =
            parse(&["--repo", "one=/a", "--repo", "two=/b", "--repo", "three=/c"]).expect("parses");
        let aliases: Vec<&str> = settings
            .repositories
            .iter()
            .map(|repository| repository.alias.as_str())
            .collect();
        assert_eq!(aliases, ["one", "two", "three"]);
    }

    #[test]
    fn arguments_and_the_environment_are_merged_for_repositories() {
        let settings = parse_settings(
            args(&["--repo", "one=/a"]),
            |name| (name == ENV_REPO).then(|| "two=/b;three=/c".to_string()),
            true,
        )
        .expect("parses");
        let aliases: Vec<&str> = settings
            .repositories
            .iter()
            .map(|repository| repository.alias.as_str())
            .collect();
        assert_eq!(aliases, ["one", "two", "three"]);
    }

    #[test]
    fn a_duplicate_alias_is_rejected_rather_than_silently_overridden() {
        let error = parse(&["--repo", "mesh=/a", "--repo", "mesh=/b"])
            .expect_err("the same alias would resolve to two different repositories");
        assert!(error.contains("configured more than once"), "{error}");
    }

    #[test]
    fn arguments_win_over_the_environment_for_scalar_limits() {
        let settings = parse_settings(
            args(&["--repo", "mesh=/a", "--max-patch-bytes", "4096"]),
            |name| match name {
                ENV_MAX_PATCH_BYTES => Some("999999".to_string()),
                _ => None,
            },
            true,
        )
        .expect("parses");
        assert_eq!(settings.limits.max_patch_bytes, 4096);
    }

    #[test]
    fn the_environment_fills_in_what_the_arguments_omit() {
        let settings = parse_settings(
            args(&["--repo", "mesh=/a"]),
            |name| match name {
                ENV_MAX_COMMITS => Some("77".to_string()),
                ENV_MAX_BLAME_LINES => Some("120".to_string()),
                ENV_NO_CONTENT => Some("yes".to_string()),
                ENV_REDACT_EMAILS => Some("1".to_string()),
                _ => None,
            },
            true,
        )
        .expect("parses");
        assert_eq!(settings.limits.commit_ceiling, 77);
        assert_eq!(settings.limits.max_blame_lines, 120);
        assert!(!settings.disclosure.content);
        assert!(settings.disclosure.redact_emails);
    }

    #[test]
    fn no_content_and_redact_emails_are_off_until_asked_for() {
        let settings = parse(&["--repo", "mesh=/a"]).expect("parses");
        assert!(settings.disclosure.content);
        assert!(!settings.disclosure.redact_emails);

        let narrowed =
            parse(&["--repo", "mesh=/a", "--no-content", "--redact-emails"]).expect("parses");
        assert!(!narrowed.disclosure.content);
        assert!(narrowed.disclosure.redact_emails);
    }

    #[test]
    fn unknown_arguments_are_rejected_rather_than_ignored() {
        // The reason this is an error and not a warning: an operator who
        // mistypes --no-content believes content is withheld when it is not.
        let error = parse(&["--repo", "mesh=/a", "--no-contents"])
            .expect_err("a near-miss flag must not be swallowed");
        assert!(error.contains("unknown argument"), "{error}");
        assert!(error.contains("--no-content"), "{error}");
    }

    #[test]
    fn a_flag_without_its_value_is_reported_by_name() {
        let error = parse(&["--repo"]).expect_err("no value");
        assert!(error.contains("--repo requires a value"), "{error}");
    }

    #[test]
    fn limits_are_bounded_on_both_ends() {
        for (flag, value) in [
            ("--max-commits", "0"),
            ("--max-commits", "999999"),
            ("--max-patch-bytes", "16"),
            ("--max-patch-bytes", "99999999999"),
            ("--max-rename-candidates", "0"),
            ("--max-rename-candidates", "99999999"),
            ("--max-blame-lines", "0"),
            ("--max-blame-lines", "99999999"),
            ("--max-blame-file-bytes", "1"),
            ("--max-blame-file-bytes", "999999999999"),
            ("--max-scan-commits", "0"),
        ] {
            let error =
                parse(&["--repo", "mesh=/a", flag, value]).unwrap_err_or_else_message(flag, value);
            assert!(error.contains("must be between"), "{flag} {value}: {error}");
        }
    }

    #[test]
    fn non_numeric_limits_are_rejected() {
        let error =
            parse(&["--repo", "mesh=/a", "--max-patch-bytes", "256KiB"]).expect_err("not a number");
        assert!(error.contains("non-negative integer"), "{error}");
    }

    #[test]
    fn boolean_environment_values_accept_the_usual_spellings() {
        for (value, expected) in [
            ("1", true),
            ("true", true),
            ("ON", true),
            ("0", false),
            ("no", false),
            ("off", false),
        ] {
            let settings = parse_settings(
                args(&["--repo", "mesh=/a"]),
                |name| (name == ENV_REDACT_EMAILS).then(|| value.to_string()),
                true,
            )
            .expect("parses");
            assert_eq!(
                settings.disclosure.redact_emails, expected,
                "value {value:?}"
            );
        }

        let error = parse_settings(
            args(&["--repo", "mesh=/a"]),
            |name| (name == ENV_NO_CONTENT).then(|| "maybe".to_string()),
            true,
        )
        .expect_err("not a boolean");
        assert!(error.contains("1/0, true/false"), "{error}");
    }

    #[test]
    fn alias_rules_reject_traversal_shaped_and_leading_punctuation_names() {
        assert!(validate_alias("mesh").is_ok());
        assert!(validate_alias("tdcc-mesh").is_ok());
        assert!(validate_alias("repo_2").is_ok());

        for bad in ["", "..", "../etc", "-mesh", "_mesh", "me sh", "me/sh", "mé"] {
            assert!(
                validate_alias(bad).is_err(),
                "alias {bad:?} should be refused"
            );
        }
        assert!(validate_alias(&"a".repeat(MAX_ALIAS_LEN + 1)).is_err());
    }

    #[test]
    fn an_entry_without_an_equals_names_the_expected_shape() {
        let error =
            parse(&["--repo", "/srv/repos/tdcc-mesh"]).expect_err("a bare path is not alias=path");
        assert!(error.contains("expected alias=path"), "{error}");
    }

    #[test]
    fn an_empty_path_is_refused() {
        let error = parse(&["--repo", "mesh="]).expect_err("no path");
        assert!(error.contains("empty path"), "{error}");
    }

    /// Small helper so the bounds table above reads as a table.
    trait UnwrapErrMessage {
        fn unwrap_err_or_else_message(self, flag: &str, value: &str) -> String;
    }

    impl UnwrapErrMessage for Result<Settings, String> {
        fn unwrap_err_or_else_message(self, flag: &str, value: &str) -> String {
            match self {
                Ok(_) => panic!("{flag} {value} should have been refused"),
                Err(message) => message,
            }
        }
    }
}
