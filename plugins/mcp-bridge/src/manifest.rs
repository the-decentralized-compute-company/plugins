//! Everything `mcp-bridge` contributes to the host, in one declaration plus one
//! loop.
//!
//! The declaration is the ordinary `plugin!` macro: three management tools, the
//! same three as HTTP routes, one capability, and a health hook.
//!
//! The loop is the part that makes this plugin what it is. Every tool an
//! upstream server published at startup is appended to the same manifest as an
//! operation carrying **the upstream's own JSON Schema**, with a handler that
//! forwards the call. The `plugin!` macro cannot express that — its tool
//! builder takes a Rust type and derives a schema from it, which is exactly the
//! wrong thing to do to somebody else's contract — so the bridged tools are
//! added through the SDK's lower-level `OperationRouter` and
//! `proto::OperationManifest` instead. The host cannot tell the difference: a
//! bridged tool and a hand-written one are the same two fields in the same
//! manifest.
//!
//! **The manifest is sent once.** `PluginRuntime` puts it in the initialize
//! response and the plugin protocol has no update message, so discovery must
//! finish before the runtime starts. That is why `main` connects to every
//! server before it calls `PluginRuntime::run`, and why every response that
//! could mislead somebody about it repeats
//! [`crate::bridge::MANIFEST_FROZEN_NOTICE`].
//!
//! Macro field order is fixed: `metadata`, `startup_policy`, `provides`,
//! `config`, `web_ui`, `mesh`, `events`, `mcp`, `http`, `inference`,
//! `admission`, then the lifecycle hooks.
//!
//! No `mesh` and no `events` are declared. Delivery is allowlist-based, so this
//! plugin receives no channel messages and no mesh events — a component whose
//! job is handing arguments to third-party binaries is the last place that
//! should accept unsolicited input from the network.

use std::collections::BTreeSet;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value};
use tdcc_plugin::{
    OperationRouter, Plugin, PluginError, PluginMetadata, SimplePlugin, capability, http, mcp,
    operation_with_schema, plugin, plugin_server_info, proto,
};

use crate::bridge::{Bridge, PLUGIN_NAME, PLUGIN_VERSION};
use crate::forward::arguments_from_value;
use crate::schema::permissive_schema_json;

/// Arguments for the `status` tool.
///
/// It takes none: `status` has to keep answering when everything else is
/// broken, so there is nothing to get wrong.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StatusArgs {}

/// Arguments for the `tools` tool.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ToolsArgs {
    /// Only report tools from the server the operator gave this alias — the
    /// part of a bridged tool name before the double underscore. Omit it for
    /// every server.
    #[serde(default)]
    pub server: Option<String>,

    /// Also list the tools the operator's `allow_tools` or `deny_tools` kept
    /// out, with the pattern that excluded each one. Defaults to true, because
    /// "the server has that tool but this node does not expose it" is usually
    /// the answer somebody is looking for.
    #[serde(default)]
    pub include_excluded: Option<bool>,
}

/// Arguments for the `reconnect` tool.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReconnectArgs {
    /// Alias of the one server to reconnect, exactly as it appears in the
    /// mcp-bridge server list. There is deliberately no "all" — relaunching
    /// every third-party process on the machine should take one call per
    /// process.
    pub server: String,
}

