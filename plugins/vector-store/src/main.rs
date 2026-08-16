//! `vector-store` — the retrieval half of RAG, as local infrastructure.
//!
//! Run it the way the host does (no arguments beyond its own configuration
//! flags): the runtime connects to `TDCC_PLUGIN_ENDPOINT` over
//! `TDCC_PLUGIN_TRANSPORT` and serves the manifest. Run it with
//! `--print-package-manifest` to emit the `plugin-manifest.json` that would go
//! in a release archive — for this plugin that is `{}`, because it declares
//! neither a config schema nor a web UI, so the file may be left out entirely.
//!
//! Four things shape everything else, and they are worth knowing before
//! reading further:
//!
//! * **Chunking is the product.** A bad split ruins retrieval no matter how
//!   good the search is, so the splitter follows the document's own structure
//!   — headings, paragraphs, whole code fences — keeps an overlap, and records
//!   the source label and line span on every passage so a citation is
//!   possible. See [`chunk`].
//! * **Search is an exact brute-force cosine scan**, and stops being the right
//!   design past a few tens of thousands of chunks per collection. That
//!   ceiling is enforced, not merely mentioned. See [`store`].
//! * **Embedding spaces are never mixed.** A collection pins the embedding
//!   model that built it; a query or an ingest with a different one is
//!   refused, because a cosine between two embedding spaces is a
//!   plausible-looking number that means nothing.
//! * **It needs an embeddings endpoint, and the node does not have one.** The
//!   TDCC OpenAI frontend on `127.0.0.1:9337` routes `/v1/models`,
//!   `/v1/chat/completions`, `/v1/completions` and `/v1/responses`; embeddings
//!   are documented there as out of scope. Point `--embeddings-url` at a local
//!   embeddings server. See README.md and [`embeddings`].

mod chunk;
mod config;
mod embeddings;
mod manifest;
mod names;
mod similarity;
mod store;

#[cfg(test)]
mod testsupport;

/// End-to-end tests against a stub embeddings server on loopback.
#[cfg(test)]
mod end_to_end;

use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use tdcc_plugin::{Plugin, PluginRuntime, package_manifest_json};

use crate::config::{Config, prepare_data_dir};
use crate::manifest::{AppState, vector_store_plugin};
use crate::store::VectorStore;

#[tokio::main]
async fn main() -> Result<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();

    // Packaging path. Deliberately ahead of configuration parsing and ahead of
    // touching the filesystem: emitting a manifest must not require a data
    // directory, a reachable endpoint, or a valid chunk size.
    if argv
        .iter()
        .any(|argument| argument == "--print-package-manifest")
    {
        if argv.len() != 1 {
            bail!("--print-package-manifest takes no other arguments");
        }
        println!("{}", package_manifest_json(&manifest_only()?)?);
        return Ok(());
    }

    let config = Config::resolve(&argv, &config::collect_environment())?;

    // Startup logging goes to stderr: stdout belongs to
    // `--print-package-manifest`, and the control connection is a socket, not
    // stdio. The summary contains no secrets by construction — see
    // `Config::startup_summary`.
    eprintln!("[vector-store] {}", config.startup_summary());
    for advisory in config.advisories() {
        eprintln!("[vector-store] warning: {advisory}");
    }

    // The data directory is created and canonicalized once, here, and every
    // later containment check compares against that canonical path.
    let root = prepare_data_dir(&config.data_dir).map_err(|error| anyhow!("{error}"))?;

    // Every collection is replayed now rather than lazily. A corrupt log is a
    // startup failure an operator sees in the host log, not a query failure
    // three days later.
    let store = Arc::new(
        VectorStore::open(&root, config.store_limits())
            .map_err(|error| anyhow!("{error}"))
            .context("opening the vector store")?,
    );
    let (collections, chunks) = store.counts();
    eprintln!(
        "[vector-store] loaded {collections} collection(s), {chunks} chunk(s) from {}",
        names::display_path(&root)
    );

    let state = Arc::new(AppState::new(config, store).map_err(|error| anyhow!(error))?);

    // The embeddings endpoint is not probed here. Endpoint health and plugin
    // health are separate concerns in this architecture: a server that is
    // still loading a model must not stop the plugin from starting, and
    // `status` exists precisely so an operator can ask about it on demand.
    PluginRuntime::run(vector_store_plugin(state)).await
}

/// The manifest, built without touching the filesystem or the network.
///
/// `--print-package-manifest` runs during packaging, on a machine that may
/// have no data directory and certainly has no embeddings server, so it uses a
/// store rooted at a scratch path that is created and removed immediately.
fn manifest_only() -> Result<tdcc_plugin::proto::PluginManifest> {
    let config = Config::default();
    let root = std::env::temp_dir().join(format!(
        "vector-store-manifest-{}-{}",
        std::process::id(),
        store::now_unix_ms()
    ));
    let store = Arc::new(
        VectorStore::open(&root, config.store_limits())
            .map_err(|error| anyhow!("{error}"))
            .context("preparing a scratch store for the packaged manifest")?,
    );
    let state = Arc::new(AppState::new(config, store).map_err(|error| anyhow!(error))?);
    let manifest = vector_store_plugin(state)
        .manifest()
        .context("vector-store manifest")?;
    let _ = std::fs::remove_dir_all(&root);
    Ok(manifest)
}
