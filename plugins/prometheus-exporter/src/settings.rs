//! Configuration, and the two rules configuration is not allowed to break.
//!
//! The exporter reads node state from the local `tdcc` HTTP API. That makes the
//! address it dials the single most security-relevant setting in the plugin, so
//! it is validated here rather than at the call site:
//!
//! 1. **Loopback only.** The host may be `localhost`, an IPv4 loopback address,
//!    or bracketed `[::1]`. Anything else is rejected at startup with a message
//!    naming the offending host. A metrics plugin has no business dialling the
//!    wider network, and refusing at parse time means no later code path can.
//! 2. **Plaintext `http://` only.** The exporter links no TLS stack at all, so
//!    accepting an `https://` URL would be a promise it cannot keep. It says so
//!    instead of failing at connect time.
//!
//! `[plugin.settings]` deliberately plays no part here: the host stores those
//! values and never delivers them to the plugin process, so every knob below
//! arrives through `[[plugin]].url` or `[[plugin]].args`.

use std::time::Duration;

/// Port the `tdcc` operator console listens on by default.
pub const DEFAULT_CONSOLE_PORT: u16 = 3131;
/// Per-peer series are capped so a large or churning mesh cannot inflate the
/// scrape without an operator opting in.
pub const DEFAULT_MAX_PEER_SERIES: usize = 64;
/// Loaded models are few, but a long-lived node that cycles through many of
/// them would otherwise accumulate one label set per model name ever seen.
pub const DEFAULT_MAX_MODEL_SERIES: usize = 32;
/// Generous for any single machine; still a hard ceiling.
pub const DEFAULT_MAX_GPU_SERIES: usize = 32;
/// A scrape that cannot read node state in this long reports `tdcc_up 0`
/// rather than holding the Prometheus scrape open.
pub const DEFAULT_COLLECT_TIMEOUT_MS: u64 = 2_000;

/// Upper bound accepted for any of the `--max-*-series` knobs.
///
/// The point of the knobs is to bound cardinality; letting them be set to
/// millions would defeat that, so the ceiling is stated rather than implied.
pub const MAX_SERIES_LIMIT: usize = 4_096;
/// Upper bound accepted for `--collect-timeout-ms`.
pub const MAX_COLLECT_TIMEOUT_MS: u64 = 60_000;

/// A validated loopback HTTP endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeEndpoint {
    /// Host as written by the operator, without IPv6 brackets.
    host: String,
    port: u16,
    /// `true` when the host is an IPv6 literal and needs brackets in a `Host:`
    /// header or a URL.
    bracketed: bool,
}

impl NodeEndpoint {
    /// Host for `TcpStream::connect`, which wants the bare address.
    pub fn connect_host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Authority for the `Host:` request header and for log messages.
    pub fn authority(&self) -> String {
        if self.bracketed {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.authority())
    }
}

/// Everything the exporter was told at launch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub node: NodeEndpoint,
    pub max_peer_series: usize,
    pub max_model_series: usize,
    pub max_gpu_series: usize,
    pub collect_timeout: Duration,
}

impl Settings {
    /// Series ceiling implied by these settings, for the `check` tool and the
    /// README's cardinality claim. See `render::FIXED_SERIES` for the constant
    /// part.
    pub fn max_series(&self) -> usize {
        crate::render::FIXED_SERIES
            + 3 * self.max_model_series
            + 3 * self.max_gpu_series
            + 6 * self.max_peer_series
    }
}

