//! The server list: what an operator writes, and what it is allowed to say.
//!
//! Parsing and validation are pure — `&str` in, a [`Document`] or a list of
//! complaints out. Nothing here reads the clock, the filesystem, the network,
//! or the process environment.
//!
//! Four decisions in this module are worth reading before changing anything:
//!
//! * **Every table sets `deny_unknown_fields`.** A silently ignored
//!   `deny_tool = ["write_file"]` — singular, a plausible typo — is a denylist
//!   that does not exist on a machine whose owner believes it does.
//! * **Validation is all-or-nothing.** A document with one bad server produces
//!   no document at all. Loading the rest would launch a subset of the
//!   operator's servers under a configuration they never approved.
//! * **A credential is a variable *name*, never a value.** `bearer_token_env`
//!   takes the name of an environment variable in the `tdcc` process; a value
//!   that does not look like a variable name is refused, because this file sits
//!   in a home directory and gets pasted into issues.
//! * **`TDCC_PLUGIN_*` can never be forwarded to a child.** That prefix carries
//!   the node's plugin control endpoint. A third-party binary holding it could
//!   open the host's control connection and register itself as a plugin.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use url::Url;

use crate::naming::validate_alias;

/// The only document version this build understands.
pub const DOCUMENT_VERSION: u32 = 1;

/// Upper bound on servers in one document. Every one of them is a process or a
/// connection this node holds open.
pub const MAX_SERVERS: usize = 32;
/// Upper bound on `allow_tools` / `deny_tools` entries per server.
pub const MAX_PATTERNS: usize = 256;
/// Upper bound on `args` entries per server.
pub const MAX_ARGS: usize = 64;
/// Upper bound on `env` / `env_from` entries per server.
pub const MAX_ENV_ENTRIES: usize = 64;

pub const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 30;
pub const DEFAULT_CALL_TIMEOUT_SECS: u64 = 120;
pub const DEFAULT_MAX_RESULT_BYTES: u64 = 4_000_000;
pub const DEFAULT_MAX_TOOLS_PER_SERVER: u64 = 128;

const MIN_CONNECT_TIMEOUT_SECS: u64 = 1;
const MAX_CONNECT_TIMEOUT_SECS: u64 = 600;
const MIN_CALL_TIMEOUT_SECS: u64 = 1;
const MAX_CALL_TIMEOUT_SECS: u64 = 3_600;
const MIN_MAX_RESULT_BYTES: u64 = 1_024;
const MAX_MAX_RESULT_BYTES: u64 = 64 * 1_024 * 1_024;
const MAX_TOOLS_CEILING: u64 = 512;

/// Environment variable prefixes that are never handed to a bridged server,
/// whatever the configuration says.
///
/// `TDCC_PLUGIN_ENDPOINT` and `TDCC_PLUGIN_TRANSPORT` are the node's plugin
/// control connection. `MESH_LLM_PLUGIN_*` is the host's pre-rename mirror of
/// exactly the same values. `TDCC_MCP_BRIDGE_*` is this plugin's own
/// configuration and a bridged server has no business reading it.
pub const RESERVED_ENV_PREFIXES: &[&str] =
    &["TDCC_PLUGIN_", "MESH_LLM_PLUGIN_", "TDCC_MCP_BRIDGE_"];

// ---------------------------------------------------------------------------
// The validated shapes
// ---------------------------------------------------------------------------

/// How this node reaches one upstream server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    /// A child process this node launches and supervises, speaking MCP over
    /// its stdin and stdout.
    Stdio {
        /// Resolved through `PATH` at launch time. Not run through a shell.
        command: String,
        args: Vec<String>,
        cwd: Option<PathBuf>,
        /// Literal values from the server list. Never credentials — the file
        /// is not a secret store.
        env: BTreeMap<String, String>,
        /// Names copied from the `tdcc` process environment, so a key stays in
        /// the environment and out of the file.
        env_from: Vec<String>,
        /// Hand the child the whole `tdcc` environment. Off by default;
        /// [`RESERVED_ENV_PREFIXES`] is stripped even when it is on.
        inherit_env: bool,
    },
    /// An already-running server reached over MCP Streamable HTTP.
    Http {
        url: Url,
        /// Name of the environment variable holding a bearer token, if the
        /// server needs one. Never the token itself.
        bearer_token_env: Option<String>,
    },
}

