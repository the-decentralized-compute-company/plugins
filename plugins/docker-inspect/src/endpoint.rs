//! Where the Docker Engine API lives, and which forms of that answer this
//! plugin is willing to accept.
//!
//! Three transports exist in the wild and all three are parsed here so the
//! error for an unsupported one is written once: a Unix socket (macOS, Linux),
//! a Windows named pipe, and a TCP endpoint. Parsing is deliberately platform
//! independent — a `unix://` value is understood on Windows and then refused
//! with a sentence explaining that this build cannot open one, rather than
//! producing a confusing IO error later.
//!
//! Two forms are refused outright and by name:
//!
//! * `https://` — this binary links no TLS stack at all (see `Cargo.toml`), so
//!   it cannot verify a certificate or present a client one. Pretending
//!   otherwise would mean sending Docker API traffic in the clear to something
//!   the operator believed was authenticated.
//! * `ssh://` — that form means "shell out to `ssh`", and this plugin spawns no
//!   subprocesses.

use std::fmt;
use std::path::PathBuf;

/// Where a Docker daemon listens on macOS and Linux unless told otherwise.
pub const DEFAULT_UNIX_SOCKET: &str = "/var/run/docker.sock";
/// Where Docker Desktop listens on Windows unless told otherwise.
pub const DEFAULT_NAMED_PIPE: &str = r"\\.\pipe\docker_engine";
/// The port Docker uses for a cleartext TCP endpoint. `2376` is the TLS port,
/// which this plugin cannot speak.
pub const DEFAULT_TCP_PORT: u16 = 2375;

/// A resolved Docker endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Endpoint {
    /// A Unix domain socket, the default on macOS and Linux.
    Unix(PathBuf),
    /// A Windows named pipe, the default on Windows.
    NamedPipe(String),
    /// A cleartext TCP endpoint. Requires `--allow-tcp`; see `settings.rs`.
    Tcp { host: String, port: u16 },
}

impl Endpoint {
    /// The default for the platform this binary was built for.
    pub fn platform_default() -> Self {
        if cfg!(windows) {
            Self::NamedPipe(DEFAULT_NAMED_PIPE.to_string())
        } else {
            Self::Unix(PathBuf::from(DEFAULT_UNIX_SOCKET))
        }
    }

    /// Short machine-readable transport name, reported by the `status` tool.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Unix(_) => "unix",
            Self::NamedPipe(_) => "npipe",
            Self::Tcp { .. } => "tcp",
        }
    }

    /// The value of the `Host:` header. Local transports have no meaningful
    /// authority, and Docker ignores it, but a well-formed HTTP/1.1 request
    /// needs one.
    pub fn host_header(&self) -> String {
        match self {
            Self::Unix(_) | Self::NamedPipe(_) => "localhost".to_string(),
            Self::Tcp { host, port } => format_authority(host, *port),
        }
    }

    /// Whether this endpoint carries API traffic over a network rather than a
    /// local IPC object. Only a TCP endpoint does, and only that one needs the
    /// operator to opt in.
    pub fn is_network(&self) -> bool {
        matches!(self, Self::Tcp { .. })
    }

    /// Whether this build can open this kind of endpoint at all.
    ///
    /// A Unix socket on Windows (or a named pipe on Unix) is an operator
    /// mistake worth naming at startup rather than a connect error at the first
    /// tool call.
    pub fn platform_support(&self) -> Result<(), String> {
        match self {
            Self::Unix(_) if cfg!(not(unix)) => Err(format!(
                "a Unix socket endpoint ({self}) cannot be opened by a Windows build of \
                 docker-inspect. Use the named pipe `{DEFAULT_NAMED_PIPE}` (the default here), or \
                 a `tcp://` endpoint with --allow-tcp."
            )),
            Self::NamedPipe(_) if cfg!(not(windows)) => Err(format!(
                "a Windows named pipe endpoint ({self}) cannot be opened on this platform. Use a \
                 Unix socket such as `{DEFAULT_UNIX_SOCKET}` (the default here), or a `tcp://` \
                 endpoint with --allow-tcp."
            )),
            _ => Ok(()),
        }
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unix(path) => write!(formatter, "unix://{}", path.display()),
            Self::NamedPipe(path) => write!(formatter, "npipe://{path}"),
            Self::Tcp { host, port } => {
                write!(formatter, "tcp://{}", format_authority(host, *port))
            }
        }
    }
}

