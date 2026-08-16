//! The whole contribution surface of `web-search` in one declaration.
//!
//! Two MCP tools, the same two operations mounted over HTTP, one capability,
//! and a health hook. The host synthesizes `tools/list`, `tools/call`, the JSON
//! Schema for every argument, and the request validation that runs before a
//! handler is entered — this plugin opens no socket and speaks no MCP.
//!
//! There is deliberately **no** `config_schema` and no `web_ui`. `[plugin.settings]`
//! never reaches a plugin process, so a schema here would render console
//! controls whose values could not affect the running search. See `config.rs`
//! for where settings actually come from.
//!
//! Macro field order is fixed: `metadata`, `startup_policy`, `provides`,
//! `config`, `web_ui`, `mesh`, `events`, `mcp`, `http`, `inference`, then the
//! lifecycle hooks.

use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use tdcc_plugin::{
    PluginMetadata, SimplePlugin, capability, http, mcp, plugin, plugin_server_info,
};

use crate::config::{PLUGIN_NAME, PLUGIN_VERSION};
use crate::engine::Engine;

/// Arguments for the `search` tool.
///
/// Every doc comment in this struct becomes a `description` in the JSON Schema
/// the host advertises, so a model reads these words when it decides how to
/// call the tool. They are written for that audience.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchArgs {
    /// What to search for, in plain words. Ordinary search syntax such as
    /// quoted phrases and `-excluded` terms is passed through to the backend.
    pub query: String,

    /// Restrict results to one domain, for example `doc.rust-lang.org`. Give a
    /// bare hostname, not a URL or a path.
    #[serde(default)]
    pub site: Option<String>,

    /// How many results to return. Clamped to the range the plugin was
    /// configured with; omit it to use the operator's default.
    #[serde(default)]
    pub count: Option<u32>,
}

/// Arguments for the `fetch` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct FetchArgs {
    /// The absolute http or https URL to read. Private, loopback, and
    /// link-local addresses are refused unless the operator opted in.
    pub url: String,

    /// Stop after roughly this many characters of extracted text. Clamped to
    /// the operator's configured ceiling; omit it to use that ceiling.
    #[serde(default)]
    pub max_chars: Option<u32>,
}

pub fn web_search_plugin(engine: Arc<Engine>) -> SimplePlugin {
    // One clone per handler closure; they all share the single HTTP client and
    // robots.txt cache inside the engine.
    let for_search_tool = Arc::clone(&engine);
    let for_fetch_tool = Arc::clone(&engine);
    let for_search_route = Arc::clone(&engine);
    let for_fetch_route = Arc::clone(&engine);
    let for_health = Arc::clone(&engine);

    plugin! {
        metadata: PluginMetadata::new(
            PLUGIN_NAME,
            PLUGIN_VERSION,
            plugin_server_info(
                PLUGIN_NAME,
                PLUGIN_VERSION,
                "Web search",
                "Web search and readable page fetching over a configurable backend",
                None::<String>,
            ),
        ),

        // A stable name for "something on this node can search the web", so a
        // caller can depend on the capability rather than on this plugin's id.
        provides: [capability("web-search.v1")],

        mcp: [
            // Projected as `web-search.search` on the host MCP endpoint.
            mcp::tool("search")
                .title("Search the web")
                .description(
                    "Search the web and return ranked results with a title, URL, and snippet \
                     for each. Use this to find pages; use `web-search.fetch` to read one. \
                     Requires the operator to have configured a search backend — either a \
                     Brave Search API key or a self-hosted SearXNG instance — and returns a \
                     message naming the missing setting when they have not.",
                )
                .input::<SearchArgs>()
                .handle(move |args: SearchArgs, _context| {
                    let engine = Arc::clone(&for_search_tool);
                    Box::pin(async move {
                        engine
                            .search(&args.query, args.site.as_deref(), args.count)
                            .await
                    })
                }),

            // Projected as `web-search.fetch`.
            mcp::tool("fetch")
                .title("Read a web page")
                .description(
                    "Fetch one http or https URL and return its readable text: scripts, styles, \
                     navigation, cookie banners, and other page furniture are removed, and \
                     headings and lists are kept. Honours robots.txt, follows a limited number \
                     of redirects, caps the response size, and refuses anything that is not \
                     HTML, plain text, or JSON.",
                )
                .input::<FetchArgs>()
                .handle(move |args: FetchArgs, _context| {
                    let engine = Arc::clone(&for_fetch_tool);
                    Box::pin(async move { engine.fetch(&args.url, args.max_chars).await })
                }),
        ],

        http: [
            // GET /api/plugins/web-search/http/search?query=…&count=…
            http::get("/search")
                .description("Search the web and return ranked results.")
                .input::<SearchArgs>()
                .handle(move |args: SearchArgs, _context| {
                    let engine = Arc::clone(&for_search_route);
                    Box::pin(async move {
                        engine
                            .search(&args.query, args.site.as_deref(), args.count)
                            .await
                    })
                }),

            // GET /api/plugins/web-search/http/fetch?url=…
            http::get("/fetch")
                .description("Fetch one URL and return its readable text.")
                .input::<FetchArgs>()
                .handle(move |args: FetchArgs, _context| {
                    let engine = Arc::clone(&for_fetch_route);
                    Box::pin(async move { engine.fetch(&args.url, args.max_chars).await })
                }),
        ],

        // Health must stay fast and independent of long-running work, so it
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

    use crate::config::Config;

    fn manifest() -> tdcc_plugin::proto::PluginManifest {
        let config = Config::parse(&[], &Default::default()).expect("defaults parse");
        let engine = Engine::new(config).expect("client builds");
        web_search_plugin(engine)
            .manifest()
            .expect("declarative plugins have a manifest")
    }

    #[test]
    fn both_tools_are_declared_with_descriptions_and_schemas() {
        let manifest = manifest();

        for name in ["search", "fetch"] {
            let operation = manifest
                .operations
                .iter()
                .find(|operation| operation.name == name)
                .unwrap_or_else(|| panic!("`{name}` is declared"));
            assert!(
                operation.description.len() > 40,
                "`{name}` needs a description a model can act on"
            );
            let schema = &operation.input_schema_json;
            assert!(schema.contains("\"properties\""), "{schema}");
        }
    }

    #[test]
    fn the_argument_schemas_carry_the_doc_comments_a_model_reads() {
        let manifest = manifest();
        let search = manifest
            .operations
            .iter()
            .find(|operation| operation.name == "search")
            .expect("search is declared");

        let schema = &search.input_schema_json;
        assert!(
            schema.contains("Restrict results to one domain"),
            "{schema}"
        );
        assert!(schema.contains("\"required\""), "{schema}");
    }

    #[test]
    fn the_http_routes_mirror_the_two_tools() {
        let manifest = manifest();

        let paths: Vec<&str> = manifest
            .http_bindings
            .iter()
            .map(|binding| binding.path.as_str())
            .collect();
        assert!(paths.contains(&"/search"), "{paths:?}");
        assert!(paths.contains(&"/fetch"), "{paths:?}");
    }

    #[test]
    fn no_config_schema_or_web_ui_is_declared() {
        let manifest = manifest();

        // Both would be dishonest surfaces for this plugin: settings never
        // reach the process, and there is no bundle to serve.
        assert!(manifest.config_schema.is_none());
        assert!(manifest.web_ui.is_none());
        assert_eq!(manifest.capabilities, vec!["web-search.v1".to_string()]);
    }
}