/// Parse `http://<loopback-host>[:port]` with an optional trailing slash.
///
/// Deliberately stricter than a general URL parser: credentials, query strings,
/// fragments and paths are all rejected, because none of them can appear in a
/// legitimate value and each of them would widen what this plugin can reach.
pub fn parse_node_url(raw: &str) -> Result<NodeEndpoint, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("node URL is empty".to_string());
    }

    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("https://") {
        return Err(format!(
            "node URL '{trimmed}' uses https, but this plugin links no TLS stack \
             and only talks to loopback; use http://"
        ));
    }
    let rest = trimmed
        .get("http://".len()..)
        .filter(|_| lower.starts_with("http://"))
        .ok_or_else(|| format!("node URL '{trimmed}' must start with http://"))?;

    if rest.contains('@') {
        return Err(format!(
            "node URL '{trimmed}' carries credentials; the local node API takes none"
        ));
    }
    if rest.contains('?') || rest.contains('#') {
        return Err(format!(
            "node URL '{trimmed}' must not carry a query string or fragment"
        ));
    }

    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, path),
        None => (rest, ""),
    };
    if !path.is_empty() {
        return Err(format!(
            "node URL '{trimmed}' must not carry a path; the exporter appends its own"
        ));
    }
    if authority.is_empty() {
        return Err(format!("node URL '{trimmed}' has no host"));
    }

    let (host, port_text, bracketed) = if let Some(after_bracket) = authority.strip_prefix('[') {
        let (host, tail) = after_bracket
            .split_once(']')
            .ok_or_else(|| format!("node URL '{trimmed}' has an unterminated IPv6 literal"))?;
        let port_text = match tail {
            "" => None,
            tail => Some(tail.strip_prefix(':').ok_or_else(|| {
                format!("node URL '{trimmed}' has trailing text after the IPv6 literal")
            })?),
        };
        (host, port_text, true)
    } else {
        match authority.split_once(':') {
            // A bare `::1` would be ambiguous with `host:port`, so require the
            // bracket form for IPv6 rather than guessing.
            Some((host, _)) if host.contains(':') || authority.matches(':').count() > 1 => {
                return Err(format!(
                    "node URL '{trimmed}' looks like an IPv6 address; write it as http://[::1]:{DEFAULT_CONSOLE_PORT}"
                ));
            }
            Some((host, port_text)) => (host, Some(port_text), false),
            None => (authority, None, false),
        }
    };

    if !is_loopback_host(host) {
        return Err(format!(
            "node URL host '{host}' is not loopback; this plugin only reads the node it runs on"
        ));
    }

    let port = match port_text {
        None => DEFAULT_CONSOLE_PORT,
        Some(text) => {
            let port: u16 = text
                .parse()
                .map_err(|_| format!("node URL '{trimmed}' has an invalid port '{text}'"))?;
            if port == 0 {
                return Err(format!("node URL '{trimmed}' has port 0"));
            }
            port
        }
    };

    Ok(NodeEndpoint {
        host: host.to_string(),
        port,
        bracketed: bracketed || host.contains(':'),
    })
}

/// `localhost`, or any address the IP stack considers loopback.
pub fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .is_ok_and(|address| address.is_loopback())
}

/// Build settings from process arguments and the `TDCC_PLUGIN_URL` the host
/// exported from `[[plugin]].url`.
///
/// `args` excludes the executable name. An explicit `--node-url` wins over the
/// host-supplied URL so a locally built binary can be pointed at a second node
/// on the same machine without editing `config.toml`.
pub fn settings_from(args: &[String], plugin_url: Option<&str>) -> Result<Settings, String> {
    let mut node_url: Option<String> = None;
    let mut max_peer_series = DEFAULT_MAX_PEER_SERIES;
    let mut max_model_series = DEFAULT_MAX_MODEL_SERIES;
    let mut max_gpu_series = DEFAULT_MAX_GPU_SERIES;
    let mut collect_timeout_ms = DEFAULT_COLLECT_TIMEOUT_MS;

    let mut index = 0;
    while index < args.len() {
        let (flag, inline_value) = match args[index].split_once('=') {
            Some((flag, value)) => (flag, Some(value.to_string())),
            None => (args[index].as_str(), None),
        };
        let mut take_value = |flag: &str| -> Result<String, String> {
            if let Some(value) = inline_value.clone() {
                return Ok(value);
            }
            index += 1;
            args.get(index)
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value"))
        };

        match flag {
            "--node-url" => node_url = Some(take_value(flag)?),
            "--max-peer-series" => max_peer_series = parse_series_limit(flag, &take_value(flag)?)?,
            "--max-model-series" => {
                max_model_series = parse_series_limit(flag, &take_value(flag)?)?
            }
            "--max-gpu-series" => max_gpu_series = parse_series_limit(flag, &take_value(flag)?)?,
            "--collect-timeout-ms" => {
                let raw = take_value(flag)?;
                let value: u64 = raw
                    .parse()
                    .map_err(|_| format!("{flag} expects a whole number of milliseconds"))?;
                if !(1..=MAX_COLLECT_TIMEOUT_MS).contains(&value) {
                    return Err(format!(
                        "{flag} must be between 1 and {MAX_COLLECT_TIMEOUT_MS}, got {value}"
                    ));
                }
                collect_timeout_ms = value;
            }
            other => {
                return Err(format!(
                    "unknown option '{other}'; accepted: --node-url, --max-peer-series, \
                     --max-model-series, --max-gpu-series, --collect-timeout-ms"
                ));
            }
        }
        index += 1;
    }

    let raw_url = node_url
        .or_else(|| plugin_url.map(str::to_string))
        .unwrap_or_else(|| format!("http://127.0.0.1:{DEFAULT_CONSOLE_PORT}"));

    Ok(Settings {
        node: parse_node_url(&raw_url)?,
        max_peer_series,
        max_model_series,
        max_gpu_series,
        collect_timeout: Duration::from_millis(collect_timeout_ms),
    })
}

