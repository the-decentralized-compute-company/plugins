//! Startup options.
//!
//! Policy lives in a file, and the path to that file arrives through
//! `[[plugin]].args` or the environment — never through `[plugin.settings]`,
//! which the host stores but never delivers to the plugin process. See
//! "Why the policy is a file" in README.md.

use std::path::PathBuf;

use crate::state::OnInvalidPolicy;

pub const POLICY_FILE_ENV: &str = "TDCC_WORKLOAD_POLICY_FILE";
pub const ON_INVALID_ENV: &str = "TDCC_WORKLOAD_POLICY_ON_INVALID";
/// Path appended to the operator's home directory when nothing else says where
/// the policy lives.
pub const DEFAULT_RELATIVE_PATH: &str = ".tdcc/workload-policy.toml";

pub const USAGE: &str = "\
workload-policy — node-side admission policy for TDCC

The host normally starts this binary with no arguments. Running it by hand
outside a host exits with 'TDCC_PLUGIN_ENDPOINT is not set for plugin process',
which is correct: the host owns the control endpoint.

Options:
  --policy <path>              Policy file to load.
                               Default: $HOME/.tdcc/workload-policy.toml
                               (%USERPROFILE% on Windows).
  --on-invalid-policy <what>   What to do when the policy file exists but does
                               not load: 'deny' (default, refuse every request)
                               or 'allow' (keep serving; your rules are NOT
                               applied). See README.md.
  --print-package-manifest     Print plugin-manifest.json and exit.
  --help                       Print this text and exit.

Environment (arguments win over environment):
  TDCC_WORKLOAD_POLICY_FILE        same as --policy
  TDCC_WORKLOAD_POLICY_ON_INVALID  same as --on-invalid-policy
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Connect to the host and serve the manifest.
    Run,
    /// Emit `plugin-manifest.json` for packaging.
    PrintPackageManifest,
    /// Print [`USAGE`].
    Help,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub command: Command,
    pub policy_path: PathBuf,
    pub on_invalid: OnInvalidPolicy,
}

/// The pieces of the process environment this plugin reads, captured so
/// argument parsing stays a pure function in tests.
#[derive(Debug, Clone, Default)]
pub struct Environment {
    pub policy_file: Option<String>,
    pub on_invalid: Option<String>,
    pub home: Option<String>,
}

impl Environment {
    pub fn from_process() -> Self {
        Self {
            policy_file: std::env::var(POLICY_FILE_ENV).ok(),
            on_invalid: std::env::var(ON_INVALID_ENV).ok(),
            // `USERPROFILE` first so a Unix-style `HOME` set by a shell inside
            // Windows does not send the policy somewhere the operator will not
            // find it.
            home: std::env::var("USERPROFILE")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .or_else(|| std::env::var("HOME").ok())
                .filter(|value| !value.trim().is_empty()),
        }
    }
}

pub fn parse<I>(arguments: I, environment: &Environment) -> Result<Options, String>
where
    I: IntoIterator<Item = String>,
{
    let mut command = Command::Run;
    let mut policy_file = environment.policy_file.clone();
    let mut on_invalid_raw = environment.on_invalid.clone();

    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--help" | "-h" => command = Command::Help,
            "--print-package-manifest" => command = Command::PrintPackageManifest,
            _ => {
                if let Some(value) = value_of("--policy", &argument, &mut arguments)? {
                    policy_file = Some(value);
                } else if let Some(value) =
                    value_of("--on-invalid-policy", &argument, &mut arguments)?
                {
                    on_invalid_raw = Some(value);
                } else {
                    return Err(format!("unknown option: {argument}"));
                }
            }
        }
    }

    let on_invalid = match on_invalid_raw.as_deref() {
        Some(value) => OnInvalidPolicy::parse(value)?,
        None => OnInvalidPolicy::Deny,
    };

    let policy_path = match policy_file {
        Some(value) if !value.trim().is_empty() => PathBuf::from(value.trim()),
        _ => default_policy_path(environment)?,
    };

    Ok(Options {
        command,
        policy_path,
        on_invalid,
    })
}