impl Transport {
    /// A label safe to print in a tool response, a log line, or an error.
    ///
    /// The URL is rendered without userinfo — which [`parse_document`] refuses
    /// anyway — and a token variable name is named but never resolved here.
    pub fn label(&self) -> String {
        match self {
            Self::Stdio { command, .. } => format!("stdio: {command}"),
            Self::Http { url, .. } => format!("http: {url}"),
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Stdio { .. } => "stdio",
            Self::Http { .. } => "http",
        }
    }
}

/// One upstream server, validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSpec {
    /// The operator's name for this server. Prefixes every tool it publishes.
    pub alias: String,
    pub enabled: bool,
    pub transport: Transport,
    pub allow_tools: Vec<String>,
    pub deny_tools: Vec<String>,
    pub connect_timeout: Duration,
    pub call_timeout: Duration,
    pub max_result_bytes: usize,
    pub max_tools: usize,
    /// Reconnect with backoff when the connection drops.
    pub restart: bool,
    /// Free text from the operator, shown in `status`. Not sent anywhere.
    pub description: Option<String>,
}

/// A validated server list.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Document {
    pub servers: Vec<ServerSpec>,
}

impl Document {
    pub fn enabled_servers(&self) -> impl Iterator<Item = &ServerSpec> {
        self.servers.iter().filter(|server| server.enabled)
    }
}

