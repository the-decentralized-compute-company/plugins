//! Where `web-search` gets its settings, and why it is not `[plugin.settings]`.
//!
//! `[plugin.settings]` never reaches a plugin process. The host stores those
//! values and the console renders them, but there is no settings field in the
//! launch contract or the initialize handshake — only a web UI bundle can read
//! them back. This plugin has no web UI, so declaring a `config_schema` would
//! draw console controls whose values went nowhere.
//!
//! Everything therefore arrives the two ways a plugin process can actually
//! receive configuration: `[[plugin]].args` and the environment of the `tdcc`
//! process. `[[plugin]].url` is forwarded by the host as `TDCC_PLUGIN_URL` and
//! is accepted as a SearXNG base URL, which is the idiomatic use of that field.
//!
//! **The API key is environment-only, deliberately.** `args` is written into
//! `~/.tdcc/config.toml` and echoed back by `tdcc plugins info`; a credential
//! belongs in neither.

use std::collections::BTreeMap;
use std::time::Duration;

use reqwest::Url;

/// Values read from the process environment, as a map so the parser stays a
/// pure function that tests can drive without touching real environment state.
pub type EnvMap = BTreeMap<String, String>;

pub const PLUGIN_NAME: &str = "web-search";
pub const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The product token this plugin identifies itself with, in the `User-Agent`
/// header and when matching `User-agent:` groups in `robots.txt`.
pub const PRODUCT_TOKEN: &str = "tdcc-web-search";
pub const PRODUCT_URL: &str = "https://github.com/the-decentralized-compute-company/tdcc-plugins";

pub const DEFAULT_BRAVE_ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";
pub const DEFAULT_TIMEOUT_SECS: u64 = 20;
pub const DEFAULT_MAX_FETCH_BYTES: u64 = 2_000_000;
pub const DEFAULT_MAX_TEXT_CHARS: u64 = 40_000;
pub const DEFAULT_MAX_REDIRECTS: u32 = 5;
pub const DEFAULT_RESULT_COUNT: u32 = 8;
pub const MAX_RESULT_COUNT: u32 = 20;

pub const ENV_BACKEND: &str = "TDCC_WEB_SEARCH_BACKEND";
pub const ENV_BRAVE_API_KEY: &str = "TDCC_WEB_SEARCH_BRAVE_API_KEY";
pub const ENV_BRAVE_ENDPOINT: &str = "TDCC_WEB_SEARCH_BRAVE_ENDPOINT";
pub const ENV_SEARXNG_URL: &str = "TDCC_WEB_SEARCH_SEARXNG_URL";
pub const ENV_TIMEOUT_SECS: &str = "TDCC_WEB_SEARCH_TIMEOUT_SECS";
pub const ENV_MAX_FETCH_BYTES: &str = "TDCC_WEB_SEARCH_MAX_FETCH_BYTES";
pub const ENV_MAX_TEXT_CHARS: &str = "TDCC_WEB_SEARCH_MAX_TEXT_CHARS";
pub const ENV_MAX_REDIRECTS: &str = "TDCC_WEB_SEARCH_MAX_REDIRECTS";
pub const ENV_RESULTS: &str = "TDCC_WEB_SEARCH_RESULTS";
pub const ENV_CONTACT: &str = "TDCC_WEB_SEARCH_CONTACT";
pub const ENV_RESPECT_ROBOTS: &str = "TDCC_WEB_SEARCH_RESPECT_ROBOTS";
pub const ENV_ALLOW_PRIVATE_NETWORK: &str = "TDCC_WEB_SEARCH_ALLOW_PRIVATE_NETWORK";
/// Set by the host from `[[plugin]].url`; accepted as the SearXNG base URL.
pub const ENV_PLUGIN_URL: &str = "TDCC_PLUGIN_URL";

const BOOL_FLAGS: &[&str] = &["--ignore-robots", "--allow-private-network"];
const VALUE_FLAGS: &[&str] = &[
    "--backend",
    "--brave-endpoint",
    "--contact",
    "--max-fetch-bytes",
    "--max-redirects",
    "--max-text-chars",
    "--results",
    "--searxng-url",
    "--timeout-secs",
];

