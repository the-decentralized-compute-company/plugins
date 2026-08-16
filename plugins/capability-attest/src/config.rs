//! Configuration resolution.
//!
//! `[plugin.settings]` never reaches a plugin process — the host stores those
//! values and the console renders them, but nothing delivers them across the
//! control connection. Everything here therefore comes from `[[plugin]].args`,
//! `[[plugin]].url` (as `TDCC_PLUGIN_URL`), or the environment.
//!
//! [`resolve`] is a pure function over an argument slice and an environment
//! map, so the precedence rules and every validation below are testable without
//! mutating process-global state.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use url::Url;

use crate::profile::{BenchmarkProfile, DEFAULT_FILLER_SENTENCE};

/// A snapshot of the process environment. `BTreeMap` rather than `HashMap` so
/// error messages that list keys come out in a stable order.
pub type EnvMap = BTreeMap<String, String>;

/// Every flag is also readable as `TDCC_ATTEST_<FLAG_IN_SCREAMING_SNAKE>`.
const ENV_PREFIX: &str = "TDCC_ATTEST_";

/// Set by the host from `[[plugin]].url`. Used as the endpoint of last resort.
const HOST_URL_ENV: &str = "TDCC_PLUGIN_URL";

/// Flags that take no value. Everything else requires `--flag value` or
/// `--flag=value`.
const BOOLEAN_FLAGS: &[&str] = &["allow-remote-endpoint"];

/// The full accepted flag set, used both for validation and for `--help`.
/// (flag, value placeholder, help text)
pub const FLAGS: &[(&str, &str, &str)] = &[
    (
        "endpoint",
        "<url>",
        "OpenAI-compatible base URL to benchmark, e.g. http://127.0.0.1:8000/v1. \
         Defaults to TDCC_PLUGIN_URL. Must be loopback unless --allow-remote-endpoint is set.",
    ),
    (
        "model",
        "<id>",
        "Model id sent in the benchmark request. Required; it is pinned into every record.",
    ),
    (
        "allow-remote-endpoint",
        "",
        "Permit a non-loopback endpoint. The record is then labelled endpoint_locality=\"remote\", \
         because a remote endpoint does not measure this node.",
    ),
    (
        "api-key-env",
        "<name>",
        "Name of the environment variable holding a bearer token for the endpoint. \
         The value is never logged and never enters a record. Default: TDCC_ATTEST_API_KEY.",
    ),
    (
        "interval-secs",
        "<secs>",
        "How often the background loop attempts a benchmark. Default: 3600.",
    ),
    (
        "min-interval-secs",
        "<secs>",
        "Cooldown floor between completed benchmarks. Default: 300.",
    ),
    (
        "record-ttl-secs",
        "<secs>",
        "How long a published record claims to be valid. Default: 7200.",
    ),
    (
        "context-tokens",
        "<n>",
        "Approximate prompt length, in tokens, used to build the pinned prompt. Default: 1024.",
    ),
    (
        "max-output-tokens",
        "<n>",
        "max_tokens for the benchmark request. Default: 128.",
    ),
    ("temperature", "<f>", "Sampling temperature. Default: 0."),
    ("top-p", "<f>", "Sampling top_p. Default: 1."),
    (
        "seed",
        "<n>",
        "Sampling seed sent to the endpoint. Default: 42.",
    ),
    (
        "warmup-runs",
        "<n>",
        "Discarded runs before measurement. Default: 1.",
    ),
    (
        "measured-runs",
        "<n>",
        "Runs whose results go into the record. Default: 3.",
    ),
    (
        "request-timeout-secs",
        "<secs>",
        "Per-request timeout for benchmark and probe traffic. Default: 120.",
    ),
    (
        "busy-url",
        "<url>",
        "Loopback URL returning JSON with a count of in-flight requests. When set and \
         unreachable, the benchmark defers rather than guessing.",
    ),
    (
        "busy-pointer",
        "<json-pointer>",
        "JSON pointer into the busy-url response. Default: /active_requests.",
    ),
    (
        "busy-threshold",
        "<n>",
        "Highest in-flight count that still counts as idle. Default: 0.",
    ),
    (
        "max-guard-ttft-ms",
        "<ms>",
        "Fallback contention check when no busy-url is configured: a one-token probe slower \
         than this is treated as a busy node. Default: 750.",
    ),
    (
        "vram-probe",
        "nvidia-smi|off",
        "VRAM probe to run. Default: nvidia-smi.",
    ),
    (
        "vram-total-mib",
        "<mib>",
        "Operator-declared total VRAM, used when no probe can measure it. Recorded as \
         vram.source=\"operator-declared\" so verifiers can discount it.",
    ),
    (
        "node-key-path",
        "<path>",
        "Override the node signing key path. Default: <TDCC_HOME>/.tdcc/key.",
    ),
    (
        "filler-sentence",
        "<text>",
        "Sentence repeated to build the pinned prompt. Change it and records stop being \
         comparable with the default profile, by design.",
    ),
];

