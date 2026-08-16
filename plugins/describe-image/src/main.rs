//! `describe-image` — put the mesh's vision models to work on a picture.
//!
//! Run it the way the host does (no arguments beyond its own configuration
//! flags): the runtime connects to `TDCC_PLUGIN_ENDPOINT` over
//! `TDCC_PLUGIN_TRANSPORT` and serves the manifest. Run it with
//! `--print-package-manifest` to emit the `plugin-manifest.json` that would go
//! in a release archive — for this plugin that is `{}`, because it declares
//! neither a config schema nor a web UI, so the file may be left out entirely.

mod chat;
mod config;
mod engine;
mod manifest;
mod models;
mod net;
mod render;
mod source;

use anyhow::{Context, Result};
use tdcc_plugin::{Plugin, PluginRuntime, package_manifest_json};

use crate::config::Config;
use crate::engine::Engine;

#[tokio::main]
async fn main() -> Result<()> {
    // `--print-package-manifest` must work without any configuration, so the
    // packaging path is decided before the config is resolved.
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
    .context("describe-image configuration")?;

    if !packaging {
        // One line of what this process will do, then the choices worth
        // knowing about. Both go to stderr, where the host's log picks them up.
        eprintln!("describe-image: {}", config.startup_summary());
        for root in &config.roots {
            eprintln!(
                "describe-image: reading images under {}",
                config::display_path(root)
            );
        }
        for advisory in config.advisories() {
            eprintln!("describe-image: {advisory}");
        }
    }

    let engine = Engine::new(config).map_err(|error| anyhow::anyhow!("{error}"))?;
    let plugin = manifest::describe_image_plugin(engine);

    match std::env::args().nth(1).as_deref() {
        Some("--print-package-manifest") => {
            let manifest = plugin.manifest().context("describe-image manifest")?;
            println!("{}", package_manifest_json(&manifest)?);
            Ok(())
        }
        // Anything else is a configuration flag, already validated above.
        _ => PluginRuntime::run(plugin).await,
    }
}
