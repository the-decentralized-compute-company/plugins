//! `rest-client` — let a model call an API the operator has declared, without
//! giving it a general outbound socket.
//!
//! Run it the way the host does (no arguments beyond its own configuration
//! flags): the runtime connects to `TDCC_PLUGIN_ENDPOINT` over
//! `TDCC_PLUGIN_TRANSPORT` and serves the manifest. Run it with
//! `--print-package-manifest` to emit the `plugin-manifest.json` that would go
//! in a release archive — for this plugin that is `{}`, because it declares
//! neither a config schema nor a web UI, so the file may be left out entirely.
//!
//! Startup, in order:
//!
//! 1. Parse arguments and the environment. An unknown flag is fatal.
//! 2. Read the declaration file. **A file that exists but does not parse is a
//!    startup failure**, deliberately: an operator who wrote a declaration and
//!    got a silently empty catalog would believe their node offers APIs that it
//!    does not, or — worse for the next edit — that a restriction they wrote is
//!    being applied. A file that is simply *absent* is not a failure; the
//!    plugin starts inert and every tool says so.
//! 3. Resolve each endpoint's credential from the environment once. A missing
//!    one disables that endpoint and nothing else.
//! 4. Build the manifest, including the `call` tool description generated from
//!    the declaration.

mod auth;
mod catalog;
mod cli;
mod engine;
mod manifest;
mod net;
mod pathmatch;
mod ratelimit;
mod request;
mod schema;

use anyhow::{Context, Result, anyhow};
use tdcc_plugin::{Plugin, PluginRuntime, package_manifest_json};

use crate::catalog::Catalog;
use crate::engine::{CatalogSource, Engine};

pub const PLUGIN_NAME: &str = "rest-client";
pub const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The product token this plugin identifies itself with in the `User-Agent` of
/// every outbound request.
pub const PRODUCT_TOKEN: &str = "tdcc-rest-client";
pub const PRODUCT_URL: &str = "https://github.com/the-decentralized-compute-company/plugins";

#[tokio::main]
async fn main() -> Result<()> {
    let options = cli::parse(std::env::args().skip(1), &cli::Environment::from_process())
        .map_err(|error| anyhow!("{error}"))
        .context("rest-client configuration")?;

    if options.command == cli::Command::Help {
        print!("{}", cli::USAGE);
        return Ok(());
    }

    let (catalog, source) = load(&options.config_path)?;
    let environment = std::env::vars().collect();
    let engine = Engine::new(
        catalog,
        source,
        &environment,
        &cli::user_agent(options.contact.as_deref()),
    )
    .map_err(|error| anyhow!("{error}"))?;

    // One line on stderr, where the host's log picks it up, so an operator can
    // see what was loaded without calling a tool.
    eprintln!("rest-client: {}", engine.health());

    let plugin = manifest::rest_client_plugin(engine);
    match options.command {
        cli::Command::PrintPackageManifest => {
            let manifest = plugin.manifest().context("rest-client manifest")?;
            println!("{}", package_manifest_json(&manifest)?);
            Ok(())
        }
        cli::Command::Help => unreachable!("handled above"),
        cli::Command::Run => PluginRuntime::run(plugin).await,
    }
}

/// Read and validate the declaration.
///
/// Absent is fine; unreadable and unparseable are not. The distinction matters:
/// "you have not written one yet" and "the one you wrote is wrong" call for
/// completely different reactions from the operator, and only one of them is
/// safe to continue past.
fn load(path: &std::path::Path) -> Result<(Catalog, CatalogSource)> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let catalog = catalog::parse(&text).map_err(|error| {
                anyhow!(
                    "the endpoint declaration at {} is not valid:\n{error}\n\nrest-client refuses \
                     to start with a declaration it cannot read, rather than starting with no \
                     endpoints and leaving you to discover that later.",
                    path.display()
                )
            })?;
            Ok((catalog, CatalogSource::Loaded(path.to_path_buf())))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok((
            Catalog::default(),
            CatalogSource::Missing(path.to_path_buf()),
        )),
        Err(error) => Err(anyhow!(
            "could not read the endpoint declaration at {}: {error}",
            path.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("rest-client-{}-{name}.toml", std::process::id()));
        path
    }

    #[test]
    fn an_absent_declaration_starts_the_plugin_inert_rather_than_failing() {
        let path = temp_path("absent");
        let _ = std::fs::remove_file(&path);

        let (catalog, source) = load(&path).expect("an absent file is not an error");

        assert!(catalog.endpoints.is_empty());
        assert_eq!(source, CatalogSource::Missing(path));
    }

    #[test]
    fn a_valid_declaration_is_loaded() {
        let path = temp_path("valid");
        std::fs::write(&path, catalog::SAMPLE).expect("write the fixture");

        let (catalog, source) = load(&path).expect("a valid declaration loads");

        assert_eq!(catalog.names(), vec!["example".to_string()]);
        assert_eq!(source, CatalogSource::Loaded(path.clone()));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_invalid_declaration_is_a_startup_failure_that_quotes_the_problem() {
        let path = temp_path("invalid");
        std::fs::write(&path, "version = 1\n[[endpoint]]\nname = \"x\"\n")
            .expect("write the fixture");

        let error = load(&path).expect_err("a broken declaration must not start the plugin");

        let message = format!("{error}");
        assert!(message.contains("is not valid"), "{message}");
        assert!(message.contains("refuses to start"), "{message}");
        let _ = std::fs::remove_file(&path);
    }
}
