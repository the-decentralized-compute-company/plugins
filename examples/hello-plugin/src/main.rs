//! The smallest complete TDCC plugin.
//!
//! It declares one MCP tool and nothing else. Everything the host needs to
//! project that tool over MCP and HTTP comes from the manifest built by the
//! `plugin!` macro; the plugin itself never opens a socket, serves HTTP, or
//! speaks MCP JSON-RPC.

use anyhow::{Context, Result, bail};
use schemars::JsonSchema;
use serde::Deserialize;
use tdcc_plugin::{
    Plugin, PluginMetadata, PluginRuntime, SimplePlugin, mcp, package_manifest_json, plugin,
    plugin_server_info,
};

/// Typed input for the `greet` tool.
///
/// `.input::<GreetArgs>()` turns this into the JSON Schema the host advertises
/// in `tools/list` and validates incoming arguments against, so the handler
/// never has to parse raw JSON.
#[derive(Debug, Deserialize, JsonSchema)]
struct GreetArgs {
    /// Who to greet. Defaults to "world" when omitted.
    #[serde(default)]
    name: Option<String>,
}

fn build_plugin() -> SimplePlugin {
    plugin! {
        metadata: PluginMetadata::new(
            "hello-plugin",
            "0.1.0",
            plugin_server_info(
                "hello-plugin",
                "0.1.0",
                "Hello plugin",
                "Minimal TDCC plugin example",
                None::<String>,
            ),
        ),

        mcp: [
            mcp::tool("greet")
                .description("Return a greeting for the supplied name.")
                .input::<GreetArgs>()
                .handle(|args: GreetArgs, _context| {
                    Box::pin(async move {
                        let name = args.name.unwrap_or_else(|| "world".to_string());
                        Ok(serde_json::json!({ "greeting": format!("hello, {name}") }))
                    })
                }),
        ],
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let plugin = build_plugin();
    match std::env::args().nth(1).as_deref() {
        // Packaging path: the same declaration the runtime registers also
        // produces `plugin-manifest.json`, so the packaged metadata cannot
        // drift from the running manifest.
        Some("--print-package-manifest") => {
            let manifest = plugin.manifest().context("hello-plugin manifest")?;
            println!("{}", package_manifest_json(&manifest)?);
            Ok(())
        }
        Some(argument) => bail!("unknown option: {argument}"),
        // Runtime path: connect to the control endpoint the host passed in
        // TDCC_PLUGIN_ENDPOINT / TDCC_PLUGIN_TRANSPORT and serve the manifest.
        None => PluginRuntime::run(plugin).await,
    }
}
