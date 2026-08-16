//! The whole contribution surface of `git-tools` in one declaration.
//!
//! Seven MCP tools and nothing else: no config schema, no web UI, no HTTP
//! routes, no mesh channels, no events. Declaring the smallest set that does
//! the job is the guidance in the plugin guide, and it matters more than usual
//! here — this plugin reads the history of repositories on hardware somebody
//! else contributed.
//!
//! There is deliberately no `config_schema`. The console would render the
//! settings and the host would store them, but `[plugin.settings]` is never
//! delivered to the plugin process, so a repository list rendered there would
//! look authoritative and change nothing. The repositories arrive through
//! `[[plugin]].args` instead; see [`crate::settings`].
//!
//! **Every tool is read-only.** Nothing declared below commits, checks out,
//! fetches, pushes, tags, or writes configuration, and the one libgit2 call
//! that could have written to disk on its own — the status index refresh — is
//! explicitly disabled. A model with write access to a repository is a bad idea
//! and this is not the plugin to explore it.
//!
//! Every handler hands its work to `spawn_blocking`. libgit2 is synchronous,
//! and the control connection has to keep answering health checks while a walk
//! is running.

use std::sync::Arc;

use tdcc_plugin::{PluginError, PluginMetadata, SimplePlugin, mcp, plugin, plugin_server_info};

use crate::blame::{BlameArgs, blame};
use crate::changes::{DiffArgs, diff};
use crate::history::{LogArgs, ShowArgs, log, show};
use crate::inventory::{RefsArgs, RepoStatusArgs, StatusArgs, refs, repo_status, status};
use crate::repos::Registry;
use crate::settings::{PLUGIN_NAME, PLUGIN_VERSION};

/// Run one repository operation off the async runtime's worker threads.
///
/// A `JoinError` here means the blocking task panicked, which is a bug in this
/// plugin and not something the caller did — it is reported as an internal
/// error, and the control session survives it.
macro_rules! blocking_tool {
    ($registry:expr, $args:ident, $operation:path) => {{
        let registry = $registry;
        Box::pin(async move {
            tokio::task::spawn_blocking(move || $operation(&registry, $args))
                .await
                .map_err(|error| {
                    PluginError::internal(format!(
                        "git-tools {} task failed: {error}",
                        stringify!($operation)
                    ))
                })?
        })
    }};
}

