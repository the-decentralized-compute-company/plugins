//! `code-context` — give the model your repository without pasting it.
//!
//! Indexes one local directory and exposes search, read, and tree as MCP tools.
//! Results carry paths and line numbers so a model can cite the source instead
//! of paraphrasing it, and every response is capped so a single answer cannot
//! swallow a context window.
//!
//! The plugin never opens a socket, serves HTTP, or speaks MCP JSON-RPC. It
//! declares five tools; the host synthesizes `tools/list`, `tools/call`, the
//! JSON Schemas, and the HTTP projection from that declaration.
//!
//! Run it the way the host does (no arguments beyond its configuration): the
//! runtime connects to `TDCC_PLUGIN_ENDPOINT` over `TDCC_PLUGIN_TRANSPORT`.
//! Run it with `--print-package-manifest` to emit the `plugin-manifest.json`
//! that would belong in a release archive.

mod filters;
mod index;
mod options;
mod paths;
mod search;
mod symbols;
#[cfg(test)]
mod testsupport;
mod tools;
mod tree;
mod workspace;

use anyhow::{Context, Result, bail};
use tdcc_plugin::{Plugin, PluginRuntime, package_manifest_json};

use crate::workspace::Workspace;

#[tokio::main]
async fn main() -> Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();

    match arguments.first().map(String::as_str) {
        Some("--help" | "-h") => {
            println!("{}", options::USAGE);
            Ok(())
        }

        // Packaging path: the same declaration the runtime registers also
        // produces `plugin-manifest.json`, so packaged metadata cannot drift
        // from the running manifest. No root is needed — and none is used.
        Some("--print-package-manifest") => {
            if arguments.len() > 1 {
                bail!("--print-package-manifest takes no other arguments");
            }
            let plugin = tools::code_context_plugin(Workspace::for_manifest_only());
            let manifest = plugin.manifest().context("code-context manifest")?;
            println!("{}", package_manifest_json(&manifest)?);
            Ok(())
        }

        // Runtime path.
        _ => {
            let options = options::parse(&arguments, |name| std::env::var(name).ok())?;
            let workspace = Workspace::open(options)?;
            // stderr, not stdout, and only here: this is the one place the
            // absolute root is printed. An operator debugging a misconfigured
            // plugin needs it; tool responses deliberately never carry it.
            eprintln!(
                "code-context: confined to {}",
                paths::display_path(workspace.root())
            );
            PluginRuntime::run(tools::code_context_plugin(workspace)).await
        }
    }
}
