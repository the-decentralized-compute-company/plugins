//! `mcp-bridge` — expose MCP servers an operator listed as tools on this node.
//!
//! Run it the way the host does (no arguments): the runtime connects to
//! `TDCC_PLUGIN_ENDPOINT` over `TDCC_PLUGIN_TRANSPORT` and serves the manifest.
//! Run it with `--check-config` to validate the server list without launching
//! anything, `--print-package-manifest` to emit the `plugin-manifest.json` that
//! belongs in a release archive, or `--help` for the options.
//!
//! Layout:
//!
//! * `cli`       — startup options: where the server list lives
//! * `config`    — the server list an operator writes: parse and validate
//! * `naming`    — `<alias>__<tool>`, and why that is unambiguous
//! * `filter`    — the allowlist and denylist, as a pure decision
//! * `schema`    — forwarding an upstream JSON Schema, and the three cases
//!   where there is nothing to forward
//! * `childenv`  — what environment a launched server is given, and what it is
//!   never given
//! * `backoff`   — when a dead server is tried again
//! * `forward`   — bounding an answer and stamping it with its origin
//! * `upstream`  — the live connection: launch, discover, supervise, call
//! * `bridge`    — the registry and the three management tools' answers
//! * `manifest`  — what the host projects, including one operation per bridged
//!   upstream tool
//! * `testserver` — `cfg(test)` only: a real, deliberately fallible MCP server
//!   the end-to-end tests drive over an in-process pipe
//!
//! **Discovery happens before the runtime starts.** A plugin's manifest is sent
//! once, in the initialize response, so the tool list has to be complete before
//! `PluginRuntime::run` is called. Every server is contacted concurrently, so
//! startup costs the slowest server's `connect_timeout_secs` rather than the
//! sum of all of them.

mod backoff;
mod bridge;
mod childenv;
mod cli;
mod config;
mod filter;
mod forward;
mod manifest;
mod naming;
mod schema;
#[cfg(test)]
mod testserver;
mod upstream;

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use tdcc_plugin::{Plugin, PluginRuntime, package_manifest_json};
use tokio::task::JoinSet;

use crate::bridge::{Bridge, ConfigState};
use crate::cli::{Command, Environment};
use crate::config::{Document, ServerSpec, Transport, parse_document};
use crate::upstream::{LinkState, Upstream, supervise};

#[tokio::main]
async fn main() -> Result<()> {
    let options = cli::parse(std::env::args().skip(1), &Environment::from_process())
        .map_err(|error| anyhow!("{error}\n\n{}", cli::USAGE))?;
    let path_label = options.servers_path.display().to_string();

    match options.command {
        Command::Help => {
            print!("{}", cli::USAGE);
            Ok(())
        }

        // Packaging path: the same declaration the runtime registers also
        // produces `plugin-manifest.json`, so packaged metadata cannot drift
        // from the running manifest. It deliberately does not read the server
        // list and launches nothing — packaging must not depend on, or disturb,
        // a node's configuration.
        Command::PrintPackageManifest => {
            let bridge = Bridge::detached(path_label, ConfigState::Absent);
            let plugin = manifest::mcp_bridge_plugin(Arc::new(bridge));
            let manifest = plugin.manifest().context("mcp-bridge manifest")?;
            println!("{}", package_manifest_json(&manifest)?);
            Ok(())
        }

        // Validation path: parse the server list and print the plan. Launches
        // no process and opens no connection, so it is safe to run on a machine
        // whose server list you have not read yet.
        Command::CheckConfig => {
            let (state, document) = load_document(&options.servers_path);
            print!("{}", describe_plan(&path_label, &state, &document));
            match state {
                ConfigState::Invalid { .. } | ConfigState::Unreadable { .. } => {
                    Err(anyhow!("the server list did not load"))
                }
                _ => Ok(()),
            }
        }

        Command::Run => {
            let (state, document) = load_document(&options.servers_path);
            for line in startup_messages(&path_label, &state, &document) {
                eprintln!("{line}");
            }

            let servers = start_servers(&document).await;
            for server in &servers {
                let snapshot = server.snapshot().await;
                match snapshot.state {
                    LinkState::Ready => eprintln!(
                        "mcp-bridge: MCP server '{}' ready — {} of {} tool(s) bridged as '{}__…'",
                        snapshot.alias,
                        snapshot.tools_projected,
                        snapshot.tools_published,
                        snapshot.alias
                    ),
                    LinkState::Disabled => eprintln!(
                        "mcp-bridge: MCP server '{}' is disabled in the server list; not launched",
                        snapshot.alias
                    ),
                    _ => eprintln!(
                        "mcp-bridge: MCP server '{}' could not be reached, so it contributes no \
                         tools to this node until the plugin restarts: {}",
                        snapshot.alias,
                        snapshot
                            .last_error
                            .as_deref()
                            .unwrap_or("no reason recorded")
                    ),
                }
            }

            let bridge = Arc::new(Bridge::new(path_label, state, &document, servers));
            eprintln!("mcp-bridge: {}", bridge.health().await);

            // Supervision starts before the runtime does, so a server that
            // drops during initialize is already being retried.
            for server in bridge.servers() {
                tokio::spawn(supervise(Arc::clone(server)));
            }

            PluginRuntime::run(manifest::mcp_bridge_plugin(bridge)).await
        }
    }
}

