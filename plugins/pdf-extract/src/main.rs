//! `pdf-extract` — get the text out of the documents people actually have.
//!
//! Reads PDFs from configured directories and exposes them as five MCP tools:
//! extract text with page numbers, describe a document, pull out tables, list
//! what is available, and report the limits in force. Reading order is
//! recovered from geometry rather than from operator order, so a two-column
//! page comes back one column at a time instead of interleaved, and a page
//! that is only a scanned image is reported as one rather than as an empty
//! success.
//!
//! The plugin never opens a socket, serves HTTP, or speaks MCP JSON-RPC. It
//! declares five tools; the host synthesizes `tools/list`, `tools/call`, the
//! JSON Schemas, and the HTTP projection from that declaration.
//!
//! Run it the way the host does (no arguments beyond its configuration): the
//! runtime connects to `TDCC_PLUGIN_ENDPOINT` over `TDCC_PLUGIN_TRANSPORT`.
//! Run it with `--print-package-manifest` to emit the `plugin-manifest.json`
//! that would belong in a release archive — for this plugin that is `{}`,
//! because it declares neither a config schema nor a web UI, so the file may be
//! left out of the archive entirely.

mod budget;
mod glyphs;
mod layout;
mod listing;
mod options;
mod paths;
mod pdf;
mod tables;
#[cfg(test)]
mod testsupport;
mod tools;

use anyhow::{Context, Result, bail};
use tdcc_plugin::{Plugin, PluginRuntime, package_manifest_json};

use crate::tools::Library;

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
            let plugin = tools::pdf_extract_plugin(Library::for_manifest_only());
            let manifest = plugin.manifest().context("pdf-extract manifest")?;
            println!("{}", package_manifest_json(&manifest)?);
            Ok(())
        }

        // Runtime path.
        _ => {
            let options = options::parse(&arguments, |name| std::env::var(name).ok())?;
            let library = Library::open(options)?;
            // stderr, not stdout, and only here: this is the one place the
            // absolute roots are printed. An operator debugging a misconfigured
            // plugin needs them; tool responses deliberately never carry them.
            for root in library.roots().iter() {
                eprintln!(
                    "pdf-extract: `{}/` is {}",
                    root.label,
                    paths::display_path(&root.directory)
                );
            }
            PluginRuntime::run(tools::pdf_extract_plugin(library)).await
        }
    }
}
