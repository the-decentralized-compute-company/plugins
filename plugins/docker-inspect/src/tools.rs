//! The tool handlers, and the shapes they hand back to a model.
//!
//! Three rules shape every response here:
//!
//! * **A bounded answer says it is bounded.** Every list reports how many
//!   entries it returned, how many the filter hid, and whether a cap trimmed
//!   it. A model that silently receives 200 of 4,000 containers will confidently
//!   report the wrong total.
//! * **A view that is filtered says so.** `hidden_by_filter` appears even when
//!   it is zero, so a caller never has to guess whether it is seeing the whole
//!   machine.
//! * **Failure is an error, never an empty success.** An unreachable daemon and
//!   a machine with no containers look identical if both return `[]`, and the
//!   difference is the entire value of the tool.
//!
//! Every handler that takes a container reference resolves it against the
//! *visible* listing first, so a hidden container cannot be reached even by its
//! full id, and a caller's string never becomes part of a request path.

use std::time::{SystemTime, UNIX_EPOCH};

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use tdcc_plugin::{PluginError, PluginResult};

use crate::api::{ApiError, Docker};
use crate::logs::{self, LogOptions};
use crate::model::{ContainerSummary, EnvMode, ImageSummary};
use crate::settings::{PLUGIN_NAME, PLUGIN_VERSION, Settings};
use crate::stats;
use crate::visibility::{ResolveError, resolve};

/// The sentence that travels with every log response.
///
/// It is repeated in the tool description and in the README, and it is here as
/// well because a response can outlive both: whatever reads this output should
/// see the warning attached to the thing it is warning about.
pub const LOG_WARNING: &str = "Container logs frequently contain credentials, tokens, personal data, and internal \
     hostnames. These lines were read from a container on the operator's own machine and handed \
     to a model. Treat them as sensitive.";