/// Read and validate the server list.
///
/// A missing file is not an error: an operator who installs this plugin before
/// writing a server list gets a node that bridges nothing and says so. A file
/// that exists but does not parse **is** treated as fail-closed — nothing is
/// launched — because the file existing is evidence somebody meant something by
/// it, and guessing which half of it to honour is worse than honouring none.
fn load_document(path: &Path) -> (ConfigState, Document) {
    match std::fs::read_to_string(path) {
        Ok(text) => match parse_document(&text) {
            Ok(document) => (ConfigState::Loaded, document),
            Err(error) => (ConfigState::Invalid { error }, Document::default()),
        },
        Err(error) if error.kind() == ErrorKind::NotFound => {
            (ConfigState::Absent, Document::default())
        }
        Err(error) => (
            ConfigState::Unreadable {
                error: error.to_string(),
            },
            Document::default(),
        ),
    }
}

/// Contact every enabled server at once.
///
/// Concurrent rather than sequential because the host has its own startup
/// timeout for a plugin, and three servers that each take twenty seconds to
/// start would otherwise add up past it.
async fn start_servers(document: &Document) -> Vec<Arc<Upstream>> {
    let parent_env: Arc<BTreeMap<String, String>> = Arc::new(std::env::vars().collect());
    let mut tasks: JoinSet<(usize, Arc<Upstream>)> = JoinSet::new();
    for (index, spec) in document.servers.iter().enumerate() {
        let spec = spec.clone();
        let parent_env = Arc::clone(&parent_env);
        tasks.spawn(async move { (index, Upstream::start(spec, parent_env).await) });
    }

    let mut started: Vec<(usize, Arc<Upstream>)> = Vec::with_capacity(document.servers.len());
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(entry) => started.push(entry),
            // A panic inside one server's startup must not take the plugin
            // down; the others are still worth bridging.
            Err(error) => eprintln!("mcp-bridge: a server failed to start: {error}"),
        }
    }
    started.sort_by_key(|(index, _)| *index);
    started.into_iter().map(|(_, server)| server).collect()
}

/// The lines printed to stderr at startup, where the host's log picks them up.
fn startup_messages(path: &str, state: &ConfigState, document: &Document) -> Vec<String> {
    match state {
        ConfigState::Absent => vec![format!(
            "mcp-bridge: no server list at {path}, so nothing is bridged. Create that file — see \
             the plugin's README — and restart tdcc. Nothing is auto-discovered."
        )],
        ConfigState::Unreadable { error } => vec![format!(
            "mcp-bridge: the server list at {path} could not be read ({error}). No MCP server was \
             launched."
        )],
        ConfigState::Invalid { error } => vec![
            format!(
                "mcp-bridge: the server list at {path} did not validate, so NO MCP server was \
                 launched:"
            ),
            error.clone(),
        ],
        ConfigState::Loaded => vec![format!(
            "mcp-bridge: {path} lists {} server(s), {} enabled",
            document.servers.len(),
            document.enabled_servers().count()
        )],
    }
}

