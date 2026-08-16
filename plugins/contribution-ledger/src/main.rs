//! `contribution-ledger` — an honest local record of what this node gave to
//! the mesh.
//!
//! It is a **record**, not a currency. There is no balance, no credit, no
//! transfer, and nothing here settles against anything. The file it writes is
//! this node's own statement about its own work, kept on this node's own disk.
//!
//! Run it the way the host does (no arguments): the runtime connects to
//! `TDCC_PLUGIN_ENDPOINT` over `TDCC_PLUGIN_TRANSPORT` and serves the manifest.
//! Run it with `--print-package-manifest` to emit the `plugin-manifest.json`
//! that belongs in the release archive.

mod clock;
mod config;
mod journal;
mod ledger;
mod manifest;
mod sampler;
mod source;

use std::sync::Arc;

use anyhow::{Context, Result};
use tdcc_plugin::{Plugin, PluginRuntime, package_manifest_json};

use crate::ledger::Ledger;

/// Must equal the `plugin.toml` name, the crate name, the archive directory
/// name, the executable name, and the configured `[[plugin]].name`. The host
/// rejects the initialize handshake if the manifest id disagrees with the
/// configured name.
pub const PLUGIN_NAME: &str = "contribution-ledger";
pub const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");

#[tokio::main]
async fn main() -> Result<()> {
    // Packaging path first, so it works without a host and, more importantly,
    // without touching the operator's real history. The manifest comes from the
    // same declaration the runtime serves, so the two cannot drift — but that
    // declaration owns a ledger, so this builds a throwaway one in a private
    // temp directory and removes it again.
    if std::env::args().nth(1).as_deref() == Some("--print-package-manifest") {
        let scratch =
            std::env::temp_dir().join(format!("{PLUGIN_NAME}-manifest-{}", std::process::id()));
        let rendered = {
            let placeholder = Ledger::open(config::LedgerConfig {
                state_dir: scratch.clone(),
                source: config::StatusSource::OptedOut,
                poll_secs: config::DEFAULT_POLL_SECS,
                record_peers: false,
                compact_after_days: config::DEFAULT_COMPACT_AFTER_DAYS,
                retain_days: config::DEFAULT_RETAIN_DAYS,
            })
            .context("building a throwaway ledger to render the package manifest")?;
            manifest::contribution_ledger_plugin(Arc::new(placeholder))
                .manifest()
                .context("contribution-ledger manifest")?
        };
        let _ = std::fs::remove_dir_all(&scratch);
        println!("{}", package_manifest_json(&rendered)?);
        return Ok(());
    }

    // `args().skip(1)` is `[[plugin]].args` verbatim. An unknown option is
    // fatal: a typo that silently disabled the sampler would produce a ledger
    // full of honest-looking zeroes.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let settings = config::parse(&args, &config::Env::from_process())
        .context("reading contribution-ledger configuration from [[plugin]].args")?;

    // A ledger that cannot write is not a ledger, so a bad state directory
    // fails the process at startup rather than silently dropping history.
    let ledger = Ledger::open(settings).context("opening the contribution ledger")?;

    // Runtime path: connect to the control endpoint the host passed in and
    // serve the manifest. The sampler starts from `on_initialized`.
    PluginRuntime::run(manifest::contribution_ledger_plugin(Arc::new(ledger))).await
}