pub fn git_tools_plugin(registry: Arc<Registry>) -> SimplePlugin {
    let for_status = Arc::clone(&registry);
    let for_log = Arc::clone(&registry);
    let for_show = Arc::clone(&registry);
    let for_diff = Arc::clone(&registry);
    let for_blame = Arc::clone(&registry);
    let for_refs = Arc::clone(&registry);
    let for_repo_status = registry;

    plugin! {
        metadata: PluginMetadata::new(
            PLUGIN_NAME,
            PLUGIN_VERSION,
            plugin_server_info(
                PLUGIN_NAME,
                PLUGIN_VERSION,
                "Git history",
                "Read the history of repositories an operator listed: log, show, diff, blame, \
                 refs, and working-tree status",
                Some(
                    "These tools answer questions about a repository's history that searching its \
                     files cannot: when a line changed, who changed it, what landed between two \
                     releases, and what is uncommitted right now. Every tool is read-only and \
                     confined to the repositories the machine's operator listed; call status \
                     first to see which those are. Quote commit ids and paths from the responses \
                     verbatim rather than reconstructing them.",
                ),
            ),
        ),

        mcp: [
            // Projected as `git-tools.status` on the host MCP endpoint and
            // callable at POST /api/plugins/git-tools/tools/status.
            mcp::tool("status")
                .description(
                    "Report which repositories this plugin can read, whether each one currently \
                     opens, where its HEAD is, and the limits and disclosure policy the machine's \
                     operator set. Reads no history and contacts nothing, so it keeps answering \
                     when everything else is failing. Call it first to learn the repository \
                     aliases the other tools take.",
                )
                .input::<StatusArgs>()
                .handle(move |args: StatusArgs, _context| {
                    blocking_tool!(Arc::clone(&for_status), args, status)
                }),

            mcp::tool("log")
                .description(
                    "List commits, newest first, with author, date, message, and parents. Filter \
                     by path, author, message text, and date, and use rev with exclude_rev to get \
                     exactly what landed between two releases. This is the tool for 'when did \
                     this change' and 'what went into this version'.",
                )
                .input::<LogArgs>()
                .handle(move |args: LogArgs, _context| {
                    blocking_tool!(Arc::clone(&for_log), args, log)
                }),

            mcp::tool("show")
                .description(
                    "Show one commit in full: its message, author, committer, parents, and the \
                     files it changed with per-file line counts. Pass patch=true for the diff \
                     text itself. A merge commit is shown against its first parent only.",
                )
                .input::<ShowArgs>()
                .handle(move |args: ShowArgs, _context| {
                    blocking_tool!(Arc::clone(&for_show), args, show)
                }),

            mcp::tool("diff")
                .description(
                    "Compare two revisions and report every file that differs, with insertion and \
                     deletion counts and rename detection. Pass patch=true for the unified diff \
                     text, and paths to scope it. Set use_merge_base=true to see only what the \
                     newer side added, which is what a review of a branch usually means.",
                )
                .input::<DiffArgs>()
                .handle(move |args: DiffArgs, _context| {
                    blocking_tool!(Arc::clone(&for_diff), args, diff)
                }),

            mcp::tool("blame")
                .description(
                    "Attribute each line of one file to the commit that last changed it, with the \
                     author and date. Use start_line and end_line to ask about the range you \
                     actually care about — this is the most expensive tool here, and the whole \
                     file is rarely the question. This answers 'who wrote this and why'.",
                )
                .input::<BlameArgs>()
                .handle(move |args: BlameArgs, _context| {
                    blocking_tool!(Arc::clone(&for_blame), args, blame)
                }),

            mcp::tool("refs")
                .description(
                    "List the repository's branches and tags with the commit each points at, \
                     ordered newest first. Use it to find the release tags and branch names the \
                     other tools take as revisions. Nothing here contacts a remote: remote \
                     branches are whatever the last fetch left on disk.",
                )
                .input::<RefsArgs>()
                .handle(move |args: RefsArgs, _context| {
                    blocking_tool!(Arc::clone(&for_refs), args, refs)
                }),

            mcp::tool("repo_status")
                .description(
                    "Report the working tree: staged, modified, deleted, renamed, and untracked \
                     files, plus the branch HEAD is on and whether an operation such as a rebase \
                     or a merge is in progress. This is `git status`; the separate status tool \
                     reports the plugin's own configuration. Bare repositories have no working \
                     tree and are refused here.",
                )
                .input::<RepoStatusArgs>()
                .handle(move |args: RepoStatusArgs, _context| {
                    blocking_tool!(Arc::clone(&for_repo_status), args, repo_status)
                }),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tdcc_plugin::Plugin;

    fn manifest() -> tdcc_plugin::proto::PluginManifest {
        git_tools_plugin(Arc::new(Registry::for_manifest_only()))
            .manifest()
            .expect("declarative plugins have a manifest")
    }

    #[test]
    fn the_manifest_declares_exactly_the_seven_documented_tools() {
        let manifest = manifest();

        let mut names: Vec<&str> = manifest
            .operations
            .iter()
            .map(|operation| operation.name.as_str())
            .collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec![
                "blame",
                "diff",
                "log",
                "refs",
                "repo_status",
                "show",
                "status"
            ],
            "tool names are part of the contract other people write down"
        );
    }

    #[test]
    fn nothing_else_is_contributed() {
        let manifest = manifest();
        // No HTTP surface, no mesh channel, no event subscription, no web UI,
        // and no config schema to render. Delivery is allowlist-based, so
        // declaring nothing here means receiving nothing.
        assert!(manifest.http_bindings.is_empty());
        assert!(manifest.mesh_channels.is_empty());
        assert!(manifest.mesh_event_subscriptions.is_empty());
        assert!(manifest.web_ui.is_none());
        assert!(manifest.config_schema.is_none());
        assert!(manifest.capabilities.is_empty());
    }

    #[test]
    fn every_tool_advertises_an_input_schema() {
        for operation in &manifest().operations {
            assert!(
                !operation.input_schema_json.is_empty(),
                "{} has no input schema, so the host cannot validate its arguments",
                operation.name
            );
        }
    }

    #[test]
    fn no_tool_name_suggests_a_write() {
        // The read-only claim in the README is worth a test rather than a
        // promise: a future tool called `commit` or `fetch` should fail here.
        const WRITE_SHAPED: &[&str] = &[
            "commit", "push", "fetch", "pull", "checkout", "merge", "rebase", "tag", "reset",
            "clone", "apply", "revert", "cherry", "stash", "config", "remote", "write", "delete",
        ];
        for operation in &manifest().operations {
            for shape in WRITE_SHAPED {
                assert!(
                    !operation.name.contains(shape),
                    "{} looks like a write operation",
                    operation.name
                );
            }
        }
    }

    #[test]
    fn every_tool_description_is_written_for_a_stranger() {
        for operation in &manifest().operations {
            let description = operation.description.as_str();
            assert!(
                description.len() > 80,
                "{} has a description too short to choose it by: {description:?}",
                operation.name
            );
        }
    }

    #[test]
    fn every_argument_schema_refuses_unknown_fields() {
        // `deny_unknown_fields` on every argument struct means there is nowhere
        // for stray prompt content to land in a request, which is a guarantee
        // worth pinning rather than re-reading eight structs to confirm.
        for operation in &manifest().operations {
            let schema: serde_json::Value = serde_json::from_str(&operation.input_schema_json)
                .unwrap_or_else(|error| {
                    panic!("{} has an unparseable schema: {error}", operation.name)
                });
            assert_eq!(
                schema.get("additionalProperties"),
                Some(&serde_json::Value::Bool(false)),
                "{} accepts unknown fields",
                operation.name
            );
        }
    }
}