/// Bracket an IPv6 literal so `host:port` stays unambiguous.
fn format_authority(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Parse one endpoint value, in any of the forms `DOCKER_HOST` accepts.
///
/// `source` names where the value came from (`--endpoint`, an environment
/// variable, `[[plugin]].url`) so a rejection points at the thing the operator
/// actually wrote.
pub fn parse_endpoint(raw: &str, source: &str) -> Result<Endpoint, String> {
    let value = raw.trim();
    if value.is_empty() {
        return Err(format!("{source} is empty."));
    }

    if let Some(rest) = strip_scheme(value, "unix") {
        let path = rest.trim();
        if path.is_empty() {
            return Err(format!(
                "{source} is `unix://` with no socket path. Write the full path, for example \
                 `unix://{DEFAULT_UNIX_SOCKET}`."
            ));
        }
        return Ok(Endpoint::Unix(PathBuf::from(path)));
    }

    if let Some(rest) = strip_scheme(value, "npipe") {
        return parse_named_pipe(rest, source);
    }

    if let Some(rest) = strip_scheme(value, "tcp") {
        return parse_tcp(rest, source, "tcp://");
    }

    if let Some(rest) = strip_scheme(value, "http") {
        return parse_tcp(rest, source, "http://");
    }

    if strip_scheme(value, "https").is_some() {
        return Err(format!(
            "{source} is an `https://` endpoint, which docker-inspect cannot use: this plugin \
             links no TLS stack, so it can neither verify the daemon's certificate nor present a \
             client one. Reach the daemon over its local socket or pipe instead. (A TLS Docker \
             endpoint usually means a remote host — that is a different machine's containers, \
             which is not what this plugin is for.)"
        ));
    }

    if strip_scheme(value, "ssh").is_some() {
        return Err(format!(
            "{source} is an `ssh://` endpoint. That form works by running the `ssh` binary, and \
             docker-inspect spawns no subprocesses. Use the local socket or pipe."
        ));
    }

    // Bare paths, because operators write them and both are unambiguous.
    if value.starts_with(r"\\") || value.to_ascii_lowercase().contains(r"\pipe\") {
        return parse_named_pipe(value, source);
    }
    if value.starts_with('/') {
        return Ok(Endpoint::Unix(PathBuf::from(value)));
    }

    Err(format!(
        "{source} is `{value}`, which is not a Docker endpoint this plugin recognises. Use \
         `unix:///var/run/docker.sock`, `npipe:////./pipe/docker_engine`, `tcp://host:port`, or a \
         bare socket path starting with `/`."
    ))
}

/// Case-insensitive `scheme://` prefix match, returning what follows it.
fn strip_scheme<'a>(value: &'a str, scheme: &str) -> Option<&'a str> {
    let prefix = format!("{scheme}://");
    if value.len() >= prefix.len() && value[..prefix.len()].eq_ignore_ascii_case(&prefix) {
        Some(&value[prefix.len()..])
    } else {
        None
    }
}