fn parse_series_limit(flag: &str, raw: &str) -> Result<usize, String> {
    let value: usize = raw
        .parse()
        .map_err(|_| format!("{flag} expects a whole number, got '{raw}'"))?;
    if value > MAX_SERIES_LIMIT {
        return Err(format!(
            "{flag} must not exceed {MAX_SERIES_LIMIT}; the point of the cap is to bound cardinality"
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

    #[test]
    fn default_url_is_the_local_console() {
        let settings = settings_from(&[], None).expect("defaults parse");
        assert_eq!(settings.node.base_url(), "http://127.0.0.1:3131");
        assert_eq!(settings.collect_timeout, Duration::from_millis(2_000));
    }

    #[test]
    fn plugin_url_is_used_when_no_flag_overrides_it() {
        let settings =
            settings_from(&[], Some("http://localhost:9999")).expect("plugin url parses");
        assert_eq!(settings.node.authority(), "localhost:9999");
    }

    #[test]
    fn explicit_flag_beats_the_host_supplied_url() {
        let settings = settings_from(
            &args(&["--node-url", "http://127.0.0.2:4000"]),
            Some("http://localhost:9999"),
        )
        .expect("flag parses");
        assert_eq!(settings.node.authority(), "127.0.0.2:4000");
    }

    #[test]
    fn ipv6_loopback_keeps_its_brackets_in_the_authority() {
        let endpoint = parse_node_url("http://[::1]:3131").expect("bracketed ipv6 parses");
        assert_eq!(endpoint.connect_host(), "::1");
        assert_eq!(endpoint.authority(), "[::1]:3131");
        assert_eq!(endpoint.base_url(), "http://[::1]:3131");
    }

    #[test]
    fn port_defaults_to_the_console_port() {
        let endpoint = parse_node_url("http://localhost").expect("bare host parses");
        assert_eq!(endpoint.port(), DEFAULT_CONSOLE_PORT);
        let with_slash = parse_node_url("http://localhost/").expect("trailing slash parses");
        assert_eq!(with_slash.port(), DEFAULT_CONSOLE_PORT);
    }

    #[test]
    fn non_loopback_hosts_are_refused() {
        for url in [
            "http://10.0.0.5:3131",
            "http://example.com",
            "http://0.0.0.0:3131",
            "http://[2001:db8::1]:3131",
        ] {
            let error = parse_node_url(url).expect_err("must refuse non-loopback");
            assert!(
                error.contains("not loopback"),
                "unexpected error for {url}: {error}"
            );
        }
    }

    #[test]
    fn https_is_refused_with_an_explanation_rather_than_a_connect_failure() {
        let error = parse_node_url("https://127.0.0.1:3131").expect_err("must refuse https");
        assert!(error.contains("no TLS stack"), "{error}");
    }

    #[test]
    fn credentials_paths_and_queries_are_refused() {
        for url in [
            "http://user:pass@127.0.0.1:3131",
            "http://127.0.0.1:3131/api/status",
            "http://127.0.0.1:3131/?a=1",
        ] {
            assert!(parse_node_url(url).is_err(), "{url} should be refused");
        }
    }

    #[test]
    fn unbracketed_ipv6_gets_a_useful_message() {
        let error = parse_node_url("http://::1:3131").expect_err("ambiguous authority");
        assert!(error.contains("[::1]"), "{error}");
    }

    #[test]
    fn zero_and_garbage_ports_are_refused() {
        assert!(parse_node_url("http://127.0.0.1:0").is_err());
        assert!(parse_node_url("http://127.0.0.1:notaport").is_err());
        assert!(parse_node_url("http://127.0.0.1:70000").is_err());
    }

    #[test]
    fn series_caps_accept_both_flag_spellings_and_zero() {
        let spaced = settings_from(&args(&["--max-peer-series", "0"]), None).expect("spaced form");
        assert_eq!(spaced.max_peer_series, 0);
        let inline = settings_from(&args(&["--max-peer-series=8"]), None).expect("inline form");
        assert_eq!(inline.max_peer_series, 8);
    }

    #[test]
    fn series_caps_have_a_ceiling() {
        let error = settings_from(&args(&["--max-model-series", "99999"]), None)
            .expect_err("ceiling enforced");
        assert!(error.contains("bound cardinality"), "{error}");
    }

    #[test]
    fn timeout_is_range_checked() {
        assert!(settings_from(&args(&["--collect-timeout-ms", "0"]), None).is_err());
        assert!(settings_from(&args(&["--collect-timeout-ms", "600000"]), None).is_err());
        let ok = settings_from(&args(&["--collect-timeout-ms", "500"]), None).expect("in range");
        assert_eq!(ok.collect_timeout, Duration::from_millis(500));
    }

    #[test]
    fn a_missing_value_and_an_unknown_flag_both_name_themselves() {
        let missing = settings_from(&args(&["--node-url"]), None).expect_err("missing value");
        assert!(missing.contains("--node-url"), "{missing}");
        let unknown = settings_from(&args(&["--sneak"]), None).expect_err("unknown flag");
        assert!(unknown.contains("--sneak"), "{unknown}");
    }

    #[test]
    fn the_series_ceiling_matches_the_documented_default() {
        let settings = settings_from(&[], None).expect("defaults parse");
        // README > Cardinality quotes this number; keep them in step.
        assert_eq!(settings.max_series(), 635);
    }
}
