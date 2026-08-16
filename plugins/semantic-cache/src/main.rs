//! `semantic-cache` — reuse a completion when a *reworded but equivalent*
//! prompt arrives, so the same workload costs measurably fewer tokens.
//!
//! Run it the way the host does (no arguments beyond configuration flags): the
//! runtime connects to `TDCC_PLUGIN_ENDPOINT` over `TDCC_PLUGIN_TRANSPORT` and
//! serves the manifest. Run it with `--print-package-manifest` to emit the
//! `plugin-manifest.json` that would belong in a release archive — this plugin
//! declares neither a config schema nor a web UI, so that output is `{}` and
//! the file can be left out of the archive entirely.
//!
//! Two things are worth knowing before reading further, because they shape
//! everything else:
//!
//! * **The cache never guesses.** Semantic matching applies to exactly one
//!   thing — the trailing user message. The model, the sampling parameters,
//!   the tool set and the entire message prefix (system prompt included) must
//!   match exactly, or the two requests live in different buckets and can
//!   never see each other's answers. See [`keying`].
//! * **It needs an embeddings endpoint, and the node does not have one.** The
//!   TDCC OpenAI frontend on `127.0.0.1:9337` serves `/v1/models`,
//!   `/v1/chat/completions`, `/v1/completions` and `/v1/responses`; embeddings
//!   are documented there as out of scope. Point `--embeddings-url` at a local
//!   embeddings server. See README.md and [`embeddings`].

mod config;
mod embeddings;
mod keying;
mod manifest;
mod policy;
mod store;

/// End-to-end tests against a stub embeddings server on loopback.
#[cfg(test)]
mod end_to_end;

use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use tdcc_plugin::{Plugin, PluginRuntime, package_manifest_json};

use crate::config::Config;
use crate::manifest::{AppState, semantic_cache_plugin};

#[tokio::main]
async fn main() -> Result<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();

    // Packaging path. Deliberately ahead of configuration parsing: emitting a
    // manifest must not require a reachable endpoint or a valid threshold.
    if argv
        .iter()
        .any(|argument| argument == "--print-package-manifest")
    {
        if argv.len() != 1 {
            bail!("--print-package-manifest takes no other arguments");
        }
        let state = Arc::new(AppState::new(Config::default()).map_err(|error| anyhow!(error))?);
        let plugin = semantic_cache_plugin(state);
        let manifest = plugin.manifest().context("semantic-cache manifest")?;
        println!("{}", package_manifest_json(&manifest)?);
        return Ok(());
    }

    let config = Config::resolve(&argv, &config::collect_environment())?;

    // Startup logging goes to stderr: stdout belongs to
    // `--print-package-manifest`, and the control connection is a socket, not
    // stdio. The summary contains no secrets by construction — see
    // `Config::startup_summary`.
    eprintln!("[semantic-cache] {}", config.startup_summary());
    for advisory in config.advisories() {
        eprintln!("[semantic-cache] warning: {advisory}");
    }

    let state = Arc::new(AppState::new(config).map_err(|error| anyhow!(error))?);

    // The endpoint is not probed here. Endpoint health and plugin health are
    // separate concerns in this architecture: an embeddings server that is
    // still starting must not stop the plugin from loading, and `status`
    // exists precisely so an operator can ask about it on demand.
    PluginRuntime::run(semantic_cache_plugin(state)).await
}
