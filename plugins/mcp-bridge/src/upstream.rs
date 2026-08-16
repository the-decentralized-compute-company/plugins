//! One bridged MCP server: connecting to it, asking what it can do, keeping it
//! alive, and forwarding calls to it.
//!
//! This is the only module that launches a process or opens a socket. The
//! protocol itself is `rmcp`'s — the same crate, at the same version, that the
//! TDCC host uses for its own MCP surfaces — so a bridged server is talking to
//! one MCP implementation rather than to a second one written here.
//!
//! Three behaviours are worth stating outright:
//!
//! * **Tools are discovered once, before the host is told anything.** The
//!   plugin manifest is sent in the initialize response and there is no update
//!   path in the plugin protocol, so the set of bridged tools is fixed for the
//!   life of the plugin process. A server that is down at startup contributes
//!   no tools until the plugin is restarted, and `status` says so rather than
//!   leaving an operator to wonder.
//! * **A dropped connection is reconnected with backoff, and the tools stay
//!   declared.** Calls in the meantime fail with an error naming the server and
//!   its state, which is what tells a caller apart from a tool that never
//!   existed.
//! * **A slow server times out.** `call_timeout_secs` bounds every call, so a
//!   wedged upstream cannot hold a host request open indefinitely.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use rmcp::model::{CallToolRequestParams, CallToolResult, JsonObject};
use rmcp::service::{Peer, RoleClient, RunningService, ServiceExt};
use rmcp::transport::{
    StreamableHttpClientTransport, TokioChildProcess,
    streamable_http_client::StreamableHttpClientTransportConfig,
};
use schemars::JsonSchema;
use serde::Serialize;
use tokio::sync::Mutex;

use crate::backoff::{HEALTH_INTERVAL, delay_for_attempt};
use crate::childenv::{baseline, child_environment};
use crate::config::{ServerSpec, Transport};
use crate::filter::{self, FilterOutcome};
use crate::forward::{check_result_size, stamp_provenance};
use crate::naming::{self, NameNote};
use crate::schema::{self, SchemaNote};

/// Upper bound on the raw `tools/list` response before anything else looks at
/// it. A server that publishes more than this is broken or hostile.
const RAW_TOOL_LIMIT: usize = 2_048;
/// Upper bound on an upstream description carried into the manifest.
const MAX_DESCRIPTION_CHARS: usize = 4_000;

// ---------------------------------------------------------------------------
// What a bridged tool is
// ---------------------------------------------------------------------------

/// One upstream tool, as this node projects it.
#[derive(Debug, Clone)]
pub struct BridgedTool {
    /// The operator's alias for the server that publishes it.
    pub alias: String,
    /// `<alias>__<tool>`; `mcp-bridge.<alias>__<tool>` on the host endpoint.
    pub bridged_name: String,
    /// Exactly what the upstream calls it. Calls go out under this name.
    pub upstream_name: String,
    pub title: Option<String>,
    /// The description a model reads, composed by [`compose_description`].
    pub description: String,
    pub input_schema_json: String,
    pub schema_note: SchemaNote,
    pub name_notes: Vec<NameNote>,
}

/// One upstream tool that the operator's allowlist or denylist kept out.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ExcludedTool {
    pub upstream_name: String,
    pub outcome: FilterOutcome,
}

/// Everything one server's `tools/list` turned into.
#[derive(Debug, Clone, Default)]
pub struct Discovery {
    pub tools: Vec<BridgedTool>,
    pub excluded: Vec<ExcludedTool>,
    /// Names that could not be made unique and were left out entirely.
    pub dropped: Vec<String>,
    /// Set when the server published more tools than it was allowed to project.
    pub capped_at: Option<usize>,
    pub upstream_tool_count: usize,
}

/// Compose the description a model reads for a bridged tool.
///
/// The upstream's own words come first, because they are the contract. The
/// sentence after them exists so that a person reading a transcript — or a
/// model choosing between `files__search` and `github__search` — can tell which
/// server is on the other end without decoding the name.
///
/// The transport *kind* is named, not the address: an internal hostname is not
/// something a model needs, and tool descriptions travel further than logs do.
pub fn compose_description(
    alias: &str,
    upstream_name: &str,
    upstream_description: Option<&str>,
    transport_kind: &str,
) -> String {
    let body = match upstream_description
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        Some(text) if text.chars().count() > MAX_DESCRIPTION_CHARS => {
            let mut cut: String = text.chars().take(MAX_DESCRIPTION_CHARS).collect();
            cut.push('…');
            cut
        }
        Some(text) => text.to_string(),
        None => "The upstream server published no description for this tool. Call \
                 `mcp-bridge.tools` to see what it is and where it comes from."
            .to_string(),
    };
    format!(
        "[{alias}] {body}\n\nBridged by mcp-bridge: this call is forwarded to the \
         `{upstream_name}` tool on the MCP server the operator listed as `{alias}` \
         ({transport_kind}). It runs third-party code on this node."
    )
}

fn build_tools(spec: &ServerSpec, listed: &[rmcp::model::Tool]) -> Discovery {
    let mut discovery = Discovery {
        upstream_tool_count: listed.len(),
        ..Discovery::default()
    };

    let listed: &[rmcp::model::Tool] = if listed.len() > RAW_TOOL_LIMIT {
        discovery.capped_at = Some(RAW_TOOL_LIMIT);
        &listed[..RAW_TOOL_LIMIT]
    } else {
        listed
    };

    // Filter first, then cap: capping first would let a server hide the very
    // tools an operator put on the allowlist behind forty it did not ask for.
    let mut exposed: Vec<&rmcp::model::Tool> = Vec::new();
    for tool in listed {
        let outcome = filter::decide(tool.name.as_ref(), &spec.allow_tools, &spec.deny_tools);
        if outcome.is_exposed() {
            exposed.push(tool);
        } else {
            discovery.excluded.push(ExcludedTool {
                upstream_name: tool.name.to_string(),
                outcome,
            });
        }
    }

    if exposed.len() > spec.max_tools {
        discovery.capped_at = Some(spec.max_tools);
        exposed.truncate(spec.max_tools);
    }

    let names: Vec<String> = exposed.iter().map(|tool| tool.name.to_string()).collect();
    let (assigned, dropped) = naming::assign_names(&spec.alias, &names);
    discovery.dropped = dropped;

    let by_name: BTreeMap<&str, &rmcp::model::Tool> = exposed
        .iter()
        .map(|tool| (tool.name.as_ref(), *tool))
        .collect();

    for name in assigned {
        let Some(tool) = by_name.get(name.upstream.as_str()) else {
            continue;
        };
        let decision = schema::decide(tool.input_schema.as_ref());
        discovery.tools.push(BridgedTool {
            alias: spec.alias.clone(),
            bridged_name: name.bridged,
            upstream_name: name.upstream,
            title: tool.title.clone(),
            description: compose_description(
                &spec.alias,
                tool.name.as_ref(),
                tool.description.as_deref(),
                spec.transport.kind(),
            ),
            input_schema_json: decision.json,
            schema_note: decision.note,
            name_notes: name.notes,
        });
    }

    discovery
}