/// The `--check-config` report: what would be launched, and what would not.
fn describe_plan(path: &str, state: &ConfigState, document: &Document) -> String {
    let mut out = String::new();
    out.push_str(&format!("server list: {path}\nstate: {}\n", state.as_str()));

    match state {
        ConfigState::Absent => {
            out.push_str(
                "\nThere is no file at that path, so mcp-bridge would bridge nothing. Nothing is \
                 auto-discovered: every server has to be written down.\n",
            );
            return out;
        }
        ConfigState::Unreadable { error } | ConfigState::Invalid { error } => {
            out.push_str(&format!("\n{error}\n\nNo server would be launched.\n"));
            return out;
        }
        ConfigState::Loaded => {}
    }

    out.push_str(&format!("servers: {}\n", document.servers.len()));
    let environment: BTreeMap<String, String> = std::env::vars().collect();

    for spec in &document.servers {
        out.push_str(&format!(
            "\n[{}] {} — {}\n",
            spec.alias,
            if spec.enabled {
                "enabled"
            } else {
                "disabled, would not be launched"
            },
            spec.transport.label()
        ));
        out.push_str(&format!(
            "  tools prefixed  {}__…\n  timeouts        connect {} s, call {} s\n  result cap      \
             {} bytes\n  restart         {}\n",
            spec.alias,
            spec.connect_timeout.as_secs(),
            spec.call_timeout.as_secs(),
            spec.max_result_bytes,
            if spec.restart {
                "yes, with backoff"
            } else {
                "no"
            }
        ));
        out.push_str(&format!(
            "  allow_tools     {}\n  deny_tools      {}\n",
            describe_patterns(&spec.allow_tools, "(all tools)"),
            describe_patterns(&spec.deny_tools, "(none)")
        ));
        out.push_str(&describe_secrets(spec, &environment));
    }

    out.push_str(
        "\nNothing above was launched and no connection was opened. Each entry runs with the \
         privileges of the tdcc process when it does.\n",
    );
    out
}

fn describe_patterns(patterns: &[String], empty: &str) -> String {
    if patterns.is_empty() {
        empty.to_string()
    } else {
        patterns.join(", ")
    }
}

