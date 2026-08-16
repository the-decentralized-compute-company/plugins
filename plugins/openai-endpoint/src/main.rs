//! `openai-endpoint` — attach hardware TDCC does not manage itself.
//!
//! A machine already running vLLM, TGI, Ollama, LM Studio, or `llama-server`
//! joins the mesh without re-tooling: this plugin registers that server as an
//! OpenAI-compatible inference endpoint, and the host routes to it directly.
//!
//! Run it the way the host does (no arguments): the runtime connects to
//! `TDCC_PLUGIN_ENDPOINT` over `TDCC_PLUGIN_TRANSPORT` and serves the manifest.
//! Run it with `--print-package-manifest` to emit the `plugin-manifest.json`
//! that would go in a release archive, or `--help` for the configuration flags.

mod config;
mod manifest;
mod openai;
mod upstream;

use anyhow::{Context, Result};
use tdcc_plugin::{Plugin, PluginRuntime, package_manifest_json};

use crate::config::EndpointConfig;
use crate::manifest::PLUGIN_NAME;
use crate::upstream::ProbeCache;

/// `[[plugin]].url`, delivered by the launch contract. The two remaining
/// launch variables — the control endpoint and its transport —
/// `PluginRuntime::run` consumes on its own; a plugin never reads those.
const URL_ENV: &str = "TDCC_PLUGIN_URL";
/// Pre-rename mirror the host also exports, read only as a fallback so a node
/// still running an older host keeps working.
const LEGACY_URL_ENV: &str = "MESH_LLM_PLUGIN_URL";

const HELP: &str = "\
openai-endpoint — attach an already-running OpenAI-compatible server to a TDCC node.

The host launches this binary; it is not meant to be run by hand. Configure it
through the plugin's table in ~/.tdcc/config.toml:

  [[plugin]]
  name = \"openai-endpoint\"
  url  = \"http://127.0.0.1:8000/v1\"
  args = [\"--endpoint-id\", \"vllm\"]

Arguments (from [[plugin]].args):
  --url <base>          API base URL. Overrides [[plugin]].url. Must be http://
                        and must be the base, not an operation path.
  --endpoint-id <id>    Endpoint id within this plugin. Default: upstream.
  --api-key-env <NAME>  NAME of an environment variable holding a bearer token
                        for this plugin's own probes. Never the key itself.
  --timeout-secs <n>    Per-probe timeout, 1-120. Default: 10.
  --model <name>        Model used by verify_stream and compat when the caller
                        does not name one.

Options:
  --print-package-manifest   Print the packaged plugin-manifest.json and exit.
  --help                     Show this text.
";

#[tokio::main]
async fn main() -> Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();

    match arguments.first().map(String::as_str) {
        Some("--help" | "-h") => {
            print!("{HELP}");
            return Ok(());
        }
        // Packaging path: the same declaration the runtime registers also
        // produces `plugin-manifest.json`, so packaged metadata cannot drift
        // from the running manifest. It is built from a placeholder
        // configuration because the packaged file carries only `config_schema`
        // and `web_ui` — neither of which depends on the endpoint address — so
        // packaging works on a build machine with no backend to point at.
        Some("--print-package-manifest") => {
            let plugin = manifest::openai_endpoint_plugin(
                EndpointConfig::packaging_placeholder(),
                ProbeCache::default(),
            )?;
            let manifest = plugin.manifest().context("openai-endpoint manifest")?;
            println!("{}", package_manifest_json(&manifest)?);
            return Ok(());
        }
        _ => {}
    }

    // Runtime path. Configuration is resolved before the control connection is
    // opened: a plugin that cannot be routed to should fail loudly at startup
    // rather than come up and advertise an endpoint that never receives
    // traffic. Set `optional = true` in [plugin.startup] if a misconfigured
    // endpoint should leave the rest of the node running.
    let launch_url = std::env::var(URL_ENV)
        .or_else(|_| std::env::var(LEGACY_URL_ENV))
        .ok()
        .map(|url| url.trim().to_string())
        .filter(|url| !url.is_empty());

    let config = EndpointConfig::from_launch(arguments, launch_url)
        .with_context(|| format!("{PLUGIN_NAME}: invalid endpoint configuration"))?;

    eprintln!(
        "[{PLUGIN_NAME}] attaching {} as endpoint '{}'",
        config.endpoint_address(),
        config.endpoint_id()
    );

    let plugin = manifest::openai_endpoint_plugin(config, ProbeCache::default())?;
    PluginRuntime::run(plugin).await
}
