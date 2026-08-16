//! Effective configuration, assembled from the launch contract.
//!
//! Everything here comes from `[[plugin]].url` (delivered as
//! `TDCC_PLUGIN_URL`) and `[[plugin]].args`. It deliberately does **not** come
//! from `[plugin.settings]`: host-owned settings are never delivered to the
//! plugin process, so a `config_schema` would render controls in the console
//! that this process could not read. See the plugins README, "Host owns /
//! plugin owns".
//!
//! Parsing is a pure function of `(args, env)` so the validation rules below
//! are unit-tested without a host, a server, or a network.

use std::time::Duration;

use anyhow::{Result, bail};
use url::Url;

/// Endpoint id used when `--endpoint-id` is not given. The host namespaces it,
/// so this shows up as `openai-endpoint:upstream`.
pub const DEFAULT_ENDPOINT_ID: &str = "upstream";
/// Budget for a single diagnostic request, including reading the body.
pub const DEFAULT_TIMEOUT_SECS: u64 = 10;
const MIN_TIMEOUT_SECS: u64 = 1;
const MAX_TIMEOUT_SECS: u64 = 120;
const MAX_ENDPOINT_ID_LEN: usize = 64;
const MAX_ENV_NAME_LEN: usize = 128;
const MAX_MODEL_NAME_LEN: usize = 256;

/// Path segments that mean the operator pasted an operation URL instead of the
/// API base. Routing concatenates the base path with the client's path, so
/// `.../v1/chat/completions` as a base produces a doubled path that fails in a
/// confusing way at request time rather than at startup.
const OPERATION_SUFFIXES: [&str; 5] = [
    "/chat/completions",
    "/completions",
    "/embeddings",
    "/responses",
    "/models",
];

/// Where the base URL came from. Reported by the `status` tool so an operator
/// looking at a surprising address knows which knob produced it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UrlSource {
    /// `--url` in `[[plugin]].args`.
    Argument,
    /// `TDCC_PLUGIN_URL`, i.e. `[[plugin]].url`.
    LaunchContract,
}

#[derive(Clone, Debug)]
pub struct EndpointConfig {
    base_url: Url,
    url_source: UrlSource,
    endpoint_id: String,
    api_key_env: Option<String>,
    timeout: Duration,
    default_model: Option<String>,
}

impl EndpointConfig {
    /// Normalized base URL. Its path never has a trailing slash beyond the
    /// root, so joining a relative operation path onto it is unambiguous.
    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    pub fn url_source(&self) -> UrlSource {
        self.url_source
    }

    pub fn endpoint_id(&self) -> &str {
        &self.endpoint_id
    }

    /// Name of the environment variable holding a bearer token, if configured.
    /// The name, never the value — the value is read at request time and is
    /// never logged, returned by a tool, or written into the manifest.
    pub fn api_key_env(&self) -> Option<&str> {
        self.api_key_env.as_deref()
    }