/// Report which named variables are present, by **name only**. A value is never
/// printed, and neither is its length.
fn describe_secrets(spec: &ServerSpec, environment: &BTreeMap<String, String>) -> String {
    let names: Vec<&String> = match &spec.transport {
        Transport::Stdio { env_from, .. } => env_from.iter().collect(),
        Transport::Http {
            bearer_token_env, ..
        } => bearer_token_env.iter().collect(),
    };
    if names.is_empty() {
        return String::new();
    }
    let mut out = String::from("  from environment\n");
    for name in names {
        let present = environment
            .get(name)
            .is_some_and(|value| !value.trim().is_empty());
        out.push_str(&format!(
            "    {name}: {}\n",
            if present {
                "set"
            } else {
                "NOT SET in the tdcc process — this server would refuse to start"
            }
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(text: &str) -> Document {
        parse_document(text).expect("test document parses")
    }

    const TWO_SERVERS: &str = r#"
version = 1

[[server]]
alias = "files"
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/srv/shared"]
allow_tools = ["read_file", "list_directory"]

[[server]]
alias = "notes"
transport = "http"
url = "http://127.0.0.1:7777/mcp"
enabled = false
"#;

    #[test]
    fn a_missing_server_list_says_so_and_says_nothing_is_auto_discovered() {
        let lines = startup_messages(
            "/home/operator/.tdcc/mcp-bridge.toml",
            &ConfigState::Absent,
            &Document::default(),
        );

        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("nothing is bridged"), "{}", lines[0]);
        assert!(lines[0].contains("auto-discovered"), "{}", lines[0]);
    }

    #[test]
    fn an_invalid_server_list_says_loudly_that_nothing_was_launched() {
        let lines = startup_messages(
            "/x.toml",
            &ConfigState::Invalid {
                error: "alias 'Files' is not usable".to_string(),
            },
            &Document::default(),
        );

        assert!(lines[0].contains("NO MCP server was launched"), "{lines:?}");
        assert!(lines[1].contains("Files"), "{lines:?}");
    }

    #[test]
    fn a_loaded_list_reports_how_many_servers_are_enabled() {
        let lines = startup_messages("/x.toml", &ConfigState::Loaded, &document(TWO_SERVERS));

        assert!(lines[0].contains("2 server(s), 1 enabled"), "{lines:?}");
    }

    #[test]
    fn the_plan_shows_the_prefix_the_filters_and_the_timeouts_for_each_server() {
        let plan = describe_plan("/x.toml", &ConfigState::Loaded, &document(TWO_SERVERS));

        assert!(plan.contains("files__…"), "{plan}");
        assert!(plan.contains("read_file, list_directory"), "{plan}");
        assert!(plan.contains("(none)"), "{plan}");
        assert!(plan.contains("disabled, would not be launched"), "{plan}");
        assert!(plan.contains("no connection was opened"), "{plan}");
    }

    #[test]
    fn the_plan_for_an_invalid_list_repeats_the_errors_and_launches_nothing() {
        let plan = describe_plan(
            "/x.toml",
            &ConfigState::Invalid {
                error: "[[server]] #1 (alias 'Files'): server alias 'Files' is not usable"
                    .to_string(),
            },
            &Document::default(),
        );

        assert!(plan.contains("Files"), "{plan}");
        assert!(plan.contains("No server would be launched"), "{plan}");
    }

    #[test]
    fn the_plan_names_a_missing_variable_without_printing_any_value() {
        let document = document(
            r#"
version = 1

[[server]]
alias = "remote"
transport = "http"
url = "https://mcp.example.com/mcp"
bearer_token_env = "MCP_BRIDGE_TEST_TOKEN_ABSENT"
"#,
        );
        let environment: BTreeMap<String, String> =
            [("SOMETHING_ELSE".to_string(), "value".to_string())]
                .into_iter()
                .collect();

        let rendered = describe_secrets(&document.servers[0], &environment);

        assert!(
            rendered.contains("MCP_BRIDGE_TEST_TOKEN_ABSENT"),
            "{rendered}"
        );
        assert!(rendered.contains("NOT SET"), "{rendered}");
    }

    #[test]
    fn the_plan_reports_a_present_variable_by_name_only() {
        let document = document(
            r#"
version = 1

[[server]]
alias = "files"
transport = "stdio"
command = "npx"
env_from = ["SECRET_TOKEN"]
"#,
        );
        let environment: BTreeMap<String, String> =
            [("SECRET_TOKEN".to_string(), "super-secret-value".to_string())]
                .into_iter()
                .collect();

        let rendered = describe_secrets(&document.servers[0], &environment);

        assert!(rendered.contains("SECRET_TOKEN: set"), "{rendered}");
        assert!(!rendered.contains("super-secret-value"), "{rendered}");
    }

    #[tokio::test]
    async fn starting_an_empty_document_launches_nothing_and_returns_nothing() {
        let servers = start_servers(&Document::default()).await;

        assert!(servers.is_empty());
    }

    #[tokio::test]
    async fn servers_come_back_in_the_order_the_operator_wrote_them() {
        let document = document(
            r#"
version = 1

[[server]]
alias = "first"
transport = "stdio"
command = "definitely-not-a-real-binary-9c1f"
connect_timeout_secs = 2

[[server]]
alias = "second"
transport = "stdio"
command = "definitely-not-a-real-binary-9c1f"
connect_timeout_secs = 2

[[server]]
alias = "third"
transport = "stdio"
command = "definitely-not-a-real-binary-9c1f"
connect_timeout_secs = 2
"#,
        );

        let servers = start_servers(&document).await;

        let aliases: Vec<&str> = servers.iter().map(|server| server.alias()).collect();
        assert_eq!(aliases, vec!["first", "second", "third"]);
    }

    #[test]
    fn a_server_list_that_does_not_exist_reads_as_absent_rather_than_as_an_error() {
        let (state, document) = load_document(Path::new(
            "./definitely-not-a-real-path-9c1f/mcp-bridge.toml",
        ));

        assert_eq!(state, ConfigState::Absent);
        assert!(document.servers.is_empty());
    }
}
