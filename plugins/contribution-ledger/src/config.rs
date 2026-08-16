//! Process configuration, parsed once at startup from `[[plugin]].args`,
//! `[[plugin]].url`, and the launch-contract environment.
//!
//! Deliberately *not* from `[plugin.settings]`. Host-stored settings never
//! reach the plugin process — there is no settings field in the launch contract
//! or the initialize handshake — so anything this process needs has to arrive
//! as an argument, as `TDCC_PLUGIN_URL`, or as its own state. The settings this
//! plugin declares in its config schema are read by its console bundle only.

use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

/// Poll interval used when `--poll-secs` is not given.
pub const DEFAULT_POLL_SECS: u64 = 30;
/// Below this the poller adds load without adding resolution.
pub const MIN_POLL_SECS: u64 = 5;
/// Above this a poll interval starts skipping whole hourly buckets.
pub const MAX_POLL_SECS: u64 = 900;

/// Hourly rows older than this are merged into daily rows.
pub const DEFAULT_COMPACT_AFTER_DAYS: u64 = 14;
/// Daily rows older than this are dropped.
pub const DEFAULT_RETAIN_DAYS: u64 = 400;
pub const MAX_RETAIN_DAYS: u64 = 3_650;

/// Default port of the TDCC operator console, which is also where the
/// management API (`/api/...`) is served. The OpenAI-compatible API on 9337 is
/// a different surface and does not answer `/api/status`.
pub const DEFAULT_CONSOLE_PORT: u16 = 3131;

/// A validated loopback HTTP target.
///
/// Constructing one is the only way the ledger can name a network destination,
/// and it refuses anything that is not plain HTTP to a loopback address. That
/// is the whole of the plugin's outbound network blast radius.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoopbackEndpoint {
    /// Literal loopback IP to connect to.
    pub ip: IpAddr,
    pub port: u16,
    /// Value for the `Host:` header, preserving the operator's spelling.
    pub authority: String,
}

impl LoopbackEndpoint {
    pub fn socket_addr(&self) -> (IpAddr, u16) {
        (self.ip, self.port)
    }
}

/// Where — or whether — the ledger samples the host's counters.
///
/// The distinction between the last two variants is the whole point of this
/// enum. An operator who passed `--no-host-api` knows the ledger is not
/// measuring anything and gets a quiet, clearly-labelled uptime record. An
/// operator who simply never set `[[plugin]].url` gets a loud error from every
/// tool that needs counters, because a ledger full of honest-looking zeroes is
/// worse than no ledger at all.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StatusSource {
    Endpoint(LoopbackEndpoint),
    /// Explicitly disabled with `--no-host-api`.
    OptedOut,
    /// Neither `--host-api` nor `[[plugin]].url` was configured.
    Unset,
}

impl StatusSource {
    pub fn endpoint(&self) -> Option<&LoopbackEndpoint> {
        match self {
            Self::Endpoint(endpoint) => Some(endpoint),
            _ => None,
        }
    }