    /// Resolve the bearer token from the environment. `None` when no variable
    /// is configured, or when the configured one is unset or empty.
    pub fn api_key(&self) -> Option<String> {
        let name = self.api_key_env.as_deref()?;
        std::env::var(name)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Model used by `verify_stream` and `probe_completion` when the caller
    /// does not name one and discovery cannot pick one.
    pub fn default_model(&self) -> Option<&str> {
        self.default_model.as_deref()
    }

    /// The address declared to the host in the inference endpoint manifest.
    ///
    /// The host re-parses this string, so it stays a plain absolute URL. The
    /// trailing slash is trimmed only for readability; the host trims it too.
    pub fn endpoint_address(&self) -> String {
        self.base_url.as_str().trim_end_matches('/').to_string()
    }

    /// Base path as the host sees it when mapping an incoming request path
    /// onto this endpoint: `""` for a root-mounted API, `/v1`, `/api/v1`, …
    pub fn base_path(&self) -> String {
        let path = self.base_url.path().trim_end_matches('/');
        if path == "/" {
            String::new()
        } else {
            path.to_string()
        }
    }

    /// Build a URL for one API operation under the base.
    ///
    /// `operation` is always a literal from this crate — never a tool argument
    /// — so a caller cannot steer a request off the configured host or walk up
    /// out of the configured path.
    pub fn operation_url(&self, operation: &str) -> Url {
        let mut url = self.base_url.clone();
        let base = self.base_path();
        let base = if base.is_empty() {
            "/v1"
        } else {
            base.as_str()
        };
        url.set_path(&format!("{base}/{}", operation.trim_start_matches('/')));
        url.set_query(None);
        url.set_fragment(None);
        url
    }

    /// Configuration used by `--print-package-manifest`.
    ///
    /// Safe because the packaged `plugin-manifest.json` carries only
    /// `config_schema` and `web_ui`, and this plugin declares neither — the
    /// endpoint address never reaches the packaged file. Keeping the packaging
    /// path independent of configuration means packaging works on a build
    /// machine that has no endpoint to point at.
    pub fn packaging_placeholder() -> Self {
        Self {
            base_url: Url::parse("http://127.0.0.1:8000/v1").expect("literal URL parses"),
            url_source: UrlSource::LaunchContract,
            endpoint_id: DEFAULT_ENDPOINT_ID.to_string(),
            api_key_env: None,
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            default_model: None,
        }
    }

    /// Assemble configuration from `[[plugin]].args` and `TDCC_PLUGIN_URL`.
    ///
    /// `--url` wins over the launch-contract value so a locally built binary
    /// can be pointed somewhere else without editing `config.toml`.
    pub fn from_launch<I, S>(args: I, launch_url: Option<String>) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let parsed = RawArgs::parse(args)?;

        let (raw_url, url_source) = match (parsed.url, launch_url) {
            (Some(url), _) => (url, UrlSource::Argument),
            (None, Some(url)) => (url, UrlSource::LaunchContract),
            (None, None) => bail!(
                "no endpoint URL configured: set `url = \"http://127.0.0.1:8000/v1\"` in the \
                 plugin's [[plugin]] table, or pass `--url <base>` in [[plugin]].args"
            ),
        };

        Ok(Self {
            base_url: validate_base_url(&raw_url)?,
            url_source,
            endpoint_id: match parsed.endpoint_id {
                Some(id) => validate_endpoint_id(&id)?,
                None => DEFAULT_ENDPOINT_ID.to_string(),
            },
            api_key_env: parsed
                .api_key_env
                .map(|n| validate_env_name(&n))
                .transpose()?,
            timeout: match parsed.timeout_secs {
                Some(raw) => validate_timeout(&raw)?,
                None => Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            },
            default_model: parsed.model.map(|m| validate_model_name(&m)).transpose()?,
        })
    }
}

/// Raw flag values, before any of them are validated.
#[derive(Default)]
struct RawArgs {
    url: Option<String>,
    endpoint_id: Option<String>,
    api_key_env: Option<String>,
    timeout_secs: Option<String>,
    model: Option<String>,
}

impl RawArgs {
    fn parse<I, S>(args: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut parsed = Self::default();
        let mut args = args.into_iter().map(Into::into);
        while let Some(flag) = args.next() {
            // Accept `--flag=value` as well as `--flag value`; operators write
            // both, and TOML arrays make the split form easy to get wrong.
            let (name, inline) = match flag.split_once('=') {
                Some((name, value)) => (name.to_string(), Some(value.to_string())),
                None => (flag.clone(), None),
            };
            let slot = match name.as_str() {
                "--url" => &mut parsed.url,
                "--endpoint-id" => &mut parsed.endpoint_id,
                "--api-key-env" => &mut parsed.api_key_env,
                "--timeout-secs" => &mut parsed.timeout_secs,
                "--model" => &mut parsed.model,
                other => bail!(
                    "unknown argument '{other}'; supported: --url, --endpoint-id, \
                     --api-key-env, --timeout-secs, --model"
                ),
            };
            let value = match inline {
                Some(value) => value,
                None => match args.next() {
                    Some(value) => value,
                    None => bail!("argument '{name}' needs a value"),
                },
            };
            if slot.is_some() {
                bail!("argument '{name}' was given more than once");
            }
            *slot = Some(value);
        }
        Ok(parsed)
    }
}