// ---------------------------------------------------------------------------
// Link state
// ---------------------------------------------------------------------------

/// Where one bridged server stands right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum LinkState {
    /// `enabled = false` in the server list. Nothing was launched.
    Disabled,
    /// Connected, and the tools below were projected.
    Ready,
    /// The connection is gone. Calls fail; the supervisor is retrying.
    Down,
    /// The first connection never succeeded, so this server contributed no
    /// tools to the manifest.
    NeverConnected,
}

impl LinkState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Ready => "ready",
            Self::Down => "down",
            Self::NeverConnected => "never-connected",
        }
    }
}

#[derive(Debug, Default)]
struct Status {
    state: Option<LinkState>,
    /// Consecutive failed connect attempts.
    attempts: u32,
    /// Successful connects after the first one.
    reconnects: u64,
    last_error: Option<String>,
    connected_at: Option<u64>,
    server_name: Option<String>,
    server_version: Option<String>,
    protocol_version: Option<String>,
    /// Tools the server published on reconnect that were not in the manifest,
    /// and manifest tools it no longer publishes.
    drift_added: Vec<String>,
    drift_missing: Vec<String>,
}

/// A live MCP session with one upstream server.
///
/// Dropping this closes the transport; for a stdio server that also kills the
/// child process, which is what makes a failed reconnect not leak one.
struct Connection {
    peer: Peer<RoleClient>,
    running: RunningService<RoleClient, ()>,
}

impl Connection {
    fn is_closed(&self) -> bool {
        self.running.is_closed()
    }
}

/// One bridged server and everything this node knows about it.
pub struct Upstream {
    pub spec: ServerSpec,
    /// Frozen at startup: what went into the manifest.
    pub discovery: Discovery,
    parent_env: Arc<BTreeMap<String, String>>,
    connection: Mutex<Option<Arc<Connection>>>,
    /// Serializes connect attempts so the supervisor and a `reconnect` call
    /// cannot spawn two children at once.
    connect_lock: Mutex<()>,
    status: Mutex<Status>,
}

impl Upstream {
    /// Assemble an upstream around an MCP session that is already open.
    ///
    /// The tests use this to run the whole discovery-and-forward path — the
    /// initialize handshake, `tools/list`, naming, filtering, schema
    /// forwarding, `tools/call`, and the result coming back — against a real
    /// MCP server over an in-process pipe, instead of launching a child. It is
    /// the same code every production call takes; only the transport differs.
    #[cfg(test)]
    pub(crate) async fn from_session(
        spec: ServerSpec,
        running: RunningService<RoleClient, ()>,
    ) -> Result<Arc<Self>, String> {
        let peer = running.peer().clone();
        let connection = Connection { peer, running };
        let listed = list_tools(&connection, &spec).await?;
        let discovery = build_tools(&spec, &listed);
        Ok(Arc::new(Self {
            spec,
            discovery,
            parent_env: Arc::new(BTreeMap::new()),
            connection: Mutex::new(Some(Arc::new(connection))),
            connect_lock: Mutex::new(()),
            status: Mutex::new(Status {
                state: Some(LinkState::Ready),
                connected_at: Some(now_unix()),
                ..Status::default()
            }),
        }))
    }

    /// Connect to a server and discover what it publishes.
    ///
    /// Never returns `Err`: a server that cannot be reached is a server in the
    /// [`LinkState::NeverConnected`] state with its reason recorded, not a
    /// reason for the node to refuse to start.
    pub async fn start(spec: ServerSpec, parent_env: Arc<BTreeMap<String, String>>) -> Arc<Self> {
        let assemble = |discovery, connection: Option<Connection>, status| {
            Arc::new(Self {
                spec: spec.clone(),
                discovery,
                parent_env: Arc::clone(&parent_env),
                connection: Mutex::new(connection.map(Arc::new)),
                connect_lock: Mutex::new(()),
                status: Mutex::new(status),
            })
        };

        if !spec.enabled {
            return assemble(
                Discovery::default(),
                None,
                Status {
                    state: Some(LinkState::Disabled),
                    ..Status::default()
                },
            );
        }

        match connect(&spec, &parent_env).await {
            Ok(connection) => match list_tools(&connection, &spec).await {
                Ok(listed) => {
                    let mut status = Status {
                        state: Some(LinkState::Ready),
                        connected_at: Some(now_unix()),
                        ..Status::default()
                    };
                    if let Some(info) = connection.running.peer_info() {
                        status.server_name = Some(info.server_info.name.to_string());
                        status.server_version = Some(info.server_info.version.to_string());
                        status.protocol_version = Some(info.protocol_version.to_string());
                    }
                    assemble(build_tools(&spec, &listed), Some(connection), status)
                }
                Err(error) => assemble(
                    Discovery::default(),
                    None,
                    Status {
                        state: Some(LinkState::NeverConnected),
                        attempts: 1,
                        last_error: Some(error),
                        ..Status::default()
                    },
                ),
            },
            Err(error) => assemble(
                Discovery::default(),
                None,
                Status {
                    state: Some(LinkState::NeverConnected),
                    attempts: 1,
                    last_error: Some(error),
                    ..Status::default()
                },
            ),
        }
    }

    pub fn alias(&self) -> &str {
        &self.spec.alias
    }

    pub async fn state(&self) -> LinkState {
        self.status
            .lock()
            .await
            .state
            .unwrap_or(LinkState::NeverConnected)
    }