pub fn mcp_bridge_plugin(bridge: Arc<Bridge>) -> SimplePlugin {
    let for_status_tool = Arc::clone(&bridge);
    let for_tools_tool = Arc::clone(&bridge);
    let for_reconnect_tool = Arc::clone(&bridge);
    let for_status_route = Arc::clone(&bridge);
    let for_tools_route = Arc::clone(&bridge);
    let for_reconnect_route = Arc::clone(&bridge);
    let for_health = Arc::clone(&bridge);

    let plugin = plugin! {
        metadata: PluginMetadata::new(
            PLUGIN_NAME,
            PLUGIN_VERSION,
            plugin_server_info(
                PLUGIN_NAME,
                PLUGIN_VERSION,
                "MCP bridge",
                "Expose MCP servers the operator listed as tools on this node",
                None::<String>,
            ),
        ),

        // A stable name for "this node can reach MCP servers the operator
        // added", so something else can depend on the capability rather than on
        // this plugin's id.
        provides: [capability("mcp-bridge.v1")],

        mcp: [
            // Projected as `mcp-bridge.status`.
            mcp::tool("status")
                .title("Bridged MCP servers")
                .description(
                    "Report every MCP server this node bridges: where its definition came from, \
                     whether it is connected, how many of its tools are exposed, how many the \
                     operator's allowlist or denylist kept out, and why the last attempt failed \
                     if one did. Touches no server and opens no connection, so it keeps \
                     answering when a bridged server does not.",
                )
                .input::<StatusArgs>()
                .handle(move |_args: StatusArgs, _context| {
                    let bridge = Arc::clone(&for_status_tool);
                    Box::pin(async move { Ok(bridge.status().await) })
                }),

            // Projected as `mcp-bridge.tools`.
            mcp::tool("tools")
                .title("Bridged tool map")
                .description(
                    "Map every bridged tool back to the server and the upstream tool name it \
                     forwards to, and say whether that server's own JSON Schema was forwarded \
                     unchanged. Use it to find out which server answers a given tool, why a tool \
                     you expected is missing, or why a name looks different from the upstream's \
                     documentation.",
                )
                .input::<ToolsArgs>()
                .handle(move |args: ToolsArgs, _context| {
                    let bridge = Arc::clone(&for_tools_tool);
                    Box::pin(async move {
                        Ok(bridge.tools_report(
                            args.server.as_deref(),
                            args.include_excluded.unwrap_or(true),
                        ))
                    })
                }),

            // Projected as `mcp-bridge.reconnect`.
            mcp::tool("reconnect")
                .title("Reconnect one bridged server")
                .description(
                    "Drop the connection to one bridged MCP server and open a new one now, \
                     instead of waiting for the backoff schedule. For a stdio server this kills \
                     the current child process and launches a replacement, so it restarts \
                     third-party code on this machine: name the one server you mean. The set of \
                     bridged tools does not change — it is fixed when the plugin starts.",
                )
                .input::<ReconnectArgs>()
                .handle(move |args: ReconnectArgs, _context| {
                    let bridge = Arc::clone(&for_reconnect_tool);
                    Box::pin(async move {
                        bridge
                            .reconnect(args.server.trim())
                            .await
                            .map_err(PluginError::invalid_params)
                    })
                }),
        ],

        http: [
            // GET /api/plugins/mcp-bridge/http/status
            http::get("/status")
                // Explicit binding ids. The default is derived from the method
                // and the path and comes out as `http_get__status`, which
                // contains the `__` that separates a bridged tool's alias from
                // its name — so a server aliased `http_get` publishing
                // `status` would land on exactly that operation name.
                .binding_id("http_status")
                .description("Report every bridged MCP server and its state.")
                .input::<StatusArgs>()
                .handle(move |_args: StatusArgs, _context| {
                    let bridge = Arc::clone(&for_status_route);
                    Box::pin(async move { Ok(bridge.status().await) })
                }),

            // GET /api/plugins/mcp-bridge/http/tools?server=files
            http::get("/tools")
                .binding_id("http_tools")
                .description("Map bridged tool names back to their servers and upstream names.")
                .input::<ToolsArgs>()
                .handle(move |args: ToolsArgs, _context| {
                    let bridge = Arc::clone(&for_tools_route);
                    Box::pin(async move {
                        Ok(bridge.tools_report(
                            args.server.as_deref(),
                            args.include_excluded.unwrap_or(true),
                        ))
                    })
                }),

            // POST /api/plugins/mcp-bridge/http/reconnect
            http::post("/reconnect")
                .binding_id("http_reconnect")
                .description("Reconnect one bridged MCP server now.")
                .input::<ReconnectArgs>()
                .handle(move |args: ReconnectArgs, _context| {
                    let bridge = Arc::clone(&for_reconnect_route);
                    Box::pin(async move {
                        bridge
                            .reconnect(args.server.trim())
                            .await
                            .map_err(PluginError::invalid_params)
                    })
                }),
        ],

        // Health must stay fast and independent of long-running work, so it
        // reads recorded state and never touches a bridged server.
        health: move |_context| {
            let bridge = Arc::clone(&for_health);
            Box::pin(async move { Ok(bridge.health().await) })
        },
    };

    attach_bridged_tools(plugin, bridge)
}