/// Normalise the several ways a named pipe gets written.
///
/// `DOCKER_HOST` uses `npipe:////./pipe/docker_engine`; people also write
/// `npipe://./pipe/docker_engine` and the native `\\.\pipe\docker_engine`. All
/// three name the same object, so all three are accepted and normalised to the
/// native form that `CreateFile` wants.
fn parse_named_pipe(rest: &str, source: &str) -> Result<Endpoint, String> {
    let trimmed = rest.trim().replace('/', r"\");
    let without_leading = trimmed.trim_start_matches('\\');
    if without_leading.is_empty() {
        return Err(format!(
            "{source} names no pipe. Write the full name, for example \
             `npipe:////./pipe/docker_engine`."
        ));
    }
    Ok(Endpoint::NamedPipe(format!(r"\\{without_leading}")))
}

/// Parse a `host:port` authority, tolerating an IPv6 literal and a bare `/`
/// path, and refusing anything with a real path because Docker API paths are
/// built by this plugin and not by an operator.
fn parse_tcp(rest: &str, source: &str, scheme: &str) -> Result<Endpoint, String> {
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, ""),
    };
    if !path.is_empty() && path != "/" {
        return Err(format!(
            "{source} has a path (`{path}`). A Docker endpoint is just a host and port, for \
             example `{scheme}127.0.0.1:{DEFAULT_TCP_PORT}`."
        ));
    }
    if authority.contains('@') {
        return Err(format!(
            "{source} contains credentials. The Docker API has no username or password; a TCP \
             endpoint is either open to whoever can reach it or protected by TLS client \
             certificates, which this plugin does not speak."
        ));
    }

    let (host, port) = split_authority(authority)
        .ok_or_else(|| format!("{source} is not a valid `host:port` value: `{authority}`"))?;
    if host.is_empty() {
        return Err(format!("{source} has no host: `{authority}`"));
    }
    let port = match port {
        Some(raw) => raw.parse::<u16>().map_err(|_| {
            format!("{source} has an invalid port `{raw}`; it must be a number from 1 to 65535.")
        })?,
        None => DEFAULT_TCP_PORT,
    };
    if port == 0 {
        return Err(format!(
            "{source} has port 0, which cannot be connected to."
        ));
    }
    Ok(Endpoint::Tcp {
        host: host.to_string(),
        port,
    })
}

