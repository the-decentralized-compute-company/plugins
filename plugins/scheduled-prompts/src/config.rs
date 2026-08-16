//! Process configuration: where the jobs file is, where state and output go,
//! and which endpoint the prompts are sent to.
//!
//! Deliberately **not** `[plugin.settings]`. Host-stored settings never reach a
//! plugin process — there is no settings field in the launch contract or the
//! initialize handshake, and only a web UI bundle can read them back. This
//! plugin ships no web UI, so a declared `config_schema` would draw console
//! controls that could not move a single job. Everything therefore arrives
//! through the two channels a plugin process actually has: `[[plugin]].args`
//! and the environment of the `tdcc` process.
//!
//! **The API key is environment-only, on purpose.** `args` is written into
//! `~/.tdcc/config.toml`, echoed back by `tdcc plugins info`, and visible in a
//! process listing on most systems. A credential belongs in none of those.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use reqwest::Url;

pub const PLUGIN_NAME: &str = "scheduled-prompts";
pub const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The node's own OpenAI-compatible API, which is where prompts go unless the
/// operator says otherwise.
pub const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:9337/v1";

/// How often the scheduler wakes to look for due jobs.
///
/// Cron resolution is one minute, so anything at or below 60 seconds fires on
/// time; 20 seconds keeps a job that becomes eligible mid-minute (a window
/// opening, a backoff expiring) from waiting long.
pub const DEFAULT_TICK_SECS: u64 = 20;
pub const MIN_TICK_SECS: u64 = 5;
pub const MAX_TICK_SECS: u64 = 300;

pub const ENV_JOBS_FILE: &str = "TDCC_SCHEDULED_PROMPTS_JOBS_FILE";
pub const ENV_STATE_DIR: &str = "TDCC_SCHEDULED_PROMPTS_STATE_DIR";
pub const ENV_OUTPUT_DIR: &str = "TDCC_SCHEDULED_PROMPTS_OUTPUT_DIR";
pub const ENV_ENDPOINT: &str = "TDCC_SCHEDULED_PROMPTS_ENDPOINT";
pub const ENV_TICK_SECS: &str = "TDCC_SCHEDULED_PROMPTS_TICK_SECS";
pub const ENV_ALLOW_REMOTE_ENDPOINT: &str = "TDCC_SCHEDULED_PROMPTS_ALLOW_REMOTE_ENDPOINT";
pub const ENV_API_KEY: &str = "TDCC_SCHEDULED_PROMPTS_API_KEY";

/// Set by the host from `[[plugin]].url`; accepted as the endpoint base.
pub const ENV_PLUGIN_URL: &str = "TDCC_PLUGIN_URL";
pub const ENV_PLUGIN_DIR: &str = "TDCC_PLUGIN_DIR";
pub const ENV_PLUGIN_NAME: &str = "TDCC_PLUGIN_NAME";

pub const USAGE: &str = "\
scheduled-prompts — run operator-declared prompts on a schedule.

The host launches this binary with no arguments beyond [[plugin]].args. Run it
outside a host and it exits with `TDCC_PLUGIN_ENDPOINT is not set`, which is
correct: the host owns the control endpoint.

Options (also settable in the environment of the tdcc process):

  --jobs <path>            Jobs file. Env: TDCC_SCHEDULED_PROMPTS_JOBS_FILE
                           Default: $HOME/.tdcc/scheduled-prompts.toml
  --state-dir <path>       Run history and cursors.
                           Env: TDCC_SCHEDULED_PROMPTS_STATE_DIR
  --output-dir <path>      Root every file sink is confined to.
                           Env: TDCC_SCHEDULED_PROMPTS_OUTPUT_DIR
                           Default: <state-dir>/out
  --endpoint <url>         OpenAI-compatible base URL.
                           Env: TDCC_SCHEDULED_PROMPTS_ENDPOINT, then
                           [[plugin]].url. Default: http://127.0.0.1:9337/v1
  --allow-remote-endpoint  Permit a non-loopback endpoint. Off by default.
                           Env: TDCC_SCHEDULED_PROMPTS_ALLOW_REMOTE_ENDPOINT
  --tick-secs <5-300>      Scheduler wake interval. Default: 20
  --help                   Print this text.

