//! Startup options.
//!
//! The endpoint declaration lives in a file, and the path to that file arrives
//! through `[[plugin]].args` or the environment — never through
//! `[plugin.settings]`, which the host stores but never delivers to a plugin
//! process. See "Why the declaration is a file" in README.md.
//!
//! `[[plugin]].url` is deliberately **not** read. Seven of the plugins in this
//! repository use it and mean four different things by it; here it could only
//! mean "the one API", which would quietly contradict the file that declares
//! several. An operator who sets it gets no behaviour change and no surprise.
//!
//! Parsing is a pure function over an argument list and a captured environment,
//! so every rule below is tested without touching real process state.

use std::path::PathBuf;

pub const CONFIG_ENV: &str = "TDCC_REST_CLIENT_CONFIG";
pub const CONTACT_ENV: &str = "TDCC_REST_CLIENT_CONTACT";

/// Path appended to the operator's home directory when nothing says where the
/// declaration lives.
pub const DEFAULT_RELATIVE_PATH: &str = ".tdcc/rest-client.toml";

pub const USAGE: &str = "\
rest-client — call APIs a node's operator declared, from a model

The host normally starts this binary with no arguments. Running it by hand
outside a host exits with 'TDCC_PLUGIN_ENDPOINT is not set for plugin process',
which is correct: the host owns the control endpoint.

Options:
  --config <path>            Endpoint declaration to load.
                             Default: $HOME/.tdcc/rest-client.toml
                             (%USERPROFILE% on Windows).
  --contact <email or url>   Appended to the User-Agent this plugin sends, so a
                             remote API owner has somebody to reach.
  --print-package-manifest   Print plugin-manifest.json and exit.
  --help                     Print this text and exit.

Environment (arguments win over environment):
  TDCC_REST_CLIENT_CONFIG    same as --config
  TDCC_REST_CLIENT_CONTACT   same as --contact

Credentials are never read from here. Each endpoint's [endpoint.auth] table
names an environment variable, and that variable is read from the environment
of the tdcc process.
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
    pub config_path: PathBuf,
    pub contact: Option<String>,
}

/// The pieces of the process environment this module reads, captured so
/// argument parsing stays a pure function in tests.
#[derive(Debug, Clone, Default)]
pub struct Environment {
    pub config: Option<String>,
    pub contact: Option<String>,
    pub home: Option<String>,
}

impl Environment {
    pub fn from_process() -> Self {
        Self {
            config: std::env::var(CONFIG_ENV).ok(),
            contact: std::env::var(CONTACT_ENV).ok(),
            // `USERPROFILE` first so a Unix-style `HOME` set by a shell inside
            // Windows does not send the declaration somewhere the operator will
            // not find it.
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
    let mut config = environment.config.clone();
    let mut contact = environment.contact.clone();

    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--help" | "-h" => command = Command::Help,
            "--print-package-manifest" => command = Command::PrintPackageManifest,
            _ => {
                if let Some(value) = value_of("--config", &argument, &mut arguments)? {
                    config = Some(value);
                } else if let Some(value) = value_of("--contact", &argument, &mut arguments)? {
                    contact = Some(value);
                } else {
                    // An unknown flag is a hard error, not a warning. A typo in
                    // `--config` that was quietly ignored would leave a node
                    // running against the wrong declaration — possibly an empty
                    // one, possibly a stale one.
                    return Err(format!(
                        "unknown option: {argument}. Run with --help for the supported options."
                    ));
                }
            }
        }
    }

    let config_path = match config {
        Some(value) if !value.trim().is_empty() => PathBuf::from(value.trim()),
        _ => default_config_path(environment)?,
    };