/// Append one operation per discovered upstream tool.
///
/// The manifest entry carries the upstream's `description` and its schema
/// verbatim; the router entry carries a handler that forwards the call to the
/// server that published it.
fn attach_bridged_tools(plugin: SimplePlugin, bridge: Arc<Bridge>) -> SimplePlugin {
    let mut manifest = plugin
        .manifest()
        .expect("a declarative plugin always carries a manifest");
    let mut router = OperationRouter::new();

    // Belt and braces over `naming`'s guarantees. Adding an operation whose
    // name is already declared would replace this plugin's own handler with a
    // third-party server's, which is not a property to leave resting on one
    // module's invariants and one module's tests.
    let mut taken: BTreeSet<String> = manifest
        .operations
        .iter()
        .map(|operation| operation.name.clone())
        .collect();

    for tool in bridge.bridged_tools() {
        if !reserve(&mut taken, &tool.bridged_name) {
            eprintln!(
                "mcp-bridge: refusing to bridge '{}' from server '{}': that operation name is \
                 already declared by this plugin",
                tool.bridged_name, tool.alias
            );
            continue;
        }

        // Two representations of the same schema: the manifest wants the JSON
        // text, the router wants the parsed object. They come from one string
        // so they cannot disagree.
        let schema = parse_schema_object(&tool.input_schema_json);

        manifest.operations.push(proto::OperationManifest {
            name: tool.bridged_name.clone(),
            description: tool.description.clone(),
            input_schema_json: tool.input_schema_json.clone(),
            // The upstream's output schema is deliberately not forwarded: the
            // host drops any output schema that is not an object, and a wrong
            // one is worse than none.
            output_schema_json: None,
            title: tool.title.clone(),
        });

        let declaration =
            operation_with_schema(tool.bridged_name.clone(), tool.description.clone(), schema);
        let bridge = Arc::clone(&bridge);
        let bridged_name = tool.bridged_name.clone();
        router.add_raw(declaration, move |request, _context| {
            let bridge = Arc::clone(&bridge);
            let bridged_name = bridged_name.clone();
            Box::pin(async move {
                let arguments = arguments_from_value(&request.arguments)
                    .map_err(PluginError::invalid_params)?;
                bridge
                    .call(&bridged_name, arguments)
                    .await
                    // An unreachable upstream is an error, never an empty
                    // success: a caller cannot tell "the server is down" from
                    // "the answer was nothing" any other way.
                    .map_err(PluginError::internal)
            })
        });
    }

    plugin
        .with_manifest(manifest)
        .extend_operation_router(router)
}

/// Claim one operation name, refusing a name that is already declared.
fn reserve(taken: &mut BTreeSet<String>, name: &str) -> bool {
    taken.insert(name.to_string())
}

