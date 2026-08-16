//! The whole contribution surface of `docker-inspect` in one declaration.
//!
//! Seven MCP tools, one capability, and a health hook. The host synthesizes
//! `tools/list`, `tools/call`, the JSON Schema for every argument, and the
//! request validation that runs before a handler is entered — this plugin opens
//! no socket and speaks no MCP.
//!
//! Three surfaces are deliberately absent:
//!
//! * **No `http`.** Every tool here reads container configuration, environment
//!   variable names, and log lines. Mounting them as console routes would put
//!   that behind a second door with a different audience, for no gain that the
//!   MCP projection does not already provide.
//! * **No `config_schema`.** `[plugin.settings]` never reaches a plugin
//!   process, and every setting this plugin has is a limit that must be enforced
//!   inside it. A console control that looked authoritative and changed nothing
//!   would be worse than no control at all. See `settings.rs`.
//! * **No `mesh` and no `events`.** Delivery is allowlist-based, so declaring
//!   nothing means receiving nothing, which is the right posture for a plugin
//!   whose job is reading what runs on somebody's own machine.
//!
//! Macro field order is fixed: `metadata`, `startup_policy`, `provides`,
//! `config`, `web_ui`, `mesh`, `events`, `mcp`, `http`, `inference`, then the
//! lifecycle hooks.

use std::sync::Arc;

use tdcc_plugin::{PluginMetadata, SimplePlugin, capability, mcp, plugin, plugin_server_info};

use crate::settings::{PLUGIN_NAME, PLUGIN_VERSION};
use crate::tools::{self, Inspector};