// ---------------------------------------------------------------------------
// The raw shapes, exactly as an operator writes them
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDocument {
    version: u32,
    #[serde(default)]
    defaults: RawDefaults,
    #[serde(default, rename = "server")]
    servers: Vec<RawServer>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDefaults {
    connect_timeout_secs: Option<u64>,
    call_timeout_secs: Option<u64>,
    max_result_bytes: Option<u64>,
    max_tools_per_server: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServer {
    alias: String,
    transport: String,
    enabled: Option<bool>,
    description: Option<String>,

    command: Option<String>,
    args: Option<Vec<String>>,
    cwd: Option<String>,
    env: Option<BTreeMap<String, String>>,
    env_from: Option<Vec<String>>,
    inherit_env: Option<bool>,

    url: Option<String>,
    bearer_token_env: Option<String>,

    allow_tools: Option<Vec<String>>,
    deny_tools: Option<Vec<String>>,
    connect_timeout_secs: Option<u64>,
    call_timeout_secs: Option<u64>,
    max_result_bytes: Option<u64>,
    max_tools: Option<u64>,
    restart: Option<bool>,
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parse and validate a server list.
///
/// Every complaint the document produces is reported at once, because an
/// operator fixing a config file would rather see four problems than find them
/// one restart at a time.
pub fn parse_document(text: &str) -> Result<Document, String> {
    let raw: RawDocument = toml::from_str(text).map_err(|error| {
        format!(
            "the server list is not valid TOML, or contains a key this build does not know: {error}"
        )
    })?;

    if raw.version != DOCUMENT_VERSION {
        return Err(format!(
            "server list version is {}, but this build of mcp-bridge understands version \
             {DOCUMENT_VERSION}",
            raw.version
        ));
    }

    let mut problems: Vec<String> = Vec::new();

    if raw.servers.len() > MAX_SERVERS {
        problems.push(format!(
            "the server list has {} servers; the limit is {MAX_SERVERS}. Each one is a process or \
             connection this node holds open.",
            raw.servers.len()
        ));
    }

    let defaults = Defaults::resolve(&raw.defaults, &mut problems);

    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut servers = Vec::new();
    for (index, raw_server) in raw.servers.iter().enumerate() {
        let position = format!("[[server]] #{} (alias '{}')", index + 1, raw_server.alias);
        if !seen.insert(raw_server.alias.clone()) {
            problems.push(format!(
                "{position}: alias '{}' is used more than once. Aliases are what keep two servers' \
                 tools apart, so they have to be unique.",
                raw_server.alias
            ));
        }
        match validate_server(raw_server, &defaults, &position) {
            Ok(server) => servers.push(server),
            Err(mut server_problems) => problems.append(&mut server_problems),
        }
    }

    if !problems.is_empty() {
        // All-or-nothing on purpose: half a server list is a configuration the
        // operator never wrote.
        return Err(problems.join("\n"));
    }

    Ok(Document { servers })
}

struct Defaults {
    connect_timeout_secs: u64,
    call_timeout_secs: u64,
    max_result_bytes: u64,
    max_tools_per_server: u64,
}

impl Defaults {
    fn resolve(raw: &RawDefaults, problems: &mut Vec<String>) -> Self {
        Self {
            connect_timeout_secs: bounded(
                raw.connect_timeout_secs,
                DEFAULT_CONNECT_TIMEOUT_SECS,
                MIN_CONNECT_TIMEOUT_SECS,
                MAX_CONNECT_TIMEOUT_SECS,
                "[defaults].connect_timeout_secs",
                problems,
            ),
            call_timeout_secs: bounded(
                raw.call_timeout_secs,
                DEFAULT_CALL_TIMEOUT_SECS,
                MIN_CALL_TIMEOUT_SECS,
                MAX_CALL_TIMEOUT_SECS,
                "[defaults].call_timeout_secs",
                problems,
            ),
            max_result_bytes: bounded(
                raw.max_result_bytes,
                DEFAULT_MAX_RESULT_BYTES,
                MIN_MAX_RESULT_BYTES,
                MAX_MAX_RESULT_BYTES,
                "[defaults].max_result_bytes",
                problems,
            ),
            max_tools_per_server: bounded(
                raw.max_tools_per_server,
                DEFAULT_MAX_TOOLS_PER_SERVER,
                1,
                MAX_TOOLS_CEILING,
                "[defaults].max_tools_per_server",
                problems,
            ),
        }
    }
}

fn bounded(
    value: Option<u64>,
    default: u64,
    min: u64,
    max: u64,
    label: &str,
    problems: &mut Vec<String>,
) -> u64 {
    match value {
        None => default,
        Some(value) if value >= min && value <= max => value,
        Some(value) => {
            problems.push(format!(
                "{label} is {value}; it must be between {min} and {max}"
            ));
            default
        }
    }
}

fn validate_server(
    raw: &RawServer,
    defaults: &Defaults,
    position: &str,
) -> Result<ServerSpec, Vec<String>> {
    let mut problems = Vec::new();

    if let Err(error) = validate_alias(&raw.alias) {
        problems.push(format!("{position}: {error}"));
    }

    let transport = match raw.transport.trim() {
        "stdio" => validate_stdio(raw, position, &mut problems),
        "http" => validate_http(raw, position, &mut problems),
        other => {
            problems.push(format!(
                "{position}: transport is '{other}'. Use \"stdio\" for a server this node \
                 launches, or \"http\" for an already-running server reached over MCP Streamable \
                 HTTP."
            ));
            None
        }
    };

    let allow_tools = validate_patterns(
        raw.allow_tools.as_deref().unwrap_or_default(),
        "allow_tools",
        position,
        &mut problems,
    );
    let deny_tools = validate_patterns(
        raw.deny_tools.as_deref().unwrap_or_default(),
        "deny_tools",
        position,
        &mut problems,
    );

    let connect_timeout_secs = bounded(
        raw.connect_timeout_secs,
        defaults.connect_timeout_secs,
        MIN_CONNECT_TIMEOUT_SECS,
        MAX_CONNECT_TIMEOUT_SECS,
        &format!("{position}: connect_timeout_secs"),
        &mut problems,
    );
    let call_timeout_secs = bounded(
        raw.call_timeout_secs,
        defaults.call_timeout_secs,
        MIN_CALL_TIMEOUT_SECS,
        MAX_CALL_TIMEOUT_SECS,
        &format!("{position}: call_timeout_secs"),
        &mut problems,
    );
    let max_result_bytes = bounded(
        raw.max_result_bytes,
        defaults.max_result_bytes,
        MIN_MAX_RESULT_BYTES,
        MAX_MAX_RESULT_BYTES,
        &format!("{position}: max_result_bytes"),
        &mut problems,
    );
    let max_tools = bounded(
        raw.max_tools,
        defaults.max_tools_per_server,
        1,
        MAX_TOOLS_CEILING,
        &format!("{position}: max_tools"),
        &mut problems,
    );

    if let Some(description) = &raw.description
        && description.len() > 500
    {
        problems.push(format!(
            "{position}: description is {} characters; the limit is 500",
            description.len()
        ));
    }

    match (transport, problems.is_empty()) {
        (Some(transport), true) => Ok(ServerSpec {
            alias: raw.alias.clone(),
            enabled: raw.enabled.unwrap_or(true),
            transport,
            allow_tools,
            deny_tools,
            connect_timeout: Duration::from_secs(connect_timeout_secs),
            call_timeout: Duration::from_secs(call_timeout_secs),
            max_result_bytes: max_result_bytes as usize,
            max_tools: max_tools as usize,
            restart: raw.restart.unwrap_or(true),
            description: raw.description.clone(),
        }),
        _ => Err(problems),
    }
}

fn validate_stdio(
    raw: &RawServer,
    position: &str,
    problems: &mut Vec<String>,
) -> Option<Transport> {
    for (field, present) in [
        ("url", raw.url.is_some()),
        ("bearer_token_env", raw.bearer_token_env.is_some()),
    ] {
        if present {
            problems.push(format!(
                "{position}: {field} belongs to transport = \"http\" and has no meaning for a \
                 stdio server"
            ));
        }
    }

    let command = match raw.command.as_deref().map(str::trim) {
        Some(command) if !command.is_empty() => command.to_string(),
        _ => {
            problems.push(format!(
                "{position}: transport = \"stdio\" needs a command, for example \
                 command = \"npx\". It is executed directly, never through a shell, so put each \
                 argument in args rather than writing a command line."
            ));
            return None;
        }
    };

    let args = raw.args.clone().unwrap_or_default();
    if args.len() > MAX_ARGS {
        problems.push(format!(
            "{position}: {} args; the limit is {MAX_ARGS}",
            args.len()
        ));
    }

    let env = raw.env.clone().unwrap_or_default();
    if env.len() > MAX_ENV_ENTRIES {
        problems.push(format!(
            "{position}: {} env entries; the limit is {MAX_ENV_ENTRIES}",
            env.len()
        ));
    }
    for name in env.keys() {
        validate_env_name(name, "env", position, problems);
    }

    let env_from = raw.env_from.clone().unwrap_or_default();
    if env_from.len() > MAX_ENV_ENTRIES {
        problems.push(format!(
            "{position}: {} env_from entries; the limit is {MAX_ENV_ENTRIES}",
            env_from.len()
        ));
    }
    for name in &env_from {
        validate_env_name(name, "env_from", position, problems);
    }

    let cwd = match raw.cwd.as_deref().map(str::trim) {
        Some("") => {
            problems.push(format!(
                "{position}: cwd is empty; remove it or give a directory"
            ));
            None
        }
        Some(path) => Some(PathBuf::from(path)),
        None => None,
    };

    Some(Transport::Stdio {
        command,
        args,
        cwd,
        env,
        env_from,
        inherit_env: raw.inherit_env.unwrap_or(false),
    })
}

fn validate_http(raw: &RawServer, position: &str, problems: &mut Vec<String>) -> Option<Transport> {
    for (field, present) in [
        ("command", raw.command.is_some()),
        ("args", raw.args.is_some()),
        ("cwd", raw.cwd.is_some()),
        ("env", raw.env.is_some()),
        ("env_from", raw.env_from.is_some()),
        ("inherit_env", raw.inherit_env.is_some()),
    ] {
        if present {
            problems.push(format!(
                "{position}: {field} belongs to transport = \"stdio\" and has no meaning for an \
                 http server — this node does not launch it"
            ));
        }
    }

    let raw_url = match raw.url.as_deref().map(str::trim) {
        Some(url) if !url.is_empty() => url,
        _ => {
            problems.push(format!(
                "{position}: transport = \"http\" needs a url, for example \
                 url = \"http://127.0.0.1:7777/mcp\""
            ));
            return None;
        }
    };

    let url = match Url::parse(raw_url) {
        Ok(url) => url,
        Err(error) => {
            problems.push(format!(
                "{position}: url is not a valid URL ({error}): {raw_url}"
            ));
            return None;
        }
    };
    if !matches!(url.scheme(), "http" | "https") {
        problems.push(format!(
            "{position}: url scheme is '{}'; only http and https are supported",
            url.scheme()
        ));
        return None;
    }
    if url.host_str().is_none() {
        problems.push(format!("{position}: url has no host: {raw_url}"));
        return None;
    }
    if !url.username().is_empty() || url.password().is_some() {
        // Credentials in a URL end up in every log line and error message that
        // ever prints the endpoint. Refuse rather than redact.
        problems.push(format!(
            "{position}: url carries a username or password. Remove them and use \
             bearer_token_env, which names an environment variable instead of storing a \
             credential in this file."
        ));
        return None;
    }

    let bearer_token_env = match raw.bearer_token_env.as_deref().map(str::trim) {
        None => None,
        Some("") => {
            problems.push(format!(
                "{position}: bearer_token_env is empty; remove it or name a variable"
            ));
            None
        }
        Some(name) => {
            validate_env_name(name, "bearer_token_env", position, problems);
            if name.len() > 64 || name.contains(char::is_whitespace) {
                problems.push(format!(
                    "{position}: bearer_token_env must be the NAME of an environment variable in \
                     the tdcc process, not the token itself."
                ));
            }
            Some(name.to_string())
        }
    };

    Some(Transport::Http {
        url,
        bearer_token_env,
    })
}

fn validate_env_name(name: &str, field: &str, position: &str, problems: &mut Vec<String>) {
    let shaped_like_a_name = !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_');
    if !shaped_like_a_name {
        problems.push(format!(
            "{position}: {field} entry '{name}' is not a valid environment variable name (letters, \
             digits, and underscores, not starting with a digit)"
        ));
        return;
    }
    if let Some(prefix) = RESERVED_ENV_PREFIXES
        .iter()
        .find(|prefix| name.starts_with(**prefix))
    {
        problems.push(format!(
            "{position}: {field} may not carry '{name}'. Everything under '{prefix}' belongs to \
             the node's own plugin control connection, and handing it to a third-party server \
             would let that server talk to the host as a plugin."
        ));
    }
}

fn validate_patterns(
    patterns: &[String],
    field: &str,
    position: &str,
    problems: &mut Vec<String>,
) -> Vec<String> {
    if patterns.len() > MAX_PATTERNS {
        problems.push(format!(
            "{position}: {} {field} entries; the limit is {MAX_PATTERNS}",
            patterns.len()
        ));
    }
    for pattern in patterns {
        if pattern.trim().is_empty() {
            problems.push(format!(
                "{position}: {field} contains an empty pattern. Use \"*\" if you meant every tool."
            ));
        }
    }
    patterns.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
version = 1

[[server]]
alias = "files"
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/srv/shared"]
"#;

    #[test]
    fn a_minimal_stdio_server_parses_with_safe_defaults() {
        let document = parse_document(MINIMAL).expect("minimal document parses");

        assert_eq!(document.servers.len(), 1);
        let server = &document.servers[0];
        assert_eq!(server.alias, "files");
        assert!(server.enabled);
        assert!(server.restart);
        assert_eq!(
            server.connect_timeout,
            Duration::from_secs(DEFAULT_CONNECT_TIMEOUT_SECS)
        );
        assert_eq!(
            server.call_timeout,
            Duration::from_secs(DEFAULT_CALL_TIMEOUT_SECS)
        );
        assert!(server.allow_tools.is_empty());
        assert!(server.deny_tools.is_empty());

        let Transport::Stdio {
            command,
            args,
            inherit_env,
            env,
            env_from,
            cwd,
        } = &server.transport
        else {
            panic!("expected a stdio transport");
        };
        assert_eq!(command, "npx");
        assert_eq!(args.len(), 3);
        // The safe state is what you get by doing nothing.
        assert!(!inherit_env);
        assert!(env.is_empty());
        assert!(env_from.is_empty());
        assert!(cwd.is_none());
    }

    #[test]
    fn an_empty_document_is_valid_and_bridges_nothing() {
        let document = parse_document("version = 1\n").expect("an empty list is valid");

        assert!(document.servers.is_empty());
        assert_eq!(document.enabled_servers().count(), 0);
    }

    #[test]
    fn a_wrong_version_is_refused_by_number() {
        let error = parse_document("version = 2\n").expect_err("version 2 is unknown");

        assert!(error.contains("version is 2"), "{error}");
        assert!(error.contains(&DOCUMENT_VERSION.to_string()), "{error}");
    }

    #[test]
    fn an_unknown_key_is_an_error_rather_than_a_silently_ignored_denylist() {
        let error = parse_document(
            r#"
version = 1

[[server]]
alias = "files"
transport = "stdio"
command = "npx"
deny_tool = ["write_file"]
"#,
        )
        .expect_err("a misspelled deny_tools is refused");

        assert!(error.contains("deny_tool"), "{error}");
    }

    #[test]
    fn every_problem_in_the_document_is_reported_at_once() {
        let error = parse_document(
            r#"
version = 1

[[server]]
alias = "Files"
transport = "stdio"
command = "npx"
call_timeout_secs = 999999

[[server]]
alias = "other"
transport = "carrier-pigeon"
"#,
        )
        .expect_err("both servers are wrong");

        assert!(error.contains("Files"), "{error}");
        assert!(error.contains("call_timeout_secs"), "{error}");
        assert!(error.contains("carrier-pigeon"), "{error}");
    }

    #[test]
    fn one_bad_server_invalidates_the_whole_document() {
        let error = parse_document(
            r#"
version = 1

[[server]]
alias = "good"
transport = "stdio"
command = "npx"

[[server]]
alias = "bad"
transport = "stdio"
"#,
        )
        .expect_err("a server without a command is refused");

        // Not "one server loaded" — a partial list is a configuration the
        // operator never wrote.
        assert!(error.contains("needs a command"), "{error}");
    }

    #[test]
    fn duplicate_aliases_are_refused_because_they_are_what_keeps_tools_apart() {
        let error = parse_document(
            r#"
version = 1

[[server]]
alias = "files"
transport = "stdio"
command = "a"

[[server]]
alias = "files"
transport = "stdio"
command = "b"
"#,
        )
        .expect_err("duplicate aliases are refused");

        assert!(error.contains("more than once"), "{error}");
    }

    #[test]
    fn the_plugin_control_endpoint_can_never_be_forwarded_to_a_child() {
        for document in [
            r#"
version = 1
[[server]]
alias = "files"
transport = "stdio"
command = "npx"
env_from = ["TDCC_PLUGIN_ENDPOINT"]
"#,
            r#"
version = 1
[[server]]
alias = "files"
transport = "stdio"
command = "npx"
env = { MESH_LLM_PLUGIN_ENDPOINT = "/tmp/sock" }
"#,
        ] {
            let error = parse_document(document).expect_err("reserved prefixes are refused");
            assert!(error.contains("plugin control connection"), "{error}");
        }
    }

    #[test]
    fn a_url_with_embedded_credentials_is_refused_rather_than_redacted() {
        let error = parse_document(
            r#"
version = 1

[[server]]
alias = "remote"
transport = "http"
url = "https://user:hunter2@mcp.example.com/mcp"
"#,
        )
        .expect_err("userinfo in a URL is refused");

        assert!(error.contains("bearer_token_env"), "{error}");
        // The refusal must not echo the credential back into the log.
        assert!(!error.contains("hunter2"), "{error}");
    }

    #[test]
    fn a_token_written_where_a_variable_name_belongs_is_refused() {
        let error = parse_document(
            r#"
version = 1

[[server]]
alias = "remote"
transport = "http"
url = "https://mcp.example.com/mcp"
bearer_token_env = "sk-lots-of-secret-characters-here"
"#,
        )
        .expect_err("a literal token is refused");

        assert!(error.contains("bearer_token_env"), "{error}");
    }

    #[test]
    fn a_valid_bearer_variable_name_is_kept_and_the_url_is_parsed() {
        let document = parse_document(
            r#"
version = 1

[[server]]
alias = "remote"
transport = "http"
url = "https://mcp.example.com/mcp"
bearer_token_env = "REMOTE_MCP_TOKEN"
"#,
        )
        .expect("valid http server");

        let Transport::Http {
            url,
            bearer_token_env,
        } = &document.servers[0].transport
        else {
            panic!("expected an http transport");
        };
        assert_eq!(url.as_str(), "https://mcp.example.com/mcp");
        assert_eq!(bearer_token_env.as_deref(), Some("REMOTE_MCP_TOKEN"));
    }

    #[test]
    fn fields_from_the_wrong_transport_are_refused_rather_than_ignored() {
        let error = parse_document(
            r#"
version = 1

[[server]]
alias = "remote"
transport = "http"
url = "http://127.0.0.1:7777/mcp"
command = "npx"
"#,
        )
        .expect_err("a command on an http server is refused");
        assert!(error.contains("does not launch it"), "{error}");

        let error = parse_document(
            r#"
version = 1

[[server]]
alias = "files"
transport = "stdio"
command = "npx"
bearer_token_env = "SOME_TOKEN"
"#,
        )
        .expect_err("a bearer token on a stdio server is refused");
        assert!(error.contains("no meaning for a stdio server"), "{error}");
    }

    #[test]
    fn a_non_http_url_scheme_is_refused() {
        let error = parse_document(
            r#"
version = 1

[[server]]
alias = "remote"
transport = "http"
url = "file:///etc/passwd"
"#,
        )
        .expect_err("file URLs are refused");

        assert!(error.contains("only http and https"), "{error}");
    }

    #[test]
    fn out_of_range_numbers_name_their_field_and_bounds() {
        let error = parse_document(
            r#"
version = 1

[defaults]
max_result_bytes = 1
"#,
        )
        .expect_err("a one-byte result cap is refused");

        assert!(error.contains("max_result_bytes"), "{error}");
        assert!(error.contains(&MIN_MAX_RESULT_BYTES.to_string()), "{error}");
    }

    #[test]
    fn defaults_apply_to_every_server_and_a_server_may_override_them() {
        let document = parse_document(
            r#"
version = 1

[defaults]
connect_timeout_secs = 5
call_timeout_secs = 10

[[server]]
alias = "quick"
transport = "stdio"
command = "a"

[[server]]
alias = "slow"
transport = "stdio"
command = "b"
call_timeout_secs = 300
"#,
        )
        .expect("valid document");

        assert_eq!(document.servers[0].connect_timeout, Duration::from_secs(5));
        assert_eq!(document.servers[0].call_timeout, Duration::from_secs(10));
        assert_eq!(document.servers[1].connect_timeout, Duration::from_secs(5));
        assert_eq!(document.servers[1].call_timeout, Duration::from_secs(300));
    }

    #[test]
    fn a_disabled_server_is_kept_in_the_document_but_not_started() {
        let document = parse_document(
            r#"
version = 1

[[server]]
alias = "off"
transport = "stdio"
command = "a"
enabled = false
"#,
        )
        .expect("valid document");

        assert_eq!(document.servers.len(), 1);
        assert_eq!(document.enabled_servers().count(), 0);
    }

    #[test]
    fn allow_and_deny_lists_survive_parsing_verbatim() {
        let document = parse_document(
            r#"
version = 1

[[server]]
alias = "files"
transport = "stdio"
command = "a"
allow_tools = ["read_file", "list_directory"]
deny_tools = ["write_*"]
"#,
        )
        .expect("valid document");

        assert_eq!(
            document.servers[0].allow_tools,
            ["read_file", "list_directory"]
        );
        assert_eq!(document.servers[0].deny_tools, ["write_*"]);
    }

    #[test]
    fn an_empty_pattern_is_refused_because_it_reads_like_a_wildcard() {
        let error = parse_document(
            r#"
version = 1

[[server]]
alias = "files"
transport = "stdio"
command = "a"
deny_tools = [""]
"#,
        )
        .expect_err("an empty pattern is refused");

        assert!(error.contains("\"*\""), "{error}");
    }

    #[test]
    fn too_many_servers_is_refused_with_the_limit() {
        let mut document = String::from("version = 1\n");
        for index in 0..=MAX_SERVERS {
            document.push_str(&format!(
                "\n[[server]]\nalias = \"s{index}\"\ntransport = \"stdio\"\ncommand = \"a\"\n"
            ));
        }

        let error = parse_document(&document).expect_err("too many servers");
        assert!(error.contains(&MAX_SERVERS.to_string()), "{error}");
    }

    #[test]
    fn a_transport_label_never_carries_a_credential() {
        let document = parse_document(
            r#"
version = 1

[[server]]
alias = "remote"
transport = "http"
url = "https://mcp.example.com/mcp"
bearer_token_env = "REMOTE_MCP_TOKEN"
"#,
        )
        .expect("valid document");

        let label = document.servers[0].transport.label();
        assert_eq!(label, "http: https://mcp.example.com/mcp");
        assert!(!label.contains("REMOTE_MCP_TOKEN"), "{label}");
    }
}
