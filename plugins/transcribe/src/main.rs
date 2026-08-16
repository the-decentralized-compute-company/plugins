//! `transcribe` — turn an audio file into text and timestamped segments.
//!
//! Run it the way the host does (no arguments beyond its own configuration
//! flags): the runtime connects to `TDCC_PLUGIN_ENDPOINT` over
//! `TDCC_PLUGIN_TRANSPORT` and serves the manifest. Run it with
//! `--print-package-manifest` to emit the `plugin-manifest.json` that would go
//! in a release archive — for this plugin that is `{}`, because it declares
//! neither a config schema nor a web UI, so the file may be left out entirely.

mod audio;
mod backend;
mod config;
mod engine;
mod listing;
mod manifest;
mod plan;
mod roots;
mod segments;
#[cfg(test)]
mod testutil;

use anyhow::{Context, Result};
use tdcc_plugin::{Plugin, PluginRuntime, package_manifest_json};

use crate::config::{BackendSetup, Config, USAGE};
use crate::engine::Engine;

#[tokio::main]
async fn main() -> Result<()> {
    // `--help` and `--print-package-manifest` must work without any
    // configuration, so they are handled before the config is resolved.
    let first = std::env::args().nth(1);
    if matches!(first.as_deref(), Some("--help" | "-h")) {
        println!("{USAGE}");
        return Ok(());
    }
    let packaging = matches!(first.as_deref(), Some("--print-package-manifest"));

    let config = if packaging {
        Config::parse(&[], &Default::default())
    } else {
        Config::from_process()
    }
    .map_err(|error| anyhow::anyhow!("{error}"))
    .context("transcribe configuration")?;

    // Neither a missing backend nor a missing root is fatal: `status` stays
    // useful without either, and an operator who has just installed the plugin
    // should see it running with an explanation rather than a process that
    // refuses to start. Both are reported once, on stderr, where the host's log
    // picks them up, and again from every tool that needs them.
    if !packaging {
        if let BackendSetup::Unconfigured(message) = &config.backend {
            eprintln!("transcribe: {message}");
        }
        if config.roots.is_empty() {
            eprintln!("transcribe: {}", Config::no_roots_message());
        }
    }

    let engine = Engine::new(config).map_err(|error| anyhow::anyhow!("{error}"))?;
    let plugin = manifest::transcribe_plugin(engine);

    if packaging {
        let manifest = plugin.manifest().context("transcribe manifest")?;
        println!("{}", package_manifest_json(&manifest)?);
        return Ok(());
    }
    PluginRuntime::run(plugin).await
}
