//! Where `docker-inspect` gets its configuration, and why none of it is
//! `[plugin.settings]`.
//!
//! `[plugin.settings]` values are stored by the host and rendered by the
//! console, but they are **never delivered to the plugin process** — there is
//! no settings field in the launch contract or the initialize handshake, and
//! only a web UI bundle can read them back. This plugin ships no web UI, and
//! every setting here is a limit that has to be enforced inside the process:
//! which containers are visible, how many log lines may leave the machine,
//! whether a TCP endpoint may be opened at all. A console control that looked
//! like it restricted those and did not would be worse than no control.
//!
//! So everything arrives through the two channels a plugin process can actually
//! receive: `[[plugin]].args` and the environment of the `tdcc` process.
//!
//! **An unknown flag or an out-of-range value is a startup error.** A typo in
//! `--container` would silently widen an allowlist from "one service" to "every
//! container on the machine", which is exactly the failure this plugin exists
//! to avoid.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::endpoint::{Endpoint, parse_endpoint};
use crate::visibility::{LabelSelector, NamePattern, Visibility};

pub const PLUGIN_NAME: &str = "docker-inspect";
pub const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Values read from the process environment, as a map so the parser stays a
/// pure function that tests can drive without touching real environment state.
pub type EnvMap = BTreeMap<String, String>;

/// The Docker API version every request is made against.
///
/// Pinned rather than left to the daemon's default so a future API change
/// cannot alter what this plugin sends. `v1.41` ships with Docker 20.10
/// (October 2020) and is accepted by everything newer; `--api-version` exists
/// for a daemon older than that, which reports its minimum in the error.
pub const DEFAULT_API_VERSION: &str = "v1.41";

pub const DEFAULT_TIMEOUT_SECS: u64 = 20;
pub const DEFAULT_MAX_RESPONSE_BYTES: u64 = 8 * 1024 * 1024;
pub const DEFAULT_MAX_LOG_LINES: u64 = 100;
pub const DEFAULT_MAX_LOG_BYTES: u64 = 256 * 1024;
pub const DEFAULT_MAX_LINE_CHARS: u64 = 2_000;
pub const DEFAULT_MAX_CONTAINERS: u64 = 200;
pub const DEFAULT_MAX_IMAGES: u64 = 200;
pub const DEFAULT_MAX_LABELS: u64 = 32;

pub const ENV_ENDPOINT: &str = "TDCC_DOCKER_INSPECT_ENDPOINT";
pub const ENV_API_VERSION: &str = "TDCC_DOCKER_INSPECT_API_VERSION";
pub const ENV_TIMEOUT_SECS: &str = "TDCC_DOCKER_INSPECT_TIMEOUT_SECS";
pub const ENV_MAX_RESPONSE_BYTES: &str = "TDCC_DOCKER_INSPECT_MAX_RESPONSE_BYTES";
pub const ENV_MAX_LOG_LINES: &str = "TDCC_DOCKER_INSPECT_MAX_LOG_LINES";
pub const ENV_MAX_LOG_BYTES: &str = "TDCC_DOCKER_INSPECT_MAX_LOG_BYTES";
pub const ENV_MAX_LINE_CHARS: &str = "TDCC_DOCKER_INSPECT_MAX_LINE_CHARS";
pub const ENV_MAX_CONTAINERS: &str = "TDCC_DOCKER_INSPECT_MAX_CONTAINERS";
pub const ENV_MAX_IMAGES: &str = "TDCC_DOCKER_INSPECT_MAX_IMAGES";
pub const ENV_MAX_LABELS: &str = "TDCC_DOCKER_INSPECT_MAX_LABELS";
pub const ENV_CONTAINERS: &str = "TDCC_DOCKER_INSPECT_CONTAINERS";
pub const ENV_LABELS: &str = "TDCC_DOCKER_INSPECT_LABELS";
pub const ENV_ALLOW_TCP: &str = "TDCC_DOCKER_INSPECT_ALLOW_TCP";
pub const ENV_SHOW_ENV: &str = "TDCC_DOCKER_INSPECT_SHOW_ENV";
/// Positive polarity: set it to `false` to do what `--no-logs` does.
pub const ENV_LOGS: &str = "TDCC_DOCKER_INSPECT_LOGS";
pub const ENV_ALL_IMAGES: &str = "TDCC_DOCKER_INSPECT_ALL_IMAGES";
/// Set by the host from `[[plugin]].url`.
pub const ENV_PLUGIN_URL: &str = "TDCC_PLUGIN_URL";
/// The variable every other Docker tool reads. Honoured last, because it is
/// ambient: it may have been exported for something else entirely.
pub const ENV_DOCKER_HOST: &str = "DOCKER_HOST";