    Ok(Options {
        command,
        config_path,
        contact: contact
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty()),
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

fn default_config_path(environment: &Environment) -> Result<PathBuf, String> {
    // Guessing a path relative to the working directory would put the file
    // somewhere different depending on how the host was started, which is a
    // terrible property for the file that decides which APIs a machine will
    // call.
    let home = environment.home.as_deref().ok_or_else(|| {
        format!(
            "cannot determine a home directory for the default declaration path; pass \
             --config <path> or set {CONFIG_ENV}"
        )
    })?;
    let mut path = PathBuf::from(home);
    path.push(DEFAULT_RELATIVE_PATH);
    Ok(path)
}

/// A truthful `User-Agent`: it names this software and its version, and where
/// to find out what it is. An operator may append a contact so an API owner
/// seeing unfamiliar traffic has somebody to reach.
pub fn user_agent(contact: Option<&str>) -> String {
    let base = format!(
        "{}/{} (+{})",
        crate::PRODUCT_TOKEN,
        crate::PLUGIN_VERSION,
        crate::PRODUCT_URL
    );
    match contact.map(str::trim).filter(|contact| !contact.is_empty()) {
        Some(contact) => format!("{base} contact/{contact}"),
        None => base,
    }
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
    fn no_arguments_means_run_against_the_default_path() {
        let options = parse_args(&[], &environment()).expect("defaults are valid");

        assert_eq!(options.command, Command::Run);
        assert_eq!(
            options.config_path,
            PathBuf::from("/home/operator").join(DEFAULT_RELATIVE_PATH)
        );
        assert_eq!(options.contact, None);
    }

    #[test]
    fn both_flag_spellings_are_accepted() {
        let spaced = parse_args(&["--config", "/etc/rest.toml"], &environment()).expect("valid");
        let joined = parse_args(&["--config=/etc/rest.toml"], &environment()).expect("valid");

        assert_eq!(spaced.config_path, PathBuf::from("/etc/rest.toml"));
        assert_eq!(joined.config_path, spaced.config_path);
    }

    #[test]
    fn arguments_win_over_the_environment() {
        let environment = Environment {
            config: Some("/from/env.toml".to_string()),
            contact: Some("env@example.org".to_string()),
            home: Some("/home/operator".to_string()),
        };

        let from_env = parse_args(&[], &environment).expect("valid");
        assert_eq!(from_env.config_path, PathBuf::from("/from/env.toml"));
        assert_eq!(from_env.contact.as_deref(), Some("env@example.org"));

        let overridden = parse_args(
            &[
                "--config",
                "/from/args.toml",
                "--contact",
                "ops@example.org",
            ],
            &environment,
        )
        .expect("valid");
        assert_eq!(overridden.config_path, PathBuf::from("/from/args.toml"));
        assert_eq!(overridden.contact.as_deref(), Some("ops@example.org"));
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
        let error = parse_args(&["--confg", "/x.toml"], &environment())
            .expect_err("a misspelled flag is an error");
        assert!(error.contains("unknown option"), "{error}");

        assert!(parse_args(&["--config"], &environment()).is_err());
        assert!(parse_args(&["--config="], &environment()).is_err());
        assert!(parse_args(&["--allow-anything"], &environment()).is_err());
    }

    #[test]
    fn a_missing_home_directory_asks_for_an_explicit_path() {
        let error = parse_args(&[], &Environment::default()).expect_err("no default is knowable");

        assert!(error.contains("--config"), "{error}");
        assert!(error.contains(CONFIG_ENV), "{error}");
    }

    #[test]
    fn a_blank_contact_is_treated_as_absent() {
        let options = parse_args(&["--contact", "   "], &environment()).expect("valid");
        assert_eq!(options.contact, None);
    }

    #[test]
    fn the_user_agent_names_the_software_and_optionally_a_contact() {
        let plain = user_agent(None);
        assert!(plain.starts_with(crate::PRODUCT_TOKEN), "{plain}");
        assert!(plain.contains(crate::PLUGIN_VERSION), "{plain}");
        assert!(plain.contains(crate::PRODUCT_URL), "{plain}");

        assert!(user_agent(Some("ops@example.org")).ends_with("contact/ops@example.org"));
        assert!(!user_agent(Some("  ")).contains("contact/"));
    }
}
