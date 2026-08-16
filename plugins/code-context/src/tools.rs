//! The whole contribution surface of `code-context` in one declaration.
//!
//! Five MCP tools and nothing else: no config schema, no web UI, no HTTP
//! routes, no mesh channels, no events. Declaring the smallest set that does
//! the job is the guidance in the plugin guide, and it matters more than usual
//! here — this plugin reads files on hardware somebody else contributed.
//!
//! There is deliberately no `config_schema`. The console would render the
//! settings and the host would store them, but `[plugin.settings]` is never
//! delivered to the plugin process, so a `root` setting would look
//! authoritative and do nothing. The root arrives through `[[plugin]].args`
//! instead; see [`crate::options`].
//!
//! Every handler hands its work to `spawn_blocking`. Indexing and searching are
//! synchronous file I/O, and the control connection has to keep answering
//! health checks while a scan is running.

use std::sync::Arc;

use tdcc_plugin::{PluginError, PluginMetadata, SimplePlugin, mcp, plugin, plugin_server_info};

use crate::workspace::{ReadArgs, ReindexArgs, SearchArgs, StatusArgs, TreeArgs, Workspace};

pub const PLUGIN_NAME: &str = "code-context";
pub const PLUGIN_VERSION: &str = "0.1.0";

/// Run one workspace operation off the async runtime's worker threads.
///
/// A `JoinError` here means the blocking task panicked, which is a bug in this
/// plugin and not something the caller did — it is reported as an internal
/// error, and the control session survives it.
macro_rules! blocking_tool {
    ($workspace:expr, $args:ident, $operation:ident) => {{
        let workspace = $workspace;
        Box::pin(async move {
            tokio::task::spawn_blocking(move || workspace.$operation($args))
                .await
                .map_err(|error| {
                    PluginError::internal(format!(
                        "code-context {} task failed: {error}",
                        stringify!($operation)
                    ))
                })?
        })
    }};
}

pub fn code_context_plugin(workspace: Arc<Workspace>) -> SimplePlugin {
    let for_search = Arc::clone(&workspace);
    let for_read = Arc::clone(&workspace);
    let for_tree = Arc::clone(&workspace);
    let for_status = Arc::clone(&workspace);
    let for_reindex = workspace;

    plugin! {
        metadata: PluginMetadata::new(
            PLUGIN_NAME,
            PLUGIN_VERSION,
            plugin_server_info(
                PLUGIN_NAME,
                PLUGIN_VERSION,
                "Code context",
                "Search and read a local codebase, confined to one configured root",
                Some(
                    "Use search to locate code and read to pull the exact lines. Every result \
                     carries a path and a line number: cite those instead of paraphrasing. \
                     Paths are relative to the configured root; absolute paths and '..' are \
                     refused.",
                ),
            ),
        ),

        mcp: [
            // Projected as `code-context.search` on the host MCP endpoint and
            // callable at POST /api/plugins/code-context/tools/search.
            mcp::tool("search")
                .description(
                    "Search the indexed codebase and return matching paths with line numbers. \
                     kind=content scans file text; kind=symbol matches declaration names \
                     (functions, structs, classes) recorded in the index. Results are capped, \
                     and long lines are windowed around the match.",
                )
                .input::<SearchArgs>()
                .handle(move |args: SearchArgs, _context| {
                    blocking_tool!(Arc::clone(&for_search), args, search)
                }),

            mcp::tool("read")
                .description(
                    "Read a file, or a line range of one, from the indexed codebase. Paths are \
                     relative to the configured root. Returns the text plus start_line, \
                     end_line, and total_lines so the range can be cited exactly. A single call \
                     returns at most 2000 lines or 192 KiB.",
                )
                .input::<ReadArgs>()
                .handle(move |args: ReadArgs, _context| {
                    blocking_tool!(Arc::clone(&for_read), args, read)
                }),

            mcp::tool("tree")
                .description(
                    "Draw the directory tree of the indexed codebase, optionally scoped to a \
                     subdirectory. Shows exactly the files search and read can reach, so a \
                     directory holding only ignored or excluded files does not appear.",
                )
                .input::<TreeArgs>()
                .handle(move |args: TreeArgs, _context| {
                    blocking_tool!(Arc::clone(&for_tree), args, tree)
                }),

            mcp::tool("status")
                .description(
                    "Report what is indexed: file, byte, and symbol counts, why files were \
                     skipped, when the index was last refreshed, and the active limits. Useful \
                     when search returns less than expected.",
                )
                .input::<StatusArgs>()
                .handle(move |args: StatusArgs, _context| {
                    blocking_tool!(Arc::clone(&for_status), args, status)
                }),

            mcp::tool("reindex")
                .description(
                    "Reindex now instead of waiting for the automatic refresh. Incremental by \
                     default: unchanged files are not reread. Pass force=true to reread and \
                     reparse everything, which is the way to pick up an edit that preserved a \
                     file's size and modification time.",
                )
                .input::<ReindexArgs>()
                .handle(move |args: ReindexArgs, _context| {
                    blocking_tool!(Arc::clone(&for_reindex), args, reindex)
                }),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tdcc_plugin::Plugin;

    #[test]
    fn the_manifest_declares_exactly_the_five_documented_tools() {
        let plugin = code_context_plugin(Workspace::for_manifest_only());
        let manifest = plugin
            .manifest()
            .expect("declarative plugins have a manifest");

        let mut names: Vec<&str> = manifest
            .operations
            .iter()
            .map(|operation| operation.name.as_str())
            .collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec!["read", "reindex", "search", "status", "tree"],
            "tool names are part of the contract other people write down"
        );

        // Nothing else is contributed: no HTTP surface, no mesh channel, no
        // event subscription, no web UI, no config schema to render.
        assert!(manifest.http_bindings.is_empty());
        assert!(manifest.mesh_channels.is_empty());
        assert!(manifest.mesh_event_subscriptions.is_empty());
        assert!(manifest.web_ui.is_none());
        assert!(manifest.config_schema.is_none());
    }

    #[test]
    fn every_tool_advertises_an_input_schema() {
        let plugin = code_context_plugin(Workspace::for_manifest_only());
        let manifest = plugin.manifest().expect("manifest");

        for operation in &manifest.operations {
            assert!(
                !operation.input_schema_json.is_empty(),
                "{} has no input schema, so the host cannot validate its arguments",
                operation.name
            );
        }
    }
}