const BOOL_FLAGS: &[&str] = &["--allow-tcp", "--show-env", "--no-logs", "--all-images"];
const VALUE_FLAGS: &[&str] = &[
    "--api-version",
    "--container",
    "--endpoint",
    "--label",
    "--max-containers",
    "--max-images",
    "--max-labels",
    "--max-line-chars",
    "--max-log-bytes",
    "--max-log-lines",
    "--max-response-bytes",
    "--timeout-secs",
];
/// Flags an operator may pass more than once. Everything else takes its last
/// occurrence, which is the ordinary command-line convention.
const REPEATABLE_FLAGS: &[&str] = &["--container", "--label"];

/// The caps a log read runs under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogLimits {
    /// `false` when the operator passed `--no-logs`; the tool then refuses and
    /// names the flag rather than returning an empty list.
    pub enabled: bool,
    pub max_lines: usize,
    pub max_bytes: usize,
    pub max_line_chars: usize,
}

/// Everything the plugin was configured with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub endpoint: Endpoint,
    /// Which setting the endpoint came from, for `status` and error messages.
    pub endpoint_source: String,
    pub api_version: String,
    pub timeout: Duration,
    pub max_response_bytes: usize,
    pub logs: LogLimits,
    pub max_containers: usize,
    pub max_images: usize,
    pub max_labels: usize,
    /// `--show-env`: include environment variable values in `inspect_container`.
    pub show_env: bool,
    /// `--all-images`: list every image even when a container filter is set.
    pub all_images: bool,
    pub visibility: Visibility,
}

impl Settings {
    /// Parse `[[plugin]].args` and the process environment.
    pub fn parse(args: &[String], env: &EnvMap) -> Result<Self, String> {
        let flags = parse_flags(args)?;

        let (endpoint, endpoint_source) = resolve_endpoint(&flags, env)?;
        endpoint.platform_support()?;

        let allow_tcp = toggle(&flags, env, "--allow-tcp", ENV_ALLOW_TCP, true)?;
        if endpoint.is_network() && !allow_tcp {
            return Err(format!(
                "{endpoint_source} is a TCP endpoint ({endpoint}), which docker-inspect will not \
                 open unless you pass `--allow-tcp`. Understand what you are enabling: a \
                 cleartext Docker endpoint has no authentication at all, so everyone who can \
                 reach that port can create containers on that machine, and anyone who can create \
                 a container can mount the host filesystem and become root on it. Prefer the \
                 local socket or pipe."
            ));
        }

        let visibility = Visibility {
            names: repeated(&flags, env, "--container", ENV_CONTAINERS)
                .into_iter()
                .map(NamePattern::new)
                .collect(),
            labels: repeated(&flags, env, "--label", ENV_LABELS)
                .into_iter()
                .map(|raw| LabelSelector::parse(&raw, "`--label`"))
                .collect::<Result<Vec<_>, _>>()?,
        };

        let logs = LogLimits {
            enabled: !toggle(&flags, env, "--no-logs", ENV_LOGS, false)?,
            max_lines: number(
                &flags,
                env,
                "--max-log-lines",
                ENV_MAX_LOG_LINES,
                DEFAULT_MAX_LOG_LINES,
                1,
                5_000,
            )? as usize,
            max_bytes: number(
                &flags,
                env,
                "--max-log-bytes",
                ENV_MAX_LOG_BYTES,
                DEFAULT_MAX_LOG_BYTES,
                4_096,
                8 * 1024 * 1024,
            )? as usize,
            max_line_chars: number(
                &flags,
                env,
                "--max-line-chars",
                ENV_MAX_LINE_CHARS,
                DEFAULT_MAX_LINE_CHARS,
                80,
                20_000,
            )? as usize,
        };

        Ok(Self {
            endpoint,
            endpoint_source,
            api_version: resolve_api_version(&flags, env)?,
            timeout: Duration::from_secs(number(
                &flags,
                env,
                "--timeout-secs",
                ENV_TIMEOUT_SECS,
                DEFAULT_TIMEOUT_SECS,
                1,
                300,
            )?),
            max_response_bytes: number(
                &flags,
                env,
                "--max-response-bytes",
                ENV_MAX_RESPONSE_BYTES,
                DEFAULT_MAX_RESPONSE_BYTES,
                64 * 1024,
                128 * 1024 * 1024,
            )? as usize,
            logs,
            max_containers: number(
                &flags,
                env,
                "--max-containers",
                ENV_MAX_CONTAINERS,
                DEFAULT_MAX_CONTAINERS,
                1,
                1_000,
            )? as usize,
            max_images: number(
                &flags,
                env,
                "--max-images",
                ENV_MAX_IMAGES,
                DEFAULT_MAX_IMAGES,
                1,
                1_000,
            )? as usize,
            max_labels: number(
                &flags,
                env,
                "--max-labels",
                ENV_MAX_LABELS,
                DEFAULT_MAX_LABELS,
                0,
                200,
            )? as usize,
            show_env: toggle(&flags, env, "--show-env", ENV_SHOW_ENV, true)?,
            all_images: toggle(&flags, env, "--all-images", ENV_ALL_IMAGES, true)?,
            visibility,
        })
    }