/// How VRAM is discovered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VramProbeKind {
    /// Run `nvidia-smi` with a fixed argument list.
    NvidiaSmi,
    /// Run nothing. VRAM is reported from `--vram-total-mib`, or as unavailable.
    Off,
}

/// Whether the endpoint can plausibly be measuring *this* node.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EndpointLocality {
    Loopback,
    Remote,
}

impl EndpointLocality {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Loopback => "loopback",
            Self::Remote => "remote",
        }
    }
}

#[derive(Clone, Debug)]
pub struct AttestConfig {
    pub endpoint: Url,
    pub endpoint_locality: EndpointLocality,
    /// The bearer token itself, resolved from the environment variable named by
    /// `--api-key-env`. Never logged, never serialised into a record.
    pub api_key: Option<String>,
    pub interval: Duration,
    pub min_interval: Duration,
    pub record_ttl: Duration,
    pub request_timeout: Duration,
    pub busy_url: Option<Url>,
    pub busy_pointer: String,
    pub busy_threshold: u64,
    pub max_guard_ttft_ms: u64,
    pub vram_probe: VramProbeKind,
    pub vram_total_mib_override: Option<u64>,
    pub node_key_path: Option<String>,
    pub profile: BenchmarkProfile,
}

impl AttestConfig {
    /// The chat-completions URL derived from the configured base URL.
    ///
    /// Joining rather than formatting keeps a base URL with or without a
    /// trailing slash working the same way.
    pub fn chat_completions_url(&self) -> Result<Url> {
        let mut base = self.endpoint.clone();
        if !base.path().ends_with('/') {
            let path = format!("{}/", base.path());
            base.set_path(&path);
        }
        base.join("chat/completions")
            .map_err(|err| anyhow!("cannot derive a chat/completions URL from --endpoint: {err}"))
    }
}