    pub const fn mode(&self) -> &'static str {
        match self {
            Self::Endpoint(_) => "endpoint",
            Self::OptedOut => "opted_out",
            Self::Unset => "unset",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LedgerConfig {
    /// Directory holding `journal.jsonl` and `cursor.json`. The ledger reads
    /// and writes those two names here and nothing else, anywhere.
    pub state_dir: PathBuf,
    pub source: StatusSource,
    pub poll_secs: u64,
    /// Whether co-present peer id prefixes are written to disk at all.
    pub record_peers: bool,
    pub compact_after_days: u64,
    pub retain_days: u64,
}

/// Environment the parser reads, injected so tests do not touch the real one.
#[derive(Clone, Debug, Default)]
pub struct Env {
    pub plugin_url: Option<String>,
    pub plugin_dir: Option<String>,
    pub plugin_name: Option<String>,
    pub home: Option<String>,
}

impl Env {
    /// Read the launch contract from the real process environment.
    pub fn from_process() -> Self {
        Self {
            plugin_url: std::env::var("TDCC_PLUGIN_URL").ok(),
            plugin_dir: std::env::var("TDCC_PLUGIN_DIR").ok(),
            plugin_name: std::env::var("TDCC_PLUGIN_NAME").ok(),
            // `HOME` on Unix, `USERPROFILE` on Windows.
            home: std::env::var("HOME")
                .ok()
                .or_else(|| std::env::var("USERPROFILE").ok()),
        }
    }
}

/// Parse `[[plugin]].args` on top of the launch-contract environment.
///
/// `args` excludes the executable name. Unknown options are an error rather
/// than a warning: a typo in `config.toml` that silently disables the poller
/// would turn into a ledger full of honest-looking zeroes.
pub fn parse(args: &[String], env: &Env) -> Result<LedgerConfig> {
    let mut state_dir: Option<PathBuf> = None;
    let mut host_api_arg: Option<String> = None;
    let mut opted_out = false;
    let mut poll_secs = DEFAULT_POLL_SECS;
    let mut record_peers = true;
    let mut compact_after_days = DEFAULT_COMPACT_AFTER_DAYS;
    let mut retain_days = DEFAULT_RETAIN_DAYS;

    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        match flag {
            "--no-peer-ids" => {
                record_peers = false;
                index += 1;
            }
            "--no-host-api" => {
                opted_out = true;
                index += 1;
            }
            "--state-dir"
            | "--host-api"
            | "--poll-secs"
            | "--compact-after-days"
            | "--retain-days" => {
                let value = args
                    .get(index + 1)
                    .with_context(|| format!("{flag} needs a value"))?;
                match flag {
                    "--state-dir" => state_dir = Some(PathBuf::from(value)),
                    "--host-api" => host_api_arg = Some(value.clone()),
                    "--poll-secs" => {
                        poll_secs = value.parse().with_context(|| {
                            format!("--poll-secs expects an integer, got {value}")
                        })?;
                    }
                    "--compact-after-days" => {
                        compact_after_days = value.parse().with_context(|| {
                            format!("--compact-after-days expects an integer, got {value}")
                        })?;
                    }
                    "--retain-days" => {
                        retain_days = value.parse().with_context(|| {
                            format!("--retain-days expects an integer, got {value}")
                        })?;
                    }
                    _ => unreachable!("flag list and match arms are the same set"),
                }
                index += 2;
            }
            other => bail!(
                "unknown option {other}; supported: --state-dir, --host-api, --no-host-api, \
                 --poll-secs, --no-peer-ids, --compact-after-days, --retain-days"
            ),
        }
    }

    if !(MIN_POLL_SECS..=MAX_POLL_SECS).contains(&poll_secs) {
        bail!("--poll-secs must be between {MIN_POLL_SECS} and {MAX_POLL_SECS}, got {poll_secs}");
    }
    if !(1..=MAX_RETAIN_DAYS).contains(&retain_days) {
        bail!("--retain-days must be between 1 and {MAX_RETAIN_DAYS}, got {retain_days}");
    }
    if compact_after_days > retain_days {
        bail!(
            "--compact-after-days ({compact_after_days}) cannot exceed --retain-days ({retain_days})"
        );
    }

    // Precedence: `--no-host-api` wins outright, then an explicit `--host-api`,
    // then `[[plugin]].url` as delivered in `TDCC_PLUGIN_URL`.
    let source = if opted_out {
        StatusSource::OptedOut
    } else {
        match host_api_arg
            .or_else(|| env.plugin_url.clone())
            .as_deref()
            .map(str::trim)
        {
            None | Some("") => StatusSource::Unset,
            Some(raw) => StatusSource::Endpoint(parse_loopback_base(raw)?),
        }
    };

    let state_dir = match state_dir {
        Some(explicit) => explicit,
        None => default_state_dir(env),
    };

    Ok(LedgerConfig {
        state_dir,
        source,
        poll_secs,
        record_peers,
        compact_after_days,
        retain_days,
    })
}

/// `<plugin store root>/<plugin name>/ledger`.
///
/// Following `TDCC_PLUGIN_DIR` matters for development: pointing the store at a
/// scratch directory has to move the ledger's files too, or a test run writes
/// into the operator's real history.
fn default_state_dir(env: &Env) -> PathBuf {
    let name = env
        .plugin_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(crate::PLUGIN_NAME);
    let root = match env.plugin_dir.as_deref().map(str::trim) {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => match env.home.as_deref().map(str::trim) {
            Some(home) if !home.is_empty() => PathBuf::from(home).join(".tdcc").join("plugins"),
            // No home directory to anchor to. A relative path keeps the process
            // alive and keeps the files somewhere the operator can find them,
            // and `status` reports the resolved directory either way.
            _ => PathBuf::from(".tdcc").join("plugins"),
        },
    };
    root.join(name).join("ledger")
}

/// Parse an `http://<loopback>[:port]` base URL.
///
/// Rejects, on purpose:
///
/// - anything but `http://` — the ledger has no TLS stack and must never be
///   pointed at a remote endpoint by a config typo;
/// - any host that is not a literal loopback address or the exact word
///   `localhost`, which is mapped to `127.0.0.1` here rather than resolved,
///   so a hosts-file entry cannot redirect it off the machine;
/// - a path, query, fragment, or userinfo, so the only request this plugin can
///   ever issue is `GET /api/status` against a loopback socket.
pub fn parse_loopback_base(raw: &str) -> Result<LoopbackEndpoint> {
    let raw = raw.trim();
    let rest = match raw.split_once("://") {
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("http") => rest,
        Some((scheme, _)) => bail!(
            "host API base must use http:// (got {scheme}://): the ledger only ever talks to a \
             loopback address in the clear and has no TLS stack"
        ),
        None => bail!("host API base must be an absolute URL such as http://127.0.0.1:3131"),
    };

    // Split rather than trim, so `trailing` stays a suffix of `rest` and the
    // byte index below cannot drift.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let trailing = &rest[authority.len()..];
    if !trailing.is_empty() && trailing != "/" {
        bail!(
            "host API base must not carry a path, query, or fragment (got {trailing:?}); the \
             ledger appends /api/status itself"
        );
    }
    if authority.contains('@') {
        bail!("host API base must not carry credentials");
    }
    if authority.is_empty() {
        bail!("host API base is missing a host");
    }

    let (host, port) = split_authority(authority)?;
    let ip = resolve_loopback(&host)?;

    Ok(LoopbackEndpoint {
        ip,
        port: port.unwrap_or(80),
        authority: authority.to_string(),
    })
}

/// Split `host[:port]`, handling the bracketed IPv6 form.
fn split_authority(authority: &str) -> Result<(String, Option<u16>)> {
    if let Some(closing) = authority.strip_prefix('[') {
        let (host, remainder) = closing
            .split_once(']')
            .context("bracketed IPv6 host is missing its closing ]")?;
        let port = match remainder {
            "" => None,
            other => Some(parse_port(other.strip_prefix(':').with_context(|| {
                format!("expected :port after the bracketed host, got {other:?}")
            })?)?),
        };
        return Ok((host.to_string(), port));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) => Ok((host.to_string(), Some(parse_port(port)?))),
        None => Ok((authority.to_string(), None)),
    }
}