// ---------------------------------------------------------------------------
// Tool arguments
//
// The doc comment on each field becomes its description in the JSON Schema the
// host publishes, so these are written for the model that will read them.
// `deny_unknown_fields` keeps anything else from riding along in a call.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NoArgs {}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListContainersArgs {
    /// Include containers that are not running. Off by default, which matches
    /// `docker ps` and answers "what is running here" without the history.
    #[serde(default)]
    pub all: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContainerArgs {
    /// Which container, as a name or an id from `list_containers`. A short id
    /// prefix works, exactly as it does with the `docker` command. Containers
    /// the operator did not make visible cannot be named here.
    pub container: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LogsArgs {
    /// Which container, as a name or an id from `list_containers`.
    pub container: String,

    /// How many of the most recent lines to return. Clamped to the operator's
    /// line cap, which is reported by `status`; omit it to use that cap.
    #[serde(default)]
    pub tail: Option<u32>,

    /// Only return lines written in the last this-many seconds. Useful for
    /// "what happened just now" without pulling the whole tail.
    #[serde(default)]
    pub since_seconds: Option<u64>,

    /// Prefix each line with the timestamp the daemon recorded for it. Off by
    /// default because it costs roughly thirty characters per line.
    #[serde(default)]
    pub timestamps: Option<bool>,
}

/// Everything the tools need: the configuration and one client for it.
#[derive(Clone, Debug)]
pub struct Inspector {
    settings: Settings,
    docker: Docker,
}

impl Inspector {
    pub fn new(settings: Settings) -> Self {
        let docker = Docker::new(&settings);
        Self { settings, docker }
    }

    /// A one-line status for the host's health check.
    ///
    /// Deliberately local: health has to stay fast and independent of anything
    /// that can block, and the Docker socket is exactly the kind of thing that
    /// can. Whether the daemon answers is what the `daemon` tool is for.
    pub fn health(&self) -> String {
        self.settings.summary()
    }

    /// What this plugin is configured as. Contacts nothing.
    pub fn status(&self) -> Value {
        let settings = &self.settings;
        json!({
            "plugin": PLUGIN_NAME,
            "version": PLUGIN_VERSION,
            "read_only": true,
            "endpoint": {
                "value": settings.endpoint.to_string(),
                "transport": settings.endpoint.kind(),
                "configured_by": settings.endpoint_source,
                "over_network": settings.endpoint.is_network(),
                "api_version": settings.api_version,
            },
            "visibility": {
                "filtered": settings.visibility.is_filtered(),
                "shows": settings.visibility.describe(),
                "name_patterns": settings.visibility.names.iter()
                    .map(|pattern| pattern.as_str().to_string()).collect::<Vec<String>>(),
                "label_selectors": settings.visibility.labels.iter()
                    .map(|selector| selector.describe()).collect::<Vec<String>>(),
            },
            "logs": {
                "enabled": settings.logs.enabled,
                "max_lines": settings.logs.max_lines,
                "max_bytes": settings.logs.max_bytes,
                "max_line_chars": settings.logs.max_line_chars,
                "warning": LOG_WARNING,
            },
            "limits": {
                "max_containers_listed": settings.max_containers,
                "max_images_listed": settings.max_images,
                "max_labels_per_container": settings.max_labels,
                "max_response_bytes": settings.max_response_bytes,
                "timeout_seconds": settings.timeout.as_secs(),
            },
            "environment_variables": if settings.show_env {
                "values shown (--show-env)"
            } else {
                "names only; values hidden"
            },
            "tools": ["status", "daemon", "list_containers", "inspect_container",
                      "container_logs", "container_stats", "list_images"],
            "note": "This tool contacts nothing. Call `daemon` to find out whether the Docker \
                     endpoint actually answers. docker-inspect can only read: it has no tool that \
                     creates, starts, stops, executes in, or removes anything.",
        })
    }

    /// Probe the daemon: reachability, versions, and what it reports about the
    /// host. This is the tool that fails when the socket is wrong.
    pub async fn daemon(&self) -> PluginResult<Value> {
        self.docker.ping().await.map_err(to_plugin_error)?;
        let version = self.docker.version().await.map_err(to_plugin_error)?;
        let info = self.docker.info().await.map_err(to_plugin_error)?;

        Ok(json!({
            "reachable": true,
            "endpoint": self.settings.endpoint.to_string(),
            "requested_api_version": self.settings.api_version,
            "daemon": version.to_json(),
            "host": info.to_json(),
            "note": "Counts under `host` are daemon-wide and are not affected by this plugin's \
                     visibility filter.",
        }))
    }

    /// The container list, filtered, capped, and honest about both.
    pub async fn list_containers(&self, args: ListContainersArgs) -> PluginResult<Value> {
        let include_stopped = args.all.unwrap_or(false);
        let (visible, hidden) = self.visible_containers().await?;

        let selected: Vec<&ContainerSummary> = visible
            .iter()
            .filter(|container| include_stopped || container.is_running())
            .collect();
        let total = selected.len();
        let returned: Vec<Value> = selected
            .iter()
            .take(self.settings.max_containers)
            .map(|container| container.to_json(self.settings.max_labels))
            .collect();

        Ok(json!({
            "returned": returned.len(),
            "matching": total,
            "truncated": total > returned.len(),
            "hidden_by_filter": hidden,
            "filter": self.settings.visibility.describe(),
            "includes_stopped": include_stopped,
            "results": returned,
        }))
    }

    /// One container in detail, with its environment redacted by default.
    pub async fn inspect_container(&self, args: ContainerArgs) -> PluginResult<Value> {
        let container = self.resolve_reference(&args.container).await?;
        let inspect = self
            .docker
            .inspect(&container.id)
            .await
            .map_err(to_plugin_error)?;

        let env_mode = if self.settings.show_env {
            EnvMode::Full
        } else {
            EnvMode::NamesOnly
        };
        Ok(inspect.to_json(env_mode, self.settings.max_labels))
    }

    /// A bounded tail of one container's logs.
    pub async fn container_logs(&self, args: LogsArgs) -> PluginResult<Value> {
        if !self.settings.logs.enabled {
            return Err(PluginError::invalid_request(format!(
                "reading container logs is turned off on this node: docker-inspect was started \
                 with `--no-logs` (or {}=false). Every other tool still works.",
                crate::settings::ENV_LOGS
            )));
        }

        let container = self.resolve_reference(&args.container).await?;
        // The log stream is framed unless the container was started with a TTY,
        // and inspect is the only place that says which. Guessing produces
        // eight bytes of binary at the start of every line.
        let inspect = self
            .docker
            .inspect(&container.id)
            .await
            .map_err(to_plugin_error)?;

        let limits = self.settings.logs;
        let requested = args.tail.unwrap_or(limits.max_lines as u32) as usize;
        let tail = requested.clamp(1, limits.max_lines);
        let timestamps = args.timestamps.unwrap_or(false);
        let since = args
            .since_seconds
            .map(|seconds| since_unix(now_unix(), seconds));

        let (body, byte_capped) = self
            .docker
            .logs(&container.id, tail, timestamps, since)
            .await
            .map_err(to_plugin_error)?;

        let output = logs::assemble(
            &body,
            inspect.config.tty,
            &LogOptions {
                max_lines: limits.max_lines,
                max_line_chars: limits.max_line_chars,
                timestamps,
            },
        );

        Ok(json!({
            "container": {
                "id": container.short_id(),
                "name": container.primary_name(),
                "state": container.state,
            },
            "lines": output.lines,
            "returned_lines": output.lines.len(),
            "tail_used": tail,
            "max_lines": limits.max_lines,
            "dropped_older_lines": output.dropped_leading_lines,
            "lines_cut_to_length": output.truncated_lines,
            "byte_cap_reached": byte_capped,
            "since_unix": since,
            "warning": LOG_WARNING,
            "note": if byte_capped {
                Some(format!(
                    "The log read stopped at the {} byte cap, so the newest lines may be missing. \
                     Ask for a smaller `tail`, or raise --max-log-bytes.",
                    limits.max_bytes
                ))
            } else {
                None
            },
        }))
    }

    /// One resource sample for a running container.
    pub async fn container_stats(&self, args: ContainerArgs) -> PluginResult<Value> {
        let container = self.resolve_reference(&args.container).await?;
        if !container.is_running() {
            // The daemon answers a stats request for a stopped container with a
            // page of zeroes, which reads exactly like an idle container.
            return Err(PluginError::invalid_request(format!(
                "`{}` is {}, not running, so there are no resource statistics to sample. Use \
                 `inspect_container` for its configuration and last exit state.",
                container.primary_name(),
                container.state
            )));
        }

        let sample = self
            .docker
            .stats(&container.id)
            .await
            .map_err(to_plugin_error)?;

        let mut rendered = stats::to_json(&sample);
        rendered["container"] = json!({
            "id": container.short_id(),
            "name": container.primary_name(),
            "image": container.image,
        });
        Ok(rendered)
    }

    /// The local image list, scoped to what the filter allows.
    pub async fn list_images(&self, _args: NoArgs) -> PluginResult<Value> {
        let (visible, hidden) = self.visible_containers().await?;
        let images = self.docker.images().await.map_err(to_plugin_error)?;

        let scoped = self.settings.visibility.is_filtered() && !self.settings.all_images;
        let selected: Vec<&ImageSummary> = images
            .iter()
            .filter(|image| !scoped || visible.iter().any(|container| image.is_used_by(container)))
            .collect();

        let total = selected.len();
        let returned: Vec<Value> = selected
            .iter()
            .take(self.settings.max_images)
            .map(|image| {
                let used_by: Vec<String> = visible
                    .iter()
                    .filter(|container| image.is_used_by(container))
                    .map(ContainerSummary::primary_name)
                    .collect();
                image.to_json(used_by)
            })
            .collect();

        Ok(json!({
            "returned": returned.len(),
            "matching": total,
            "truncated": total > returned.len(),
            "scope": if scoped {
                "images used by containers this plugin may show"
            } else {
                "every image on this machine"
            },
            "hidden_containers": hidden,
            "results": returned,
            "note": if scoped {
                Some("A container filter is configured, so this list is limited to the images \
                      those containers use. Start docker-inspect with --all-images to list every \
                      image on the machine.")
            } else {
                None
            },
        }))
    }

    /// The visible containers and how many the filter hid.
    async fn visible_containers(&self) -> PluginResult<(Vec<ContainerSummary>, usize)> {
        // Always asked for with `all=1`: a stopped container is still something
        // an operator asks about, and the caller decides what to show.
        let containers = self
            .docker
            .containers(true)
            .await
            .map_err(to_plugin_error)?;
        Ok(self.settings.visibility.apply(containers))
    }

    /// Turn a caller's reference into a container the operator allowed.
    async fn resolve_reference(&self, reference: &str) -> PluginResult<ContainerSummary> {
        let (visible, _) = self.visible_containers().await?;
        match resolve(reference, &visible) {
            Ok(container) => Ok(container.clone()),
            Err(ResolveError::Malformed(message)) => Err(PluginError::invalid_params(message)),
            Err(ResolveError::NotFound) => Err(PluginError::invalid_request(format!(
                "no container matching `{reference}` is visible to docker-inspect. Call \
                 `list_containers` with `all: true` to see what is. {}",
                if self.settings.visibility.is_filtered() {
                    format!(
                        "This node's operator restricted this plugin to {}, so containers outside \
                         that are not reachable through it at all.",
                        self.settings.visibility.describe()
                    )
                } else {
                    "The container may have been removed since the last listing.".to_string()
                }
            ))),
            Err(ResolveError::Ambiguous(candidates)) => Err(PluginError::invalid_params(format!(
                "`{reference}` matches more than one container ({}). Name one exactly, or use a \
                 longer id prefix.",
                candidates.join(", ")
            ))),
        }
    }
}

/// Map an API failure onto the right JSON-RPC error class.
///
/// A container that does not exist is the caller's mistake; an unreachable
/// socket is the node's. Both are errors — neither is an empty result.
fn to_plugin_error(error: ApiError) -> PluginError {
    if error.is_caller_error() {
        PluginError::invalid_request(error.to_string())
    } else {
        PluginError::internal(error.to_string())
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or_default()
}

/// `since_seconds` counted back from now, saturating at the epoch so a
/// ridiculous value asks for everything rather than wrapping into the future.
fn since_unix(now: u64, seconds_ago: u64) -> u64 {
    now.saturating_sub(seconds_ago)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::frame;
    use crate::testsupport::{StubDaemon, ok};

    /// Two containers, one of which a filter will hide.
    const TWO_CONTAINERS: &str = r#"[
        {"Id":"1111111111111111111111111111111111111111111111111111111111111111",
         "Names":["/tdcc-node"],"Image":"tdcc:latest","ImageID":"sha256:aaaa",
         "State":"running","Status":"Up 2 hours","Labels":{"role":"mesh"},
         "Created":1700000000,"Ports":[{"IP":"0.0.0.0","PrivatePort":9337,"PublicPort":9337,"Type":"tcp"}]},
        {"Id":"2222222222222222222222222222222222222222222222222222222222222222",
         "Names":["/billing-db"],"Image":"postgres:16","ImageID":"sha256:bbbb",
         "State":"exited","Status":"Exited (0) 3 days ago","Labels":null,
         "Created":1700000000,"Ports":null}
    ]"#;

    fn inspector(daemon: &StubDaemon, extra: &[&str]) -> Inspector {
        Inspector::new(daemon.settings(extra))
    }

    #[tokio::test]
    async fn status_reports_the_configuration_without_contacting_anything() {
        let daemon = StubDaemon::spawn(Vec::new());

        let status = inspector(&daemon, &["--container", "tdcc-*"]).status();

        assert_eq!(status["read_only"], json!(true));
        assert_eq!(status["visibility"]["filtered"], json!(true));
        assert_eq!(status["visibility"]["name_patterns"], json!(["tdcc-*"]));
        assert!(daemon.requests().is_empty(), "status must make no request");
    }

    #[tokio::test]
    async fn listing_shows_running_containers_by_default_and_all_on_request() {
        let daemon = StubDaemon::spawn(vec![ok(TWO_CONTAINERS), ok(TWO_CONTAINERS)]);
        let inspector = inspector(&daemon, &[]);

        let running = inspector
            .list_containers(ListContainersArgs { all: None })
            .await
            .expect("the stub answers");
        let everything = inspector
            .list_containers(ListContainersArgs { all: Some(true) })
            .await
            .expect("the stub answers");

        assert_eq!(running["returned"], json!(1));
        assert_eq!(running["results"][0]["name"], json!("tdcc-node"));
        assert_eq!(running["hidden_by_filter"], json!(0));
        assert_eq!(everything["returned"], json!(2));
        assert_eq!(everything["includes_stopped"], json!(true));
    }

    #[tokio::test]
    async fn a_published_port_and_the_created_time_reach_the_caller() {
        let daemon = StubDaemon::spawn(vec![ok(TWO_CONTAINERS)]);

        let listing = inspector(&daemon, &[])
            .list_containers(ListContainersArgs { all: None })
            .await
            .expect("the stub answers");

        let port = &listing["results"][0]["ports"][0];
        assert_eq!(port["host_port"], json!(9337));
        assert_eq!(port["published_to_all_interfaces"], json!(true));
        assert_eq!(
            listing["results"][0]["created"],
            json!("2023-11-14T22:13:20Z")
        );
    }

    #[tokio::test]
    async fn a_filter_hides_containers_and_the_response_says_how_many() {
        let daemon = StubDaemon::spawn(vec![ok(TWO_CONTAINERS)]);

        let listing = inspector(&daemon, &["--container", "tdcc-*"])
            .list_containers(ListContainersArgs { all: Some(true) })
            .await
            .expect("the stub answers");

        assert_eq!(listing["returned"], json!(1));
        assert_eq!(listing["hidden_by_filter"], json!(1));
        assert_eq!(listing["filter"], json!("names matching tdcc-*"));
        assert!(!listing.to_string().contains("billing-db"));
    }

    #[tokio::test]
    async fn a_hidden_container_cannot_be_inspected_even_by_its_full_id() {
        let daemon = StubDaemon::spawn(vec![ok(TWO_CONTAINERS)]);

        let error = inspector(&daemon, &["--container", "tdcc-*"])
            .inspect_container(ContainerArgs {
                container: "2".repeat(64),
            })
            .await
            .expect_err("a hidden container is out of reach");

        assert!(error.to_string().contains("not reachable"), "{error}");
        assert_eq!(
            daemon.requests().len(),
            1,
            "only the listing was requested; no inspect was sent"
        );
    }

    #[tokio::test]
    async fn inspecting_a_container_redacts_its_environment_by_default() {
        let inspect = r#"{
            "Id":"1111111111111111111111111111111111111111111111111111111111111111",
            "Name":"/tdcc-node","Created":"2023-11-14T22:13:20Z",
            "State":{"Status":"running","Running":true},
            "Config":{"Image":"tdcc:latest","Tty":false,
                      "Env":["PATH=/usr/bin","POSTGRES_PASSWORD=hunter2"],
                      "Cmd":["serve","--token","abcd"]},
            "HostConfig":{"Privileged":true,"NetworkMode":"bridge"},
            "Mounts":[]
        }"#;
        let daemon = StubDaemon::spawn(vec![ok(TWO_CONTAINERS), ok(inspect)]);

        let rendered = inspector(&daemon, &[])
            .inspect_container(ContainerArgs {
                container: "tdcc-node".into(),
            })
            .await
            .expect("the stub answers");

        let text = rendered.to_string();
        assert!(!text.contains("hunter2"), "{text}");
        assert!(!text.contains("abcd"), "{text}");
        assert_eq!(rendered["env"]["redacted"], json!(true));
        assert_eq!(rendered["config"]["command_redacted"], json!(true));
        assert!(
            rendered["security_notes"]
                .as_array()
                .expect("notes are a list")
                .iter()
                .any(|note| note.as_str().unwrap_or_default().contains("privileged"))
        );
    }

    #[tokio::test]
    async fn show_env_reveals_the_values_it_was_asked_to_reveal() {
        let inspect = r#"{"Id":"11","Name":"/tdcc-node","Config":{"Env":["TOKEN=abcd"]}}"#;
        let daemon = StubDaemon::spawn(vec![ok(TWO_CONTAINERS), ok(inspect)]);

        let rendered = inspector(&daemon, &["--show-env"])
            .inspect_container(ContainerArgs {
                container: "tdcc-node".into(),
            })
            .await
            .expect("the stub answers");

        assert_eq!(rendered["env"]["values"]["TOKEN"], json!("abcd"));
    }

    #[tokio::test]
    async fn logs_are_capped_labelled_and_carry_the_warning() {
        let body: String = (0..40)
            .map(|index| format!("line {index}\n"))
            .collect::<Vec<String>>()
            .concat();
        let framed = frame(1, &body);
        let inspect = r#"{"Id":"11","Name":"/tdcc-node","Config":{"Tty":false}}"#;
        let daemon = StubDaemon::spawn(vec![
            ok(TWO_CONTAINERS),
            ok(inspect),
            ok(&String::from_utf8_lossy(&framed)),
        ]);

        let rendered = inspector(&daemon, &["--max-log-lines", "5"])
            .container_logs(LogsArgs {
                container: "tdcc-node".into(),
                tail: Some(1000),
                since_seconds: None,
                timestamps: None,
            })
            .await
            .expect("the stub answers");

        assert_eq!(rendered["returned_lines"], json!(5));
        assert_eq!(rendered["tail_used"], json!(5), "clamped to the cap");
        assert_eq!(rendered["dropped_older_lines"], json!(35));
        assert_eq!(rendered["lines"][4]["text"], json!("line 39"));
        assert_eq!(rendered["lines"][0]["stream"], json!("stdout"));
        assert!(
            rendered["warning"]
                .as_str()
                .expect("every log response carries the warning")
                .contains("credentials")
        );
    }

    #[tokio::test]
    async fn logs_refuse_with_a_named_setting_when_the_operator_turned_them_off() {
        let daemon = StubDaemon::spawn(Vec::new());

        let error = inspector(&daemon, &["--no-logs"])
            .container_logs(LogsArgs {
                container: "tdcc-node".into(),
                tail: None,
                since_seconds: None,
                timestamps: None,
            })
            .await
            .expect_err("logs are disabled");

        assert!(error.to_string().contains("--no-logs"), "{error}");
        assert!(
            daemon.requests().is_empty(),
            "nothing was asked of the daemon"
        );
    }

    #[tokio::test]
    async fn stats_for_a_stopped_container_refuse_instead_of_reporting_zeroes() {
        let daemon = StubDaemon::spawn(vec![ok(TWO_CONTAINERS)]);

        let error = inspector(&daemon, &[])
            .container_stats(ContainerArgs {
                container: "billing-db".into(),
            })
            .await
            .expect_err("a stopped container has no statistics");

        assert!(error.to_string().contains("not running"), "{error}");
        assert_eq!(
            daemon.requests().len(),
            1,
            "no stats request was sent for a stopped container"
        );
    }

    #[tokio::test]
    async fn stats_report_the_container_alongside_the_sample() {
        let sample = r#"{
            "read":"2024-05-01T10:00:00Z",
            "cpu_stats":{"cpu_usage":{"total_usage":2000000000},"system_cpu_usage":40000000000,"online_cpus":4},
            "precpu_stats":{"cpu_usage":{"total_usage":1000000000},"system_cpu_usage":20000000000},
            "memory_stats":{"usage":209715200,"limit":1073741824,"stats":{"inactive_file":104857600}}
        }"#;
        let daemon = StubDaemon::spawn(vec![ok(TWO_CONTAINERS), ok(sample)]);

        let rendered = inspector(&daemon, &[])
            .container_stats(ContainerArgs {
                container: "tdcc-node".into(),
            })
            .await
            .expect("the stub answers");

        assert_eq!(rendered["container"]["name"], json!("tdcc-node"));
        assert_eq!(rendered["cpu"]["percent"], json!(20.0));
        assert_eq!(rendered["memory"]["usage"], json!("100.0 MiB"));
    }

    #[tokio::test]
    async fn images_are_scoped_to_visible_containers_when_a_filter_is_configured() {
        let images = r#"[
            {"Id":"sha256:aaaa","RepoTags":["tdcc:latest"],"Size":104857600,"Created":1700000000},
            {"Id":"sha256:bbbb","RepoTags":["postgres:16"],"Size":419430400,"Created":1700000000}
        ]"#;
        let daemon = StubDaemon::spawn(vec![ok(TWO_CONTAINERS), ok(images)]);

        let rendered = inspector(&daemon, &["--container", "tdcc-*"])
            .list_images(NoArgs {})
            .await
            .expect("the stub answers");

        assert_eq!(rendered["returned"], json!(1));
        assert_eq!(rendered["results"][0]["tags"], json!(["tdcc:latest"]));
        assert_eq!(
            rendered["results"][0]["used_by_visible_containers"],
            json!(["tdcc-node"])
        );
        assert!(rendered["scope"].as_str().unwrap().contains("may show"));
    }

    #[tokio::test]
    async fn all_images_widens_the_list_back_to_the_whole_machine() {
        let images = r#"[
            {"Id":"sha256:aaaa","RepoTags":["tdcc:latest"],"Size":1,"Created":1700000000},
            {"Id":"sha256:bbbb","RepoTags":["postgres:16"],"Size":2,"Created":1700000000}
        ]"#;
        let daemon = StubDaemon::spawn(vec![ok(TWO_CONTAINERS), ok(images)]);

        let rendered = inspector(&daemon, &["--container", "tdcc-*", "--all-images"])
            .list_images(NoArgs {})
            .await
            .expect("the stub answers");

        assert_eq!(rendered["returned"], json!(2));
        assert_eq!(rendered["note"], json!(null));
    }

    #[tokio::test]
    async fn an_unreachable_daemon_is_an_error_rather_than_an_empty_list() {
        // A stub with no queued responses closes as soon as it has served its
        // (empty) queue, so the connection is refused.
        let daemon = StubDaemon::spawn(Vec::new());

        let error = inspector(&daemon, &[])
            .list_containers(ListContainersArgs { all: None })
            .await
            .expect_err("an unreachable daemon must not look like an empty machine");

        assert!(error.to_string().contains("Docker daemon"), "{error}");
    }

    #[tokio::test]
    async fn an_ambiguous_reference_lists_the_candidates() {
        let containers = r#"[
            {"Id":"ab11111111111111111111111111111111111111111111111111111111111111",
             "Names":["/one"],"State":"running"},
            {"Id":"ab22222222222222222222222222222222222222222222222222222222222222",
             "Names":["/two"],"State":"running"}
        ]"#;
        let daemon = StubDaemon::spawn(vec![ok(containers)]);

        let error = inspector(&daemon, &[])
            .inspect_container(ContainerArgs {
                container: "ab".into(),
            })
            .await
            .expect_err("an ambiguous prefix must not be guessed at");

        assert!(error.to_string().contains("one"), "{error}");
        assert!(error.to_string().contains("two"), "{error}");
    }

    #[test]
    fn a_since_window_counts_back_from_now_and_cannot_wrap() {
        assert_eq!(since_unix(1_700_000_000, 3_600), 1_699_996_400);
        assert_eq!(since_unix(100, u64::MAX), 0);
    }

    /// Drive every tool against the Docker daemon on this machine.
    ///
    /// Ignored by default because it needs a running daemon and reads real
    /// containers — the stub tests above pin the behaviour, and this proves the
    /// same code reaches a real one over the platform's own local transport (a
    /// Unix socket, or the named pipe on Windows), which no stub can cover.
    ///
    /// ```text
    /// cargo test -- --ignored --nocapture
    /// ```
    #[tokio::test]
    #[ignore = "needs a running Docker daemon on this machine"]
    async fn every_tool_answers_against_the_real_local_daemon() {
        use crate::settings::{EnvMap, Settings};

        let settings = Settings::parse(&[], &EnvMap::new()).expect("defaults parse");
        println!("endpoint: {}", settings.endpoint);
        let inspector = Inspector::new(settings);

        let daemon = inspector.daemon().await.expect("the daemon answers");
        println!("daemon: {}", serde_json::to_string_pretty(&daemon).unwrap());
        assert_eq!(daemon["reachable"], json!(true));

        let listing = inspector
            .list_containers(ListContainersArgs { all: Some(true) })
            .await
            .expect("the daemon lists containers");
        println!(
            "containers: {}",
            serde_json::to_string_pretty(&listing).unwrap()
        );

        let images = inspector
            .list_images(NoArgs {})
            .await
            .expect("the daemon lists images");
        println!("images: {}", images["returned"]);

        let Some(first) = listing["results"].as_array().and_then(|all| all.first()) else {
            println!("no containers on this machine; skipping the per-container tools");
            return;
        };
        let name = first["name"].as_str().expect("a name").to_string();

        let inspected = inspector
            .inspect_container(ContainerArgs {
                container: name.clone(),
            })
            .await
            .expect("the daemon inspects a container");
        println!(
            "inspect {name}: {}",
            serde_json::to_string_pretty(&inspected).unwrap()
        );
        assert_eq!(inspected["env"]["redacted"], json!(true));

        let logs = inspector
            .container_logs(LogsArgs {
                container: name.clone(),
                tail: Some(5),
                since_seconds: None,
                timestamps: Some(true),
            })
            .await
            .expect("the daemon returns logs");
        println!(
            "logs {name}: {}",
            serde_json::to_string_pretty(&logs).unwrap()
        );

        match inspector
            .container_stats(ContainerArgs {
                container: name.clone(),
            })
            .await
        {
            Ok(sample) => println!(
                "stats {name}: {}",
                serde_json::to_string_pretty(&sample).unwrap()
            ),
            // A stopped container refuses, on purpose. Print it rather than
            // failing: which containers exist here is not this test's business.
            Err(error) => println!("stats {name}: refused — {error}"),
        }
    }
}