    /// A snapshot for the `status` and `servers` responses. Reads no network.
    pub async fn snapshot(&self) -> StatusSnapshot {
        let status = self.status.lock().await;
        StatusSnapshot {
            alias: self.spec.alias.clone(),
            enabled: self.spec.enabled,
            transport: self.spec.transport.kind().to_string(),
            endpoint: self.spec.transport.label(),
            state: status.state.unwrap_or(LinkState::NeverConnected),
            tools_projected: self.discovery.tools.len(),
            tools_published: self.discovery.upstream_tool_count,
            tools_excluded: self.discovery.excluded.len(),
            tools_dropped: self.discovery.dropped.clone(),
            tools_capped_at: self.discovery.capped_at,
            failed_attempts: status.attempts,
            reconnects: status.reconnects,
            connected_at_unix: status.connected_at,
            last_error: status.last_error.clone(),
            server_name: status.server_name.clone(),
            server_version: status.server_version.clone(),
            protocol_version: status.protocol_version.clone(),
            drift_added: status.drift_added.clone(),
            drift_missing: status.drift_missing.clone(),
            description: self.spec.description.clone(),
            call_timeout_secs: self.spec.call_timeout.as_secs(),
            connect_timeout_secs: self.spec.connect_timeout.as_secs(),
            max_result_bytes: self.spec.max_result_bytes,
            restart: self.spec.restart,
            allow_tools: self.spec.allow_tools.clone(),
            deny_tools: self.spec.deny_tools.clone(),
        }
    }

    async fn live_connection(&self) -> Option<Arc<Connection>> {
        let mut slot = self.connection.lock().await;
        match slot.as_ref() {
            Some(connection) if !connection.is_closed() => Some(Arc::clone(connection)),
            Some(_) => {
                // Drop the dead one here so the child is reaped promptly rather
                // than at the next supervisor tick.
                *slot = None;
                None
            }
            None => None,
        }
    }

