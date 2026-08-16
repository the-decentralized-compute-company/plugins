//! `git-tools` — the questions about a repository that file search cannot
//! answer.
//!
//! `code-context` indexes what a repository's files *say*. This plugin exposes
//! what its history *did*: when a line changed, who changed it, what landed
//! between two releases, and what is uncommitted right now. Seven MCP tools,
//! all read-only, all confined to repositories the machine's operator listed by
//! path.
//!
//! The plugin never opens a socket, serves HTTP, or speaks MCP JSON-RPC. It
//! declares seven tools; the host synthesizes `tools/list`, `tools/call`, the
//! JSON Schemas, and the HTTP projection from that declaration.
//!
//! Run it the way the host does (no arguments beyond its configuration): the
//! runtime connects to `TDCC_PLUGIN_ENDPOINT` over `TDCC_PLUGIN_TRANSPORT`.
//! Run it with `--print-package-manifest` to emit the `plugin-manifest.json`
//! that would belong in a release archive — for this plugin that is `{}`,
//! because it declares neither a config schema nor a web UI.

mod blame;
mod changes;
mod guard;
mod history;
mod inventory;
mod render;
mod repos;
mod resolve;
mod settings;
#[cfg(test)]
mod testsupport;
mod tools;

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use tdcc_plugin::{Plugin, PluginRuntime, package_manifest_json};

use crate::repos::{Registry, display_path};

#[tokio::main]
async fn main() -> Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();

    match arguments.first().map(String::as_str) {
        Some("--help" | "-h") => {
            println!("{}", settings::USAGE);
            Ok(())
        }

        // Packaging path: the same declaration the runtime registers also
        // produces `plugin-manifest.json`, so packaged metadata cannot drift
        // from the running manifest. No repository is needed — and none is
        // opened.
        Some("--print-package-manifest") => {
            if arguments.len() > 1 {
                bail!("--print-package-manifest takes no other arguments");
            }
            let plugin = tools::git_tools_plugin(Arc::new(Registry::for_manifest_only()));
            let manifest = plugin.manifest().context("git-tools manifest")?;
            println!("{}", package_manifest_json(&manifest)?);
            Ok(())
        }

        // Runtime path.
        _ => {
            let settings =
                settings::parse_settings(arguments, |name| std::env::var(name).ok(), true)
                    .map_err(|error| anyhow::anyhow!("{error}"))
                    .context("git-tools configuration")?;

            let registry =
                Registry::resolve(&settings.repositories, settings.limits, settings.disclosure);

            // stderr, not stdout, and only here: this is the one place an
            // absolute path is printed. An operator debugging a misconfigured
            // plugin needs it; tool responses deliberately never carry it.
            for problem in registry.problems() {
                eprint!(
                    "git-tools: repository {:?} at {} is unavailable: {}",
                    problem.alias,
                    display_path(&problem.configured),
                    problem.error
                );
                match problem.error.detail() {
                    Some(detail) => eprintln!(" ({detail})"),
                    None => eprintln!(),
                }
            }

            if registry.repositories().is_empty() {
                bail!(
                    "no configured repository could be opened, so every tool would fail. Check \
                     the paths in [[plugin]].args and the reasons printed above"
                );
            }

            for repository in registry.repositories() {
                eprintln!(
                    "git-tools: reading {:?} at {} (read-only{}{})",
                    repository.alias,
                    display_path(&repository.root),
                    if repository.bare { ", bare" } else { "" },
                    if settings.disclosure.content {
                        ""
                    } else {
                        ", no content"
                    }
                );
            }

            PluginRuntime::run(tools::git_tools_plugin(Arc::new(registry))).await
        }
    }
}
