//! `docker-inspect` — answer questions about what is running on this machine.
//!
//! Seven read-only MCP tools over the Docker Engine API: list containers,
//! inspect one, tail its logs, sample its resource use, list images, probe the
//! daemon, and report how the plugin itself is configured.
//!
//! **Read-only is structural, not a policy.** Access to the Docker socket is
//! equivalent to root on the host — anyone who can create a container can mount
//! the host filesystem into it — so this plugin is built so that the write
//! verbs do not exist in the binary rather than merely going uncalled:
//!
//! * `src/transport.rs` writes the method as a literal `GET`. There is no
//!   parameter for it and no other function that writes to the socket.
//! * `src/paths.rs` owns a newtype whose only constructors are the eight read
//!   paths this plugin uses. Nothing else in the crate can build one.
//! * No Docker client library is linked, so `create`, `exec`, `start`, `stop`,
//!   and `remove` are absent from the compiled artifact instead of merely
//!   unused.
//!
//! Run it the way the host does (no arguments beyond its own configuration
//! flags): the runtime connects to `TDCC_PLUGIN_ENDPOINT` over
//! `TDCC_PLUGIN_TRANSPORT` and serves the manifest. Run it with
//! `--print-package-manifest` to emit the `plugin-manifest.json` that would go
//! in a release archive — for this plugin that is `{}`, because it declares
//! neither a config schema nor a web UI, so the file may be left out entirely.

mod api;
mod endpoint;
mod logs;
mod manifest;
mod model;
mod paths;
mod settings;
mod stats;
#[cfg(test)]
mod testsupport;
mod tools;
mod transport;
mod visibility;

use std::sync::Arc;

use anyhow::{Context, Result};
use tdcc_plugin::{Plugin, PluginRuntime, package_manifest_json};

use crate::settings::{EnvMap, Settings};
use crate::tools::Inspector;

#[tokio::main]
async fn main() -> Result<()> {
    // `--print-package-manifest` must work without any configuration, so the
    // packaging path is resolved before the settings are.
    let packaging = matches!(
        std::env::args().nth(1).as_deref(),
        Some("--print-package-manifest")
    );

    let settings = if packaging {
        Settings::parse(&[], &EnvMap::new())
    } else {
        Settings::from_process()
    }
    .map_err(|error| anyhow::anyhow!("{error}"))
    .context("docker-inspect configuration")?;

    if !packaging {
        announce(&settings);
    }

    let plugin = manifest::docker_inspect_plugin(Arc::new(Inspector::new(settings)));

    match std::env::args().nth(1).as_deref() {
        Some("--print-package-manifest") => {
            let manifest = plugin.manifest().context("docker-inspect manifest")?;
            println!("{}", package_manifest_json(&manifest)?);
            Ok(())
        }
        // Anything else is a configuration flag, already validated above.
        _ => PluginRuntime::run(plugin).await,
    }
}

/// Say, once, on stderr — where the host's log picks it up — exactly what this
/// process was configured to expose.
///
/// An operator granting a plugin access to the Docker socket should be able to
/// read back what that turned into without calling a tool, and the two lines
/// that matter most are the visibility filter and whether logs are readable.
fn announce(settings: &Settings) {
    eprintln!("docker-inspect: {}", settings.summary());

    if settings.endpoint.is_network() {
        eprintln!(
            "docker-inspect: WARNING — {} is a cleartext TCP Docker endpoint, enabled with \
             --allow-tcp. A TCP Docker endpoint has no authentication: everyone who can reach \
             that port can create containers on that machine, and anyone who can create a \
             container can become root on it. This is a serious misconfiguration to leave in \
             place, whatever this plugin does with it.",
            settings.endpoint
        );
    }

    if !settings.visibility.is_filtered() {
        eprintln!(
            "docker-inspect: every container on this machine is visible to this plugin, including \
             ones belonging to other work. Restrict it with --container <name pattern> or --label \
             <key>[=<value>] in [[plugin]].args."
        );
    }

    if settings.show_env {
        eprintln!(
            "docker-inspect: --show-env is set, so container environment variable *values* are \
             included in inspect_container output. Those routinely hold credentials."
        );
    }

    // A socket that is not there is worth saying at startup rather than only at
    // the first tool call — but it is not fatal: Docker may start later, and
    // every tool already reports the problem with the setting that fixes it.
    if let crate::endpoint::Endpoint::Unix(path) = &settings.endpoint
        && !path.exists()
    {
        eprintln!(
            "docker-inspect: {} does not exist yet. Docker may not be running, or may listen \
             elsewhere — every tool will say so and name the setting that changes it.",
            path.display()
        );
    }
}