    /// Forward one call to the upstream server.
    pub async fn call(
        &self,
        tool: &BridgedTool,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, String> {
        let Some(connection) = self.live_connection().await else {
            let status = self.status.lock().await;
            let state = status.state.unwrap_or(LinkState::NeverConnected);
            let reason = status
                .last_error
                .clone()
                .unwrap_or_else(|| "no reason was recorded".to_string());
            return Err(format!(
                "MCP server '{}' is {} — this call was not sent. Last reason: {reason}. The \
                 bridge keeps retrying with backoff; `mcp-bridge.status` shows the current state \
                 and `mcp-bridge.reconnect` forces an attempt now.",
                self.spec.alias,
                state.as_str()
            ));
        };

        let mut params = CallToolRequestParams::new(tool.upstream_name.clone());
        params.arguments = arguments;

        let outcome =
            tokio::time::timeout(self.spec.call_timeout, connection.peer.call_tool(params)).await;

        let result = match outcome {
            Err(_elapsed) => {
                // A wedged upstream must not hold a host request open. The
                // connection is left alone: a slow tool is not a dead server.
                return Err(format!(
                    "MCP server '{}' did not answer its '{}' tool within {} s. Raise \
                     call_timeout_secs for this server in the mcp-bridge server list if the tool \
                     legitimately takes longer.",
                    self.spec.alias,
                    tool.upstream_name,
                    self.spec.call_timeout.as_secs()
                ));
            }
            Ok(Err(error)) => {
                let message = error.to_string();
                self.record_failure(&message).await;
                return Err(format!(
                    "MCP server '{}' failed the call to its '{}' tool: {message}",
                    self.spec.alias, tool.upstream_name
                ));
            }
            Ok(Ok(result)) => result,
        };

        check_result_size(
            &result,
            &self.spec.alias,
            &tool.upstream_name,
            self.spec.max_result_bytes,
        )?;

        Ok(stamp_provenance(
            result,
            &self.spec.alias,
            &tool.upstream_name,
        ))
    }

    async fn record_failure(&self, message: &str) {
        let mut status = self.status.lock().await;
        status.last_error = Some(message.to_string());
    }

    /// Drop whatever connection there is and open a new one.
    ///
    /// Returns the new state, or the reason it could not be reached.
    pub async fn reconnect(&self) -> Result<LinkState, String> {
        if !self.spec.enabled {
            return Err(format!(
                "MCP server '{}' has enabled = false in the server list, so there is nothing to \
                 reconnect to.",
                self.spec.alias
            ));
        }

        let _guard = self.connect_lock.lock().await;
        // Dropping the old connection first is what kills a stdio child before
        // a replacement is launched, so a flapping server cannot accumulate
        // processes.
        *self.connection.lock().await = None;

        match connect(&self.spec, &self.parent_env).await {
            Ok(connection) => {
                let drift = match list_tools(&connection, &self.spec).await {
                    Ok(listed) => Some(self.drift_against_manifest(&listed)),
                    Err(_) => None,
                };
                let info = connection.running.peer_info();
                *self.connection.lock().await = Some(Arc::new(connection));

                let mut status = self.status.lock().await;
                status.state = Some(LinkState::Ready);
                status.attempts = 0;
                status.reconnects += 1;
                status.last_error = None;
                status.connected_at = Some(now_unix());
                if let Some(info) = info {
                    status.server_name = Some(info.server_info.name.to_string());
                    status.server_version = Some(info.server_info.version.to_string());
                    status.protocol_version = Some(info.protocol_version.to_string());
                }
                if let Some((added, missing)) = drift {
                    status.drift_added = added;
                    status.drift_missing = missing;
                }
                Ok(LinkState::Ready)
            }
            Err(error) => {
                let mut status = self.status.lock().await;
                if status.state != Some(LinkState::NeverConnected) {
                    status.state = Some(LinkState::Down);
                }
                status.attempts = status.attempts.saturating_add(1);
                status.last_error = Some(error.clone());
                Err(error)
            }
        }
    }

    /// Compare what the server publishes now against what the manifest froze.
    fn drift_against_manifest(&self, listed: &[rmcp::model::Tool]) -> (Vec<String>, Vec<String>) {
        let now: BTreeSet<String> = listed.iter().map(|tool| tool.name.to_string()).collect();
        let declared: BTreeSet<String> = self
            .discovery
            .tools
            .iter()
            .map(|tool| tool.upstream_name.clone())
            .collect();
        let excluded: BTreeSet<String> = self
            .discovery
            .excluded
            .iter()
            .map(|tool| tool.upstream_name.clone())
            .collect();

        let added: Vec<String> = now
            .iter()
            .filter(|name| !declared.contains(*name) && !excluded.contains(*name))
            .cloned()
            .collect();
        let missing: Vec<String> = declared
            .iter()
            .filter(|name| !now.contains(*name))
            .cloned()
            .collect();
        (added, missing)
    }

    async fn note_down(&self) -> u32 {
        let mut status = self.status.lock().await;
        if status.state == Some(LinkState::Ready) {
            status.state = Some(LinkState::Down);
            status.last_error.get_or_insert_with(|| {
                "the connection closed; the upstream server exited or the transport failed"
                    .to_string()
            });
            eprintln!(
                "mcp-bridge: MCP server '{}' went away; reconnecting with backoff",
                self.spec.alias
            );
        }
        status.attempts = status.attempts.saturating_add(1);
        status.attempts
    }
}

/// Keep one server connected for as long as the plugin runs.
///
/// Runs until the process exits. A server with `restart = false` is watched but
/// never relaunched, so its state still shows up in `status` as `down` rather
/// than looking like a server that was never configured.
pub async fn supervise(upstream: Arc<Upstream>) {
    if !upstream.spec.enabled {
        return;
    }
    loop {
        tokio::time::sleep(HEALTH_INTERVAL).await;

        if upstream.live_connection().await.is_some() {
            continue;
        }
        if !upstream.spec.restart {
            let mut status = upstream.status.lock().await;
            if status.state == Some(LinkState::Ready) {
                status.state = Some(LinkState::Down);
                status.last_error = Some(
                    "the connection closed and restart = false for this server in the server list"
                        .to_string(),
                );
            }
            continue;
        }

        let attempt = upstream.note_down().await;
        tokio::time::sleep(delay_for_attempt(attempt)).await;
        match upstream.reconnect().await {
            Ok(_) => eprintln!("mcp-bridge: MCP server '{}' is back", upstream.spec.alias),
            Err(error) => eprintln!(
                "mcp-bridge: MCP server '{}' is still unreachable (attempt {attempt}): {error}",
                upstream.spec.alias
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Connecting
// ---------------------------------------------------------------------------

async fn connect(
    spec: &ServerSpec,
    parent_env: &BTreeMap<String, String>,
) -> Result<Connection, String> {
    let attempt = async {
        match &spec.transport {
            Transport::Stdio {
                command,
                args,
                cwd,
                env,
                env_from,
                inherit_env,
            } => {
                let (child_env, missing) =
                    child_environment(parent_env, baseline(), env_from, env, *inherit_env);
                if !missing.is_empty() {
                    let names: Vec<&str> = missing.iter().map(|item| item.name.as_str()).collect();
                    return Err(format!(
                        "env_from names {} for MCP server '{}', but {} not set in the environment \
                         of the tdcc process. Export it there — mcp-bridge deliberately cannot \
                         read a credential out of the server list.",
                        names.join(", "),
                        spec.alias,
                        if names.len() == 1 {
                            "it is"
                        } else {
                            "they are"
                        }
                    ));
                }

                let mut process = resolve_command(command)?;
                process.args(args);
                if let Some(cwd) = cwd {
                    process.current_dir(cwd);
                }
                // Clear first: a child launched from inside tdcc would
                // otherwise inherit the node's plugin control endpoint and
                // every key exported for every other plugin.
                process.env_clear();
                process.envs(&child_env);

                let transport = TokioChildProcess::new(process).map_err(|error| {
                    format!(
                        "could not launch MCP server '{}' ({command}): {error}",
                        spec.alias
                    )
                })?;
                ().serve(transport).await.map_err(|error| {
                    format!(
                        "MCP server '{}' launched but did not complete the MCP handshake: {error}",
                        spec.alias
                    )
                })
            }
            Transport::Http {
                url,
                bearer_token_env,
            } => {
                let mut config = StreamableHttpClientTransportConfig::with_uri(url.to_string());
                if let Some(name) = bearer_token_env {
                    let token = parent_env.get(name).map(|value| value.trim()).unwrap_or("");
                    if token.is_empty() {
                        return Err(format!(
                            "MCP server '{}' names {name} as its bearer_token_env, but that \
                             variable is not set in the environment of the tdcc process. Export \
                             it there — mcp-bridge deliberately cannot read a token out of the \
                             server list.",
                            spec.alias
                        ));
                    }
                    config = config.auth_header(token.to_string());
                }
                let transport = StreamableHttpClientTransport::from_config(config);
                ().serve(transport).await.map_err(|error| {
                    // The endpoint is named; the token never is.
                    format!(
                        "could not reach MCP server '{}' at {url}: {error}",
                        spec.alias
                    )
                })
            }
        }
    };

    match tokio::time::timeout(spec.connect_timeout, attempt).await {
        Ok(Ok(running)) => {
            let peer = running.peer().clone();
            Ok(Connection { peer, running })
        }
        Ok(Err(error)) => Err(error),
        Err(_elapsed) => Err(format!(
            "MCP server '{}' did not finish connecting within {} s. Raise connect_timeout_secs \
             for this server in the mcp-bridge server list if it is genuinely slow to start.",
            spec.alias,
            spec.connect_timeout.as_secs()
        )),
    }
}

/// Turn a configured command into something spawnable.
///
/// A bare name is resolved through `PATH` — including Windows `PATHEXT`, which
/// is what makes `npx` and `uvx` work there at all, since they are `.cmd`
/// shims that `CreateProcess` will not find on its own. A command that already
/// contains a path separator is used as written, so a path relative to `cwd`
/// still works.
fn resolve_command(command: &str) -> Result<tokio::process::Command, String> {
    if command.contains('/') || command.contains('\\') {
        return Ok(tokio::process::Command::new(command));
    }
    rmcp::transport::which_command(command).map_err(|error| {
        format!(
            "command '{command}' was not found on PATH ({error}). mcp-bridge resolves a bare \
             command name through the PATH of the tdcc process; give an absolute path in the \
             server list if that is not where it lives."
        )
    })
}

async fn list_tools(
    connection: &Connection,
    spec: &ServerSpec,
) -> Result<Vec<rmcp::model::Tool>, String> {
    match tokio::time::timeout(spec.connect_timeout, connection.peer.list_all_tools()).await {
        Ok(Ok(tools)) => Ok(tools),
        Ok(Err(error)) => Err(format!(
            "MCP server '{}' connected but could not list its tools: {error}",
            spec.alias
        )),
        Err(_elapsed) => Err(format!(
            "MCP server '{}' connected but did not answer tools/list within {} s",
            spec.alias,
            spec.connect_timeout.as_secs()
        )),
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Response shapes
// ---------------------------------------------------------------------------

/// One server's line in a `status` response.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct StatusSnapshot {
    /// The operator's alias, and the prefix on every tool from this server.
    pub alias: String,
    pub enabled: bool,
    /// `stdio` or `http`.
    pub transport: String,
    /// The command or URL, with no credential in it.
    pub endpoint: String,
    /// `ready`, `down`, `never-connected`, or `disabled`.
    pub state: LinkState,
    /// Tools from this server in the host's tool list.
    pub tools_projected: usize,
    /// Tools the server published when it was first asked.
    pub tools_published: usize,
    /// Tools kept out by `allow_tools` or `deny_tools`.
    pub tools_excluded: usize,
    /// Upstream names that could not be given a unique bridged name and were
    /// left out entirely.
    pub tools_dropped: Vec<String>,
    /// Set when the server published more tools than `max_tools` allows, in
    /// which case the ones beyond the limit are not bridged.
    pub tools_capped_at: Option<usize>,
    /// Consecutive failed connect attempts.
    pub failed_attempts: u32,
    /// Successful reconnects since the plugin started.
    pub reconnects: u64,
    /// Unix seconds of the current connection, if there is one.
    pub connected_at_unix: Option<u64>,
    /// Why the last attempt or call failed, if one did.
    pub last_error: Option<String>,
    pub server_name: Option<String>,
    pub server_version: Option<String>,
    pub protocol_version: Option<String>,
    /// Tools the server published on a later reconnect that are not in this
    /// node's tool list, because the manifest is fixed at startup.
    pub drift_added: Vec<String>,
    /// Tools in this node's tool list that the server no longer publishes.
    pub drift_missing: Vec<String>,
    /// The operator's note from the server list.
    pub description: Option<String>,
    pub call_timeout_secs: u64,
    pub connect_timeout_secs: u64,
    pub max_result_bytes: usize,
    pub restart: bool,
    pub allow_tools: Vec<String>,
    pub deny_tools: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse_document;
    use rmcp::model::Tool;
    use serde_json::json;
    use std::sync::Arc as StdArc;

    pub(super) fn spec(document: &str) -> ServerSpec {
        let mut parsed = parse_document(document).expect("test document parses");
        parsed.servers.remove(0)
    }

    fn tool(name: &str, description: Option<&str>, schema: serde_json::Value) -> Tool {
        let mut tool = Tool::new(
            name.to_string(),
            description.unwrap_or_default().to_string(),
            StdArc::new(schema.as_object().cloned().expect("object schema")),
        );
        if description.is_none() {
            tool.description = None;
        }
        tool
    }

    fn object_schema() -> serde_json::Value {
        json!({ "type": "object", "properties": { "path": { "type": "string" } } })
    }

    const FILES: &str = r#"
version = 1

[[server]]
alias = "files"
transport = "stdio"
command = "npx"
"#;

    #[test]
    fn every_projected_tool_is_prefixed_with_the_operators_alias() {
        let discovery = build_tools(
            &spec(FILES),
            &[
                tool("read_file", Some("Read a file"), object_schema()),
                tool("list_directory", Some("List a directory"), object_schema()),
            ],
        );

        let names: Vec<&str> = discovery
            .tools
            .iter()
            .map(|tool| tool.bridged_name.as_str())
            .collect();
        assert_eq!(names, vec!["files__list_directory", "files__read_file"]);
        // The call still goes out under the upstream's own name.
        assert!(
            discovery
                .tools
                .iter()
                .all(|tool| !tool.upstream_name.starts_with("files__"))
        );
    }

    #[test]
    fn the_upstream_schema_reaches_the_manifest_unchanged() {
        let schema = json!({
            "type": "object",
            "properties": { "path": { "type": "string", "description": "Which file" } },
            "required": ["path"]
        });
        let discovery = build_tools(
            &spec(FILES),
            &[tool("read_file", Some("Read"), schema.clone())],
        );

        let declared: serde_json::Value =
            serde_json::from_str(&discovery.tools[0].input_schema_json).expect("valid JSON");
        assert_eq!(declared, schema);
        assert_eq!(discovery.tools[0].schema_note, SchemaNote::Forwarded);
    }

    #[test]
    fn the_allowlist_keeps_the_rest_of_a_servers_tools_out_of_the_manifest() {
        let document = r#"
version = 1

[[server]]
alias = "files"
transport = "stdio"
command = "npx"
allow_tools = ["read_file"]
"#;
        let discovery = build_tools(
            &spec(document),
            &[
                tool("read_file", Some("Read"), object_schema()),
                tool("write_file", Some("Write"), object_schema()),
                tool("delete_file", Some("Delete"), object_schema()),
            ],
        );

        assert_eq!(discovery.tools.len(), 1);
        assert_eq!(discovery.tools[0].upstream_name, "read_file");
        // The ones that were kept out are reported, not just absent.
        assert_eq!(discovery.excluded.len(), 2);
        assert_eq!(discovery.upstream_tool_count, 3);
    }

    #[test]
    fn the_denylist_wins_over_the_allowlist_in_a_real_discovery() {
        let document = r#"
version = 1

[[server]]
alias = "files"
transport = "stdio"
command = "npx"
allow_tools = ["*"]
deny_tools = ["write_*", "delete_*"]
"#;
        let discovery = build_tools(
            &spec(document),
            &[
                tool("read_file", Some("Read"), object_schema()),
                tool("write_file", Some("Write"), object_schema()),
                tool("delete_file", Some("Delete"), object_schema()),
            ],
        );

        let exposed: Vec<&str> = discovery
            .tools
            .iter()
            .map(|tool| tool.upstream_name.as_str())
            .collect();
        assert_eq!(exposed, vec!["read_file"]);
    }

    #[test]
    fn a_tool_without_a_description_still_gets_one_a_model_can_act_on() {
        let discovery = build_tools(&spec(FILES), &[tool("mystery", None, object_schema())]);

        let description = &discovery.tools[0].description;
        assert!(description.contains("[files]"), "{description}");
        assert!(
            description.contains("published no description"),
            "{description}"
        );
        assert!(description.contains("mcp-bridge.tools"), "{description}");
    }

    #[test]
    fn the_description_names_the_server_but_never_its_address() {
        let document = r#"
version = 1

[[server]]
alias = "remote"
transport = "http"
url = "http://secret-internal-host.corp:7777/mcp"
"#;
        let discovery = build_tools(
            &spec(document),
            &[tool("ask", Some("Ask a thing"), object_schema())],
        );

        let description = &discovery.tools[0].description;
        assert!(description.contains("[remote]"), "{description}");
        assert!(description.contains("(http)"), "{description}");
        assert!(
            !description.contains("secret-internal-host"),
            "an internal hostname must not travel to a model: {description}"
        );
    }

    #[test]
    fn a_description_longer_than_the_manifest_should_carry_is_cut() {
        let long = "d".repeat(MAX_DESCRIPTION_CHARS * 2);
        let composed = compose_description("files", "read", Some(&long), "stdio");

        assert!(
            composed.chars().count() < MAX_DESCRIPTION_CHARS + 400,
            "{}",
            composed.len()
        );
        assert!(composed.contains('…'));
    }

    #[test]
    fn a_server_that_publishes_more_tools_than_allowed_is_capped_and_says_so() {
        let document = r#"
version = 1

[[server]]
alias = "many"
transport = "stdio"
command = "npx"
max_tools = 2
"#;
        let listed: Vec<Tool> = (0..10)
            .map(|index| tool(&format!("tool_{index}"), Some("x"), object_schema()))
            .collect();

        let discovery = build_tools(&spec(document), &listed);

        assert_eq!(discovery.tools.len(), 2);
        assert_eq!(discovery.capped_at, Some(2));
        assert_eq!(discovery.upstream_tool_count, 10);
    }

    #[test]
    fn the_allowlist_is_applied_before_the_cap_so_it_cannot_be_hidden_behind_noise() {
        let document = r#"
version = 1

[[server]]
alias = "many"
transport = "stdio"
command = "npx"
max_tools = 1
allow_tools = ["wanted"]
"#;
        let mut listed: Vec<Tool> = (0..40)
            .map(|index| tool(&format!("noise_{index}"), Some("x"), object_schema()))
            .collect();
        listed.push(tool("wanted", Some("the one"), object_schema()));

        let discovery = build_tools(&spec(document), &listed);

        assert_eq!(discovery.tools.len(), 1);
        assert_eq!(discovery.tools[0].upstream_name, "wanted");
    }

    #[test]
    fn a_server_publishing_a_broken_schema_still_produces_a_callable_tool() {
        let discovery = build_tools(
            &spec(FILES),
            &[
                tool("nothing", Some("No schema"), json!({})),
                tool("wrong", Some("Array schema"), json!({ "type": "array" })),
            ],
        );

        assert_eq!(discovery.tools.len(), 2);
        for bridged in &discovery.tools {
            assert!(
                !bridged.schema_note.is_verbatim(),
                "{}",
                bridged.bridged_name
            );
            let declared: serde_json::Value =
                serde_json::from_str(&bridged.input_schema_json).expect("valid JSON");
            assert_eq!(declared["type"], "object");
        }
    }

    #[test]
    fn two_servers_bridged_side_by_side_do_not_collide() {
        let files = build_tools(&spec(FILES), &[tool("search", Some("s"), object_schema())]);
        let github = build_tools(
            &spec(
                r#"
version = 1

[[server]]
alias = "github"
transport = "stdio"
command = "npx"
"#,
            ),
            &[tool("search", Some("s"), object_schema())],
        );

        assert_eq!(files.tools[0].bridged_name, "files__search");
        assert_eq!(github.tools[0].bridged_name, "github__search");
    }

    #[tokio::test]
    async fn a_disabled_server_reports_disabled_and_launches_nothing() {
        let spec = spec(
            r#"
version = 1

[[server]]
alias = "off"
transport = "stdio"
command = "definitely-not-a-real-binary-9c1f"
enabled = false
"#,
        );

        let upstream = Upstream::start(spec, Arc::new(BTreeMap::new())).await;

        assert_eq!(upstream.state().await, LinkState::Disabled);
        assert!(upstream.discovery.tools.is_empty());
    }

    #[tokio::test]
    async fn a_server_whose_command_does_not_exist_is_recorded_not_fatal() {
        let spec = spec(
            r#"
version = 1

[[server]]
alias = "missing"
transport = "stdio"
command = "definitely-not-a-real-binary-9c1f"
connect_timeout_secs = 2
"#,
        );

        let upstream = Upstream::start(spec, Arc::new(BTreeMap::new())).await;

        assert_eq!(upstream.state().await, LinkState::NeverConnected);
        assert!(upstream.discovery.tools.is_empty());
        let snapshot = upstream.snapshot().await;
        let error = snapshot.last_error.expect("a reason was recorded");
        assert!(error.contains("PATH"), "{error}");
    }

    #[tokio::test]
    async fn a_call_to_a_server_that_never_connected_is_an_error_not_an_empty_success() {
        let spec = spec(
            r#"
version = 1

[[server]]
alias = "missing"
transport = "stdio"
command = "definitely-not-a-real-binary-9c1f"
connect_timeout_secs = 2
"#,
        );
        let upstream = Upstream::start(spec, Arc::new(BTreeMap::new())).await;

        let tool = BridgedTool {
            alias: "missing".into(),
            bridged_name: "missing__read".into(),
            upstream_name: "read".into(),
            title: None,
            description: "x".into(),
            input_schema_json: "{}".into(),
            schema_note: SchemaNote::ReplacedEmpty,
            name_notes: Vec::new(),
        };

        let error = upstream
            .call(&tool, None)
            .await
            .expect_err("a down server must fail the call");

        assert!(error.contains("never-connected"), "{error}");
        assert!(error.contains("mcp-bridge.status"), "{error}");
    }

    #[tokio::test]
    async fn an_env_from_variable_the_operator_forgot_is_named_before_anything_is_launched() {
        let spec = spec(
            r#"
version = 1

[[server]]
alias = "needs_key"
transport = "stdio"
command = "definitely-not-a-real-binary-9c1f"
env_from = ["SOME_API_KEY"]
connect_timeout_secs = 2
"#,
        );

        let upstream = Upstream::start(spec, Arc::new(BTreeMap::new())).await;

        let error = upstream.snapshot().await.last_error.expect("a reason");
        assert!(error.contains("SOME_API_KEY"), "{error}");
        assert!(error.contains("environment of the tdcc process"), "{error}");
    }

    #[tokio::test]
    async fn an_http_server_missing_its_token_variable_names_it_rather_than_connecting() {
        let spec = spec(
            r#"
version = 1

[[server]]
alias = "remote"
transport = "http"
url = "http://127.0.0.1:1/mcp"
bearer_token_env = "REMOTE_MCP_TOKEN"
connect_timeout_secs = 2
"#,
        );

        let upstream = Upstream::start(spec, Arc::new(BTreeMap::new())).await;

        let error = upstream.snapshot().await.last_error.expect("a reason");
        assert!(error.contains("REMOTE_MCP_TOKEN"), "{error}");
        assert!(!error.contains("Bearer"), "{error}");
    }

    #[tokio::test]
    async fn a_disabled_server_cannot_be_reconnected_into_existence() {
        let spec = spec(
            r#"
version = 1

[[server]]
alias = "off"
transport = "stdio"
command = "definitely-not-a-real-binary-9c1f"
enabled = false
"#,
        );
        let upstream = Upstream::start(spec, Arc::new(BTreeMap::new())).await;

        let error = upstream
            .reconnect()
            .await
            .expect_err("a disabled server stays disabled");
        assert!(error.contains("enabled = false"), "{error}");
    }

    #[test]
    fn drift_reports_what_a_reconnected_server_gained_and_lost() {
        let discovery = build_tools(
            &spec(FILES),
            &[
                tool("read_file", Some("Read"), object_schema()),
                tool("list_directory", Some("List"), object_schema()),
            ],
        );
        let upstream = Upstream {
            spec: spec(FILES),
            discovery,
            parent_env: Arc::new(BTreeMap::new()),
            connection: Mutex::new(None),
            connect_lock: Mutex::new(()),
            status: Mutex::new(Status::default()),
        };

        let (added, missing) = upstream.drift_against_manifest(&[
            tool("read_file", Some("Read"), object_schema()),
            tool("brand_new", Some("New"), object_schema()),
        ]);

        assert_eq!(added, vec!["brand_new".to_string()]);
        assert_eq!(missing, vec!["list_directory".to_string()]);
    }
}

/// End-to-end tests against a real MCP server.
///
/// Everything above pins one function at a time. These drive the whole path —
/// the initialize handshake, `tools/list`, naming, filtering, schema
/// forwarding, `tools/call`, the answer coming back, and the connection
/// dropping mid-flight — over real newline-delimited JSON-RPC, with
/// [`crate::testserver`] on the other end of an in-process pipe.
///
/// What they do not cover, because there is no child process here: launching a
/// command, the environment it gets ([`crate::childenv`] covers that as a pure
/// function), and the Streamable HTTP transport. The README says so too.
#[cfg(test)]
mod end_to_end {
    use super::tests::spec;
    use super::*;
    use crate::testserver::{Behaviour, FakeTool, serve};
    use serde_json::json;
    use std::time::Duration;

    /// Bridge a fake MCP server publishing `tools`, over an in-process pipe.
    async fn bridged(document: &str, tools: Vec<FakeTool>) -> Arc<Upstream> {
        let (client_side, server_side) = tokio::io::duplex(256 * 1024);
        tokio::spawn(serve(server_side, tools));
        let running =
            ().serve(client_side)
                .await
                .expect("the fake server completes the MCP handshake");
        Upstream::from_session(spec(document), running)
            .await
            .expect("tools/list answers")
    }

    const FILES: &str = r#"
version = 1

[[server]]
alias = "files"
transport = "stdio"
command = "npx"
call_timeout_secs = 2
"#;

    #[tokio::test]
    async fn a_real_servers_tools_arrive_namespaced_with_their_own_schemas() {
        let schema = json!({
            "type": "object",
            "properties": { "path": { "type": "string", "description": "Which file" } },
            "required": ["path"],
            "additionalProperties": false
        });
        let upstream = bridged(
            FILES,
            vec![FakeTool::new("read_file", Behaviour::Echo).with_schema(schema.clone())],
        )
        .await;

        let tool = &upstream.discovery.tools[0];
        assert_eq!(tool.bridged_name, "files__read_file");
        assert_eq!(tool.upstream_name, "read_file");
        let declared: serde_json::Value =
            serde_json::from_str(&tool.input_schema_json).expect("valid JSON");
        assert_eq!(declared, schema);
        assert_eq!(tool.schema_note, SchemaNote::Forwarded);
    }

    #[tokio::test]
    async fn a_call_reaches_the_upstream_under_its_own_name_with_the_arguments_untouched() {
        let upstream = bridged(FILES, vec![FakeTool::new("read_file", Behaviour::Echo)]).await;
        let tool = upstream.discovery.tools[0].clone();

        let arguments = json!({ "value": "hello", "extra": [1, 2, 3] })
            .as_object()
            .cloned();
        let result = upstream
            .call(&tool, arguments)
            .await
            .expect("the fake server answers");

        let structured = result
            .structured_content
            .expect("the echo carries the call");
        assert_eq!(structured["tool"], "read_file");
        assert_eq!(structured["arguments"]["value"], "hello");
        assert_eq!(structured["arguments"]["extra"], json!([1, 2, 3]));
    }

    #[tokio::test]
    async fn a_forwarded_answer_is_stamped_with_the_server_that_produced_it() {
        let upstream = bridged(FILES, vec![FakeTool::new("read_file", Behaviour::Echo)]).await;
        let tool = upstream.discovery.tools[0].clone();

        let result = upstream.call(&tool, None).await.expect("answers");

        let meta = result.meta.expect("a provenance stamp");
        assert_eq!(
            meta.0.get(crate::forward::META_SERVER),
            Some(&json!("files"))
        );
        assert_eq!(
            meta.0.get(crate::forward::META_TOOL),
            Some(&json!("read_file"))
        );
    }

    /// A tool-level failure is the upstream's answer, not this plugin's, and
    /// has to arrive as the upstream sent it.
    #[tokio::test]
    async fn an_upstream_tool_error_is_forwarded_as_an_error_result() {
        let upstream = bridged(
            FILES,
            vec![FakeTool::new(
                "read_file",
                Behaviour::ToolError("no such file".into()),
            )],
        )
        .await;
        let tool = upstream.discovery.tools[0].clone();

        let result = upstream.call(&tool, None).await.expect("an answer arrives");

        assert_eq!(result.is_error, Some(true));
        let rendered = serde_json::to_string(&result.content).expect("serializes");
        assert!(rendered.contains("no such file"), "{rendered}");
    }

    #[tokio::test]
    async fn an_answer_over_the_size_cap_is_refused_and_names_the_setting() {
        let document = r#"
version = 1

[[server]]
alias = "files"
transport = "stdio"
command = "npx"
max_result_bytes = 2048
call_timeout_secs = 5
"#;
        let upstream = bridged(
            document,
            vec![FakeTool::new("read_file", Behaviour::Bytes(20_000))],
        )
        .await;
        let tool = upstream.discovery.tools[0].clone();

        let error = upstream
            .call(&tool, None)
            .await
            .expect_err("an oversized answer is refused");

        assert!(error.contains("max_result_bytes"), "{error}");
        assert!(error.contains("rather than truncated"), "{error}");
    }

    #[tokio::test]
    async fn a_server_that_never_answers_times_out_instead_of_holding_the_call_open() {
        let upstream = bridged(FILES, vec![FakeTool::new("wedged", Behaviour::Hang)]).await;
        let tool = upstream.discovery.tools[0].clone();

        let started = std::time::Instant::now();
        let error = upstream
            .call(&tool, None)
            .await
            .expect_err("a wedged server times out");

        assert!(error.contains("did not answer"), "{error}");
        assert!(error.contains("call_timeout_secs"), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the timeout has to be the server's, not the runtime's"
        );
    }

    #[tokio::test]
    async fn a_server_that_vanishes_mid_call_fails_the_call_and_shows_up_as_down() {
        let upstream = bridged(
            FILES,
            vec![FakeTool::new("read_file", Behaviour::Disconnect)],
        )
        .await;
        let tool = upstream.discovery.tools[0].clone();

        let error = upstream
            .call(&tool, None)
            .await
            .expect_err("a vanished server fails the call");
        assert!(error.contains("files"), "{error}");

        // And the next call reports the link rather than the symptom, because
        // the connection has been noticed as closed by then.
        let second = upstream.call(&tool, None).await;
        assert!(second.is_err());
        let snapshot = upstream.snapshot().await;
        assert!(snapshot.last_error.is_some());
    }

    #[tokio::test]
    async fn the_allowlist_decides_what_a_real_server_contributes() {
        let document = r#"
version = 1

[[server]]
alias = "files"
transport = "stdio"
command = "npx"
allow_tools = ["read_*"]
deny_tools = ["read_secret"]
"#;
        let upstream = bridged(
            document,
            vec![
                FakeTool::new("read_file", Behaviour::Echo),
                FakeTool::new("read_secret", Behaviour::Echo),
                FakeTool::new("write_file", Behaviour::Echo),
            ],
        )
        .await;

        let exposed: Vec<&str> = upstream
            .discovery
            .tools
            .iter()
            .map(|tool| tool.upstream_name.as_str())
            .collect();
        assert_eq!(exposed, vec!["read_file"]);
        assert_eq!(upstream.discovery.excluded.len(), 2);
    }

    #[tokio::test]
    async fn a_server_that_publishes_no_schema_still_produces_a_tool_that_can_be_called() {
        let upstream = bridged(
            FILES,
            vec![
                FakeTool::new("bare", Behaviour::Echo)
                    .with_schema(json!({}))
                    .without_description(),
            ],
        )
        .await;
        let tool = upstream.discovery.tools[0].clone();

        assert_eq!(tool.schema_note, SchemaNote::ReplacedEmpty);
        assert!(tool.description.contains("published no description"));

        let result = upstream
            .call(&tool, json!({ "anything": true }).as_object().cloned())
            .await
            .expect("a permissive schema still forwards");
        let structured = result.structured_content.expect("the echo");
        assert_eq!(structured["arguments"]["anything"], json!(true));
    }

    #[tokio::test]
    async fn a_punctuated_upstream_name_is_renamed_but_called_under_its_own_name() {
        let upstream = bridged(FILES, vec![FakeTool::new("weird name/v2", Behaviour::Echo)]).await;
        let tool = upstream.discovery.tools[0].clone();

        assert_eq!(tool.bridged_name, "files__weird_name_v2");

        let result = upstream.call(&tool, None).await.expect("answers");
        let structured = result.structured_content.expect("the echo");
        // The fake server only knows its own name, so this proves the call went
        // out under the upstream spelling rather than the bridged one.
        assert_eq!(structured["tool"], "weird name/v2");
    }

    /// The one test that reaches outside this repository.
    ///
    /// It launches a genuine third-party MCP server —
    /// `@modelcontextprotocol/server-filesystem`, fetched by `npx` — as a real
    /// child process, and proves the whole point of the plugin against
    /// something nobody here wrote: it starts, its tools arrive namespaced
    /// under the operator's alias, its own schemas come through, and a call
    /// reaches it.
    ///
    /// Ignored by default because it needs `npx` on PATH and downloads a
    /// package from the npm registry. Run it with:
    ///
    /// ```text
    /// cargo test -- --ignored --nocapture
    /// ```
    #[tokio::test]
    #[ignore = "launches npx and downloads a package from npm"]
    async fn a_real_third_party_mcp_server_is_bridged_end_to_end() {
        let root = std::env::temp_dir().join("mcp-bridge-live-test");
        std::fs::create_dir_all(&root).expect("a directory to serve");
        std::fs::write(root.join("hello.txt"), b"hello from mcp-bridge").expect("a file to read");

        let document = format!(
            r#"
version = 1

[[server]]
alias = "files"
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", {root:?}]
connect_timeout_secs = 180
call_timeout_secs = 60
"#
        );

        let parent_env: BTreeMap<String, String> = std::env::vars().collect();
        let upstream = Upstream::start(spec(&document), Arc::new(parent_env)).await;

        let snapshot = upstream.snapshot().await;
        assert_eq!(
            snapshot.state,
            LinkState::Ready,
            "{:?}",
            snapshot.last_error
        );
        assert!(!upstream.discovery.tools.is_empty(), "it published tools");
        println!(
            "serverInfo: {:?} {:?}, protocol {:?}",
            snapshot.server_name, snapshot.server_version, snapshot.protocol_version
        );
        println!(
            "bridged {} tool(s) from a real MCP server:",
            upstream.discovery.tools.len()
        );
        for tool in &upstream.discovery.tools {
            println!(
                "  {} -> {} ({:?})",
                tool.bridged_name, tool.upstream_name, tool.schema_note
            );
            assert!(tool.bridged_name.starts_with("files__"));
        }

        let listing = upstream
            .discovery
            .tools
            .iter()
            .find(|tool| tool.upstream_name == "list_directory")
            .expect("the filesystem server publishes list_directory")
            .clone();
        let arguments = serde_json::json!({ "path": root }).as_object().cloned();
        let result = upstream
            .call(&listing, arguments)
            .await
            .expect("the real server answers");

        assert_ne!(result.is_error, Some(true), "{result:?}");
        let rendered = serde_json::to_string(&result.content).expect("serializes");
        assert!(rendered.contains("hello.txt"), "{rendered}");
        let stamp = result.meta.expect("a provenance stamp");
        assert_eq!(
            stamp.0.get(crate::forward::META_SERVER),
            Some(&json!("files"))
        );
    }

    #[tokio::test]
    async fn two_servers_bridged_at_once_answer_for_themselves() {
        let files = bridged(FILES, vec![FakeTool::new("search", Behaviour::Echo)]).await;
        let github = bridged(
            r#"
version = 1

[[server]]
alias = "github"
transport = "stdio"
command = "npx"
"#,
            vec![FakeTool::new("search", Behaviour::Echo)],
        )
        .await;

        assert_eq!(files.discovery.tools[0].bridged_name, "files__search");
        assert_eq!(github.discovery.tools[0].bridged_name, "github__search");

        let from_files = files
            .call(&files.discovery.tools[0].clone(), None)
            .await
            .expect("answers");
        let stamp = from_files.meta.expect("a stamp");
        assert_eq!(
            stamp.0.get(crate::forward::META_SERVER),
            Some(&json!("files"))
        );
    }
}