/// A resolved search backend.
///
/// `Debug` is hand-written so an accidental `{:?}` — in a log line, a panic
/// message, an error context — can never print the API key.
#[derive(Clone, PartialEq, Eq)]
pub enum SearchBackend {
    /// The Brave Search API. Key-based, hosted by Brave.
    Brave { endpoint: Url, api_key: String },
    /// A self-hosted SearXNG instance with the JSON output format enabled.
    Searxng { base: Url },
}

impl SearchBackend {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Brave { .. } => "brave",
            Self::Searxng { .. } => "searxng",
        }
    }
}

impl std::fmt::Debug for SearchBackend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Brave { endpoint, .. } => formatter
                .debug_struct("Brave")
                .field("endpoint", &endpoint.as_str())
                .field("api_key", &"<redacted>")
                .finish(),
            Self::Searxng { base } => formatter
                .debug_struct("Searxng")
                .field("base", &base.as_str())
                .finish(),
        }
    }
}

/// Whether `search` can run at all.
///
/// A missing backend is not a startup failure: `fetch` needs no backend and
/// stays useful, so the plugin starts, logs the problem once, and returns
/// [`SearchSetup::Unconfigured`]'s message from every `search` call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchSetup {
    Configured(SearchBackend),
    Unconfigured(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpConfig {
    pub user_agent: String,
    pub request_timeout: Duration,
    pub max_fetch_bytes: usize,
    pub max_redirects: u32,
    pub respect_robots: bool,
    pub allow_private_network: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Limits {
    pub default_result_count: u32,
    pub max_result_count: u32,
    pub max_text_chars: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub http: HttpConfig,
    pub search: SearchSetup,
    pub limits: Limits,
}

impl Config {
    /// Parse `[[plugin]].args` and the process environment into a config.
    ///
    /// Returns `Err` only for input the operator clearly got wrong — an unknown
    /// flag, an unparseable number, a malformed URL — because silently ignoring
    /// those is how a security setting ends up not applied. A *missing* search
    /// backend is not an error here; it lands in [`SearchSetup::Unconfigured`].
    pub fn parse(args: &[String], env: &EnvMap) -> Result<Self, String> {
        let flags = parse_flags(args)?;

        let contact = value(&flags, env, "--contact", ENV_CONTACT).map(|(value, _)| value);
        let http = HttpConfig {
            user_agent: user_agent(contact.as_deref()),
            request_timeout: Duration::from_secs(number(
                &flags,
                env,
                "--timeout-secs",
                ENV_TIMEOUT_SECS,
                DEFAULT_TIMEOUT_SECS,
                1,
                600,
            )?),
            max_fetch_bytes: number(
                &flags,
                env,
                "--max-fetch-bytes",
                ENV_MAX_FETCH_BYTES,
                DEFAULT_MAX_FETCH_BYTES,
                1_024,
                64 * 1_024 * 1_024,
            )? as usize,
            max_redirects: number(
                &flags,
                env,
                "--max-redirects",
                ENV_MAX_REDIRECTS,
                u64::from(DEFAULT_MAX_REDIRECTS),
                0,
                10,
            )? as u32,
            respect_robots: !toggle(&flags, env, "--ignore-robots", ENV_RESPECT_ROBOTS, false)?,
            allow_private_network: toggle(
                &flags,
                env,
                "--allow-private-network",
                ENV_ALLOW_PRIVATE_NETWORK,
                true,
            )?,
        };

        let default_result_count = number(
            &flags,
            env,
            "--results",
            ENV_RESULTS,
            u64::from(DEFAULT_RESULT_COUNT),
            1,
            u64::from(MAX_RESULT_COUNT),
        )? as u32;

        Ok(Self {
            http,
            search: resolve_backend(&flags, env)?,
            limits: Limits {
                default_result_count,
                max_result_count: MAX_RESULT_COUNT,
                max_text_chars: number(
                    &flags,
                    env,
                    "--max-text-chars",
                    ENV_MAX_TEXT_CHARS,
                    DEFAULT_MAX_TEXT_CHARS,
                    500,
                    1_000_000,
                )? as usize,
            },
        })
    }

    /// Read the real process arguments and environment.
    pub fn from_process() -> Result<Self, String> {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let env: EnvMap = std::env::vars().collect();
        Self::parse(&args, &env)
    }
}

/// A truthful `User-Agent`: it names the software actually making the request,
/// its version, and where to find out what it is. Operators may append a
/// contact so a site owner has somebody to reach.
pub fn user_agent(contact: Option<&str>) -> String {
    let base = format!("{PRODUCT_TOKEN}/{PLUGIN_VERSION} (+{PRODUCT_URL})");
    match contact.map(str::trim).filter(|contact| !contact.is_empty()) {
        Some(contact) => format!("{base} contact/{contact}"),
        None => base,
    }
}

fn resolve_backend(flags: &Flags, env: &EnvMap) -> Result<SearchSetup, String> {
    let searxng_url = value(flags, env, "--searxng-url", ENV_SEARXNG_URL)
        .or_else(|| env_value(env, ENV_PLUGIN_URL));
    let brave_key = env_value(env, ENV_BRAVE_API_KEY);

    let selected = match value(flags, env, "--backend", ENV_BACKEND) {
        Some((raw, source)) => match raw.trim().to_ascii_lowercase().as_str() {
            "brave" => Some("brave"),
            "searxng" | "searx" => Some("searxng"),
            other => {
                return Err(format!(
                    "{source} is `{other}`, which is not a known backend. Use `brave` or `searxng`."
                ));
            }
        },
        // Nothing explicit: infer, but only when the answer is unambiguous.
        None => match (&brave_key, &searxng_url) {
            (Some(_), None) => Some("brave"),
            (None, Some(_)) => Some("searxng"),
            (Some(_), Some(_)) => {
                return Ok(SearchSetup::Unconfigured(format!(
                    "web-search has both {ENV_BRAVE_API_KEY} and a SearXNG URL configured and \
                     cannot guess which to use. Set `--backend brave` or `--backend searxng` in \
                     [[plugin]].args, or {ENV_BACKEND} in the environment."
                )));
            }
            (None, None) => None,
        },
    };

    match selected {
        Some("brave") => {
            let Some((api_key, _)) = brave_key else {
                return Ok(SearchSetup::Unconfigured(format!(
                    "web-search is set to the `brave` backend but {ENV_BRAVE_API_KEY} is not set \
                     in the environment of the tdcc process. Export it there — it is deliberately \
                     not readable from [[plugin]].args, which is stored in config.toml."
                )));
            };
            let endpoint = match value(flags, env, "--brave-endpoint", ENV_BRAVE_ENDPOINT) {
                Some((raw, source)) => parse_https_url(&raw, &source)?,
                None => Url::parse(DEFAULT_BRAVE_ENDPOINT)
                    .map_err(|error| format!("built-in Brave endpoint is invalid: {error}"))?,
            };
            Ok(SearchSetup::Configured(SearchBackend::Brave {
                endpoint,
                api_key,
            }))
        }
        Some("searxng") => {
            let Some((raw, source)) = searxng_url else {
                return Ok(SearchSetup::Unconfigured(format!(
                    "web-search is set to the `searxng` backend but no instance URL is \
                     configured. Set `--searxng-url <url>` in [[plugin]].args, {ENV_SEARXNG_URL} \
                     in the environment, or [[plugin]].url in config.toml."
                )));
            };
            Ok(SearchSetup::Configured(SearchBackend::Searxng {
                base: parse_https_url(&raw, &source)?,
            }))
        }
        Some(other) => unreachable!("unhandled backend `{other}`"),
        None => Ok(SearchSetup::Unconfigured(format!(
            "web-search has no search backend configured, so the `search` tool is unavailable \
             (`fetch` still works). Set {ENV_BRAVE_API_KEY} in the environment to use the Brave \
             Search API, or point `--searxng-url` / {ENV_SEARXNG_URL} / [[plugin]].url at a \
             self-hosted SearXNG instance with the JSON format enabled."
        ))),
    }
}

fn parse_https_url(raw: &str, source: &str) -> Result<Url, String> {
    let url = Url::parse(raw.trim())
        .map_err(|error| format!("{source} is not a valid URL ({error}): {raw}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "{source} must be an http or https URL, not `{}`.",
            url.scheme()
        ));
    }
    if url.host_str().is_none() {
        return Err(format!("{source} has no host: {raw}"));
    }
    Ok(url)
}

type Flags = BTreeMap<String, String>;

/// Accepts `--flag value`, `--flag=value`, and bare boolean flags. An unknown
/// flag is a hard error: a typo in `--allow-private-network` that was quietly
/// ignored would leave the operator believing a guard was off when it was on.
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
/// from so error messages can point at the thing the operator actually wrote.
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

/// Read a boolean whose flag and environment variable have opposite polarity.
///
/// `--ignore-robots` turns the guard off while `TDCC_WEB_SEARCH_RESPECT_ROBOTS`
/// turns it on, so `env_means` says which value of the variable corresponds to
/// the flag being present.
fn toggle(
    flags: &Flags,
    env: &EnvMap,
    flag: &str,
    var: &str,
    env_means: bool,
) -> Result<bool, String> {
    if let Some(raw) = flags.get(flag) {
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

    #[test]
    fn no_backend_settings_leaves_search_unconfigured_and_names_both_settings() {
        let config = Config::parse(&[], &env(&[])).expect("defaults parse");

        let SearchSetup::Unconfigured(message) = config.search else {
            panic!("expected an unconfigured backend");
        };
        assert!(message.contains(ENV_BRAVE_API_KEY), "{message}");
        assert!(message.contains(ENV_SEARXNG_URL), "{message}");
    }

    #[test]
    fn a_brave_key_alone_selects_brave_with_the_default_endpoint() {
        let config =
            Config::parse(&[], &env(&[(ENV_BRAVE_API_KEY, "secret-key")])).expect("parses");

        let SearchSetup::Configured(SearchBackend::Brave { endpoint, api_key }) = config.search
        else {
            panic!("expected the Brave backend");
        };
        assert_eq!(endpoint.as_str(), DEFAULT_BRAVE_ENDPOINT);
        assert_eq!(api_key, "secret-key");
    }

    #[test]
    fn a_searxng_url_alone_selects_searxng() {
        let config = Config::parse(&[], &env(&[(ENV_SEARXNG_URL, "https://searx.example/")]))
            .expect("parses");

        assert!(matches!(
            config.search,
            SearchSetup::Configured(SearchBackend::Searxng { .. })
        ));
    }

    #[test]
    fn the_hosts_plugin_url_is_accepted_as_the_searxng_base() {
        let config =
            Config::parse(&[], &env(&[(ENV_PLUGIN_URL, "http://127.0.0.1:8888")])).expect("parses");

        let SearchSetup::Configured(SearchBackend::Searxng { base }) = config.search else {
            panic!("expected the SearXNG backend");
        };
        assert_eq!(base.as_str(), "http://127.0.0.1:8888/");
    }

    #[test]
    fn an_explicit_flag_beats_an_inferred_backend() {
        let config = Config::parse(
            &args(&[
                "--backend",
                "searxng",
                "--searxng-url",
                "https://searx.example",
            ]),
            &env(&[(ENV_BRAVE_API_KEY, "secret-key")]),
        )
        .expect("parses");

        assert!(matches!(
            config.search,
            SearchSetup::Configured(SearchBackend::Searxng { .. })
        ));
    }

    #[test]
    fn two_configured_backends_without_a_choice_refuse_to_guess() {
        let config = Config::parse(
            &[],
            &env(&[
                (ENV_BRAVE_API_KEY, "secret-key"),
                (ENV_SEARXNG_URL, "https://searx.example"),
            ]),
        )
        .expect("parses");

        let SearchSetup::Unconfigured(message) = config.search else {
            panic!("expected an unconfigured backend");
        };
        assert!(message.contains("--backend"), "{message}");
    }

    #[test]
    fn selecting_brave_without_a_key_names_the_environment_variable() {
        let config = Config::parse(&args(&["--backend=brave"]), &env(&[])).expect("parses");

        let SearchSetup::Unconfigured(message) = config.search else {
            panic!("expected an unconfigured backend");
        };
        assert!(message.contains(ENV_BRAVE_API_KEY), "{message}");
    }

    #[test]
    fn selecting_searxng_without_a_url_names_every_place_it_could_come_from() {
        let config = Config::parse(&args(&["--backend", "searxng"]), &env(&[])).expect("parses");

        let SearchSetup::Unconfigured(message) = config.search else {
            panic!("expected an unconfigured backend");
        };
        assert!(message.contains("--searxng-url"), "{message}");
        assert!(message.contains(ENV_SEARXNG_URL), "{message}");
        assert!(message.contains("[[plugin]].url"), "{message}");
    }

    #[test]
    fn an_api_key_is_never_printed_by_debug() {
        let backend = SearchBackend::Brave {
            endpoint: Url::parse(DEFAULT_BRAVE_ENDPOINT).unwrap(),
            api_key: "super-secret".into(),
        };

        let rendered = format!("{backend:?}");

        assert!(!rendered.contains("super-secret"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }

    #[test]
    fn guards_default_to_the_narrow_setting() {
        let config = Config::parse(&[], &env(&[])).expect("parses");

        assert!(config.http.respect_robots);
        assert!(!config.http.allow_private_network);
    }

    #[test]
    fn opt_out_flags_widen_the_guards() {
        let config = Config::parse(
            &args(&["--ignore-robots", "--allow-private-network"]),
            &env(&[]),
        )
        .expect("parses");

        assert!(!config.http.respect_robots);
        assert!(config.http.allow_private_network);
    }

    #[test]
    fn the_environment_can_set_guards_in_either_direction() {
        let respected = Config::parse(&[], &env(&[(ENV_RESPECT_ROBOTS, "false")])).expect("parses");
        assert!(!respected.http.respect_robots);

        let still_respected =
            Config::parse(&[], &env(&[(ENV_RESPECT_ROBOTS, "true")])).expect("parses");
        assert!(still_respected.http.respect_robots);
    }

    #[test]
    fn a_misspelled_option_is_an_error_rather_than_a_silently_ignored_guard() {
        let error = Config::parse(&args(&["--allow-private-netowrk"]), &env(&[]))
            .expect_err("unknown flags are rejected");

        assert!(error.contains("unknown option"), "{error}");
    }

    #[test]
    fn out_of_range_and_unparseable_numbers_are_rejected_with_the_source_named() {
        let error = Config::parse(&args(&["--timeout-secs", "abc"]), &env(&[]))
            .expect_err("non-numeric timeout is rejected");
        assert!(error.contains("--timeout-secs"), "{error}");

        let error = Config::parse(&[], &env(&[(ENV_MAX_REDIRECTS, "99")]))
            .expect_err("out-of-range redirect budget is rejected");
        assert!(error.contains(ENV_MAX_REDIRECTS), "{error}");
    }

    #[test]
    fn a_backend_url_must_be_http_or_https() {
        let error = Config::parse(&args(&["--searxng-url", "file:///etc/passwd"]), &env(&[]))
            .expect_err("non-http backend URLs are rejected");

        assert!(error.contains("http"), "{error}");
    }

    #[test]
    fn the_user_agent_names_the_software_and_optionally_a_contact() {
        assert_eq!(
            user_agent(None),
            format!("{PRODUCT_TOKEN}/{PLUGIN_VERSION} (+{PRODUCT_URL})")
        );
        assert!(user_agent(Some("ops@example.com")).ends_with("contact/ops@example.com"));
        assert!(!user_agent(Some("   ")).contains("contact/"));
    }
}