/// Reject every base URL the host could not actually route to, at startup,
/// with the reason spelled out.
///
/// Starting successfully while advertising an endpoint that can never receive
/// traffic is the worst outcome: the node looks joined and silently is not.
pub fn validate_base_url(raw: &str) -> Result<Url> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("endpoint URL is empty");
    }
    let Ok(mut url) = Url::parse(raw) else {
        bail!(
            "endpoint URL '{raw}' is not a valid absolute URL (expected e.g. http://127.0.0.1:8000/v1)"
        );
    };

    if url.scheme() != "http" {
        bail!(
            "endpoint URL '{raw}' uses scheme '{}', but the host's external-endpoint proxy only \
             connects over cleartext http and drops every other scheme before it dials. An https \
             endpoint would pass this plugin's own probes and still never receive routed traffic, \
             so it is refused here instead. Put a local http listener in front of the TLS endpoint \
             and point `url` at that.",
            url.scheme()
        );
    }
    if url.host_str().is_none_or(str::is_empty) {
        bail!("endpoint URL '{raw}' has no host");
    }
    if url.query().is_some() || url.fragment().is_some() {
        bail!(
            "endpoint URL '{raw}' carries a query or fragment; give the API base only, e.g. \
             http://127.0.0.1:8000/v1"
        );
    }

    let path = url.path().trim_end_matches('/').to_string();
    if let Some(suffix) = OPERATION_SUFFIXES
        .iter()
        .find(|suffix| path.ends_with(*suffix))
    {
        bail!(
            "endpoint URL '{raw}' points at the operation '{suffix}'; give the API base instead, \
             e.g. http://127.0.0.1:8000/v1. Routing appends the caller's path to this one, so an \
             operation URL produces a doubled path."
        );
    }
    url.set_path(&path);

    Ok(url)
}

fn validate_endpoint_id(raw: &str) -> Result<String> {
    let id = raw.trim();
    if id.is_empty() {
        bail!("--endpoint-id is empty");
    }
    if id.len() > MAX_ENDPOINT_ID_LEN {
        bail!("--endpoint-id '{id}' is longer than {MAX_ENDPOINT_ID_LEN} characters");
    }
    // The id is concatenated into host capability strings such as
    // `endpoint:inference/…` and into the endpoint key `<plugin>:<id>`, so keep
    // it to characters that survive that without quoting.
    if !id
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '.' | '_' | '-'))
    {
        bail!("--endpoint-id '{id}' must use only lowercase letters, digits, '.', '_' and '-'");
    }
    if !id.starts_with(|ch: char| ch.is_ascii_lowercase() || ch.is_ascii_digit()) {
        bail!("--endpoint-id '{id}' must start with a lowercase letter or a digit");
    }
    Ok(id.to_string())
}

/// Accept only something shaped like an environment variable name.
///
/// This is the guard that stops an API key from being pasted into
/// `config.toml`: a real key contains characters no shell env name may hold, so
/// it fails here with an explanation instead of being persisted in plain text.
fn validate_env_name(raw: &str) -> Result<String> {
    let name = raw.trim();
    if name.is_empty() {
        bail!("--api-key-env is empty");
    }
    if name.len() > MAX_ENV_NAME_LEN {
        bail!("--api-key-env is longer than {MAX_ENV_NAME_LEN} characters");
    }
    let shaped_like_env_name = name.starts_with(|ch: char| ch.is_ascii_alphabetic() || ch == '_')
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_');
    if !shaped_like_env_name {
        bail!(
            "--api-key-env takes the NAME of an environment variable holding the key \
             (e.g. OPENAI_ENDPOINT_API_KEY), not the key itself"
        );
    }
    Ok(name.to_string())
}

fn validate_timeout(raw: &str) -> Result<Duration> {
    let Ok(secs) = raw.trim().parse::<u64>() else {
        bail!("--timeout-secs '{raw}' is not a whole number of seconds");
    };
    if !(MIN_TIMEOUT_SECS..=MAX_TIMEOUT_SECS).contains(&secs) {
        bail!("--timeout-secs must be between {MIN_TIMEOUT_SECS} and {MAX_TIMEOUT_SECS}");
    }
    Ok(Duration::from_secs(secs))
}