fn parse_port(raw: &str) -> Result<u16> {
    raw.parse()
        .with_context(|| format!("invalid port {raw:?} in host API base"))
}

/// Map a host string onto a loopback IP, refusing everything else.
fn resolve_loopback(host: &str) -> Result<IpAddr> {
    if host.eq_ignore_ascii_case("localhost") {
        return Ok(IpAddr::V4(Ipv4Addr::LOCALHOST));
    }
    let ip: IpAddr = host.parse().map_err(|_| {
        anyhow::anyhow!(
            "host API base must name a loopback address or localhost, got {host:?}; the ledger \
             refuses to send requests off this machine"
        )
    })?;
    let loopback = match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|m| m.is_loopback()),
    };
    if !loopback {
        bail!(
            "host API base {host} is not a loopback address; the ledger refuses to send requests \
             off this machine"
        );
    }
    Ok(ip)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> Env {
        Env {
            plugin_url: None,
            plugin_dir: Some("/store".to_string()),
            plugin_name: Some("contribution-ledger".to_string()),
            home: Some("/home/tester".to_string()),
        }
    }

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn defaults_need_no_arguments() {
        let config = parse(&[], &env()).expect("empty args are valid");

        assert_eq!(config.poll_secs, DEFAULT_POLL_SECS);
        assert!(config.record_peers);
        assert_eq!(
            config.source,
            StatusSource::Unset,
            "an unconfigured source must be distinguishable from a disabled one"
        );
        assert_eq!(
            config.state_dir,
            PathBuf::from("/store")
                .join("contribution-ledger")
                .join("ledger")
        );
    }

    #[test]
    fn plugin_url_supplies_the_host_api_when_no_flag_is_given() {
        let mut env = env();
        env.plugin_url = Some("http://127.0.0.1:3131".to_string());

        let config = parse(&[], &env).expect("a loopback url is accepted");

        let endpoint = config.source.endpoint().expect("url becomes the host api");
        assert_eq!(endpoint.port, 3131);
        assert_eq!(endpoint.authority, "127.0.0.1:3131");
    }

    #[test]
    fn an_explicit_flag_overrides_the_launch_contract_url() {
        let mut env = env();
        env.plugin_url = Some("http://127.0.0.1:3131".to_string());

        let config = parse(&args(&["--host-api", "http://127.0.0.1:9999"]), &env)
            .expect("an explicit override is valid");

        assert_eq!(config.source.endpoint().map(|e| e.port), Some(9999));
    }

    #[test]
    fn no_host_api_wins_over_the_launch_contract_url() {
        let mut env = env();
        env.plugin_url = Some("http://127.0.0.1:3131".to_string());

        let config = parse(&args(&["--no-host-api"]), &env).expect("opting out is valid");

        assert_eq!(config.source, StatusSource::OptedOut);
        assert_eq!(config.source.endpoint(), None);
    }

    #[test]
    fn state_dir_falls_back_to_home_then_to_a_relative_path() {
        let mut env = env();
        env.plugin_dir = None;
        assert_eq!(
            parse(&[], &env).unwrap().state_dir,
            PathBuf::from("/home/tester/.tdcc/plugins/contribution-ledger/ledger")
        );

        env.home = None;
        assert_eq!(
            parse(&[], &env).unwrap().state_dir,
            PathBuf::from(".tdcc/plugins/contribution-ledger/ledger")
        );
    }

    #[test]
    fn out_of_range_and_unknown_options_are_rejected() {
        for bad in [
            args(&["--poll-secs", "1"]),
            args(&["--poll-secs", "100000"]),
            args(&["--poll-secs", "soon"]),
            args(&["--retain-days", "0"]),
            args(&["--compact-after-days", "500", "--retain-days", "30"]),
            args(&["--state-dir"]),
            args(&["--pol-secs", "30"]),
        ] {
            assert!(parse(&bad, &env()).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn loopback_bases_are_accepted_in_every_spelling() {
        for (raw, expected_port) in [
            ("http://127.0.0.1:3131", 3131),
            ("http://127.0.0.1:3131/", 3131),
            ("HTTP://localhost:3131", 3131),
            ("http://[::1]:3131", 3131),
            ("http://127.0.0.1", 80),
        ] {
            let endpoint = parse_loopback_base(raw).unwrap_or_else(|err| panic!("{raw}: {err}"));
            assert_eq!(endpoint.port, expected_port, "{raw}");
            assert!(endpoint.ip.is_loopback(), "{raw}");
        }
    }

    #[test]
    fn non_loopback_and_non_http_bases_are_refused() {
        for raw in [
            "https://127.0.0.1:3131",
            "http://10.0.0.4:3131",
            "http://example.com:3131",
            "http://127.0.0.1:3131/api/status",
            "http://user:pass@127.0.0.1:3131",
            "http://127.0.0.1:notaport",
            "127.0.0.1:3131",
            "http://",
        ] {
            assert!(parse_loopback_base(raw).is_err(), "{raw} should be refused");
        }
    }
}