    /// Read the real process arguments and environment.
    pub fn from_process() -> Result<Self, String> {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let env: EnvMap = std::env::vars().collect();
        Self::parse(&args, &env)
    }

    /// The `User-Agent` the daemon will log. Truthful: it names the software
    /// actually making the request and its version.
    pub fn user_agent(&self) -> String {
        format!("tdcc-{PLUGIN_NAME}/{PLUGIN_VERSION}")
    }

    /// One line for the host's health check and the startup banner.
    pub fn summary(&self) -> String {
        format!(
            "read-only Docker API at {} ({}); showing {}; logs {}",
            self.endpoint,
            self.endpoint.kind(),
            self.visibility.describe(),
            if self.logs.enabled {
                format!("capped at {} lines", self.logs.max_lines)
            } else {
                "disabled by --no-logs".to_string()
            }
        )
    }
}

/// Endpoint precedence: the plugin's own flag, then its own environment
/// variable, then `[[plugin]].url`, then the ambient `DOCKER_HOST`, then the
/// platform default. The plugin-specific settings come first because
/// `DOCKER_HOST` may have been exported for something else entirely.
fn resolve_endpoint(flags: &Flags, env: &EnvMap) -> Result<(Endpoint, String), String> {
    let candidate = value(flags, env, "--endpoint", ENV_ENDPOINT)
        .or_else(|| {
            env_value(env, ENV_PLUGIN_URL).map(|(v, n)| (v, format!("`{n}` ([[plugin]].url)")))
        })
        .or_else(|| env_value(env, ENV_DOCKER_HOST).map(|(v, n)| (v, format!("`{n}`"))));

    match candidate {
        Some((raw, source)) => Ok((parse_endpoint(&raw, &source)?, source)),
        None => Ok((
            Endpoint::platform_default(),
            "the platform default".to_string(),
        )),
    }
}

/// Validate the API version prefix before it goes into every request path.
fn resolve_api_version(flags: &Flags, env: &EnvMap) -> Result<String, String> {
    let Some((raw, source)) = value(flags, env, "--api-version", ENV_API_VERSION) else {
        return Ok(DEFAULT_API_VERSION.to_string());
    };
    let candidate = raw.trim();
    let well_formed = candidate.starts_with('v')
        && candidate.len() > 1
        && candidate[1..]
            .chars()
            .all(|character| character.is_ascii_digit() || character == '.')
        && candidate.matches('.').count() <= 1;
    if !well_formed {
        return Err(format!(
            "{source} is `{candidate}`, which is not a Docker API version. Write it as `v1.41`."
        ));
    }
    Ok(candidate.to_string())
}

/// Parsed `[[plugin]].args`, keeping every occurrence so a repeatable flag can
/// be given more than once.
#[derive(Debug, Default)]
struct Flags {
    values: Vec<(String, String)>,
}

impl Flags {
    fn last(&self, name: &str) -> Option<&str> {
        self.values
            .iter()
            .rev()
            .find(|(flag, _)| flag == name)
            .map(|(_, value)| value.as_str())
    }

    fn all(&self, name: &str) -> Vec<String> {
        self.values
            .iter()
            .filter(|(flag, _)| flag == name)
            .map(|(_, value)| value.clone())
            .collect()
    }
}

