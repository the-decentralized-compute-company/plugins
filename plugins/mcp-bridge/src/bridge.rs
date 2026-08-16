//! The registry: every bridged server, every bridged tool, and the answers the
//! three management tools return.
//!
//! Nothing here opens a connection. It resolves a bridged tool name to the
//! server that owns it, hands the call to [`crate::upstream`], and assembles
//! the operator-facing views. Keeping it that way is what lets `status` stay
//! cheap enough to be the tool you call when everything else is failing.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use rmcp::model::{CallToolResult, JsonObject};
use schemars::JsonSchema;
use serde::Serialize;

use crate::config::Document;
use crate::filter::FilterOutcome;
use crate::naming::{NameNote, split_bridged_name};
use crate::schema::SchemaNote;
use crate::upstream::{BridgedTool, LinkState, StatusSnapshot, Upstream};

pub const PLUGIN_NAME: &str = "mcp-bridge";

/// This plugin's own tools, as opposed to the ones it bridges.
///
/// Every bridged tool name contains `__` and none of these do, so a
/// third-party server cannot publish a tool that shadows one of them — see
/// [`crate::naming`].
pub const MANAGEMENT_TOOLS: &[&str] = &["status", "tools", "reconnect"];
pub const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The sentence that travels with every `status` response.
///
/// It is repeated rather than left to the README because the README is not
/// what somebody reads at 2 a.m. while wondering what this plugin is doing on
/// their machine.
pub const TRUST_NOTICE: &str = "Every server listed here runs third-party code with the \
    privileges of the tdcc process, and receives whatever arguments a model chooses to send it. \
    mcp-bridge launches and connects to exactly what is written in the server list and never \
    discovers a server on its own, so trust each entry as much as you would trust running that \
    binary yourself.";

// ---------------------------------------------------------------------------
// Where the server list stands
// ---------------------------------------------------------------------------

/// What happened when the server list was read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case", tag = "state")]
pub enum ConfigState {
    /// No file at the configured path. The plugin runs and bridges nothing.
    Absent,
    /// Read and validated.
    Loaded,
    /// The file exists but could not be read.
    Unreadable { error: String },
    /// The file exists and is not a valid server list. **Nothing is launched**
    /// — a half-understood list is not a list.
    Invalid { error: String },
}

impl ConfigState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Loaded => "loaded",
            Self::Unreadable { .. } => "unreadable",
            Self::Invalid { .. } => "invalid",
        }
    }
}

/// The server list as `status` reports it.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ConfigReport {
    /// Absolute or as-configured path this plugin read.
    pub path: String,
    #[serde(flatten)]
    pub state: ConfigState,
    /// Servers in the file, enabled or not.
    pub servers_configured: usize,
}

// ---------------------------------------------------------------------------
// The registry
// ---------------------------------------------------------------------------

/// Everything this plugin bridges.
pub struct Bridge {
    config: ConfigReport,
    servers: Vec<Arc<Upstream>>,
    by_alias: BTreeMap<String, Arc<Upstream>>,
    tools: BTreeMap<String, (Arc<Upstream>, BridgedTool)>,
    started_at_unix: u64,
}

impl Bridge {
    pub fn new(
        path: String,
        state: ConfigState,
        document: &Document,
        servers: Vec<Arc<Upstream>>,
    ) -> Self {
        let mut by_alias = BTreeMap::new();
        let mut tools = BTreeMap::new();
        for server in &servers {
            by_alias.insert(server.alias().to_string(), Arc::clone(server));
            for tool in &server.discovery.tools {
                tools.insert(
                    tool.bridged_name.clone(),
                    (Arc::clone(server), tool.clone()),
                );
            }
        }
        Self {
            config: ConfigReport {
                path,
                state,
                servers_configured: document.servers.len(),
            },
            servers,
            by_alias,
            tools,
            started_at_unix: now_unix(),
        }
    }

    /// An empty bridge, for the packaging and `--help` paths that must not read
    /// a node's configuration at all.
    pub fn detached(path: String, state: ConfigState) -> Self {
        Self::new(path, state, &Document::default(), Vec::new())
    }

    pub fn servers(&self) -> &[Arc<Upstream>] {
        &self.servers
    }

    /// Every bridged tool, in the order the manifest declares them.
    pub fn bridged_tools(&self) -> Vec<BridgedTool> {
        self.tools.values().map(|(_, tool)| tool.clone()).collect()
    }