/// Accepts both `--flag value` and `--flag=value`. Returns `Ok(None)` when the
/// argument is not this flag at all, so the caller can try the next one.
fn value_of<I>(flag: &str, argument: &str, rest: &mut I) -> Result<Option<String>, String>
where
    I: Iterator<Item = String>,
{
    if argument == flag {
        return rest
            .next()
            .ok_or_else(|| format!("{flag} needs a value"))
            .map(Some);
    }
    if let Some(value) = argument.strip_prefix(&format!("{flag}=")) {
        if value.is_empty() {
            return Err(format!("{flag} needs a value"));
        }
        return Ok(Some(value.to_string()));
    }
    Ok(None)
}

fn default_policy_path(environment: &Environment) -> Result<PathBuf, String> {
    // Guessing a path relative to the working directory would put the file
    // somewhere different depending on how the host was started, which is a
    // terrible property for the file that decides what a machine accepts.
    let home = environment.home.as_deref().ok_or_else(|| {
        format!(
            "cannot determine a home directory for the default policy path; pass --policy <path> or set {POLICY_FILE_ENV}"
        )
    })?;
    let mut path = PathBuf::from(home);
    path.push(DEFAULT_RELATIVE_PATH);
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn environment() -> Environment {
        Environment {
            home: Some("/home/operator".to_string()),
            ..Environment::default()
        }
    }

    fn parse_args(arguments: &[&str], environment: &Environment) -> Result<Options, String> {
        parse(
            arguments.iter().map(|value| (*value).to_string()),
            environment,
        )
    }

    #[test]
    fn no_arguments_means_run_with_the_default_path_and_fail_closed() {
        let options = parse_args(&[], &environment()).expect("defaults are valid");

        assert_eq!(options.command, Command::Run);
        assert_eq!(options.on_invalid, OnInvalidPolicy::Deny);
        assert_eq!(
            options.policy_path,
            PathBuf::from("/home/operator").join(DEFAULT_RELATIVE_PATH)
        );
    }

    #[test]
    fn both_flag_spellings_are_accepted() {
        let spaced =
            parse_args(&["--policy", "/etc/policy.toml"], &environment()).expect("valid arguments");
        let joined =
            parse_args(&["--policy=/etc/policy.toml"], &environment()).expect("valid arguments");

        assert_eq!(spaced.policy_path, PathBuf::from("/etc/policy.toml"));
        assert_eq!(joined.policy_path, spaced.policy_path);
    }

    #[test]
    fn arguments_win_over_the_environment() {
        let environment = Environment {
            policy_file: Some("/from/env.toml".to_string()),
            on_invalid: Some("allow".to_string()),
            home: Some("/home/operator".to_string()),
        };

        let from_env = parse_args(&[], &environment).expect("valid");
        assert_eq!(from_env.policy_path, PathBuf::from("/from/env.toml"));
        assert_eq!(from_env.on_invalid, OnInvalidPolicy::Allow);

        let overridden = parse_args(
            &["--policy", "/from/args.toml", "--on-invalid-policy", "deny"],
            &environment,
        )
        .expect("valid");
        assert_eq!(overridden.policy_path, PathBuf::from("/from/args.toml"));
        assert_eq!(overridden.on_invalid, OnInvalidPolicy::Deny);
    }

    #[test]
    fn the_packaging_and_help_commands_are_recognized() {
        assert_eq!(
            parse_args(&["--print-package-manifest"], &environment())
                .expect("valid")
                .command,
            Command::PrintPackageManifest
        );
        assert_eq!(
            parse_args(&["--help"], &environment())
                .expect("valid")
                .command,
            Command::Help
        );
    }

    #[test]
    fn unknown_and_incomplete_options_are_rejected_rather_than_ignored() {
        assert!(parse_args(&["--policy"], &environment()).is_err());
        assert!(parse_args(&["--policy="], &environment()).is_err());
        assert!(parse_args(&["--enforce"], &environment()).is_err());
        assert!(parse_args(&["--on-invalid-policy", "sometimes"], &environment()).is_err());
    }

    #[test]
    fn a_missing_home_directory_asks_for_an_explicit_path() {
        let error = parse_args(&[], &Environment::default()).expect_err("no default is knowable");

        assert!(error.contains("--policy"), "{error}");
    }
}