/// Split `host:port`, `host`, `[v6]:port`, or `[v6]` into its two parts.
fn split_authority(authority: &str) -> Option<(&str, Option<&str>)> {
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        return match tail {
            "" => Some((host, None)),
            _ => tail.strip_prefix(':').map(|port| (host, Some(port))),
        };
    }
    match authority.rsplit_once(':') {
        // A bare IPv6 literal without brackets: `::1` is a host, not host:port.
        Some(_) if authority.matches(':').count() > 1 => Some((authority, None)),
        Some((host, port)) => Some((host, Some(port))),
        None => Some((authority, None)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> Result<Endpoint, String> {
        parse_endpoint(raw, "`--endpoint`")
    }

    #[test]
    fn the_docker_host_unix_form_is_accepted() {
        assert_eq!(
            parse("unix:///var/run/docker.sock"),
            Ok(Endpoint::Unix(PathBuf::from("/var/run/docker.sock")))
        );
        assert_eq!(
            parse("  unix:///run/user/1000/docker.sock  "),
            Ok(Endpoint::Unix(PathBuf::from("/run/user/1000/docker.sock")))
        );
    }

    #[test]
    fn a_bare_absolute_path_is_a_unix_socket() {
        assert_eq!(
            parse("/var/run/docker.sock"),
            Ok(Endpoint::Unix(PathBuf::from("/var/run/docker.sock")))
        );
    }

    #[test]
    fn every_way_of_writing_a_named_pipe_normalises_to_the_native_form() {
        let expected = Endpoint::NamedPipe(r"\\.\pipe\docker_engine".to_string());
        for raw in [
            "npipe:////./pipe/docker_engine",
            "npipe://./pipe/docker_engine",
            r"\\.\pipe\docker_engine",
            r"npipe://\\.\pipe\docker_engine",
        ] {
            assert_eq!(parse(raw), Ok(expected.clone()), "{raw}");
        }
    }

    #[test]
    fn tcp_endpoints_parse_with_and_without_a_port() {
        assert_eq!(
            parse("tcp://127.0.0.1:2375"),
            Ok(Endpoint::Tcp {
                host: "127.0.0.1".into(),
                port: 2375
            })
        );
        assert_eq!(
            parse("tcp://dockerhost"),
            Ok(Endpoint::Tcp {
                host: "dockerhost".into(),
                port: DEFAULT_TCP_PORT
            })
        );
        assert_eq!(
            parse("http://127.0.0.1:2375/"),
            Ok(Endpoint::Tcp {
                host: "127.0.0.1".into(),
                port: 2375
            })
        );
    }

    #[test]
    fn ipv6_literals_keep_their_brackets_out_of_the_host() {
        assert_eq!(
            parse("tcp://[::1]:2375"),
            Ok(Endpoint::Tcp {
                host: "::1".into(),
                port: 2375
            })
        );
        assert_eq!(
            parse("tcp://[fd00::1]"),
            Ok(Endpoint::Tcp {
                host: "fd00::1".into(),
                port: DEFAULT_TCP_PORT
            })
        );
        assert_eq!(
            Endpoint::Tcp {
                host: "::1".into(),
                port: 2375
            }
            .host_header(),
            "[::1]:2375"
        );
    }

    #[test]
    fn https_is_refused_by_name_because_this_binary_has_no_tls() {
        let error = parse("https://docker.example:2376").expect_err("https is refused");
        assert!(error.contains("no TLS stack"), "{error}");
    }

    #[test]
    fn ssh_is_refused_because_the_plugin_spawns_nothing() {
        let error = parse("ssh://user@host").expect_err("ssh is refused");
        assert!(error.contains("subprocess"), "{error}");
    }

    #[test]
    fn a_tcp_endpoint_with_a_path_or_credentials_is_refused() {
        assert!(
            parse("tcp://127.0.0.1:2375/containers/json")
                .expect_err("a path is refused")
                .contains("path")
        );
        assert!(
            parse("tcp://user:pass@127.0.0.1:2375")
                .expect_err("credentials are refused")
                .contains("credentials")
        );
    }

    #[test]
    fn an_unparseable_port_names_the_setting_rather_than_defaulting() {
        let error = parse("tcp://127.0.0.1:http").expect_err("a non-numeric port is refused");
        assert!(error.contains("--endpoint"), "{error}");
        assert!(error.contains("invalid port"), "{error}");
    }

    #[test]
    fn an_unrecognised_value_lists_the_forms_that_do_work() {
        let error = parse("dockerhost:2375").expect_err("a bare authority is ambiguous");
        assert!(error.contains("tcp://host:port"), "{error}");
        assert!(error.contains("unix://"), "{error}");
    }

    #[test]
    fn only_a_tcp_endpoint_counts_as_network_access() {
        assert!(
            Endpoint::Tcp {
                host: "10.0.0.5".into(),
                port: 2375
            }
            .is_network()
        );
        assert!(!Endpoint::Unix(PathBuf::from("/var/run/docker.sock")).is_network());
        assert!(!Endpoint::NamedPipe(DEFAULT_NAMED_PIPE.into()).is_network());
    }

    #[test]
    fn the_wrong_local_transport_for_this_build_is_named_at_startup() {
        let unix = Endpoint::Unix(PathBuf::from("/var/run/docker.sock"));
        let pipe = Endpoint::NamedPipe(DEFAULT_NAMED_PIPE.to_string());

        if cfg!(windows) {
            assert!(unix.platform_support().is_err());
            assert!(pipe.platform_support().is_ok());
        } else {
            assert!(unix.platform_support().is_ok());
            assert!(pipe.platform_support().is_err());
        }
        assert!(
            Endpoint::Tcp {
                host: "127.0.0.1".into(),
                port: 2375
            }
            .platform_support()
            .is_ok()
        );
    }

    #[test]
    fn the_platform_default_matches_the_build_target() {
        let default = Endpoint::platform_default();
        if cfg!(windows) {
            assert_eq!(default, Endpoint::NamedPipe(DEFAULT_NAMED_PIPE.to_string()));
        } else {
            assert_eq!(default, Endpoint::Unix(PathBuf::from(DEFAULT_UNIX_SOCKET)));
        }
        assert!(default.platform_support().is_ok());
    }

    #[test]
    fn display_round_trips_back_into_the_parser() {
        for raw in [
            "unix:///var/run/docker.sock",
            "npipe:////./pipe/docker_engine",
            "tcp://127.0.0.1:2375",
        ] {
            let parsed = parse(raw).expect("parses");
            let rendered = parsed.to_string();
            assert_eq!(parse(&rendered), Ok(parsed), "{rendered}");
        }
    }
}