    /// Forward one call. The name is the bridged one; the upstream sees its own.
    pub async fn call(
        &self,
        bridged_name: &str,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, String> {
        let Some((server, tool)) = self.tools.get(bridged_name) else {
            // Unreachable through the host, which only offers declared tools,
            // but a plugin is a process and a process gets called directly.
            // Naming the two halves separately is what turns "unknown tool"
            // into an answer.
            let detail = match split_bridged_name(bridged_name) {
                Some((alias, local)) if self.by_alias.contains_key(alias) => format!(
                    " Server '{alias}' is configured, but '{local}' is not among the tools this \
                     node bridged from it: it may have been excluded by allow_tools or \
                     deny_tools, or the server may not have been reachable at startup."
                ),
                Some((alias, _)) => format!(
                    " No server is aliased '{alias}'. Configured aliases: {}.",
                    self.alias_list()
                ),
                None => String::new(),
            };
            return Err(format!(
                "mcp-bridge does not bridge a tool called '{bridged_name}'.{detail} Call \
                 `mcp-bridge.tools` for the list this node actually projects."
            ));
        };
        server.call(tool, arguments).await
    }

    /// Cheap, network-free summary. This is the tool to call when nothing works.
    pub async fn status(&self) -> StatusResponse {
        let mut snapshots = Vec::with_capacity(self.servers.len());
        for server in &self.servers {
            snapshots.push(server.snapshot().await);
        }

        let totals = Totals {
            servers_configured: self.config.servers_configured,
            servers_enabled: snapshots.iter().filter(|item| item.enabled).count(),
            servers_ready: snapshots
                .iter()
                .filter(|item| item.state == LinkState::Ready)
                .count(),
            servers_unavailable: snapshots
                .iter()
                .filter(|item| matches!(item.state, LinkState::Down | LinkState::NeverConnected))
                .count(),
            tools_projected: self.tools.len(),
            tools_excluded: snapshots.iter().map(|item| item.tools_excluded).sum(),
        };

        StatusResponse {
            plugin: PLUGIN_NAME.to_string(),
            version: PLUGIN_VERSION.to_string(),
            started_at_unix: self.started_at_unix,
            config: self.config.clone(),
            totals,
            servers: snapshots,
            management_tools: MANAGEMENT_TOOLS
                .iter()
                .map(|name| (*name).to_string())
                .collect(),
            manifest_is_frozen: MANIFEST_FROZEN_NOTICE.to_string(),
            security: TRUST_NOTICE.to_string(),
        }
    }

    /// The full mapping between bridged names and upstream names.
    pub fn tools_report(&self, server: Option<&str>, include_excluded: bool) -> ToolsResponse {
        let mut tools = Vec::new();
        for (upstream, tool) in self.tools.values() {
            if server.is_some_and(|alias| alias != upstream.alias()) {
                continue;
            }
            tools.push(ToolReport {
                tool: tool.bridged_name.clone(),
                mcp_name: format!("{PLUGIN_NAME}.{}", tool.bridged_name),
                server: tool.alias.clone(),
                upstream_tool: tool.upstream_name.clone(),
                name_notes: tool
                    .name_notes
                    .iter()
                    .map(|note| note.as_str().to_string())
                    .collect(),
                renamed: tool
                    .name_notes
                    .iter()
                    .any(|note| !matches!(note, NameNote::Verbatim)),
                schema: tool.schema_note.clone(),
                schema_explanation: tool.schema_note.explanation().to_string(),
                transport: upstream.spec.transport.kind().to_string(),
            });
        }

        let mut excluded = Vec::new();
        if include_excluded {
            for upstream in &self.servers {
                if server.is_some_and(|alias| alias != upstream.alias()) {
                    continue;
                }
                for item in &upstream.discovery.excluded {
                    excluded.push(ExcludedReport {
                        server: upstream.alias().to_string(),
                        upstream_tool: item.upstream_name.clone(),
                        reason: item.outcome.reason(),
                        outcome: item.outcome.clone(),
                    });
                }
            }
        }

        let unknown_server = server
            .filter(|alias| !self.by_alias.contains_key(*alias))
            .map(|alias| {
                format!(
                    "no server is aliased '{alias}' in the mcp-bridge server list; configured \
                     aliases are: {}",
                    self.alias_list()
                )
            });

        ToolsResponse {
            tools,
            excluded,
            excluded_included: include_excluded,
            unknown_server,
            security: TRUST_NOTICE.to_string(),
        }
    }

    /// Force one server to be reconnected now.
    pub async fn reconnect(&self, alias: &str) -> Result<ReconnectResponse, String> {
        let Some(server) = self.by_alias.get(alias) else {
            return Err(format!(
                "no server is aliased '{alias}' in the mcp-bridge server list. Configured \
                 aliases: {}",
                self.alias_list()
            ));
        };
        let outcome = server.reconnect().await;
        let snapshot = server.snapshot().await;
        Ok(ReconnectResponse {
            server: alias.to_string(),
            reconnected: outcome.is_ok(),
            state: snapshot.state,
            error: outcome.err(),
            tools_projected: snapshot.tools_projected,
            drift_added: snapshot.drift_added,
            drift_missing: snapshot.drift_missing,
            manifest_is_frozen: MANIFEST_FROZEN_NOTICE.to_string(),
        })
    }

    fn alias_list(&self) -> String {
        if self.by_alias.is_empty() {
            return "(none)".to_string();
        }
        self.by_alias.keys().cloned().collect::<Vec<_>>().join(", ")
    }

    /// One line for the host's health check. Must stay fast and must not touch
    /// a bridged server.
    pub async fn health(&self) -> String {
        let mut ready = 0usize;
        let mut unavailable = 0usize;
        for server in &self.servers {
            match server.state().await {
                LinkState::Ready => ready += 1,
                LinkState::Down | LinkState::NeverConnected => unavailable += 1,
                LinkState::Disabled => {}
            }
        }
        format!(
            "config {}; {ready} server(s) ready, {unavailable} unavailable, {} tool(s) bridged",
            self.config.state.as_str(),
            self.tools.len()
        )
    }
}

/// Said in `status` and in every `reconnect` answer, because it is the one
/// thing about this plugin that surprises people.
pub const MANIFEST_FROZEN_NOTICE: &str = "The set of bridged tools is fixed when this plugin \
    starts: a plugin sends its manifest once, in the initialize response, and the plugin protocol \
    has no way to add a tool later. A server that was unreachable at startup contributes no tools \
    until tdcc restarts the plugin, and tools a server gains later are listed as drift rather than \
    projected.";

// ---------------------------------------------------------------------------
// Response shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Totals {
    pub servers_configured: usize,
    pub servers_enabled: usize,
    pub servers_ready: usize,
    pub servers_unavailable: usize,
    pub tools_projected: usize,
    pub tools_excluded: usize,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct StatusResponse {
    pub plugin: String,
    pub version: String,
    pub started_at_unix: u64,
    pub config: ConfigReport,
    pub totals: Totals,
    pub servers: Vec<StatusSnapshot>,
    /// This plugin's own tools, which are never bridged from anywhere.
    pub management_tools: Vec<String>,
    pub manifest_is_frozen: String,
    pub security: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ToolReport {
    /// The name inside this plugin.
    pub tool: String,
    /// The name on the node's MCP endpoint.
    pub mcp_name: String,
    /// The operator's alias for the server that answers it.
    pub server: String,
    /// What the upstream server calls it.
    pub upstream_tool: String,
    /// Whether the bridged name is anything other than a plain prefix.
    pub renamed: bool,
    pub name_notes: Vec<String>,
    pub schema: SchemaNote,
    pub schema_explanation: String,
    pub transport: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ExcludedReport {
    pub server: String,
    pub upstream_tool: String,
    pub reason: String,
    pub outcome: FilterOutcome,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ToolsResponse {
    pub tools: Vec<ToolReport>,
    pub excluded: Vec<ExcludedReport>,
    pub excluded_included: bool,
    /// Set when the caller named a server this node does not bridge.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unknown_server: Option<String>,
    pub security: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ReconnectResponse {
    pub server: String,
    pub reconnected: bool,
    pub state: LinkState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub tools_projected: usize,
    pub drift_added: Vec<String>,
    pub drift_missing: Vec<String>,
    pub manifest_is_frozen: String,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse_document;

    async fn bridge_from(document_text: &str) -> Bridge {
        let document = parse_document(document_text).expect("test document parses");
        let parent_env = Arc::new(BTreeMap::new());
        let mut servers = Vec::new();
        for spec in &document.servers {
            servers.push(Upstream::start(spec.clone(), Arc::clone(&parent_env)).await);
        }
        Bridge::new(
            "/home/operator/.tdcc/mcp-bridge.toml".to_string(),
            ConfigState::Loaded,
            &document,
            servers,
        )
    }

    const UNREACHABLE: &str = r#"
version = 1

[[server]]
alias = "files"
transport = "stdio"
command = "definitely-not-a-real-binary-9c1f"
connect_timeout_secs = 2

[[server]]
alias = "off"
transport = "stdio"
command = "definitely-not-a-real-binary-9c1f"
enabled = false
"#;

    #[tokio::test]
    async fn an_empty_bridge_reports_an_absent_server_list_rather_than_pretending() {
        let bridge = Bridge::detached("/nope/mcp-bridge.toml".to_string(), ConfigState::Absent);

        let status = bridge.status().await;
        assert_eq!(status.config.state, ConfigState::Absent);
        assert_eq!(status.totals.servers_configured, 0);
        assert_eq!(status.totals.tools_projected, 0);
        assert!(status.security.contains("never discovers a server"));
    }

    #[tokio::test]
    async fn an_invalid_server_list_launches_nothing_and_says_why() {
        let bridge = Bridge::detached(
            "/home/operator/.tdcc/mcp-bridge.toml".to_string(),
            ConfigState::Invalid {
                error: "alias 'Files' is not usable as a tool-name prefix".to_string(),
            },
        );

        let status = bridge.status().await;
        assert_eq!(status.config.state.as_str(), "invalid");
        assert!(status.servers.is_empty());
        let rendered = serde_json::to_string(&status).expect("serializes");
        assert!(
            rendered.contains("not usable as a tool-name prefix"),
            "{rendered}"
        );
    }

    #[tokio::test]
    async fn a_server_that_never_connected_is_counted_and_named_not_hidden() {
        let bridge = bridge_from(UNREACHABLE).await;

        let status = bridge.status().await;
        assert_eq!(status.totals.servers_configured, 2);
        assert_eq!(status.totals.servers_enabled, 1);
        assert_eq!(status.totals.servers_ready, 0);
        assert_eq!(status.totals.servers_unavailable, 1);

        let files = status
            .servers
            .iter()
            .find(|server| server.alias == "files")
            .expect("the files server is listed");
        assert_eq!(files.state, LinkState::NeverConnected);
        assert!(files.last_error.is_some());
    }

    #[tokio::test]
    async fn status_explains_that_the_tool_set_is_frozen_at_startup() {
        let bridge = bridge_from(UNREACHABLE).await;

        let status = bridge.status().await;
        assert!(status.manifest_is_frozen.contains("initialize response"));
    }

    #[tokio::test]
    async fn calling_a_tool_this_node_does_not_bridge_is_an_error_naming_the_tools_tool() {
        let bridge = bridge_from(UNREACHABLE).await;

        let error = bridge
            .call("files__read_file", None)
            .await
            .expect_err("nothing was discovered, so nothing is callable");

        assert!(error.contains("mcp-bridge.tools"), "{error}");
    }

    #[tokio::test]
    async fn reconnecting_a_server_that_is_not_configured_lists_the_ones_that_are() {
        let bridge = bridge_from(UNREACHABLE).await;

        let error = bridge
            .reconnect("nope")
            .await
            .expect_err("an unknown alias is an error");

        assert!(error.contains("files"), "{error}");
        assert!(error.contains("off"), "{error}");
    }

    #[tokio::test]
    async fn reconnecting_an_unreachable_server_reports_the_failure_rather_than_success() {
        let bridge = bridge_from(UNREACHABLE).await;

        let response = bridge
            .reconnect("files")
            .await
            .expect("a configured alias always answers");

        assert!(!response.reconnected);
        assert_eq!(response.state, LinkState::NeverConnected);
        assert!(response.error.is_some());
    }

    #[tokio::test]
    async fn the_tools_report_names_an_alias_this_node_does_not_have() {
        let bridge = bridge_from(UNREACHABLE).await;

        let report = bridge.tools_report(Some("ghost"), true);

        assert!(report.tools.is_empty());
        let message = report.unknown_server.expect("an unknown alias is reported");
        assert!(message.contains("files"), "{message}");
    }

    #[tokio::test]
    async fn health_is_one_line_and_names_the_config_state() {
        let bridge = bridge_from(UNREACHABLE).await;

        let health = bridge.health().await;

        assert!(!health.contains('\n'), "{health}");
        assert!(health.contains("loaded"), "{health}");
        assert!(health.contains("1 unavailable"), "{health}");
    }
}