/// Resolve configuration from arguments and environment.
///
/// Precedence, highest first: command-line flag, `TDCC_ATTEST_*` environment
/// variable, `TDCC_PLUGIN_URL` (endpoint only), built-in default.
pub fn resolve(args: &[String], env: &EnvMap) -> Result<AttestConfig> {
    let source = Source::new(args, env)?;

    let endpoint_raw = match source.get("endpoint") {
        Some(value) => value.to_string(),
        None => match env.get(HOST_URL_ENV).map(String::as_str) {
            Some(value) if !value.trim().is_empty() => value.to_string(),
            _ => bail!(
                "no inference endpoint configured. Pass --endpoint <url> in [[plugin]].args, \
                 set {ENV_PREFIX}ENDPOINT, or set url = \"...\" in the [[plugin]] table"
            ),
        },
    };
    let endpoint =
        Url::parse(endpoint_raw.trim()).map_err(|err| anyhow!("--endpoint is not a URL: {err}"))?;
    let allow_remote = source.flag("allow-remote-endpoint");
    let endpoint_locality = ensure_measurable_endpoint(&endpoint, allow_remote)?;

    let model = source
        .get("model")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "no model configured. Pass --model <id> in [[plugin]].args or set {ENV_PREFIX}MODEL. \
                 A throughput number without a pinned model is not a measurement"
            )
        })?
        .to_string();

    let busy_url = match source.get("busy-url") {
        Some(value) => {
            let url = Url::parse(value.trim())
                .map_err(|err| anyhow!("--busy-url is not a URL: {err}"))?;
            // The busy probe decides whether we are allowed to disturb the
            // node, so it has to describe *this* node.
            ensure_measurable_endpoint(&url, false)
                .map_err(|err| anyhow!("--busy-url must describe this node: {err}"))?;
            Some(url)
        }
        None => None,
    };

    let api_key_env = source.get("api-key-env").unwrap_or("TDCC_ATTEST_API_KEY");
    let api_key = env
        .get(api_key_env)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    let vram_probe = match source.get("vram-probe").unwrap_or("nvidia-smi") {
        "nvidia-smi" => VramProbeKind::NvidiaSmi,
        "off" => VramProbeKind::Off,
        other => bail!("--vram-probe must be \"nvidia-smi\" or \"off\", got {other:?}"),
    };

    let profile = BenchmarkProfile::build(
        model,
        source.parse("context-tokens", 1024u32)?,
        source.parse("max-output-tokens", 128u32)?,
        source.parse("temperature", 0.0f64)?,
        source.parse("top-p", 1.0f64)?,
        source.parse("seed", 42u64)?,
        source.parse("warmup-runs", 1u32)?,
        source.parse("measured-runs", 3u32)?,
        source
            .get("filler-sentence")
            .unwrap_or(DEFAULT_FILLER_SENTENCE)
            .to_string(),
    )?;

    let config = AttestConfig {
        endpoint,
        endpoint_locality,
        api_key,
        interval: Duration::from_secs(source.parse("interval-secs", 3600u64)?),
        min_interval: Duration::from_secs(source.parse("min-interval-secs", 300u64)?),
        record_ttl: Duration::from_secs(source.parse("record-ttl-secs", 7200u64)?),
        request_timeout: Duration::from_secs(source.parse("request-timeout-secs", 120u64)?),
        busy_url,
        busy_pointer: source
            .get("busy-pointer")
            .unwrap_or("/active_requests")
            .to_string(),
        busy_threshold: source.parse("busy-threshold", 0u64)?,
        max_guard_ttft_ms: source.parse("max-guard-ttft-ms", 750u64)?,
        vram_probe,
        vram_total_mib_override: match source.get("vram-total-mib") {
            Some(_) => Some(source.parse("vram-total-mib", 0u64)?),
            None => None,
        },
        node_key_path: source.get("node-key-path").map(str::to_string),
        profile,
    };

    validate(&config)?;
    Ok(config)
}

fn validate(config: &AttestConfig) -> Result<()> {
    if config.interval.as_secs() < 60 {
        bail!(
            "--interval-secs must be at least 60; benchmarking more often than that is itself a load"
        );
    }
    if config.min_interval.as_secs() < 30 {
        bail!("--min-interval-secs must be at least 30");
    }
    if config.record_ttl.as_secs() < 60 {
        bail!("--record-ttl-secs must be at least 60");
    }
    if config.request_timeout.as_secs() == 0 {
        bail!("--request-timeout-secs must be greater than 0");
    }
    if config.busy_pointer.is_empty() || !config.busy_pointer.starts_with('/') {
        bail!(
            "--busy-pointer must be a JSON pointer starting with '/', got {:?}",
            config.busy_pointer
        );
    }
    if config.vram_total_mib_override == Some(0) {
        bail!("--vram-total-mib must be greater than 0 when set");
    }
    Ok(())
}