fn parse_schema_object(json: &str) -> Map<String, Value> {
    serde_json::from_str::<Value>(json)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_else(|| {
            serde_json::from_str::<Value>(&permissive_schema_json())
                .ok()
                .and_then(|value| value.as_object().cloned())
                .unwrap_or_default()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use crate::bridge::{ConfigState, MANAGEMENT_TOOLS};
    use crate::config::parse_document;
    use crate::upstream::Upstream;

    async fn plugin_for(document_text: &str) -> SimplePlugin {
        let document = parse_document(document_text).expect("test document parses");
        let parent_env = Arc::new(BTreeMap::new());
        let mut servers = Vec::new();
        for spec in &document.servers {
            servers.push(Upstream::start(spec.clone(), Arc::clone(&parent_env)).await);
        }
        let bridge = Bridge::new(
            "/home/operator/.tdcc/mcp-bridge.toml".to_string(),
            ConfigState::Loaded,
            &document,
            servers,
        );
        mcp_bridge_plugin(Arc::new(bridge))
    }

    fn manifest_of(plugin: &SimplePlugin) -> proto::PluginManifest {
        plugin
            .manifest()
            .expect("declarative plugins have a manifest")
    }

    const NO_SERVERS: &str = "version = 1\n";

    #[tokio::test]
    async fn the_three_management_tools_are_declared_with_usable_descriptions() {
        let manifest = manifest_of(&plugin_for(NO_SERVERS).await);

        for name in MANAGEMENT_TOOLS {
            let operation = manifest
                .operations
                .iter()
                .find(|operation| operation.name == *name)
                .unwrap_or_else(|| panic!("`{name}` is declared"));
            assert!(
                operation.description.len() > 60,
                "`{name}` needs a description a model can act on"
            );
            assert!(
                operation.input_schema_json.contains("\"type\""),
                "{}",
                operation.input_schema_json
            );
        }
    }

    #[tokio::test]
    async fn a_node_with_no_servers_declares_only_this_plugins_own_operations() {
        let manifest = manifest_of(&plugin_for(NO_SERVERS).await);

        let declared: Vec<&str> = manifest
            .operations
            .iter()
            .map(|operation| operation.name.as_str())
            .collect();
        assert_eq!(
            declared,
            vec![
                "status",
                "tools",
                "reconnect",
                "http_status",
                "http_tools",
                "http_reconnect"
            ]
        );
    }

    /// A bridged name always contains `__`. If an HTTP binding's operation name
    /// did too — and the default derived one, `http_get__status`, does — then a
    /// server aliased `http_get` publishing `status` would collide with it.
    #[tokio::test]
    async fn no_operation_this_plugin_declares_can_be_spelled_as_a_bridged_name() {
        let manifest = manifest_of(&plugin_for(NO_SERVERS).await);

        for operation in &manifest.operations {
            assert!(
                !operation.name.contains("__"),
                "`{}` is spellable as <alias>__<tool>",
                operation.name
            );
        }
    }

    #[test]
    fn an_operation_name_that_is_already_declared_is_refused_rather_than_replaced() {
        let mut taken: BTreeSet<String> = MANAGEMENT_TOOLS
            .iter()
            .map(|name| (*name).to_string())
            .collect();

        assert!(reserve(&mut taken, "files__read_file"));
        assert!(!reserve(&mut taken, "files__read_file"));
        assert!(!reserve(&mut taken, "status"));
    }

    #[tokio::test]
    async fn a_server_that_could_not_be_reached_contributes_no_tools_but_does_not_stop_the_plugin()
    {
        let manifest = manifest_of(
            &plugin_for(
                r#"
version = 1

[[server]]
alias = "files"
transport = "stdio"
command = "definitely-not-a-real-binary-9c1f"
connect_timeout_secs = 2
"#,
            )
            .await,
        );

        // The management tools are still there, which is what makes `status`
        // able to explain the absence.
        for name in MANAGEMENT_TOOLS {
            assert!(
                manifest
                    .operations
                    .iter()
                    .any(|operation| operation.name == *name),
                "{name}"
            );
        }
        assert!(
            !manifest
                .operations
                .iter()
                .any(|operation| operation.name.contains("__"))
        );
    }

    #[tokio::test]
    async fn the_http_routes_mirror_the_three_tools() {
        let manifest = manifest_of(&plugin_for(NO_SERVERS).await);

        let paths: Vec<&str> = manifest
            .http_bindings
            .iter()
            .map(|binding| binding.path.as_str())
            .collect();
        assert!(paths.contains(&"/status"), "{paths:?}");
        assert!(paths.contains(&"/tools"), "{paths:?}");
        assert!(paths.contains(&"/reconnect"), "{paths:?}");
    }

    #[tokio::test]
    async fn no_config_schema_web_ui_mesh_channel_or_event_is_declared() {
        let manifest = manifest_of(&plugin_for(NO_SERVERS).await);

        // A settings schema would draw console controls whose values never
        // reach this process, and a bridge should receive nothing unsolicited.
        assert!(manifest.config_schema.is_none());
        assert!(manifest.web_ui.is_none());
        assert!(manifest.mesh_channels.is_empty());
        assert!(manifest.mesh_event_subscriptions.is_empty());
        assert_eq!(manifest.capabilities, vec!["mcp-bridge.v1".to_string()]);
    }

    /// The whole point of the plugin, checked at the manifest boundary: a
    /// bridged tool's declared schema is the upstream's, not one derived from a
    /// Rust type here.
    #[test]
    fn a_bridged_operation_carries_the_upstream_schema_text_it_was_given() {
        let upstream_schema =
            r#"{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}"#;

        let parsed = parse_schema_object(upstream_schema);
        let round_tripped = Value::Object(parsed).to_string();

        let original: Value = serde_json::from_str(upstream_schema).expect("valid");
        let after: Value = serde_json::from_str(&round_tripped).expect("valid");
        assert_eq!(original, after);
    }

    #[test]
    fn an_unparseable_schema_falls_back_to_a_permissive_object_rather_than_panicking() {
        let parsed = parse_schema_object("not json at all");

        assert_eq!(parsed.get("type"), Some(&Value::String("object".into())));
        assert_eq!(parsed.get("additionalProperties"), Some(&Value::Bool(true)));
    }

    #[test]
    fn management_tool_names_are_the_ones_the_naming_module_relies_on() {
        assert_eq!(MANAGEMENT_TOOLS, ["status", "tools", "reconnect"]);
    }
}