/// Accepts `--flag value`, `--flag=value`, and bare boolean flags. An unknown
/// flag is a hard error rather than a warning.
fn parse_flags(args: &[String]) -> Result<Flags, String> {
    let mut flags = Flags::default();
    let mut index = 0;

    while index < args.len() {
        let argument = args[index].as_str();
        let (name, inline) = match argument.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (argument, None),
        };

        if BOOL_FLAGS.contains(&name) {
            let value = match inline {
                Some(value) => parse_bool(&value)
                    .ok_or_else(|| format!("`{name}` expects true or false, got `{value}`"))?,
                None => true,
            };
            flags.values.push((name.to_string(), value.to_string()));
            index += 1;
        } else if VALUE_FLAGS.contains(&name) {
            let value = match inline {
                Some(value) => value,
                None => {
                    index += 1;
                    args.get(index)
                        .cloned()
                        .ok_or_else(|| format!("`{name}` expects a value"))?
                }
            };
            if !REPEATABLE_FLAGS.contains(&name) && flags.last(name).is_some() {
                return Err(format!(
                    "`{name}` was given more than once. Only {} may be repeated.",
                    REPEATABLE_FLAGS.join(" and ")
                ));
            }
            flags.values.push((name.to_string(), value));
            index += 1;
        } else {
            return Err(format!(
                "unknown option `{argument}`. Supported: {}, {}.",
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
/// from so an error can point at what the operator actually wrote.
fn value(flags: &Flags, env: &EnvMap, flag: &str, var: &str) -> Option<(String, String)> {
    flags
        .last(flag)
        .map(|value| (value.to_string(), format!("`{flag}`")))
        .or_else(|| env_value(env, var).map(|(value, name)| (value, format!("`{name}`"))))
}

/// Every occurrence of a repeatable flag, or the comma-separated environment
/// variable when the flag was not given at all.
fn repeated(flags: &Flags, env: &EnvMap, flag: &str, var: &str) -> Vec<String> {
    let from_flags = flags.all(flag);
    if !from_flags.is_empty() {
        return from_flags;
    }
    env_value(env, var)
        .map(|(value, _)| {
            value
                .split(',')
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
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

/// Read a boolean whose flag and environment variable may have opposite
/// polarity. `env_means` is the value of the variable that corresponds to the
/// flag being present — `--no-logs` matches `TDCC_DOCKER_INSPECT_LOGS=false`.
fn toggle(
    flags: &Flags,
    env: &EnvMap,
    flag: &str,
    var: &str,
    env_means: bool,
) -> Result<bool, String> {
    if let Some(raw) = flags.last(flag) {
        return parse_bool(raw).ok_or_else(|| format!("`{flag}` expects true or false: {raw}"));
    }
    match env_value(env, var) {
        Some((raw, name)) => {
            let parsed =
                parse_bool(&raw).ok_or_else(|| format!("`{name}` expects true or false: {raw}"))?;
            Ok(parsed == env_means)
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

    fn parse(values: &[&str], pairs: &[(&str, &str)]) -> Result<Settings, String> {
        Settings::parse(&args(values), &env(pairs))
    }

    /// A local endpoint this build can actually open, written the way an
    /// operator would write it, plus the value it should parse to.
    ///
    /// The endpoint tests are about precedence and reporting, not about which
    /// platform they run on, and a Unix socket is refused outright by a Windows
    /// build (and a named pipe by a Unix one) before precedence is even
    /// reached.
    fn local(name: &str) -> (String, Endpoint) {
        if cfg!(windows) {
            (
                format!("npipe:////./pipe/{name}"),
                Endpoint::NamedPipe(format!(r"\\.\pipe\{name}")),
            )
        } else {
            (
                format!("unix:///tmp/{name}.sock"),
                Endpoint::Unix(format!("/tmp/{name}.sock").into()),
            )
        }
    }

    #[test]
    fn the_defaults_are_the_local_socket_every_guard_on_and_nothing_widened() {
        let settings = parse(&[], &[]).expect("defaults parse");

        assert_eq!(settings.endpoint, Endpoint::platform_default());
        assert_eq!(settings.api_version, DEFAULT_API_VERSION);
        assert!(
            !settings.show_env,
            "environment values are hidden by default"
        );
        assert!(!settings.all_images);
        assert!(settings.logs.enabled);
        assert_eq!(settings.logs.max_lines, 100);
        assert!(!settings.visibility.is_filtered());
        assert_eq!(settings.timeout, Duration::from_secs(20));
    }

    #[test]
    fn endpoint_precedence_is_flag_then_variable_then_plugin_url_then_docker_host() {
        let (flag, from_flag) = local("flag");
        let (variable, from_variable) = local("variable");
        let (url, from_url) = local("url");
        let (ambient, from_ambient) = local("ambient");
        let every_channel = [
            (ENV_ENDPOINT, variable.as_str()),
            (ENV_PLUGIN_URL, url.as_str()),
            (ENV_DOCKER_HOST, ambient.as_str()),
        ];

        let all_four = parse(&["--endpoint", &flag], &every_channel).expect("parses");
        assert_eq!(all_four.endpoint, from_flag);

        let without_flag = parse(&[], &every_channel).expect("parses");
        assert_eq!(without_flag.endpoint, from_variable);

        let url_and_ambient = parse(
            &[],
            &[
                (ENV_PLUGIN_URL, url.as_str()),
                (ENV_DOCKER_HOST, ambient.as_str()),
            ],
        )
        .expect("parses");
        assert_eq!(url_and_ambient.endpoint, from_url);
        assert!(url_and_ambient.endpoint_source.contains("[[plugin]].url"));

        let ambient_only = parse(&[], &[(ENV_DOCKER_HOST, ambient.as_str())]).expect("parses");
        assert_eq!(ambient_only.endpoint, from_ambient);
        assert!(ambient_only.endpoint_source.contains(ENV_DOCKER_HOST));
    }

    #[test]
    fn the_wrong_local_transport_for_this_build_is_refused_at_startup() {
        let wrong = if cfg!(windows) {
            "unix:///var/run/docker.sock"
        } else {
            "npipe:////./pipe/docker_engine"
        };

        let error = parse(&["--endpoint", wrong], &[]).expect_err("this build cannot open that");

        assert!(error.contains("cannot be opened"), "{error}");
        assert!(error.contains("--allow-tcp"), "{error}");
    }

    #[test]
    fn a_tcp_endpoint_is_refused_until_the_operator_says_so_and_the_refusal_explains_why() {
        let error = parse(&["--endpoint", "tcp://10.0.0.5:2375"], &[])
            .expect_err("TCP needs an explicit opt-in");

        assert!(error.contains("--allow-tcp"), "{error}");
        assert!(error.contains("no authentication"), "{error}");
        assert!(error.contains("root"), "{error}");

        let allowed = parse(&["--endpoint", "tcp://10.0.0.5:2375", "--allow-tcp"], &[])
            .expect("an explicit opt-in is honoured");
        assert!(allowed.endpoint.is_network());
    }

    #[test]
    fn an_ambient_docker_host_pointing_at_tcp_does_not_quietly_enable_network_access() {
        let error = parse(&[], &[(ENV_DOCKER_HOST, "tcp://10.0.0.5:2375")])
            .expect_err("an inherited DOCKER_HOST is still an opt-in");

        assert!(error.contains("--allow-tcp"), "{error}");
        assert!(error.contains("DOCKER_HOST"), "{error}");
    }

    #[test]
    fn container_and_label_filters_may_be_repeated() {
        let settings = parse(
            &[
                "--container",
                "tdcc-*",
                "--container",
                "web",
                "--label",
                "com.example.expose=true",
            ],
            &[],
        )
        .expect("parses");

        assert_eq!(settings.visibility.names.len(), 2);
        assert_eq!(settings.visibility.labels.len(), 1);
        assert!(settings.visibility.is_filtered());
    }

    #[test]
    fn the_filter_environment_variables_are_comma_separated() {
        let settings = parse(
            &[],
            &[
                (ENV_CONTAINERS, "tdcc-*, web ,"),
                (ENV_LABELS, "role=mesh,com.example.expose"),
            ],
        )
        .expect("parses");

        assert_eq!(settings.visibility.names.len(), 2);
        assert_eq!(settings.visibility.labels.len(), 2);
        assert_eq!(settings.visibility.labels[0].key, "role");
        assert_eq!(settings.visibility.labels[1].value, None);
    }

    #[test]
    fn a_flag_filter_replaces_the_environment_one_rather_than_adding_to_it() {
        let settings =
            parse(&["--container", "web"], &[(ENV_CONTAINERS, "everything-*")]).expect("parses");

        assert_eq!(settings.visibility.names.len(), 1);
        assert_eq!(settings.visibility.names[0].as_str(), "web");
    }

    #[test]
    fn a_malformed_label_selector_is_a_startup_error() {
        let error = parse(&["--label", "=true"], &[]).expect_err("an empty key is rejected");
        assert!(error.contains("--label"), "{error}");
    }

    #[test]
    fn logs_can_be_turned_off_entirely_from_either_channel() {
        assert!(!parse(&["--no-logs"], &[]).expect("parses").logs.enabled);
        assert!(
            !parse(&[], &[(ENV_LOGS, "false")])
                .expect("parses")
                .logs
                .enabled
        );
        assert!(
            parse(&[], &[(ENV_LOGS, "true")])
                .expect("parses")
                .logs
                .enabled
        );
    }

    #[test]
    fn a_misspelled_option_is_an_error_rather_than_a_silently_wider_allowlist() {
        let error = parse(&["--containers", "web"], &[]).expect_err("unknown flags are rejected");

        assert!(error.contains("unknown option"), "{error}");
        assert!(error.contains("--container"), "{error}");
    }

    #[test]
    fn a_single_valued_flag_given_twice_is_an_error_rather_than_a_silent_winner() {
        let error = parse(&["--max-log-lines", "10", "--max-log-lines", "5000"], &[])
            .expect_err("repeating a single-valued flag is rejected");

        assert!(error.contains("more than once"), "{error}");
    }

    #[test]
    fn out_of_range_and_unparseable_numbers_name_where_they_came_from() {
        let error =
            parse(&["--max-log-lines", "1000000"], &[]).expect_err("the line cap has a ceiling");
        assert!(error.contains("--max-log-lines"), "{error}");
        assert!(error.contains("between 1 and 5000"), "{error}");

        let error = parse(&[], &[(ENV_TIMEOUT_SECS, "soon")]).expect_err("a timeout must parse");
        assert!(error.contains(ENV_TIMEOUT_SECS), "{error}");
    }

    #[test]
    fn the_api_version_must_look_like_one() {
        assert_eq!(
            parse(&["--api-version", "v1.44"], &[])
                .expect("parses")
                .api_version,
            "v1.44"
        );

        for bad in ["1.41", "v1.41/containers", "latest", "v", "v1.4.1"] {
            let error = parse(&["--api-version", bad], &[])
                .unwrap_err_or_panic("a malformed API version is rejected");
            assert!(error.contains("v1.41"), "{bad}: {error}");
        }
    }

    #[test]
    fn both_argument_forms_are_accepted() {
        let spaced = parse(&["--max-log-lines", "42"], &[]).expect("parses");
        let inline = parse(&["--max-log-lines=42"], &[]).expect("parses");

        assert_eq!(spaced.logs.max_lines, 42);
        assert_eq!(inline.logs.max_lines, 42);
    }

    #[test]
    fn the_summary_names_the_endpoint_the_filter_and_the_log_state() {
        let (written, expected) = local("docker");
        let settings = parse(&["--endpoint", &written, "--container", "web"], &[]).expect("parses");

        let summary = settings.summary();
        assert!(summary.contains(&expected.to_string()), "{summary}");
        assert!(summary.contains("names matching web"), "{summary}");
        assert!(summary.contains("100 lines"), "{summary}");

        let quiet = parse(&["--no-logs"], &[]).expect("parses");
        assert!(quiet.summary().contains("disabled by --no-logs"));
    }

    #[test]
    fn the_user_agent_names_this_software_and_its_version() {
        let settings = parse(&[], &[]).expect("parses");
        assert_eq!(
            settings.user_agent(),
            format!("tdcc-docker-inspect/{PLUGIN_VERSION}")
        );
    }

    /// Small helper so a loop can assert on several rejected values without a
    /// `Settings` needing `Debug` in the failure message.
    trait UnwrapErrOrPanic {
        fn unwrap_err_or_panic(self, message: &str) -> String;
    }

    impl UnwrapErrOrPanic for Result<Settings, String> {
        fn unwrap_err_or_panic(self, message: &str) -> String {
            match self {
                Ok(_) => panic!("{message}"),
                Err(error) => error,
            }
        }
    }
}