The endpoint API key is read from TDCC_SCHEDULED_PROMPTS_API_KEY in the
environment only. It is never accepted as an argument, because arguments are
stored in config.toml and echoed back by `tdcc plugins info`.

Jobs themselves are declared in the jobs file, never through a tool. See
README.md > \"Why a model cannot create a job\".
";

/// Values read from the process environment, as a map so the parser stays a
/// pure function tests can drive without touching real environment state.
pub type EnvMap = BTreeMap<String, String>;

/// An endpoint credential.
///
/// `Debug` is hand-written so an accidental `{:?}` — in a log line, a panic
/// message, an error context — can never print the key.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey(String);

impl ApiKey {
    pub fn header_value(&self) -> String {
        format!("Bearer {}", self.0)
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ApiKey(<redacted>)")
    }
}

/// What the operator asked the process to do.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    Run,
    Help,
    PrintPackageManifest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub command: Command,
    pub jobs_path: PathBuf,
    pub state_dir: PathBuf,
    pub output_dir: PathBuf,
    pub endpoint: Url,
    /// Where the endpoint came from, so an error can point at the thing the
    /// operator actually wrote.
    pub endpoint_source: String,
    pub endpoint_is_loopback: bool,
    pub tick_secs: u64,
    pub api_key: Option<ApiKey>,
}

impl Config {
    /// Read the real process arguments and environment.
    pub fn from_process() -> Result<Self, String> {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let env: EnvMap = std::env::vars().collect();
        Self::parse(&args, &env)
    }

    /// Parse `[[plugin]].args` on top of the environment.
    ///
    /// Precedence is **argument, then environment, then `[[plugin]].url`, then
    /// the built-in default**, and it is the same for every setting.
    ///
    /// An unknown flag or an out-of-range value is an error rather than a
    /// warning: a typo in `--allow-remote-endpoint` that was quietly ignored
    /// would leave an operator believing a guard was off when it was on.
    pub fn parse(args: &[String], env: &EnvMap) -> Result<Self, String> {
        let flags = parse_flags(args)?;

        let command = if flags.contains_key("--help") {
            Command::Help
        } else if flags.contains_key("--print-package-manifest") {
            Command::PrintPackageManifest
        } else {
            Command::Run
        };

        let state_dir = match value(&flags, env, "--state-dir", ENV_STATE_DIR) {
            Some((raw, _)) => PathBuf::from(raw),
            None => default_state_dir(env),
        };
        let jobs_path = match value(&flags, env, "--jobs", ENV_JOBS_FILE) {
            Some((raw, _)) => PathBuf::from(raw),
            None => tdcc_home(env).join("scheduled-prompts.toml"),
        };
        let output_dir = match value(&flags, env, "--output-dir", ENV_OUTPUT_DIR) {
            Some((raw, _)) => PathBuf::from(raw),
            None => state_dir.join("out"),
        };

        let tick_secs = number(
            &flags,
            env,
            "--tick-secs",
            ENV_TICK_SECS,
            DEFAULT_TICK_SECS,
            MIN_TICK_SECS,
            MAX_TICK_SECS,
        )?;

        let allow_remote = toggle(
            &flags,
            env,
            "--allow-remote-endpoint",
            ENV_ALLOW_REMOTE_ENDPOINT,
        )?;
        // `--help` and `--print-package-manifest` have to work on a machine
        // whose endpoint configuration is wrong — that is often exactly why
        // somebody is reading `--help` — and neither of them sends a request.
        // An unknown flag is still an error above, because that is a typo
        // either way.
        let configured = match command {
            Command::Run => value(&flags, env, "--endpoint", ENV_ENDPOINT).or_else(|| {
                env_value(env, ENV_PLUGIN_URL).map(|(raw, name)| (raw, format!("`{name}`")))
            }),
            Command::Help | Command::PrintPackageManifest => None,
        };
        let (endpoint, endpoint_source) = match configured {
            Some((raw, source)) => (parse_endpoint(&raw, &source)?, source),
            None => (
                Url::parse(DEFAULT_ENDPOINT).map_err(|error| {
                    format!("built-in endpoint {DEFAULT_ENDPOINT} is invalid: {error}")
                })?,
                "the built-in default".to_string(),
            ),
        };

        let endpoint_is_loopback = is_loopback(&endpoint);
        if !endpoint_is_loopback && !allow_remote {
            return Err(format!(
                "endpoint {endpoint} from {endpoint_source} is not on loopback. Every prompt in \
                 the jobs file would be sent off this machine. Pass \
                 `--allow-remote-endpoint` in [[plugin]].args, or set \
                 {ENV_ALLOW_REMOTE_ENDPOINT}=true, if that is what you meant."
            ));
        }

        Ok(Self {
            command,
            jobs_path,
            state_dir,
            output_dir,
            endpoint,
            endpoint_source,
            endpoint_is_loopback,
            tick_secs,
            api_key: env_value(env, ENV_API_KEY).map(|(raw, _)| ApiKey(raw)),
        })
    }