/// Model names travel in a JSON request body, never in a URL, so the only
/// rules are non-empty, bounded, and free of control characters that would
/// corrupt a log line.
pub fn validate_model_name(raw: &str) -> Result<String> {
    let model = raw.trim();
    if model.is_empty() {
        bail!("model name is empty");
    }
    if model.len() > MAX_MODEL_NAME_LEN {
        bail!("model name is longer than {MAX_MODEL_NAME_LEN} characters");
    }
    if model.chars().any(char::is_control) {
        bail!("model name contains control characters");
    }
    Ok(model.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(args: &[&str], launch_url: Option<&str>) -> Result<EndpointConfig> {
        EndpointConfig::from_launch(
            args.iter().map(|arg| arg.to_string()),
            launch_url.map(String::from),
        )
    }

    #[test]
    fn launch_contract_url_is_used_when_no_argument_overrides_it() {
        let config = config(&[], Some("http://127.0.0.1:8000/v1")).expect("valid");
        assert_eq!(config.endpoint_address(), "http://127.0.0.1:8000/v1");
        assert_eq!(config.url_source(), UrlSource::LaunchContract);
        assert_eq!(config.endpoint_id(), DEFAULT_ENDPOINT_ID);
    }

    #[test]
    fn url_argument_overrides_the_launch_contract_value() {
        let config = config(
            &["--url", "http://127.0.0.1:11434"],
            Some("http://127.0.0.1:8000/v1"),
        )
        .expect("valid");
        assert_eq!(config.endpoint_address(), "http://127.0.0.1:11434");
        assert_eq!(config.url_source(), UrlSource::Argument);
    }

    #[test]
    fn missing_url_names_both_ways_to_supply_one() {
        let error = config(&[], None).expect_err("no URL configured");
        let message = error.to_string();
        assert!(message.contains("[[plugin]]"), "{message}");
        assert!(message.contains("--url"), "{message}");
    }

    #[test]
    fn inline_and_split_flag_forms_are_equivalent() {
        let split = config(
            &["--endpoint-id", "vllm", "--timeout-secs", "30"],
            Some("http://127.0.0.1:8000/v1"),
        )
        .expect("valid");
        let inline = config(
            &["--endpoint-id=vllm", "--timeout-secs=30"],
            Some("http://127.0.0.1:8000/v1"),
        )
        .expect("valid");
        assert_eq!(split.endpoint_id(), inline.endpoint_id());
        assert_eq!(split.timeout(), inline.timeout());
        assert_eq!(inline.timeout(), Duration::from_secs(30));
    }

    #[test]
    fn unknown_and_repeated_arguments_are_rejected() {
        let unknown =
            config(&["--verbose"], Some("http://127.0.0.1:8000/v1")).expect_err("unknown flag");
        assert!(
            unknown.to_string().contains("--endpoint-id"),
            "flag list is shown"
        );

        let repeated = config(
            &["--endpoint-id", "a", "--endpoint-id", "b"],
            Some("http://127.0.0.1:8000/v1"),
        )
        .expect_err("repeated flag");
        assert!(repeated.to_string().contains("more than once"));
    }

    #[test]
    fn a_flag_without_a_value_is_rejected() {
        let error = config(&["--url"], None).expect_err("dangling flag");
        assert!(error.to_string().contains("needs a value"));
    }

    #[test]
    fn https_is_refused_because_the_host_proxy_cannot_dial_it() {
        let error = validate_base_url("https://api.example.com/v1").expect_err("https refused");
        let message = error.to_string();
        assert!(message.contains("cleartext http"), "{message}");
        assert!(
            message.contains("never receive routed traffic"),
            "{message}"
        );
    }

    #[test]
    fn operation_urls_are_refused_with_the_base_spelled_out() {
        for raw in [
            "http://127.0.0.1:8000/v1/chat/completions",
            "http://127.0.0.1:8000/v1/models",
            "http://127.0.0.1:8000/v1/embeddings",
        ] {
            let error = validate_base_url(raw).expect_err("operation URL refused");
            assert!(error.to_string().contains("API base"), "{raw}");
        }
    }

    #[test]
    fn malformed_urls_are_refused() {
        for raw in [
            "",
            "   ",
            // No scheme: `Url` reads "127.0.0.1" as the scheme, which is not http.
            "127.0.0.1:8000",
            "not a url",
            // An authority with no host at all.
            "http://",
            "http://:8000/v1",
        ] {
            assert!(validate_base_url(raw).is_err(), "{raw:?} should be refused");
        }
    }

    #[test]
    fn extra_slashes_collapse_to_a_hostname_rather_than_an_empty_host() {
        // Per the URL spec, `http:///v1` is `http://v1/` — a host named "v1",
        // not an empty host. Accepting it is correct: single-label hostnames are
        // legitimate on an intranet. Asserted so the surprise is on the record.
        let url = validate_base_url("http:///v1").expect("valid per the URL spec");
        assert_eq!(url.host_str(), Some("v1"));
        // An http URL always keeps a root path; it cannot be emptied.
        assert_eq!(url.path(), "/");
    }

    #[test]
    fn query_and_fragment_are_refused() {
        assert!(validate_base_url("http://127.0.0.1:8000/v1?key=abc").is_err());
        assert!(validate_base_url("http://127.0.0.1:8000/v1#frag").is_err());
    }

    #[test]
    fn trailing_slashes_are_normalized_away() {
        let url = validate_base_url("http://127.0.0.1:8000/v1///").expect("valid");
        assert_eq!(url.path(), "/v1");
    }

    #[test]
    fn base_path_is_empty_for_a_root_mounted_api() {
        let root = config(&[], Some("http://127.0.0.1:11434")).expect("valid");
        assert_eq!(root.base_path(), "");

        let versioned = config(&[], Some("http://127.0.0.1:8000/api/v1")).expect("valid");
        assert_eq!(versioned.base_path(), "/api/v1");
    }

    #[test]
    fn operation_urls_stay_under_the_configured_base() {
        let versioned = config(&[], Some("http://127.0.0.1:8000/api/v1")).expect("valid");
        assert_eq!(
            versioned.operation_url("chat/completions").as_str(),
            "http://127.0.0.1:8000/api/v1/chat/completions"
        );

        // A root-mounted server still gets the conventional /v1 prefix, which
        // is what the host's own probe assumes as well.
        let root = config(&[], Some("http://127.0.0.1:11434")).expect("valid");
        assert_eq!(
            root.operation_url("models").as_str(),
            "http://127.0.0.1:11434/v1/models"
        );
    }

    #[test]
    fn api_key_env_rejects_a_pasted_key() {
        let error = config(
            &["--api-key-env", "sk-live-0123456789abcdef"],
            Some("http://127.0.0.1:8000/v1"),
        )
        .expect_err("key-shaped value refused");
        assert!(error.to_string().contains("not the key itself"));
    }

    #[test]
    fn api_key_env_accepts_an_environment_variable_name() {
        let config = config(
            &["--api-key-env", "OPENAI_ENDPOINT_API_KEY"],
            Some("http://127.0.0.1:8000/v1"),
        )
        .expect("valid");
        assert_eq!(config.api_key_env(), Some("OPENAI_ENDPOINT_API_KEY"));
    }

    #[test]
    fn endpoint_ids_are_restricted_to_safe_characters() {
        for id in ["Upstream", "my endpoint", "-leading", "up/stream", ""] {
            assert!(
                validate_endpoint_id(id).is_err(),
                "{id:?} should be refused"
            );
        }
        assert_eq!(
            validate_endpoint_id("vllm-a100.0").expect("valid"),
            "vllm-a100.0"
        );
    }

    #[test]
    fn timeouts_outside_the_supported_range_are_refused() {
        assert!(validate_timeout("0").is_err());
        assert!(validate_timeout("121").is_err());
        assert!(validate_timeout("ten").is_err());
        assert_eq!(
            validate_timeout("120").expect("valid"),
            Duration::from_secs(120)
        );
    }

    #[test]
    fn model_names_are_bounded_and_free_of_control_characters() {
        assert!(validate_model_name("").is_err());
        assert!(validate_model_name("bad\nname").is_err());
        assert!(validate_model_name(&"m".repeat(MAX_MODEL_NAME_LEN + 1)).is_err());
        assert_eq!(
            validate_model_name("  Qwen/Qwen3-8B  ").expect("valid"),
            "Qwen/Qwen3-8B"
        );
    }

    #[test]
    fn the_packaging_placeholder_is_a_usable_configuration() {
        let placeholder = EndpointConfig::packaging_placeholder();
        assert_eq!(placeholder.endpoint_id(), DEFAULT_ENDPOINT_ID);
        assert_eq!(placeholder.api_key_env(), None);
    }
}