/// Reject an endpoint that cannot be measuring the node this plugin runs on.
///
/// This is the plugin's narrowest-useful-permission boundary and a correctness
/// rule at the same time. `tdcc` pools GPUs across machines, so a request sent
/// to a non-local address may be served by a peer — the resulting numbers would
/// describe someone else's hardware while being signed by this node's key. That
/// is precisely the failure this plugin exists to prevent, so it is refused by
/// default and labelled when explicitly allowed.
///
/// Plain `http` only: the target is a process on the same machine, so a TLS
/// stack would be attack surface with nothing to protect.
pub fn ensure_measurable_endpoint(url: &Url, allow_remote: bool) -> Result<EndpointLocality> {
    if url.scheme() != "http" {
        bail!(
            "endpoint must use http (the target is a process on this machine); got {:?}",
            url.scheme()
        );
    }
    if is_loopback_host(url) {
        return Ok(EndpointLocality::Loopback);
    }
    if allow_remote {
        return Ok(EndpointLocality::Remote);
    }
    bail!(
        "endpoint host {:?} is not loopback. A request sent off this machine may be served by a \
         peer, so the result would not describe this node. Point it at the node-local inference \
         server, or pass --allow-remote-endpoint to record it as endpoint_locality=\"remote\"",
        url.host_str().unwrap_or("")
    )
}

fn is_loopback_host(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(addr)) => addr.is_loopback(),
        Some(url::Host::Ipv6(addr)) => addr.is_loopback(),
        // `localhost` is the only name we accept without resolving: resolving a
        // name here would make the check depend on DNS at benchmark time.
        Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

/// Argument and environment lookup with a fixed precedence.
struct Source<'a> {
    args: BTreeMap<String, String>,
    env: &'a EnvMap,
}

impl<'a> Source<'a> {
    fn new(args: &[String], env: &'a EnvMap) -> Result<Self> {
        Ok(Self {
            args: parse_args(args)?,
            env,
        })
    }

    fn get(&self, flag: &str) -> Option<&str> {
        if let Some(value) = self.args.get(flag) {
            return Some(value.as_str());
        }
        self.env.get(&env_key(flag)).map(String::as_str)
    }

    fn flag(&self, flag: &str) -> bool {
        matches!(self.get(flag), Some("true" | "1" | "yes" | "on") | Some(""))
    }

    fn parse<T>(&self, flag: &str, default: T) -> Result<T>
    where
        T: std::str::FromStr,
        T::Err: std::fmt::Display,
    {
        match self.get(flag) {
            Some(value) => value
                .trim()
                .parse::<T>()
                .map_err(|err| anyhow!("--{flag} (or {}) is not valid: {err}", env_key(flag))),
            None => Ok(default),
        }
    }
}

fn env_key(flag: &str) -> String {
    format!(
        "{ENV_PREFIX}{}",
        flag.to_ascii_uppercase().replace('-', "_")
    )
}

