//! `web-search` — give a model the ability to search the web and read a result.
//!
//! Run it the way the host does (no arguments beyond its own configuration
//! flags): the runtime connects to `TDCC_PLUGIN_ENDPOINT` over
//! `TDCC_PLUGIN_TRANSPORT` and serves the manifest. Run it with
//! `--print-package-manifest` to emit the `plugin-manifest.json` that would go
//! in a release archive — for this plugin that is `{}`, because it declares
//! neither a config schema nor a web UI, so the file may be left out entirely.

mod config;
mod engine;
mod manifest;
mod net;
mod readable;
mod robots;
mod search;

use anyhow::{Context, Result};
use tdcc_plugin::{Plugin, PluginRuntime, package_manifest_json};

use crate::config::{Config, SearchSetup};
use crate::engine::Engine;

#[tokio::main]
async fn main() -> Result<()> {
    // `--print-package-manifest` must work without any configuration, so the
    // packaging path is handled before the config is resolved.
    let packaging = matches!(
        std::env::args().nth(1).as_deref(),
        Some("--print-package-manifest")
    );

    let config = if packaging {
        Config::parse(&[], &Default::default())
    } else {
        Config::from_process()
    }
    .map_err(|error| anyhow::anyhow!("{error}"))
    .context("web-search configuration")?;

    // A missing search backend is not fatal: `fetch` needs none. Say so once,
    // on stderr, where the host's log will pick it up, then start anyway.
    if let SearchSetup::Unconfigured(message) = &config.search
        && !packaging
    {
        eprintln!("web-search: {message}");
    }

    let engine = Engine::new(config).map_err(|error| anyhow::anyhow!("{error}"))?;
    let plugin = manifest::web_search_plugin(engine);

    match std::env::args().nth(1).as_deref() {
        Some("--print-package-manifest") => {
            let manifest = plugin.manifest().context("web-search manifest")?;
            println!("{}", package_manifest_json(&manifest)?);
            Ok(())
        }
        // Anything else is a configuration flag, already validated above.
        _ => PluginRuntime::run(plugin).await,
    }
}
