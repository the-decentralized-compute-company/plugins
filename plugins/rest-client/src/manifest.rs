//! The whole contribution surface of `rest-client` in one declaration.
//!
//! Four MCP tools, three of them also mounted over HTTP, one capability, and a
//! health hook. The host synthesizes `tools/list`, `tools/call`, the JSON
//! Schema for every argument struct, and the request validation that runs
//! before a handler is entered — this plugin opens no socket and speaks no MCP.
//!
//! There is deliberately **no** `config_schema` and no `web_ui`.
//! `[plugin.settings]` never reaches a plugin process, so a schema here would
//! render console controls that could not affect a single request, and the
//! declaration this plugin needs is a document, not a handful of scalars. See
//! `cli.rs` for where configuration actually comes from.
//!
//! Macro field order is fixed: `metadata`, `startup_policy`, `provides`,
//! `config`, `web_ui`, `mesh`, `events`, `mcp`, `http`, `inference`, then the
//! lifecycle hooks.

use std::collections::BTreeMap;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use tdcc_plugin::{
    PluginMetadata, SimplePlugin, capability, http, mcp, plugin, plugin_server_info,
};

use crate::engine::Engine;
use crate::{PLUGIN_NAME, PLUGIN_VERSION};

/// The fixed part of the `call` tool description. The generated catalog of
/// declared operations is appended to it at startup — see `schema.rs`.
const CALL_PREAMBLE: &str = "\
Call one REST API operation that this node's operator has declared. You do not \
supply a URL: name an `endpoint` and an `operation` from the list below and pass \
its declared parameters in `params`. Anything not declared here is unreachable \
through this tool, by design. A non-2xx response comes back as an error whose \
message and structured data both carry the HTTP status, so a 404 is \
distinguishable from a 500. Operations available on this node:
";

/// Arguments for the `call` tool.
///
/// Every doc comment in this struct becomes a `description` in the JSON Schema
/// the host advertises, so a model reads these words when it decides how to
/// call the tool. They are written for that audience.
///
/// `deny_unknown_fields` is load-bearing: it guarantees there is nowhere for a
/// caller to put a header, a URL, a method, or a timeout. The only things that
/// cross this boundary are a name, a name, some values, and a body.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CallArgs {
    /// Name of the declared endpoint, for example `github`. Use
    /// `rest-client.endpoints` to see which names this node offers. There is no
    /// way to reach an API that is not on that list.
    pub endpoint: String,

    /// Name of the operation on that endpoint, for example `list_issues`. The
    /// operation fixes the HTTP method and the path; you choose neither.
    pub operation: String,

    /// Values for the operation's declared parameters, as a JSON object keyed
    /// by parameter name — `{"owner": "rust-lang", "repo": "rust"}`. Omit a
    /// parameter to use its declared default. A name that is not declared on
    /// the operation is an error rather than an extra query string entry. Call
    /// `rest-client.describe` for the exact schema.
    #[serde(default)]
    pub params: Option<BTreeMap<String, Value>>,

    /// JSON request body, for operations that declare one. Sent verbatim as the
    /// operation's declared content type. Supplying a body to an operation that
    /// does not declare one is an error.
    #[serde(default)]
    pub body: Option<Value>,
}

/// Arguments for the `describe` tool.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DescribeArgs {
    /// Name of the declared endpoint to describe.
    pub endpoint: String,

    /// One operation on that endpoint. Omit it to get every operation the
    /// endpoint declares, each with its own parameter schema.
    #[serde(default)]
    pub operation: Option<String>,
}

/// Arguments for the tools that take none.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NoArgs {}