/// Parse `--flag value` and `--flag=value` pairs into a map.
///
/// Unknown flags are an error rather than being ignored: a typo in
/// `[[plugin]].args` that silently reverts a setting to its default would
/// produce records pinned to something other than what the operator wrote down.
pub fn parse_args(args: &[String]) -> Result<BTreeMap<String, String>> {
    let mut parsed = BTreeMap::new();
    let mut index = 0;
    while index < args.len() {
        let token = args[index].as_str();
        let Some(rest) = token.strip_prefix("--") else {
            bail!("unexpected argument {token:?}; every option is written as --flag value");
        };
        let (flag, inline_value) = match rest.split_once('=') {
            Some((flag, value)) => (flag, Some(value.to_string())),
            None => (rest, None),
        };
        if !FLAGS.iter().any(|(known, _, _)| *known == flag) {
            bail!(
                "unknown option --{flag}. Known options: {}",
                FLAGS
                    .iter()
                    .map(|(name, _, _)| format!("--{name}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }

        let value = match inline_value {
            Some(value) => value,
            None if BOOLEAN_FLAGS.contains(&flag) => "true".to_string(),
            None => {
                index += 1;
                args.get(index)
                    .cloned()
                    .ok_or_else(|| anyhow!("--{flag} needs a value"))?
            }
        };
        if parsed.insert(flag.to_string(), value).is_some() {
            bail!("--{flag} was given more than once");
        }
        index += 1;
    }
    Ok(parsed)
}

/// `--help` text, kept next to the flag table it is generated from.
pub fn help_text() -> String {
    let mut out = String::from(
        "capability-attest — signed, reproducible capability records for a TDCC node.\n\n\
         Run with no arguments under `tdcc`; the host supplies the control endpoint.\n\
         Options are passed through [[plugin]].args, or as TDCC_ATTEST_* environment\n\
         variables (--max-guard-ttft-ms becomes TDCC_ATTEST_MAX_GUARD_TTFT_MS).\n\n\
         Options:\n",
    );
    for (flag, placeholder, help) in FLAGS {
        out.push_str(&format!("  --{flag} {placeholder}\n      {help}\n"));
    }
    out.push_str("\n  --print-package-manifest\n      Emit plugin-manifest.json and exit.\n");
    out
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

    fn minimal() -> Vec<String> {
        args(&["--endpoint", "http://127.0.0.1:8000/v1", "--model", "demo"])
    }

    #[test]
    fn a_flag_beats_the_environment_which_beats_the_host_url() {
        let environment = env(&[
            ("TDCC_ATTEST_ENDPOINT", "http://127.0.0.1:2/v1"),
            ("TDCC_PLUGIN_URL", "http://127.0.0.1:3/v1"),
            ("TDCC_ATTEST_MODEL", "demo"),
        ]);

        let from_flag = resolve(
            &args(&["--endpoint", "http://127.0.0.1:1/v1"]),
            &environment,
        )
        .expect("flag wins");
        assert_eq!(from_flag.endpoint.port(), Some(1));

        let from_env = resolve(&[], &environment).expect("environment wins over the host url");
        assert_eq!(from_env.endpoint.port(), Some(2));

        let from_host = resolve(
            &[],
            &env(&[
                ("TDCC_PLUGIN_URL", "http://127.0.0.1:3/v1"),
                ("TDCC_ATTEST_MODEL", "demo"),
            ]),
        )
        .expect("host url is the last resort");
        assert_eq!(from_host.endpoint.port(), Some(3));
    }

    #[test]
    fn a_missing_endpoint_or_model_is_named_in_the_error() {
        let no_endpoint = resolve(&args(&["--model", "demo"]), &env(&[])).unwrap_err();
        assert!(
            no_endpoint.to_string().contains("--endpoint"),
            "{no_endpoint}"
        );

        let no_model = resolve(
            &args(&["--endpoint", "http://127.0.0.1:8000/v1"]),
            &env(&[]),
        )
        .unwrap_err();
        assert!(no_model.to_string().contains("--model"), "{no_model}");
    }

    #[test]
    fn non_loopback_endpoints_are_refused_unless_explicitly_allowed() {
        let refused = resolve(
            &args(&["--endpoint", "http://10.0.0.4:8000/v1", "--model", "demo"]),
            &env(&[]),
        )
        .unwrap_err();
        assert!(refused.to_string().contains("loopback"), "{refused}");

        let allowed = resolve(
            &args(&[
                "--endpoint",
                "http://10.0.0.4:8000/v1",
                "--model",
                "demo",
                "--allow-remote-endpoint",
            ]),
            &env(&[]),
        )
        .expect("explicitly allowed");
        assert_eq!(allowed.endpoint_locality, EndpointLocality::Remote);
    }

    #[test]
    fn loopback_forms_are_all_accepted_and_other_schemes_are_not() {
        for accepted in [
            "http://127.0.0.1:9337/v1",
            "http://localhost:9337/v1",
            "http://LOCALHOST:9337/v1",
            "http://[::1]:9337/v1",
            "http://127.9.9.9:9337/v1",
        ] {
            let url = Url::parse(accepted).unwrap();
            assert_eq!(
                ensure_measurable_endpoint(&url, false).unwrap(),
                EndpointLocality::Loopback,
                "{accepted} should be loopback"
            );
        }

        let https = Url::parse("https://127.0.0.1:9337/v1").unwrap();
        assert!(
            ensure_measurable_endpoint(&https, true)
                .unwrap_err()
                .to_string()
                .contains("http")
        );

        let external = Url::parse("http://example.com/v1").unwrap();
        assert!(ensure_measurable_endpoint(&external, false).is_err());
    }

    #[test]
    fn a_busy_url_must_describe_this_node_even_when_the_endpoint_is_remote() {
        let error = resolve(
            &args(&[
                "--endpoint",
                "http://10.0.0.4:8000/v1",
                "--allow-remote-endpoint",
                "--model",
                "demo",
                "--busy-url",
                "http://10.0.0.4:8000/stats",
            ]),
            &env(&[]),
        )
        .unwrap_err();

        assert!(error.to_string().contains("--busy-url"), "{error}");
    }

    #[test]
    fn unknown_and_duplicated_options_are_refused() {
        let unknown = parse_args(&args(&["--endpint", "x"])).unwrap_err();
        assert!(unknown.to_string().contains("unknown option"), "{unknown}");

        let duplicated = parse_args(&args(&["--model", "a", "--model", "b"])).unwrap_err();
        assert!(duplicated.to_string().contains("more than once"));

        let missing_value = parse_args(&args(&["--model"])).unwrap_err();
        assert!(missing_value.to_string().contains("needs a value"));

        let positional = parse_args(&args(&["model"])).unwrap_err();
        assert!(positional.to_string().contains("unexpected argument"));
    }

    #[test]
    fn inline_and_separated_values_parse_the_same() {
        let inline = parse_args(&args(&["--model=demo"])).unwrap();
        let separated = parse_args(&args(&["--model", "demo"])).unwrap();
        assert_eq!(inline, separated);
    }

    #[test]
    fn the_api_key_comes_from_the_named_variable_and_never_from_a_flag() {
        let config = resolve(
            &[minimal(), args(&["--api-key-env", "MY_LOCAL_TOKEN"])].concat(),
            &env(&[("MY_LOCAL_TOKEN", "s3cret")]),
        )
        .unwrap();

        assert_eq!(config.api_key.as_deref(), Some("s3cret"));
        assert!(
            !FLAGS.iter().any(|(flag, _, _)| *flag == "api-key"),
            "there must be no flag that puts a token on the process command line"
        );
    }

    #[test]
    fn implausible_intervals_are_refused() {
        let too_fast = resolve(
            &[minimal(), args(&["--interval-secs", "5"])].concat(),
            &env(&[]),
        )
        .unwrap_err();
        assert!(
            too_fast.to_string().contains("--interval-secs"),
            "{too_fast}"
        );
    }

    #[test]
    fn a_bad_number_names_both_the_flag_and_the_variable() {
        let error = resolve(
            &[minimal(), args(&["--seed", "not-a-number"])].concat(),
            &env(&[]),
        )
        .unwrap_err();

        assert!(error.to_string().contains("--seed"), "{error}");
        assert!(error.to_string().contains("TDCC_ATTEST_SEED"), "{error}");
    }

    #[test]
    fn the_chat_url_is_joined_whether_or_not_the_base_ends_in_a_slash() {
        for base in ["http://127.0.0.1:8000/v1", "http://127.0.0.1:8000/v1/"] {
            let config =
                resolve(&args(&["--endpoint", base, "--model", "demo"]), &env(&[])).unwrap();
            assert_eq!(
                config.chat_completions_url().unwrap().as_str(),
                "http://127.0.0.1:8000/v1/chat/completions",
                "base {base}"
            );
        }
    }

    #[test]
    fn help_text_documents_every_flag() {
        let help = help_text();
        for (flag, _, _) in FLAGS {
            assert!(
                help.contains(&format!("--{flag}")),
                "--{flag} is undocumented"
            );
        }
    }
}
