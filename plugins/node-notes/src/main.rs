//! `node-notes` — shared operational memory for a mesh.
//!
//! Operators and models leave short notes against a node or the whole mesh:
//! what broke, what was tried, why a model was pinned. Notes expire, are capped
//! in every direction, and — when the operator opted in with `--share` — are
//! published to directly connected peers over the plugin's own mesh channel.
//!
//! Run it the way the host does (no arguments beyond its own configuration
//! flags): the runtime connects to `TDCC_PLUGIN_ENDPOINT` over
//! `TDCC_PLUGIN_TRANSPORT` and serves the manifest. Run it with
//! `--print-package-manifest` to emit the `plugin-manifest.json` that belongs in
//! a release archive — this plugin declares a web UI, so that file is required.

mod config;
mod manifest;
mod note;
mod roll_off;
mod share;
mod store;

use std::sync::Arc;

use anyhow::{Context, Result};
use tdcc_plugin::{Plugin, PluginRuntime, package_manifest_json};

use crate::config::Config;
use crate::store::NoteStore;

#[tokio::main]
async fn main() -> Result<()> {
    // `--print-package-manifest` must work without any configuration and
    // without touching the operator's state directory, so the packaging path is
    // decided before the config is resolved.
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
    .context("node-notes configuration")?;

    // Say once, on stderr where the host's log picks it up, that this node is
    // keeping everything to itself. An operator who expected a shared notebook
    // should not have to call `status` to find out otherwise.
    if !packaging && !config.sharing.is_enabled() {
        eprintln!(
            "node-notes: sharing is off, so notes stay on this node and nothing inbound is \
             kept. Pass --share in [[plugin]].args to publish notes to directly connected peers."
        );
    }

    let store = Arc::new(NoteStore::open(config));
    let plugin = manifest::node_notes_plugin(store);

    match std::env::args().nth(1).as_deref() {
        Some("--print-package-manifest") => {
            let manifest = plugin.manifest().context("node-notes manifest")?;
            println!("{}", package_manifest_json(&manifest)?);
            Ok(())
        }
        // Anything else is a configuration flag, already validated above.
        _ => PluginRuntime::run(plugin).await,
    }
}
