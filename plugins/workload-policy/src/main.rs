//! `workload-policy` — what this machine will and will not accept.
//!
//! Run it the way the host does (no arguments): the runtime connects to
//! `TDCC_PLUGIN_ENDPOINT` over `TDCC_PLUGIN_TRANSPORT` and serves the manifest.
//! Run it with `--print-package-manifest` to emit the `plugin-manifest.json`
//! that belongs in a release archive, or with `--help` for the options.
//!
//! Layout:
//!
//! * `cli`       — startup options, including where the policy file lives
//! * `policy`    — the document an operator writes: parse and validate
//! * `evaluate`  — the decision, as a pure function
//! * `ratelimit` — token buckets for `action = "limit"` rules
//! * `clock`     — the one place that reads the wall clock
//! * `observe`   — the record that makes dry-run useful
//! * `state`     — loading, reloading, and the shapes the tools return
//! * `admit`     — the host admission hook: the decision the node acts on
//! * `manifest`  — what the host projects: the hook, plus four operations on
//!   MCP and HTTP

mod admit;
mod cli;
mod clock;
mod evaluate;
mod manifest;
mod observe;
mod policy;
mod ratelimit;
mod state;

use anyhow::{Context, Result, anyhow};
use tdcc_plugin::{Plugin, PluginRuntime, package_manifest_json};

use crate::cli::{Command, Environment};
use crate::state::PolicyState;

#[tokio::main]
async fn main() -> Result<()> {
    let options = cli::parse(std::env::args().skip(1), &Environment::from_process())
        .map_err(|error| anyhow!("{error}\n\n{}", cli::USAGE))?;

    match options.command {
        Command::Help => {
            print!("{}", cli::USAGE);
            Ok(())
        }
        // Packaging path: the same declaration the runtime registers also
        // produces `plugin-manifest.json`, so packaged metadata cannot drift
        // from the running manifest. It deliberately does not read the policy
        // file — packaging a plugin must not depend on a node's rules.
        Command::PrintPackageManifest => {
            let state = PolicyState::detached(options.policy_path, options.on_invalid);
            let plugin = manifest::workload_policy_plugin(state);
            let manifest = plugin.manifest().context("workload-policy manifest")?;
            println!("{}", package_manifest_json(&manifest)?);
            Ok(())
        }
        // Runtime path. The startup lines go to stderr, where the host's log
        // picks them up: an operator must be able to tell "policy loaded" from
        // "policy did not load" without calling a tool.
        Command::Run => {
            let (state, messages) = PolicyState::load(options.policy_path, options.on_invalid);
            for message in messages {
                eprintln!("{message}");
            }
            PluginRuntime::run(manifest::workload_policy_plugin(state)).await
        }
    }
}
