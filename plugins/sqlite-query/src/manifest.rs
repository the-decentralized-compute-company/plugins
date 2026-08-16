//! The whole contribution surface of `sqlite-query` in one declaration.
//!
//! Five MCP tools and nothing else: no HTTP routes, no mesh channels, no mesh
//! events, no config schema and no web UI. Delivery is allowlist-based, so
//! declaring nothing means the host sends nothing — which is the right posture
//! for a plugin whose job is to read other people's data.
//!
//! There is deliberately no `config_schema`. `[plugin.settings]` values are
//! host-owned and never reach the plugin process, so a settings key could not
//! actually restrict which files this plugin opens. The database list has to be
//! something the process itself receives, which means `[[plugin]].args`.
//!
//! Macro field order is fixed: `metadata`, `startup_policy`, `provides`,
//! `config`, `web_ui`, `mesh`, `events`, `mcp`, `http`, `inference`, then
//! lifecycle hooks. Omitting a field is fine, reordering is not.

use std::sync::Arc;

use tdcc_plugin::{PluginMetadata, SimplePlugin, mcp, plugin, plugin_server_info};

use crate::db::Catalog;
use crate::tools;

pub fn sqlite_query_plugin(catalog: Arc<Catalog>) -> SimplePlugin {
    // One clone per handler: the closures are `Fn`, so each needs its own
    // handle to the catalog rather than borrowing a shared one.
    let for_list_databases = Arc::clone(&catalog);
    let for_list_tables = Arc::clone(&catalog);
    let for_describe = Arc::clone(&catalog);
    let for_query = Arc::clone(&catalog);
    let for_execute = catalog;

    plugin! {
        metadata: PluginMetadata::new(
            "sqlite-query",
            "0.1.0",
            plugin_server_info(
                "sqlite-query",
                "0.1.0",
                "SQLite query",
                "Read-only SQL access to operator-configured SQLite databases",
                None::<String>,
            ),
        ),

        mcp: [
            // Projected as `sqlite-query.list_databases` on the host MCP
            // endpoint, and callable at
            // POST /api/plugins/sqlite-query/tools/list_databases.
            mcp::tool("list_databases")
                .description(
                    "List the SQLite databases this plugin is allowed to open, with their \
                     access mode and whether the file is currently readable. Start here: \
                     every other tool selects a database by one of these names.",
                )
                .input::<tools::NoArgs>()
                .handle(move |_args: tools::NoArgs, _context| {
                    let catalog = Arc::clone(&for_list_databases);
                    Box::pin(async move { tools::list_databases(catalog).await })
                }),

            mcp::tool("list_tables")
                .description(
                    "List the tables and views in a database, each with its column names. \
                     Use this to find out what exists before writing SQL.",
                )
                .input::<tools::ListTablesArgs>()
                .handle(move |args: tools::ListTablesArgs, _context| {
                    let catalog = Arc::clone(&for_list_tables);
                    Box::pin(async move { tools::list_tables(catalog, args).await })
                }),

            mcp::tool("describe_table")
                .description(
                    "Describe one table or view in full: column types, NOT NULL and \
                     defaults, the primary key, foreign keys with their referenced \
                     columns, the indexes, and the original CREATE statement. Read this \
                     before writing a join or a filter.",
                )
                .input::<tools::DescribeTableArgs>()
                .handle(move |args: tools::DescribeTableArgs, _context| {
                    let catalog = Arc::clone(&for_describe);
                    Box::pin(async move { tools::describe_table(catalog, args).await })
                }),

            mcp::tool("query")
                .description(
                    "Run one read-only SQL statement and return the rows. The connection \
                     is opened read-only, so nothing that writes can succeed. Results are \
                     capped by row count, response size and a statement timeout; when a \
                     cap is hit the response says so explicitly.",
                )
                .input::<tools::QueryArgs>()
                .handle(move |args: tools::QueryArgs, _context| {
                    let catalog = Arc::clone(&for_query);
                    Box::pin(async move { tools::query(catalog, args).await })
                }),

            // Present in the manifest but inert unless an operator registered a
            // database with `--db-rw`. Keeping it a separate tool means the
            // read-only default is visible in the tool list rather than hidden
            // in an option on `query`.
            mcp::tool("execute")
                .description(
                    "Run one statement that may modify data. This works only on a \
                     database the operator explicitly registered as writable with \
                     --db-rw; every other database refuses it. Returns the number of \
                     rows changed.",
                )
                .input::<tools::ExecuteArgs>()
                .handle(move |args: tools::ExecuteArgs, _context| {
                    let catalog = Arc::clone(&for_execute);
                    Box::pin(async move { tools::execute(catalog, args).await })
                }),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{Limits, Settings};
    use tdcc_plugin::Plugin;

    fn empty_plugin() -> SimplePlugin {
        sqlite_query_plugin(Arc::new(Catalog::new(Settings {
            databases: Vec::new(),
            limits: Limits::default(),
        })))
    }

    #[test]
    fn the_manifest_declares_exactly_the_five_tools_and_no_other_surface() {
        let manifest = empty_plugin()
            .manifest()
            .expect("declarative plugins have a manifest");

        let mut operations: Vec<_> = manifest
            .operations
            .iter()
            .map(|operation| operation.name.as_str())
            .collect();
        operations.sort_unstable();
        assert_eq!(
            operations,
            [
                "describe_table",
                "execute",
                "list_databases",
                "list_tables",
                "query"
            ]
        );

        assert!(
            manifest.http_bindings.is_empty(),
            "no HTTP routes are declared"
        );
        assert!(
            manifest.mesh_channels.is_empty(),
            "no mesh channels are declared"
        );
        assert!(
            manifest.mesh_event_subscriptions.is_empty(),
            "no mesh events are declared"
        );
        assert!(
            manifest.endpoints.is_empty(),
            "no external endpoints are declared"
        );
        assert!(manifest.web_ui.is_none(), "no web UI is declared");
        assert!(
            manifest.config_schema.is_none(),
            "settings cannot reach this process, so none are declared"
        );
    }

    #[test]
    fn every_tool_carries_a_description_a_model_can_act_on() {
        let manifest = empty_plugin().manifest().expect("manifest");
        for operation in &manifest.operations {
            assert!(
                operation.description.len() > 40,
                "{} needs a real description",
                operation.name
            );
        }
    }
}