pub fn docker_inspect_plugin(inspector: Arc<Inspector>) -> SimplePlugin {
    // One clone per handler closure: the closures are `Fn`, so each needs its
    // own handle rather than borrowing a shared one.
    let for_status = Arc::clone(&inspector);
    let for_daemon = Arc::clone(&inspector);
    let for_list = Arc::clone(&inspector);
    let for_inspect = Arc::clone(&inspector);
    let for_logs = Arc::clone(&inspector);
    let for_stats = Arc::clone(&inspector);
    let for_images = Arc::clone(&inspector);
    let for_health = inspector;

    plugin! {
        metadata: PluginMetadata::new(
            PLUGIN_NAME,
            PLUGIN_VERSION,
            plugin_server_info(
                PLUGIN_NAME,
                PLUGIN_VERSION,
                "Docker inspect",
                "Read-only answers about the containers running on this machine",
                None::<String>,
            ),
        ),

        // A stable name for "something on this node can describe local
        // containers", so a caller can depend on the capability rather than on
        // this plugin's id.
        provides: [capability("docker-inspect.v1")],

        mcp: [
            // Projected as `docker-inspect.status` on the host MCP endpoint.
            mcp::tool("status")
                .title("Show how docker-inspect is configured")
                .description(
                    "Report how this plugin is configured: which Docker endpoint it uses, which \
                     containers the machine's operator made visible to it, and the caps on log \
                     and list output. Contacts nothing, so it answers even when Docker is down — \
                     use `docker-inspect.daemon` to find out whether the daemon itself responds. \
                     Start here when another tool reports that a container is not visible.",
                )
                .input::<tools::NoArgs>()
                .handle(move |_args: tools::NoArgs, _context| {
                    let inspector = Arc::clone(&for_status);
                    Box::pin(async move { Ok(inspector.status()) })
                }),

            mcp::tool("daemon")
                .title("Check the Docker daemon")
                .description(
                    "Check that the Docker daemon answers, and report its version and what it \
                     says about this host: operating system, kernel, CPU count, total memory, \
                     storage driver, and how many containers and images exist. Returns an error \
                     naming the cause and the setting that fixes it when the endpoint cannot be \
                     reached. The counts here are daemon-wide and ignore this plugin's \
                     visibility filter.",
                )
                .input::<tools::NoArgs>()
                .handle(move |_args: tools::NoArgs, _context| {
                    let inspector = Arc::clone(&for_daemon);
                    Box::pin(async move { inspector.daemon().await })
                }),

            mcp::tool("list_containers")
                .title("List containers")
                .description(
                    "List the containers on this machine that the operator made visible to this \
                     plugin, with name, id, image, state, uptime phrasing, published ports, \
                     networks, and labels. Running containers only unless `all` is true. This is \
                     the tool to call first: every other container tool takes a name or id from \
                     here. The response reports how many containers the operator's filter hid, so \
                     an empty list can be told apart from a restricted view.",
                )
                .input::<tools::ListContainersArgs>()
                .handle(move |args: tools::ListContainersArgs, _context| {
                    let inspector = Arc::clone(&for_list);
                    Box::pin(async move { inspector.list_containers(args).await })
                }),

            mcp::tool("inspect_container")
                .title("Inspect one container")
                .description(
                    "Describe one container in detail: image, state and exit code, health check \
                     result, command, restart policy, mounts with their host paths, networks and \
                     addresses, memory and CPU limits, labels, and notes on anything that widens \
                     what it can do to the host (privileged mode, a mounted Docker socket, host \
                     networking). Environment variable *names* are listed; their values are \
                     hidden unless the operator started this plugin with --show-env, because \
                     container environments routinely hold credentials.",
                )
                .input::<tools::ContainerArgs>()
                .handle(move |args: tools::ContainerArgs, _context| {
                    let inspector = Arc::clone(&for_inspect);
                    Box::pin(async move { inspector.inspect_container(args).await })
                }),

            mcp::tool("container_logs")
                .title("Read recent container logs")
                .description(
                    "Return the most recent log lines from one container, labelled by stdout or \
                     stderr, optionally only those from the last N seconds. The number of lines \
                     is capped by the operator and clamped from the request, and the log is never \
                     followed. Note before calling: container logs frequently contain \
                     credentials, tokens, and personal data, and everything returned here enters \
                     the conversation. Ask for the smallest `tail` that answers the question.",
                )
                .input::<tools::LogsArgs>()
                .handle(move |args: tools::LogsArgs, _context| {
                    let inspector = Arc::clone(&for_logs);
                    Box::pin(async move { inspector.container_logs(args).await })
                }),

            mcp::tool("container_stats")
                .title("Sample a container's resource use")
                .description(
                    "Take one live sample of a running container's resource use: CPU as a \
                     percentage of a whole core, memory against its limit, network bytes in and \
                     out, block IO, and process count. Takes about a second, because the daemon \
                     needs two samples to compute a CPU percentage. Returns an error for a \
                     container that is not running, rather than a page of zeroes that reads like \
                     an idle one.",
                )
                .input::<tools::ContainerArgs>()
                .handle(move |args: tools::ContainerArgs, _context| {
                    let inspector = Arc::clone(&for_stats);
                    Box::pin(async move { inspector.container_stats(args).await })
                }),

            mcp::tool("list_images")
                .title("List local images")
                .description(
                    "List the container images held on this machine, with their tags, size on \
                     disk, creation time, and which of the visible containers use them. Use it to \
                     answer what a container is actually running and what disk the image store is \
                     taking. When the operator restricted which containers are visible, this is \
                     limited to the images those containers use.",
                )
                .input::<tools::NoArgs>()
                .handle(move |args: tools::NoArgs, _context| {
                    let inspector = Arc::clone(&for_images);
                    Box::pin(async move { inspector.list_images(args).await })
                }),
        ],

        // Health must stay fast and independent of anything that can block, and
        // a Docker socket is exactly that. It reports configuration only.
        health: move |_context| {
            let inspector = Arc::clone(&for_health);
            Box::pin(async move { Ok(inspector.health()) })
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tdcc_plugin::Plugin;

    use crate::settings::{EnvMap, Settings};

    const TOOLS: [&str; 7] = [
        "status",
        "daemon",
        "list_containers",
        "inspect_container",
        "container_logs",
        "container_stats",
        "list_images",
    ];

    fn manifest() -> tdcc_plugin::proto::PluginManifest {
        let settings = Settings::parse(&[], &EnvMap::new()).expect("defaults parse");
        docker_inspect_plugin(Arc::new(Inspector::new(settings)))
            .manifest()
            .expect("declarative plugins have a manifest")
    }

    #[test]
    fn every_tool_is_declared_with_a_description_and_a_schema() {
        let manifest = manifest();

        for name in TOOLS {
            let operation = manifest
                .operations
                .iter()
                .find(|operation| operation.name == name)
                .unwrap_or_else(|| panic!("`{name}` is declared"));
            assert!(
                operation.description.len() > 60,
                "`{name}` needs a description a model can act on"
            );
            assert!(
                operation.input_schema_json.contains("\"type\": \"object\"")
                    || operation.input_schema_json.contains("\"type\":\"object\""),
                "{}",
                operation.input_schema_json
            );
        }
        assert_eq!(manifest.operations.len(), TOOLS.len());
    }

    #[test]
    fn the_tools_that_take_arguments_publish_them_as_properties() {
        let manifest = manifest();

        for name in [
            "list_containers",
            "inspect_container",
            "container_logs",
            "container_stats",
        ] {
            let operation = manifest
                .operations
                .iter()
                .find(|operation| operation.name == name)
                .unwrap_or_else(|| panic!("`{name}` is declared"));
            assert!(
                operation.input_schema_json.contains("\"properties\""),
                "{}",
                operation.input_schema_json
            );
        }
    }

    #[test]
    fn no_tool_is_declared_that_could_change_anything() {
        let manifest = manifest();

        for forbidden in [
            "create", "exec", "start", "stop", "restart", "kill", "remove", "delete", "prune",
            "pull", "push", "commit", "update", "rename", "pause",
        ] {
            assert!(
                !manifest
                    .operations
                    .iter()
                    .any(|operation| operation.name.contains(forbidden)),
                "`{forbidden}` must not be reachable through this plugin"
            );
        }
    }

    #[test]
    fn the_argument_schemas_carry_the_doc_comments_a_model_reads() {
        let manifest = manifest();
        let logs = manifest
            .operations
            .iter()
            .find(|operation| operation.name == "container_logs")
            .expect("container_logs is declared");

        assert!(
            logs.input_schema_json.contains("Clamped to the operator"),
            "{}",
            logs.input_schema_json
        );
        assert!(logs.input_schema_json.contains("\"required\""));
    }

    #[test]
    fn the_log_tool_warns_about_credentials_where_a_model_will_read_it() {
        let manifest = manifest();
        let logs = manifest
            .operations
            .iter()
            .find(|operation| operation.name == "container_logs")
            .expect("container_logs is declared");

        assert!(
            logs.description.contains("credentials"),
            "{}",
            logs.description
        );
    }

    #[test]
    fn nothing_but_mcp_and_one_capability_is_contributed() {
        let manifest = manifest();

        assert!(manifest.http_bindings.is_empty());
        assert!(manifest.config_schema.is_none());
        assert!(manifest.web_ui.is_none());
        assert!(manifest.mesh_channels.is_empty());
        assert!(manifest.mesh_event_subscriptions.is_empty());
        // `endpoints` carries inference endpoints and attached external MCP
        // servers; this plugin declares neither.
        assert!(manifest.endpoints.is_empty());
        assert!(manifest.resources.is_empty());
        assert!(manifest.prompts.is_empty());
        assert_eq!(manifest.capabilities, vec!["docker-inspect.v1".to_string()]);
    }
}
