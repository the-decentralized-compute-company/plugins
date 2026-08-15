//! `notes-console` — a plugin that contributes a console page, a configuration
//! section, an operator setting, an MCP tool, and two HTTP routes.
//!
//! Run it the way the host does (no arguments): the runtime connects to
//! `TDCC_PLUGIN_ENDPOINT` over `TDCC_PLUGIN_TRANSPORT` and serves the manifest.
//! Run it with `--print-package-manifest` to emit the `plugin-manifest.json`
//! that belongs in the release archive.

mod manifest;
mod notes;

use anyhow::{Context, Result, bail};
use tdcc_plugin::{Plugin, PluginRuntime, package_manifest_json};

use crate::notes::NoteStore;

#[tokio::main]
async fn main() -> Result<()> {
    let plugin = manifest::notes_console_plugin(NoteStore::new());
    match std::env::args().nth(1).as_deref() {
        Some("--print-package-manifest") => {
            let manifest = plugin.manifest().context("notes-console manifest")?;
            println!("{}", package_manifest_json(&manifest)?);
            Ok(())
        }
        Some(argument) => bail!("unknown option: {argument}"),
        None => PluginRuntime::run(plugin).await,
    }
}
