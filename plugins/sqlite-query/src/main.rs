//! `sqlite-query` — read-only SQL access to SQLite databases an operator has
//! explicitly listed.
//!
//! The point of the plugin is that a model can answer questions about real data
//! without anyone pasting a schema into a prompt, and without the model being
//! handed a general-purpose file reader. Two structural decisions do most of
//! that work:
//!
//! * **No tool takes a path.** Databases are selected by an alias that only
//!   exists because it appeared in `[[plugin]].args` at launch, so caller input
//!   never becomes a filesystem path. See `db.rs`.
//! * **Read-only is enforced by SQLite, not by reading the SQL.** The file
//!   handle is opened `SQLITE_OPEN_READ_ONLY` and an authorizer refuses
//!   `ATTACH` and temp-object creation at compile time. See `policy.rs`.
//!
//! Run it the way the host does (no arguments beyond configuration): the
//! runtime connects to `TDCC_PLUGIN_ENDPOINT` over `TDCC_PLUGIN_TRANSPORT` and
//! serves the manifest. Run it with `--print-package-manifest` to emit the
//! `plugin-manifest.json` that would belong in a release archive.

mod db;
mod manifest;
mod policy;
mod query;
mod schema;
mod settings;
#[cfg(test)]
mod testutil;
mod tools;
mod value;

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use tdcc_plugin::{Plugin, PluginRuntime, package_manifest_json};

use crate::db::Catalog;
use crate::settings::{Limits, Settings, parse_settings};

#[tokio::main]
async fn main() -> Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();

    // Packaging path. This plugin declares neither a config schema nor a web
    // UI, so the packaged manifest is `{}` and the file may be left out of an
    // archive entirely; the option stays for parity with the other plugins.
    if arguments.first().map(String::as_str) == Some("--print-package-manifest") {
        if arguments.len() > 1 {
            bail!("--print-package-manifest takes no other arguments");
        }
        let plugin = manifest::sqlite_query_plugin(Arc::new(Catalog::new(Settings {
            databases: Vec::new(),
            limits: Limits::default(),
        })));
        let manifest = plugin.manifest().context("sqlite-query manifest")?;
        println!("{}", package_manifest_json(&manifest)?);
        return Ok(());
    }

    // A malformed argument is fatal on purpose. Starting with a
    // misunderstood configuration is how a plugin ends up exposing a database
    // the operator did not mean to expose.
    let settings = parse_settings(arguments, |key| std::env::var(key).ok())
        .map_err(|error| anyhow::anyhow!("invalid sqlite-query configuration: {error}"))?;

    // An empty but well-formed configuration is not fatal: the plugin stays
    // loaded and every tool reports exactly what is missing, which is more
    // useful to an operator than a restart loop.
    if settings.databases.is_empty() {
        eprintln!(
            "sqlite-query: no databases configured. Add --db <alias>=<path> to this \
             plugin's [[plugin]].args (or set TDCC_SQLITE_QUERY_DB). Until then every \
             tool will report that no database is available."
        );
    } else {
        for database in &settings.databases {
            eprintln!(
                "sqlite-query: {} => {} ({})",
                database.alias,
                database.path.display(),
                if database.writable {
                    "READ-WRITE, enabled by --db-rw"
                } else {
                    "read-only"
                }
            );
        }
    }

    let catalog = Arc::new(Catalog::new(settings));
    // Runtime path: connect to the control endpoint the host passed in
    // TDCC_PLUGIN_ENDPOINT / TDCC_PLUGIN_TRANSPORT and serve the manifest.
    PluginRuntime::run(manifest::sqlite_query_plugin(catalog)).await
}