pub fn rest_client_plugin(engine: Arc<Engine>) -> SimplePlugin {
    // One clone per handler closure; they all share the single HTTP client,
    // resolved credentials, and call budget inside the engine.
    let for_status_tool = Arc::clone(&engine);
    let for_endpoints_tool = Arc::clone(&engine);
    let for_describe_tool = Arc::clone(&engine);
    let for_call_tool = Arc::clone(&engine);
    let for_status_route = Arc::clone(&engine);
    let for_endpoints_route = Arc::clone(&engine);
    let for_call_route = Arc::clone(&engine);
    let for_health = Arc::clone(&engine);

    // Built once, from the operator's declaration, before the manifest exists.
    let call_description = format!("{CALL_PREAMBLE}{}", engine.call_description());

    plugin! {
        metadata: PluginMetadata::new(
            PLUGIN_NAME,
            PLUGIN_VERSION,
            plugin_server_info(
                PLUGIN_NAME,
                PLUGIN_VERSION,
                "REST client",
                "Call REST APIs an operator declared, without giving a model an outbound socket",
                None::<String>,
            ),
        ),

        // A stable name for "something on this node can call declared APIs", so
        // a caller can depend on the capability rather than on this plugin's id.
        provides: [capability("rest-client.v1")],

        mcp: [
            // Projected as `rest-client.status` on the host MCP endpoint.
            mcp::tool("status")
                .title("Show the declared endpoints and their state")
                .description(
                    "Show how rest-client is configured on this node: which declaration file it \
                     read, how many endpoints it found, and for each one the auth kind, the \
                     environment variable its credential comes from, whether that variable was \
                     present at startup, and how much of its per-minute call budget is spent. \
                     Makes no network requests. This is the tool to call when something else \
                     here is failing.",
                )
                .input::<NoArgs>()
                .handle(move |_args: NoArgs, _context| {
                    let engine = Arc::clone(&for_status_tool);
                    Box::pin(async move { Ok(engine.status()) })
                }),

            // Projected as `rest-client.endpoints`.
            mcp::tool("endpoints")
                .title("List the APIs this node can call")
                .description(
                    "List every API endpoint the operator declared, with its base URL, allowed \
                     methods and paths, and the operations it offers with their parameters. Start \
                     here: an endpoint that is not on this list cannot be reached through this \
                     plugin at all, and there is no way to pass a URL.",
                )
                .input::<NoArgs>()
                .handle(move |_args: NoArgs, _context| {
                    let engine = Arc::clone(&for_endpoints_tool);
                    Box::pin(async move { Ok(engine.endpoints()) })
                }),

            // Projected as `rest-client.describe`.
            mcp::tool("describe")
                .title("Show one operation's parameter schema")
                .description(
                    "Show one endpoint, or one operation on it, in full — including a JSON Schema \
                     for the operation's parameters, which parameters go in the path and which in \
                     the query string, and whether it takes a request body. Call this before \
                     `rest-client.call` when the summary from `rest-client.endpoints` is not \
                     enough.",
                )
                .input::<DescribeArgs>()
                .handle(move |args: DescribeArgs, _context| {
                    let engine = Arc::clone(&for_describe_tool);
                    Box::pin(async move {
                        engine.describe(&args.endpoint, args.operation.as_deref())
                    })
                }),

            // Projected as `rest-client.call`. Its description carries the
            // generated catalog of every declared operation.
            mcp::tool("call")
                .title("Call a declared API operation")
                .description(call_description)
                .input::<CallArgs>()
                .handle(move |args: CallArgs, _context| {
                    let engine = Arc::clone(&for_call_tool);
                    Box::pin(async move {
                        engine
                            .call(
                                &args.endpoint,
                                &args.operation,
                                &args.params.unwrap_or_default(),
                                args.body.as_ref(),
                            )
                            .await
                    })
                }),
        ],

        http: [
            // GET /api/plugins/rest-client/http/status
            http::get("/status")
                .description("Show how rest-client is configured on this node.")
                .input::<NoArgs>()
                .handle(move |_args: NoArgs, _context| {
                    let engine = Arc::clone(&for_status_route);
                    Box::pin(async move { Ok(engine.status()) })
                }),

            // GET /api/plugins/rest-client/http/endpoints
            http::get("/endpoints")
                .description("List the APIs this node can call.")
                .input::<NoArgs>()
                .handle(move |_args: NoArgs, _context| {
                    let engine = Arc::clone(&for_endpoints_route);
                    Box::pin(async move { Ok(engine.endpoints()) })
                }),

            // POST /api/plugins/rest-client/http/call
            http::post("/call")
                .description("Call one declared API operation.")
                .input::<CallArgs>()
                .handle(move |args: CallArgs, _context| {
                    let engine = Arc::clone(&for_call_route);
                    Box::pin(async move {
                        engine
                            .call(
                                &args.endpoint,
                                &args.operation,
                                &args.params.unwrap_or_default(),
                                args.body.as_ref(),
                            )
                            .await
                    })
                }),
        ],

        // Health must stay fast and independent of any call in flight, so it
        // reports configuration state and never makes a request.
        health: move |_context| {
            let engine = Arc::clone(&for_health);
            Box::pin(async move { Ok(engine.health()) })
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tdcc_plugin::Plugin;

    use crate::catalog;
    use crate::engine::CatalogSource;

    fn manifest_for(document: &str) -> tdcc_plugin::proto::PluginManifest {
        let catalog = catalog::parse(document).expect("the document parses");
        let engine = Engine::new(
            catalog,
            CatalogSource::Loaded(std::path::PathBuf::from("/test/rest-client.toml")),
            &Default::default(),
            "tdcc-rest-client/test",
        )
        .expect("the client builds");
        rest_client_plugin(engine)
            .manifest()
            .expect("declarative plugins have a manifest")
    }

    fn manifest() -> tdcc_plugin::proto::PluginManifest {
        manifest_for(catalog::SAMPLE)
    }

    #[test]
    fn all_four_tools_are_declared_with_descriptions_and_schemas() {
        let manifest = manifest();

        for name in ["status", "endpoints", "describe", "call"] {
            let operation = manifest
                .operations
                .iter()
                .find(|operation| operation.name == name)
                .unwrap_or_else(|| panic!("`{name}` is declared"));
            assert!(
                operation.description.len() > 40,
                "`{name}` needs a description a model can act on"
            );
            assert!(
                operation.input_schema_json.contains("\"type\""),
                "{}",
                operation.input_schema_json
            );
        }
    }

    #[test]
    fn the_call_schema_has_exactly_four_fields_and_no_room_for_a_url() {
        let manifest = manifest();
        let call = manifest
            .operations
            .iter()
            .find(|operation| operation.name == "call")
            .expect("call is declared");

        let schema: serde_json::Value =
            serde_json::from_str(&call.input_schema_json).expect("the schema is JSON");
        let properties = schema["properties"].as_object().expect("an object schema");

        let mut names: Vec<&str> = properties.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(names, ["body", "endpoint", "operation", "params"]);
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(
            schema["required"],
            serde_json::json!(["endpoint", "operation"])
        );
    }

    #[test]
    fn the_call_description_carries_the_operators_own_catalog() {
        let manifest = manifest();
        let call = manifest
            .operations
            .iter()
            .find(|operation| operation.name == "call")
            .expect("call is declared");

        assert!(
            call.description.contains("You do not supply a URL"),
            "{}",
            call.description
        );
        assert!(
            call.description
                .contains("example.get_thing — GET /things/{id}"),
            "{}",
            call.description
        );
        assert!(
            call.description
                .contains("Fetch one thing by its identifier."),
            "{}",
            call.description
        );
    }

    #[test]
    fn a_node_with_no_declaration_still_serves_a_usable_manifest() {
        let manifest = manifest_for("version = 1\n");
        let call = manifest
            .operations
            .iter()
            .find(|operation| operation.name == "call")
            .expect("call is declared even with nothing to call");

        assert!(
            call.description.contains("No endpoints are declared"),
            "{}",
            call.description
        );
    }

    #[test]
    fn the_argument_schemas_carry_the_doc_comments_a_model_reads() {
        let manifest = manifest();
        let describe = manifest
            .operations
            .iter()
            .find(|operation| operation.name == "describe")
            .expect("describe is declared");

        assert!(
            describe
                .input_schema_json
                .contains("Omit it to get every operation"),
            "{}",
            describe.input_schema_json
        );
    }

    #[test]
    fn three_of_the_four_tools_are_also_mounted_over_http() {
        let manifest = manifest();

        let paths: Vec<&str> = manifest
            .http_bindings
            .iter()
            .map(|binding| binding.path.as_str())
            .collect();
        assert!(paths.contains(&"/status"), "{paths:?}");
        assert!(paths.contains(&"/endpoints"), "{paths:?}");
        assert!(paths.contains(&"/call"), "{paths:?}");
        assert_eq!(paths.len(), 3);
    }

    #[test]
    fn no_config_schema_web_ui_mesh_or_event_surface_is_declared() {
        let manifest = manifest();

        // All four would be surfaces this plugin does not need: settings never
        // reach the process, there is no bundle, and delivery is allowlist
        // based, so declaring nothing means receiving nothing.
        assert!(manifest.config_schema.is_none());
        assert!(manifest.web_ui.is_none());
        assert!(manifest.mesh_channels.is_empty());
        assert!(manifest.mesh_event_subscriptions.is_empty());
        assert_eq!(manifest.capabilities, vec!["rest-client.v1".to_string()]);
    }
}