    /// The URL one chat completion is POSTed to.
    pub fn completions_url(&self) -> Url {
        let mut base = self.endpoint.as_str().trim_end_matches('/').to_string();
        base.push_str("/chat/completions");
        // The base was validated at parse time, so appending a fixed path
        // cannot make it unparseable.
        Url::parse(&base).unwrap_or_else(|_| self.endpoint.clone())
    }
}

/// `$HOME/.tdcc`, or `%USERPROFILE%\.tdcc` on Windows.
fn tdcc_home(env: &EnvMap) -> PathBuf {
    match env
        .get("HOME")
        .or_else(|| env.get("USERPROFILE"))
        .map(String::as_str)
        .map(str::trim)
        .filter(|home| !home.is_empty())
    {
        Some(home) => PathBuf::from(home).join(".tdcc"),
        // No home directory to anchor to. A relative path keeps the process
        // alive and keeps the files somewhere findable, and `status` reports
        // the resolved paths either way.
        None => PathBuf::from(".tdcc"),
    }
}

/// `<plugin store root>/<plugin name>/scheduled-prompts`.
///
/// Following `TDCC_PLUGIN_DIR` matters for development: pointing the store at a
/// scratch directory has to move this plugin's state too, or a test run
/// overwrites the operator's real run history.
fn default_state_dir(env: &EnvMap) -> PathBuf {
    let name = env
        .get(ENV_PLUGIN_NAME)
        .map(String::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(PLUGIN_NAME);
    let root = match env
        .get(ENV_PLUGIN_DIR)
        .map(String::as_str)
        .map(str::trim)
        .filter(|dir| !dir.is_empty())
    {
        Some(dir) => PathBuf::from(dir),
        None => tdcc_home(env).join("plugins"),
    };
    root.join(name).join("state")
}

/// Parse an endpoint base URL, refusing the shapes that would surprise.
fn parse_endpoint(raw: &str, source: &str) -> Result<Url, String> {
    let url = Url::parse(raw.trim())
        .map_err(|error| format!("{source} is not a valid URL ({error}): {raw}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "{source} must be an http or https URL, not `{}`",
            url.scheme()
        ));
    }
    if url.host_str().is_none() {
        return Err(format!("{source} has no host: {raw}"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(format!(
            "{source} carries credentials in the URL. Put the key in {ENV_API_KEY} in the \
             environment of the tdcc process instead."
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(format!(
            "{source} must not carry a query or fragment; this plugin appends \
             /chat/completions itself"
        ));
    }
    Ok(url)
}

/// Whether a URL names this machine.
///
/// `host_str` returns an IPv6 literal in brackets, and `localhost` is mapped
/// here rather than resolved so a hosts-file entry cannot decide the answer.
fn is_loopback(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let host = host.trim_start_matches('[').trim_end_matches(']');
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(address)) => address.is_loopback(),
        Ok(std::net::IpAddr::V6(address)) => {
            address.is_loopback()
                || address
                    .to_ipv4_mapped()
                    .is_some_and(|mapped| mapped.is_loopback())
        }
        Err(_) => false,
    }
}

const BOOL_FLAGS: &[&str] = &[
    "--allow-remote-endpoint",
    "--help",
    "--print-package-manifest",
];
const VALUE_FLAGS: &[&str] = &[
    "--endpoint",
    "--jobs",
    "--output-dir",
    "--state-dir",
    "--tick-secs",
];

type Flags = BTreeMap<String, String>;

/// Accepts `--flag value`, `--flag=value`, and bare boolean flags.
fn parse_flags(args: &[String]) -> Result<Flags, String> {
    let mut flags = Flags::new();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (arg, None),
        };

        if BOOL_FLAGS.contains(&name) {
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
        } else {
            return Err(format!(
                "unknown option `{arg}`. Supported: {}, {}.",
                VALUE_FLAGS.join(", "),
                BOOL_FLAGS.join(", ")
            ));
        }
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
/// from, so an error message can point at the thing the operator wrote.
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
        .parse()
        .map_err(|_| format!("{source} must be a whole number, got `{raw}`"))?;
    if parsed < min || parsed > max {
        return Err(format!(
            "{source} must be between {min} and {max}, got {parsed}"
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

    fn home() -> EnvMap {
        env(&[("HOME", "/home/tester")])
    }

    #[test]
    fn defaults_need_no_arguments_and_point_at_the_node_itself() {
        let config = Config::parse(&[], &home()).expect("defaults parse");

        assert_eq!(config.command, Command::Run);
        assert_eq!(config.endpoint.as_str(), "http://127.0.0.1:9337/v1");
        assert_eq!(
            config.completions_url().as_str(),
            "http://127.0.0.1:9337/v1/chat/completions"
        );
        assert!(config.endpoint_is_loopback);
        assert_eq!(config.tick_secs, DEFAULT_TICK_SECS);
        assert_eq!(
            config.jobs_path,
            PathBuf::from("/home/tester/.tdcc/scheduled-prompts.toml")
        );
        assert_eq!(
            config.state_dir,
            PathBuf::from("/home/tester/.tdcc/plugins/scheduled-prompts/state")
        );
        assert_eq!(config.output_dir, config.state_dir.join("out"));
        assert!(config.api_key.is_none());
    }

    #[test]
    fn the_plugin_store_root_moves_state_with_it() {
        let mut env = home();
        env.insert(ENV_PLUGIN_DIR.into(), "/scratch/store".into());
        env.insert(ENV_PLUGIN_NAME.into(), "scheduled-prompts".into());

        let config = Config::parse(&[], &env).expect("parses");

        assert_eq!(
            config.state_dir,
            PathBuf::from("/scratch/store/scheduled-prompts/state")
        );
    }

    #[test]
    fn precedence_is_argument_then_environment_then_plugin_url() {
        let mut env = home();
        env.insert(ENV_PLUGIN_URL.into(), "http://127.0.0.1:1111/v1".into());

        let from_url = Config::parse(&[], &env).expect("plugin url is accepted");
        assert_eq!(from_url.endpoint.as_str(), "http://127.0.0.1:1111/v1");

        env.insert(ENV_ENDPOINT.into(), "http://127.0.0.1:2222/v1".into());
        let from_env = Config::parse(&[], &env).expect("environment beats plugin url");
        assert_eq!(from_env.endpoint.as_str(), "http://127.0.0.1:2222/v1");

        let from_flag = Config::parse(&args(&["--endpoint", "http://127.0.0.1:3333/v1"]), &env)
            .expect("argument beats environment");
        assert_eq!(from_flag.endpoint.as_str(), "http://127.0.0.1:3333/v1");
    }

    #[test]
    fn a_remote_endpoint_is_refused_until_the_operator_opts_in() {
        let error = Config::parse(
            &args(&["--endpoint", "https://api.example.com/v1"]),
            &home(),
        )
        .expect_err("a remote endpoint needs an opt-in");

        assert!(error.contains("not on loopback"), "{error}");
        assert!(error.contains("--allow-remote-endpoint"), "{error}");

        let config = Config::parse(
            &args(&[
                "--endpoint",
                "https://api.example.com/v1",
                "--allow-remote-endpoint",
            ]),
            &home(),
        )
        .expect("the opt-in is honoured");
        assert!(!config.endpoint_is_loopback);
    }

    #[test]
    fn every_loopback_spelling_counts_as_loopback() {
        for raw in [
            "http://127.0.0.1:9337/v1",
            "http://localhost:9337/v1",
            "http://[::1]:9337/v1",
            "http://127.5.5.5:9337/v1",
        ] {
            let config =
                Config::parse(&args(&["--endpoint", raw]), &home()).unwrap_or_else(|error| {
                    panic!("{raw} should be accepted without an opt-in: {error}")
                });
            assert!(config.endpoint_is_loopback, "{raw}");
        }
    }

    #[test]
    fn an_endpoint_url_may_not_smuggle_a_credential_or_a_query() {
        for raw in [
            "http://user:pass@127.0.0.1:9337/v1",
            "http://127.0.0.1:9337/v1?key=abc",
            "http://127.0.0.1:9337/v1#frag",
            "file:///etc/passwd",
            "127.0.0.1:9337",
        ] {
            assert!(
                Config::parse(&args(&["--endpoint", raw]), &home()).is_err(),
                "{raw} should be refused"
            );
        }
    }

    #[test]
    fn the_api_key_comes_from_the_environment_only_and_never_prints() {
        let mut env = home();
        env.insert(ENV_API_KEY.into(), "super-secret".into());

        let config = Config::parse(&[], &env).expect("parses");

        let key = config.api_key.clone().expect("key is read");
        assert_eq!(key.header_value(), "Bearer super-secret");

        let rendered = format!("{config:?}");
        assert!(!rendered.contains("super-secret"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }

    #[test]
    fn a_misspelled_option_is_an_error_rather_than_a_silently_ignored_guard() {
        let error = Config::parse(&args(&["--allow-remote-endpiont"]), &home())
            .expect_err("unknown flags are rejected");

        assert!(error.contains("unknown option"), "{error}");
    }

    #[test]
    fn out_of_range_and_unparseable_numbers_name_where_they_came_from() {
        let error = Config::parse(&args(&["--tick-secs", "1"]), &home())
            .expect_err("below the floor is rejected");
        assert!(error.contains("--tick-secs"), "{error}");

        let mut env = home();
        env.insert(ENV_TICK_SECS.into(), "later".into());
        let error = Config::parse(&[], &env).expect_err("non-numeric is rejected");
        assert!(error.contains(ENV_TICK_SECS), "{error}");
    }

    #[test]
    fn help_and_packaging_work_even_when_the_endpoint_is_misconfigured() {
        // A machine whose endpoint is wrong is exactly where somebody reaches
        // for `--help`, and packaging must not depend on a node's settings.
        let broken = env(&[
            ("HOME", "/home/tester"),
            (ENV_ENDPOINT, "https://api.example.com/v1"),
        ]);

        assert_eq!(
            Config::parse(&args(&["--help"]), &broken)
                .expect("help needs no endpoint")
                .command,
            Command::Help
        );
        assert_eq!(
            Config::parse(&args(&["--print-package-manifest"]), &broken)
                .expect("packaging needs no endpoint")
                .command,
            Command::PrintPackageManifest
        );
        // The same environment is still refused for a real run.
        assert!(Config::parse(&[], &broken).is_err());
    }

    #[test]
    fn a_typo_is_still_an_error_on_the_help_and_packaging_paths() {
        assert!(Config::parse(&args(&["--help", "--nonsense"]), &home()).is_err());
    }

    #[test]
    fn the_usage_text_names_every_flag_it_accepts() {
        for flag in VALUE_FLAGS.iter().chain(BOOL_FLAGS.iter()) {
            if *flag == "--print-package-manifest" {
                continue;
            }
            assert!(USAGE.contains(flag), "{flag} is missing from --help");
        }
        assert!(USAGE.contains(ENV_API_KEY));
    }
}
