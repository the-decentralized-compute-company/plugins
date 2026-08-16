//! Startup options.
//!
//! There is exactly one setting here — where the server list lives — because
//! everything else about a bridged server (its command, its environment, its
//! timeouts, its allowlist) belongs beside that server in the file rather than
//! smeared across a command line.
//!
//! It arrives through `[[plugin]].args` or the environment of the `tdcc`
//! process, never through `[plugin.settings]`, which the host stores but never
//! delivers to a plugin process. See "Why the server list is a file" in
//! README.md.

use std::path::PathBuf;

pub const SERVERS_FILE_ENV: &str = "TDCC_MCP_BRIDGE_SERVERS";

/// Path appended to the operator's home directory when nothing else says where
/// the server list lives.
pub const DEFAULT_RELATIVE_PATH: &str = ".tdcc/mcp-bridge.toml";

pub const USAGE: &str = "\
mcp-bridge — expose third-party MCP servers through a TDCC node

The host normally starts this binary with no arguments. Running it by hand
outside a host exits with 'TDCC_PLUGIN_ENDPOINT is not set for plugin process',
which is correct: the host owns the control endpoint.

Every server this plugin launches or connects to must be listed in the server
file. Nothing is auto-discovered, and an empty or missing file bridges nothing.

Options:
  --servers <path>             Server list to load.
                               Default: $HOME/.tdcc/mcp-bridge.toml
                               (%USERPROFILE% on Windows).
  --check-config               Parse and validate the server list, print the
                               plan, and exit. Launches nothing and opens no
                               connection.
  --print-package-manifest     Print plugin-manifest.json and exit.
  --help                       Print this text and exit.

Environment (arguments win over environment):
  TDCC_MCP_BRIDGE_SERVERS      same as --servers

[[plugin]].url is deliberately not read: a server list is a file, not a URL.
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Connect to the host, discover upstream tools, and serve the manifest.
    Run,
    /// Validate the server list and print what would be bridged. Launches
    /// nothing.
    CheckConfig,
    /// Emit `plugin-manifest.json` for packaging.
    PrintPackageManifest,
    /// Print [`USAGE`].
    Help,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub command: Command,
    pub servers_path: PathBuf,
}

/// The pieces of the process environment this module reads, captured as a
/// struct so argument parsing stays a pure function in tests.
#[derive(Debug, Clone, Default)]
pub struct Environment {
    pub servers_file: Option<String>,
    pub home: Option<String>,
}

impl Environment {
    pub fn from_process() -> Self {
        Self {
            servers_file: std::env::var(SERVERS_FILE_ENV).ok(),
            // `USERPROFILE` first so a Unix-style `HOME` set by a shell inside
            // Windows does not send the default path somewhere the operator
            // will not find it.
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
    let mut servers_file = environment.servers_file.clone();

    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--help" | "-h" => command = Command::Help,
            "--check-config" => command = Command::CheckConfig,
            "--print-package-manifest" => command = Command::PrintPackageManifest,
            _ => match value_of("--servers", &argument, &mut arguments)? {
                Some(value) => servers_file = Some(value),
                // An unknown flag is a hard error rather than a warning: a typo
                // in `--servers` that was quietly ignored would bridge the
                // wrong file, or nothing at all, while looking configured.
                None => return Err(format!("unknown option: {argument}")),
            },
        }
    }

    let servers_path = match servers_file {
        Some(value) if !value.trim().is_empty() => PathBuf::from(value.trim()),
        _ => default_servers_path(environment)?,
    };

    Ok(Options {
        command,
        servers_path,
    })
}

/// Accepts both `--flag value` and `--flag=value`. Returns `Ok(None)` when the
/// argument is not this flag at all, so the caller can report it as unknown.
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

fn default_servers_path(environment: &Environment) -> Result<PathBuf, String> {
    // Guessing a path relative to the working directory would put the file
    // somewhere different depending on how the host was started, which is a
    // terrible property for the file that decides which third-party binaries
    // this node runs.
    let home = environment.home.as_deref().ok_or_else(|| {
        format!(
            "cannot determine a home directory for the default server list; pass --servers <path> \
             or set {SERVERS_FILE_ENV}"
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
    fn no_arguments_means_run_against_the_default_path() {
        let options = parse_args(&[], &environment()).expect("defaults are valid");

        assert_eq!(options.command, Command::Run);
        assert_eq!(
            options.servers_path,
            PathBuf::from("/home/operator").join(DEFAULT_RELATIVE_PATH)
        );
    }

    #[test]
    fn both_flag_spellings_are_accepted() {
        let spaced =
            parse_args(&["--servers", "/etc/mcp.toml"], &environment()).expect("valid arguments");
        let joined =
            parse_args(&["--servers=/etc/mcp.toml"], &environment()).expect("valid arguments");

        assert_eq!(spaced.servers_path, PathBuf::from("/etc/mcp.toml"));
        assert_eq!(joined.servers_path, spaced.servers_path);
    }

    #[test]
    fn arguments_win_over_the_environment() {
        let environment = Environment {
            servers_file: Some("/from/env.toml".to_string()),
            home: Some("/home/operator".to_string()),
        };

        let from_env = parse_args(&[], &environment).expect("valid");
        assert_eq!(from_env.servers_path, PathBuf::from("/from/env.toml"));

        let overridden =
            parse_args(&["--servers", "/from/args.toml"], &environment).expect("valid");
        assert_eq!(overridden.servers_path, PathBuf::from("/from/args.toml"));
    }

    #[test]
    fn the_side_commands_are_recognized() {
        for (argument, expected) in [
            ("--check-config", Command::CheckConfig),
            ("--print-package-manifest", Command::PrintPackageManifest),
            ("--help", Command::Help),
            ("-h", Command::Help),
        ] {
            assert_eq!(
                parse_args(&[argument], &environment())
                    .expect("valid")
                    .command,
                expected,
                "{argument}"
            );
        }
    }

    #[test]
    fn unknown_and_incomplete_options_are_rejected_rather_than_ignored() {
        assert!(parse_args(&["--servers"], &environment()).is_err());
        assert!(parse_args(&["--servers="], &environment()).is_err());
        assert!(parse_args(&["--serevrs", "/x.toml"], &environment()).is_err());
        assert!(parse_args(&["--allow-everything"], &environment()).is_err());
    }

    #[test]
    fn a_missing_home_directory_asks_for_an_explicit_path() {
        let error = parse_args(&[], &Environment::default()).expect_err("no default is knowable");

        assert!(error.contains("--servers"), "{error}");
        assert!(error.contains(SERVERS_FILE_ENV), "{error}");
    }

    #[test]
    fn the_usage_text_says_that_nothing_is_auto_discovered() {
        assert!(USAGE.contains("Nothing is auto-discovered"), "{USAGE}");
    }
}
