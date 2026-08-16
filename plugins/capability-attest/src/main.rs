//! `capability-attest` — measured, signed, reproducible capability records for
//! a TDCC node.
//!
//! Routing should rest on what a node can be shown to do, not on what it says
//! about itself. This plugin benchmarks the node it runs on against a pinned
//! profile, signs the result with the node's own mesh key, and publishes it to
//! peers.
//!
//! Read `record.rs` before trusting any of it: a signature proves who produced
//! a record, not that the numbers in it were earned. That distinction is the
//! whole reason this plugin is careful about everything else.
//!
//! Run it the way the host does (no arguments beyond its configured
//! `[[plugin]].args`): the runtime connects to `TDCC_PLUGIN_ENDPOINT` over
//! `TDCC_PLUGIN_TRANSPORT` and serves the manifest. Run it with
//! `--print-package-manifest` to emit the `plugin-manifest.json` that belongs
//! in a release archive, or `--help` for the option list.

mod activity;
mod attestor;
mod bench;
mod config;
mod identity;
mod manifest;
mod profile;
mod record;
#[cfg(test)]
mod testutil;
mod vram;

use anyhow::{Context, Result};
use tdcc_plugin::{Plugin, PluginRuntime, package_manifest_json};

use crate::attestor::Attestor;
use crate::config::EnvMap;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[tokio::main]
async fn main() -> Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();

    match arguments.first().map(String::as_str) {
        // Packaging path: the same declaration the runtime registers also
        // produces `plugin-manifest.json`, so the packaged metadata cannot
        // drift from the running manifest. Configuration is irrelevant here —
        // the manifest is the same whether or not an endpoint is reachable.
        Some("--print-package-manifest") => {
            let plugin =
                manifest::capability_attest_plugin(Attestor::new(&[], &EnvMap::new(), VERSION));
            let manifest = plugin.manifest().context("capability-attest manifest")?;
            println!("{}", package_manifest_json(&manifest)?);
            Ok(())
        }
        Some("--help" | "-h") => {
            println!("{}", config::help_text());
            Ok(())
        }
        // Runtime path. Configuration failures do not abort start-up: the
        // plugin comes up, `health` reports it unhealthy, and `status` says
        // exactly which setting is missing. Exiting instead would leave the
        // operator with a restart loop and no message in the console.
        _ => {
            let environment: EnvMap = std::env::vars().collect();
            let attestor = Attestor::new(&arguments, &environment, VERSION);
            PluginRuntime::run(manifest::capability_attest_plugin(attestor)).await
        }
    }
}
