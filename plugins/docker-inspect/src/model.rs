//! What the Docker Engine API returns, and what this plugin passes on.
//!
//! Every type here is a *subset*. Docker's inspect payload is tens of
//! kilobytes of nested configuration, most of which answers no question anybody
//! asks and some of which is a credential; a model paying context for all of it
//! is worse off than one reading the twenty fields that matter. Unknown fields
//! are ignored by serde, so a newer daemon adding to a payload does not break
//! an older build of this plugin.
//!
//! Two of those omissions are load-bearing rather than tidy:
//!
//! * **`Config.Env` values are redacted by default.** Environment variables are
//!   where container credentials live — `POSTGRES_PASSWORD`, `AWS_SECRET_ACCESS_KEY`,
//!   a `DATABASE_URL` with the password inline. The names are reported because
//!   they answer "is this configured at all"; the values need `--show-env`.
//! * **Command lines are filtered for secret-shaped arguments.** A best-effort
//!   filter, and it says so: it catches `--password=x`, `--token x`, and a URL
//!   with inline credentials, and it will not catch a secret passed
//!   positionally. See [`redact_arguments`].

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};

/// Deserialize a field that the daemon may send as `null`.
///
/// `#[serde(default)]` covers a *missing* field; Docker sends an explicit
/// `"Labels": null` for a container with no labels, which is a different thing
/// and would otherwise fail the whole response.
fn nullable<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

// ---------------------------------------------------------------------------
// GET /containers/json
// ---------------------------------------------------------------------------

/// One entry of the container list.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct ContainerSummary {
    pub id: String,
    /// As the API reports them, each with a leading `/`. Use [`Self::names`].
    #[serde(deserialize_with = "nullable")]
    pub names: Vec<String>,
    pub image: String,
    #[serde(rename = "ImageID")]
    pub image_id: String,
    pub command: String,
    /// Unix seconds.
    pub created: i64,
    /// `running`, `exited`, `paused`, `created`, `restarting`, `dead`.
    pub state: String,
    /// Human phrasing of the same thing: `Up 3 hours`, `Exited (0) 2 days ago`.
    pub status: String,
    #[serde(deserialize_with = "nullable")]
    pub labels: BTreeMap<String, String>,
    #[serde(deserialize_with = "nullable")]
    pub ports: Vec<Port>,
    #[serde(deserialize_with = "nullable")]
    pub network_settings: SummaryNetworkSettings,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct SummaryNetworkSettings {
    #[serde(deserialize_with = "nullable")]
    pub networks: BTreeMap<String, NetworkAttachment>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct NetworkAttachment {
    #[serde(rename = "IPAddress")]
    pub ip_address: String,
    pub gateway: String,
    pub mac_address: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Port {
    #[serde(rename = "IP")]
    pub ip: String,
    pub private_port: u16,
    pub public_port: Option<u16>,
    #[serde(rename = "Type")]
    pub protocol: String,
}

impl Port {
    /// Whether the daemon published this port on every interface, which is the
    /// difference between "reachable from this machine" and "reachable from the
    /// network". Worth surfacing: it is the most common accidental exposure.
    pub fn published_to_all_interfaces(&self) -> bool {
        self.public_port.is_some() && matches!(self.ip.as_str(), "" | "0.0.0.0" | "::")
    }

    pub fn to_json(&self) -> Value {
        json!({
            "container_port": self.private_port,
            "protocol": if self.protocol.is_empty() { "tcp" } else { &self.protocol },
            "host_port": self.public_port,
            "host_ip": if self.ip.is_empty() { None } else { Some(self.ip.clone()) },
            "published_to_all_interfaces": self.published_to_all_interfaces(),
        })
    }
}

impl ContainerSummary {
    /// Names without the leading `/` the API adds.
    pub fn names(&self) -> Vec<String> {
        self.names
            .iter()
            .map(|name| name.trim_start_matches('/').to_string())
            .collect()
    }

    pub fn primary_name(&self) -> String {
        self.names()
            .first()
            .cloned()
            .unwrap_or_else(|| self.short_id())
    }

    /// The first 12 characters of the id, which is what `docker ps` prints and
    /// what every tool here accepts back as a reference.
    pub fn short_id(&self) -> String {
        self.id.chars().take(12).collect()
    }

    pub fn is_running(&self) -> bool {
        self.state == "running"
    }

    pub fn to_json(&self, max_labels: usize) -> Value {
        let (labels, labels_truncated) = cap_labels(&self.labels, max_labels);
        let networks: Vec<Value> = self
            .network_settings
            .networks
            .iter()
            .map(|(name, attachment)| {
                json!({
                    "name": name,
                    "ip_address": empty_as_null(&attachment.ip_address),
                })
            })
            .collect();

        json!({
            "id": self.short_id(),
            "name": self.primary_name(),
            "all_names": self.names(),
            "image": self.image,
            "state": self.state,
            "status": self.status,
            "created": unix_to_rfc3339(self.created),
            "ports": self.ports.iter().map(Port::to_json).collect::<Vec<Value>>(),
            "networks": networks,
            "labels": labels,
            "labels_truncated": labels_truncated,
        })
    }
}

// ---------------------------------------------------------------------------
// GET /containers/{id}/json
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct ContainerInspect {
    pub id: String,
    pub name: String,
    /// RFC 3339, as the daemon writes it.
    pub created: String,
    pub image: String,
    pub platform: String,
    pub restart_count: i64,
    pub state: InspectState,
    pub config: InspectConfig,
    pub host_config: InspectHostConfig,
    #[serde(deserialize_with = "nullable")]
    pub mounts: Vec<Mount>,
    #[serde(deserialize_with = "nullable")]
    pub network_settings: InspectNetworkSettings,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct InspectState {
    pub status: String,
    pub running: bool,
    pub paused: bool,
    pub restarting: bool,
    #[serde(rename = "OOMKilled")]
    pub oom_killed: bool,
    pub dead: bool,
    pub pid: i64,
    pub exit_code: i64,
    pub error: String,
    pub started_at: String,
    pub finished_at: String,
    pub health: Option<Health>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Health {
    pub status: String,
    pub failing_streak: i64,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct InspectConfig {
    pub hostname: String,
    pub user: String,
    pub tty: bool,
    pub working_dir: String,
    pub image: String,
    #[serde(deserialize_with = "nullable")]
    pub env: Vec<String>,
    #[serde(deserialize_with = "nullable")]
    pub cmd: Vec<String>,
    #[serde(deserialize_with = "nullable")]
    pub entrypoint: Vec<String>,
    #[serde(deserialize_with = "nullable")]
    pub labels: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct InspectHostConfig {
    pub network_mode: String,
    pub pid_mode: String,
    pub ipc_mode: String,
    pub privileged: bool,
    pub readonly_rootfs: bool,
    /// Bytes. `0` means unlimited.
    pub memory: i64,
    /// Billionths of a CPU. `0` means unlimited.
    pub nano_cpus: i64,
    pub pids_limit: Option<i64>,
    #[serde(deserialize_with = "nullable")]
    pub cap_add: Vec<String>,
    #[serde(deserialize_with = "nullable")]
    pub cap_drop: Vec<String>,
    #[serde(deserialize_with = "nullable")]
    pub security_opt: Vec<String>,
    pub restart_policy: RestartPolicy,
    pub log_config: LogConfig,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct RestartPolicy {
    pub name: String,
    pub maximum_retry_count: i64,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct LogConfig {
    #[serde(rename = "Type")]
    pub driver: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Mount {
    #[serde(rename = "Type")]
    pub kind: String,
    pub name: String,
    /// A path on the host for a bind mount, a volume path for a volume.
    pub source: String,
    pub destination: String,
    pub mode: String,
    #[serde(rename = "RW")]
    pub writable: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct InspectNetworkSettings {
    #[serde(rename = "IPAddress")]
    pub ip_address: String,
    #[serde(deserialize_with = "nullable")]
    pub networks: BTreeMap<String, NetworkAttachment>,
}

/// How environment variables are reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvMode {
    /// Names only. The default.
    NamesOnly,
    /// Names and values, because the operator passed `--show-env`.
    Full,
}

impl ContainerInspect {
    pub fn short_id(&self) -> String {
        self.id.chars().take(12).collect()
    }

    pub fn container_name(&self) -> String {
        self.name.trim_start_matches('/').to_string()
    }

    /// The curated inspect payload.
    ///
    /// Bounded on purpose, and it says what it left out: `env` reports whether
    /// it is redacted, `labels_truncated` reports whether the label map was
    /// cut, and `command_redacted` reports whether an argument was masked.
    pub fn to_json(&self, env_mode: EnvMode, max_labels: usize) -> Value {
        let (labels, labels_truncated) = cap_labels(&self.config.labels, max_labels);
        let (cmd, cmd_redacted) = redact_arguments(&self.config.cmd);
        let (entrypoint, entrypoint_redacted) = redact_arguments(&self.config.entrypoint);

        let networks: Vec<Value> = self
            .network_settings
            .networks
            .iter()
            .map(|(name, attachment)| {
                json!({
                    "name": name,
                    "ip_address": empty_as_null(&attachment.ip_address),
                    "gateway": empty_as_null(&attachment.gateway),
                    "mac_address": empty_as_null(&attachment.mac_address),
                })
            })
            .collect();

        json!({
            "id": self.short_id(),
            "name": self.container_name(),
            "image": self.config.image,
            "image_id": self.image,
            "platform": empty_as_null(&self.platform),
            "created": self.created,
            "state": {
                "status": self.state.status,
                "running": self.state.running,
                "paused": self.state.paused,
                "restarting": self.state.restarting,
                "oom_killed": self.state.oom_killed,
                "dead": self.state.dead,
                "exit_code": self.state.exit_code,
                "error": empty_as_null(&self.state.error),
                "started_at": timestamp_or_null(&self.state.started_at),
                "finished_at": timestamp_or_null(&self.state.finished_at),
                "restart_count": self.restart_count,
                "health": self.state.health.as_ref().map(|health| json!({
                    "status": health.status,
                    "failing_streak": health.failing_streak,
                })),
            },
            "config": {
                "hostname": empty_as_null(&self.config.hostname),
                "user": empty_as_null(&self.config.user),
                "working_dir": empty_as_null(&self.config.working_dir),
                "tty": self.config.tty,
                "entrypoint": entrypoint,
                "command": cmd,
                "command_redacted": cmd_redacted || entrypoint_redacted,
                "log_driver": empty_as_null(&self.host_config.log_config.driver),
                "restart_policy": empty_as_null(&self.host_config.restart_policy.name),
            },
            "env": self.env_json(env_mode),
            "resources": {
                "memory_limit_bytes": positive(self.host_config.memory),
                "memory_limit": self.host_config.memory
                    .try_into().ok().filter(|bytes| *bytes > 0u64).map(format_bytes),
                "cpu_limit": cpu_limit(self.host_config.nano_cpus),
                "pids_limit": self.host_config.pids_limit.filter(|limit| *limit > 0),
            },
            "network": {
                "mode": empty_as_null(&self.host_config.network_mode),
                "ip_address": empty_as_null(&self.network_settings.ip_address),
                "networks": networks,
            },
            "mounts": self.mounts.iter().map(Mount::to_json).collect::<Vec<Value>>(),
            "labels": labels,
            "labels_truncated": labels_truncated,
            "security_notes": self.security_notes(),
        })
    }

    fn env_json(&self, mode: EnvMode) -> Value {
        let entries: Vec<(String, String)> = self
            .config
            .env
            .iter()
            .map(|entry| match entry.split_once('=') {
                Some((name, value)) => (name.to_string(), value.to_string()),
                None => (entry.clone(), String::new()),
            })
            .collect();

        match mode {
            EnvMode::NamesOnly => json!({
                "redacted": true,
                "count": entries.len(),
                "names": entries.into_iter().map(|(name, _)| name).collect::<Vec<String>>(),
                "note": "Values are hidden because container environments routinely hold \
                         credentials. Start docker-inspect with --show-env to include them.",
            }),
            EnvMode::Full => json!({
                "redacted": false,
                "count": entries.len(),
                "values": entries.into_iter().collect::<BTreeMap<String, String>>(),
            }),
        }
    }

    /// Facts about this container's own privileges, read straight out of the
    /// inspect payload.
    ///
    /// These are not a security audit. They are the handful of settings that
    /// change what a container could do to the machine it runs on, stated so an
    /// operator asking "what is running here" gets the answer that matters.
    pub fn security_notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        if self.host_config.privileged {
            notes.push(
                "runs privileged: it has effectively full access to the host's devices and kernel \
                 interfaces"
                    .to_string(),
            );
        }
        if let Some(mount) = self.mounts.iter().find(|mount| mount.is_docker_socket()) {
            notes.push(format!(
                "mounts the Docker socket at {}: a process in this container can create further \
                 containers and is therefore root-equivalent on this host",
                mount.destination
            ));
        }
        if self.host_config.network_mode == "host" {
            notes.push(
                "uses host networking: it shares this machine's network namespace, including \
                 loopback"
                    .to_string(),
            );
        }
        if self.host_config.pid_mode == "host" {
            notes.push(
                "uses the host PID namespace: it can see every process on this machine".to_string(),
            );
        }
        if !self.host_config.cap_add.is_empty() {
            notes.push(format!(
                "adds Linux capabilities: {}",
                self.host_config.cap_add.join(", ")
            ));
        }
        let sensitive: Vec<&Mount> = self
            .mounts
            .iter()
            .filter(|mount| mount.is_sensitive_host_write())
            .take(5)
            .collect();
        for mount in sensitive {
            notes.push(format!(
                "mounts host path {} read-write at {}",
                mount.source, mount.destination
            ));
        }
        notes
    }
}

impl Mount {
    /// Whether this mount hands the container the Docker socket, on either
    /// platform. Checked on both ends because a bind can be renamed inside the
    /// container but not on the host, and vice versa.
    pub fn is_docker_socket(&self) -> bool {
        let source = self.source.to_ascii_lowercase().replace('\\', "/");
        let destination = self.destination.to_ascii_lowercase().replace('\\', "/");
        [source, destination].iter().any(|path| {
            path.ends_with("docker.sock")
                || path.ends_with("docker_engine")
                || path.ends_with("docker.raw.sock")
        })
    }

    /// A writable bind of a host path where writes matter. Deliberately a short
    /// list rather than "any bind mount", which would fire on almost every
    /// container and stop being information.
    pub fn is_sensitive_host_write(&self) -> bool {
        if !self.writable || self.kind != "bind" {
            return false;
        }
        let source = self.source.replace('\\', "/");
        let lowered = source.to_ascii_lowercase();
        source == "/"
            || [
                "/etc", "/root", "/home", "/boot", "/proc", "/sys", "/dev", "/var/run", "/usr",
            ]
            .iter()
            .any(|root| source == *root || source.starts_with(&format!("{root}/")))
            || lowered.starts_with("c:/windows")
            || lowered.starts_with("c:/users")
    }

    pub fn to_json(&self) -> Value {
        json!({
            "type": self.kind,
            "source": empty_as_null(&self.source),
            "destination": self.destination,
            "writable": self.writable,
            "mode": empty_as_null(&self.mode),
        })
    }
}

// ---------------------------------------------------------------------------
// GET /images/json
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct ImageSummary {
    pub id: String,
    #[serde(deserialize_with = "nullable")]
    pub repo_tags: Vec<String>,
    #[serde(deserialize_with = "nullable")]
    pub repo_digests: Vec<String>,
    /// Unix seconds.
    pub created: i64,
    pub size: i64,
    /// `-1` unless the daemon was asked to compute it, which this plugin does
    /// not do — it is an expensive walk of every container's writable layer.
    pub containers: i64,
}

impl ImageSummary {
    /// `sha256:ab12…` shortened the way `docker images` shows it.
    pub fn short_id(&self) -> String {
        self.id
            .strip_prefix("sha256:")
            .unwrap_or(&self.id)
            .chars()
            .take(12)
            .collect()
    }

    /// Whether a container summary refers to this image, by id or by tag.
    pub fn is_used_by(&self, container: &ContainerSummary) -> bool {
        (!container.image_id.is_empty() && container.image_id == self.id)
            || self.repo_tags.contains(&container.image)
    }

    pub fn to_json(&self, used_by: Vec<String>) -> Value {
        json!({
            "id": self.short_id(),
            "tags": self.repo_tags,
            "digests": self.repo_digests.len(),
            "created": unix_to_rfc3339(self.created),
            "size_bytes": self.size,
            "size": u64::try_from(self.size).map(format_bytes).ok(),
            "used_by_visible_containers": used_by,
        })
    }
}

// ---------------------------------------------------------------------------
// GET /version and GET /info
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct DaemonVersion {
    pub version: String,
    pub api_version: String,
    #[serde(rename = "MinAPIVersion")]
    pub min_api_version: String,
    pub git_commit: String,
    pub go_version: String,
    pub os: String,
    pub arch: String,
    pub kernel_version: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct DaemonInfo {
    pub containers: i64,
    pub containers_running: i64,
    pub containers_paused: i64,
    pub containers_stopped: i64,
    pub images: i64,
    pub driver: String,
    pub mem_total: i64,
    #[serde(rename = "NCPU")]
    pub ncpu: i64,
    pub operating_system: String,
    #[serde(rename = "OSType")]
    pub os_type: String,
    pub architecture: String,
    pub kernel_version: String,
    pub server_version: String,
    pub name: String,
    pub docker_root_dir: String,
    pub cgroup_version: String,
    #[serde(deserialize_with = "nullable")]
    pub warnings: Vec<String>,
    #[serde(deserialize_with = "nullable")]
    pub security_options: Vec<String>,
}

impl DaemonVersion {
    pub fn to_json(&self) -> Value {
        json!({
            "version": self.version,
            "api_version": self.api_version,
            "min_api_version": empty_as_null(&self.min_api_version),
            "git_commit": empty_as_null(&self.git_commit),
            "go_version": empty_as_null(&self.go_version),
            "os": self.os,
            "arch": self.arch,
            "kernel_version": empty_as_null(&self.kernel_version),
        })
    }
}

impl DaemonInfo {
    pub fn to_json(&self) -> Value {
        json!({
            "name": empty_as_null(&self.name),
            "server_version": empty_as_null(&self.server_version),
            "operating_system": empty_as_null(&self.operating_system),
            "os_type": empty_as_null(&self.os_type),
            "architecture": empty_as_null(&self.architecture),
            "kernel_version": empty_as_null(&self.kernel_version),
            "storage_driver": empty_as_null(&self.driver),
            "cgroup_version": empty_as_null(&self.cgroup_version),
            "cpus": self.ncpu,
            "memory_bytes": self.mem_total,
            "memory": u64::try_from(self.mem_total).map(format_bytes).ok(),
            "docker_root_dir": empty_as_null(&self.docker_root_dir),
            "security_options": self.security_options,
            "warnings": self.warnings,
            "counts": {
                "containers": self.containers,
                "running": self.containers_running,
                "paused": self.containers_paused,
                "stopped": self.containers_stopped,
                "images": self.images,
            },
        })
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Arguments whose *value* is masked when a flag or variable name contains one
/// of these fragments.
///
/// Kept short and specific on purpose. A wider list (`key`, `auth`) fires on
/// `--keyspace` and `--authors` and turns a useful command line into noise.
const SECRET_NAME_FRAGMENTS: &[&str] = &[
    "password",
    "passwd",
    "secret",
    "token",
    "apikey",
    "api_key",
    "api-key",
    "credential",
    "private_key",
    "private-key",
    "privatekey",
    "access_key",
    "access-key",
    "accesskey",
];

/// The placeholder that replaces a masked value.
pub const REDACTED: &str = "<redacted>";

fn looks_secret(name: &str) -> bool {
    let lowered = name.to_ascii_lowercase();
    SECRET_NAME_FRAGMENTS
        .iter()
        .any(|fragment| lowered.contains(fragment))
}

/// Mask secret-shaped arguments in a container's command line.
///
/// Returns the filtered arguments and whether anything was actually masked, so
/// a response can say that it was rather than quietly showing something
/// different from what is running.
///
/// **This is best effort and will miss things.** It handles `--password=x`,
/// `--password x`, `KEY=value`, and a URL with inline credentials. It cannot
/// know that the third positional argument of a bespoke binary is a token. An
/// operator who does not want command lines seen at all should not expose that
/// container — see `--container` and `--label`.
pub fn redact_arguments(arguments: &[String]) -> (Vec<String>, bool) {
    let mut output: Vec<String> = Vec::with_capacity(arguments.len());
    let mut redacted = false;
    let mut mask_next = false;

    for argument in arguments {
        if mask_next {
            output.push(REDACTED.to_string());
            redacted = true;
            mask_next = false;
            continue;
        }

        match argument.split_once('=') {
            Some((name, _)) if looks_secret(name) => {
                output.push(format!("{name}={REDACTED}"));
                redacted = true;
            }
            _ => {
                let flag = argument.trim_start_matches('-');
                if argument.starts_with('-') && looks_secret(flag) {
                    // `--password secret`: the value is the next argument.
                    output.push(argument.clone());
                    mask_next = true;
                } else {
                    let (masked, changed) = redact_url_credentials(argument);
                    redacted |= changed;
                    output.push(masked);
                }
            }
        }
    }

    // A trailing `--password` with nothing after it: nothing to mask, but say
    // so rather than pretending the command line is complete.
    if mask_next {
        redacted = true;
    }
    (output, redacted)
}

/// Replace the password in a `scheme://user:password@host` URL.
///
/// `DATABASE_URL`-shaped values are the single most common way a credential
/// ends up on a command line, and unlike a positional secret they are
/// recognisable without guessing.
pub fn redact_url_credentials(value: &str) -> (String, bool) {
    let Some(scheme_end) = value.find("://") else {
        return (value.to_string(), false);
    };
    let rest = &value[scheme_end + 3..];
    let Some(at) = rest.find('@') else {
        return (value.to_string(), false);
    };
    let userinfo = &rest[..at];
    let Some((user, _)) = userinfo.split_once(':') else {
        return (value.to_string(), false);
    };
    (
        format!(
            "{}{user}:{REDACTED}@{}",
            &value[..scheme_end + 3],
            &rest[at + 1..]
        ),
        true,
    )
}

/// Cap a label map, returning the kept labels and whether anything was dropped.
///
/// Kubernetes and Compose write a lot of labels, and one container's
/// annotations can be larger than the rest of the response put together.
pub fn cap_labels(
    labels: &BTreeMap<String, String>,
    max: usize,
) -> (BTreeMap<String, String>, bool) {
    if labels.len() <= max {
        return (labels.clone(), false);
    }
    (
        labels
            .iter()
            .take(max)
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
        true,
    )
}

/// Human-readable binary size. Explicitly binary units, because "GB" meaning
/// two different things is how storage numbers stop being checkable.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Docker states a CPU limit in billionths of a CPU.
fn cpu_limit(nano_cpus: i64) -> Option<f64> {
    (nano_cpus > 0).then(|| nano_cpus as f64 / 1_000_000_000.0)
}

fn positive(value: i64) -> Option<i64> {
    (value > 0).then_some(value)
}

fn empty_as_null(value: &str) -> Option<&str> {
    (!value.is_empty()).then_some(value)
}

/// Go's zero time, which Docker writes for "this has not happened".
///
/// A running container reports `FinishedAt: 0001-01-01T00:00:00Z`. Passed
/// through, that is a date a model will read as a real one and report the
/// container as having finished in the year 1.
const ZERO_TIME: &str = "0001-01-01T00:00:00Z";

fn timestamp_or_null(value: &str) -> Option<&str> {
    empty_as_null(value).filter(|value| *value != ZERO_TIME)
}

/// Format Unix seconds as RFC 3339 in UTC.
///
/// Done by hand rather than with a date crate: this is the only date arithmetic
/// in the plugin, and the algorithm is a well-known twenty lines. A dependency
/// that runs on other people's machines should buy more than that.
pub fn unix_to_rfc3339(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let time_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        time_of_day / 3_600,
        (time_of_day % 3_600) / 60,
        time_of_day % 60
    )
}

/// Howard Hinnant's `civil_from_days`, the standard proleptic Gregorian
/// conversion used by every date library.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_position = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_position + 2) / 5 + 1) as u32;
    let month = if month_position < 10 {
        month_position + 3
    } else {
        month_position - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Build a container summary for tests in this crate.
#[cfg(test)]
pub fn test_container(
    id: impl Into<String>,
    name: &str,
    labels: &[(&str, &str)],
) -> ContainerSummary {
    ContainerSummary {
        id: id.into(),
        names: vec![format!("/{name}")],
        image: "example:latest".to_string(),
        image_id: "sha256:deadbeef".to_string(),
        command: "/bin/sh".to_string(),
        created: 1_700_000_000,
        state: "running".to_string(),
        status: "Up 2 hours".to_string(),
        labels: labels
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect(),
        ports: Vec::new(),
        network_settings: SummaryNetworkSettings::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_container_list_entry_parses_with_null_labels_and_ports() {
        let raw = r#"[{
            "Id": "abc123def4567890",
            "Names": ["/tdcc-node"],
            "Image": "ghcr.io/example/app:1.2",
            "ImageID": "sha256:aa",
            "Command": "/app",
            "Created": 1700000000,
            "State": "running",
            "Status": "Up 2 hours",
            "Labels": null,
            "Ports": null,
            "NetworkSettings": null
        }]"#;

        let containers: Vec<ContainerSummary> = serde_json::from_str(raw).expect("parses");

        assert_eq!(containers.len(), 1);
        assert_eq!(containers[0].primary_name(), "tdcc-node");
        assert_eq!(containers[0].short_id(), "abc123def456");
        assert!(containers[0].labels.is_empty());
        assert!(containers[0].is_running());
    }

    #[test]
    fn unknown_fields_from_a_newer_daemon_are_ignored() {
        let raw = r#"{"Id":"a","SomethingNew":{"nested":true},"State":"exited"}"#;
        let container: ContainerSummary = serde_json::from_str(raw).expect("parses");
        assert!(!container.is_running());
    }

    #[test]
    fn a_published_port_on_every_interface_is_flagged() {
        let published = Port {
            ip: "0.0.0.0".into(),
            private_port: 80,
            public_port: Some(8080),
            protocol: "tcp".into(),
        };
        let loopback = Port {
            ip: "127.0.0.1".into(),
            private_port: 80,
            public_port: Some(8080),
            protocol: "tcp".into(),
        };
        let internal = Port {
            ip: String::new(),
            private_port: 80,
            public_port: None,
            protocol: "tcp".into(),
        };

        assert!(published.published_to_all_interfaces());
        assert!(!loopback.published_to_all_interfaces());
        assert!(!internal.published_to_all_interfaces());
    }

    #[test]
    fn environment_values_are_hidden_by_default_and_names_are_kept() {
        let mut inspect = ContainerInspect::default();
        inspect.config.env = vec![
            "PATH=/usr/bin".to_string(),
            "POSTGRES_PASSWORD=hunter2".to_string(),
        ];

        let rendered = inspect.to_json(EnvMode::NamesOnly, 32).to_string();

        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert!(rendered.contains("POSTGRES_PASSWORD"), "{rendered}");
        assert!(rendered.contains("--show-env"), "{rendered}");
    }

    #[test]
    fn show_env_includes_the_values_and_says_it_is_not_redacted() {
        let mut inspect = ContainerInspect::default();
        inspect.config.env = vec!["POSTGRES_PASSWORD=hunter2".to_string()];

        let rendered = inspect.to_json(EnvMode::Full, 32);

        assert_eq!(rendered["env"]["redacted"], json!(false));
        assert_eq!(
            rendered["env"]["values"]["POSTGRES_PASSWORD"],
            json!("hunter2")
        );
    }

    #[test]
    fn secret_shaped_arguments_are_masked_in_a_command_line() {
        let arguments: Vec<String> = [
            "server",
            "--password=hunter2",
            "--token",
            "abcd",
            "--port",
            "8080",
        ]
        .iter()
        .map(|value| (*value).to_string())
        .collect();

        let (masked, redacted) = redact_arguments(&arguments);

        assert!(redacted);
        assert_eq!(
            masked,
            vec![
                "server",
                "--password=<redacted>",
                "--token",
                "<redacted>",
                "--port",
                "8080"
            ]
        );
    }

    #[test]
    fn an_ordinary_command_line_is_left_alone_and_reports_no_redaction() {
        let arguments: Vec<String> = ["nginx", "-g", "daemon off;"]
            .iter()
            .map(|value| (*value).to_string())
            .collect();

        let (masked, redacted) = redact_arguments(&arguments);

        assert!(!redacted);
        assert_eq!(masked, arguments);
    }

    #[test]
    fn a_url_with_inline_credentials_keeps_the_user_and_loses_the_password() {
        let (masked, redacted) =
            redact_url_credentials("postgres://app:hunter2@db.internal:5432/app");

        assert!(redacted);
        assert_eq!(masked, "postgres://app:<redacted>@db.internal:5432/app");

        let (untouched, changed) = redact_url_credentials("https://example.com/path");
        assert!(!changed);
        assert_eq!(untouched, "https://example.com/path");
    }

    #[test]
    fn a_dangling_secret_flag_still_reports_that_something_was_hidden() {
        let arguments = vec!["--password".to_string()];
        let (masked, redacted) = redact_arguments(&arguments);
        assert_eq!(masked, vec!["--password"]);
        assert!(redacted);
    }

    #[test]
    fn the_docker_socket_mount_is_recognised_on_both_platforms_and_both_ends() {
        let unix = Mount {
            kind: "bind".into(),
            source: "/var/run/docker.sock".into(),
            destination: "/var/run/docker.sock".into(),
            ..Mount::default()
        };
        let renamed = Mount {
            kind: "bind".into(),
            source: "/var/run/docker.sock".into(),
            destination: "/tmp/s".into(),
            ..Mount::default()
        };
        let pipe = Mount {
            kind: "npipe".into(),
            source: r"\\.\pipe\docker_engine".into(),
            destination: r"\\.\pipe\docker_engine".into(),
            ..Mount::default()
        };
        let ordinary = Mount {
            kind: "bind".into(),
            source: "/srv/data".into(),
            destination: "/data".into(),
            ..Mount::default()
        };

        assert!(unix.is_docker_socket());
        assert!(renamed.is_docker_socket());
        assert!(pipe.is_docker_socket());
        assert!(!ordinary.is_docker_socket());
    }

    #[test]
    fn only_writable_binds_of_sensitive_host_paths_are_called_out() {
        let etc = Mount {
            kind: "bind".into(),
            source: "/etc/ssh".into(),
            destination: "/etc/ssh".into(),
            writable: true,
            ..Mount::default()
        };
        let etc_readonly = Mount {
            writable: false,
            ..etc.clone()
        };
        let data = Mount {
            kind: "bind".into(),
            source: "/srv/data".into(),
            destination: "/data".into(),
            writable: true,
            ..Mount::default()
        };
        let volume = Mount {
            kind: "volume".into(),
            source: "/var/lib/docker/volumes/x/_data".into(),
            writable: true,
            ..Mount::default()
        };

        assert!(etc.is_sensitive_host_write());
        assert!(!etc_readonly.is_sensitive_host_write());
        assert!(!data.is_sensitive_host_write());
        assert!(!volume.is_sensitive_host_write());
    }

    #[test]
    fn security_notes_name_the_things_that_change_what_a_container_can_do() {
        let mut inspect = ContainerInspect::default();
        inspect.host_config.privileged = true;
        inspect.host_config.network_mode = "host".into();
        inspect.host_config.pid_mode = "host".into();
        inspect.host_config.cap_add = vec!["SYS_ADMIN".into()];
        inspect.mounts = vec![Mount {
            kind: "bind".into(),
            source: "/var/run/docker.sock".into(),
            destination: "/var/run/docker.sock".into(),
            writable: true,
            ..Mount::default()
        }];

        let notes = inspect.security_notes();

        assert!(
            notes.iter().any(|note| note.contains("privileged")),
            "{notes:?}"
        );
        assert!(
            notes.iter().any(|note| note.contains("Docker socket")),
            "{notes:?}"
        );
        assert!(
            notes.iter().any(|note| note.contains("host networking")),
            "{notes:?}"
        );
        assert!(
            notes.iter().any(|note| note.contains("PID namespace")),
            "{notes:?}"
        );
        assert!(
            notes.iter().any(|note| note.contains("SYS_ADMIN")),
            "{notes:?}"
        );
    }

    #[test]
    fn an_unremarkable_container_produces_no_security_notes() {
        let inspect = ContainerInspect::default();
        assert!(inspect.security_notes().is_empty());
    }

    #[test]
    fn a_running_containers_zero_finish_time_is_reported_as_nothing() {
        let mut inspect = ContainerInspect::default();
        inspect.state.running = true;
        inspect.state.started_at = "2024-05-01T10:00:00Z".into();
        // What the daemon writes for a container that has not finished.
        inspect.state.finished_at = "0001-01-01T00:00:00Z".into();

        let rendered = inspect.to_json(EnvMode::NamesOnly, 32);

        assert_eq!(
            rendered["state"]["started_at"],
            json!("2024-05-01T10:00:00Z")
        );
        assert_eq!(rendered["state"]["finished_at"], json!(null));
    }

    #[test]
    fn labels_are_capped_and_the_response_says_so() {
        let labels: BTreeMap<String, String> = (0..40)
            .map(|index| (format!("label-{index:02}"), "value".to_string()))
            .collect();

        let (kept, truncated) = cap_labels(&labels, 8);

        assert_eq!(kept.len(), 8);
        assert!(truncated);
        assert!(kept.contains_key("label-00"));

        let (all, untruncated) = cap_labels(&labels, 100);
        assert_eq!(all.len(), 40);
        assert!(!untruncated);
    }

    #[test]
    fn byte_sizes_render_in_binary_units() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KiB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MiB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    #[test]
    fn unix_seconds_render_as_utc_rfc3339() {
        assert_eq!(unix_to_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(unix_to_rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(unix_to_rfc3339(1_000_000_000), "2001-09-09T01:46:40Z");
        // A leap day, because the calendar arithmetic is the only thing here
        // that could be quietly wrong.
        assert_eq!(unix_to_rfc3339(1_709_164_800), "2024-02-29T00:00:00Z");
    }

    #[test]
    fn an_image_is_matched_to_a_container_by_id_or_by_tag() {
        let image = ImageSummary {
            id: "sha256:deadbeef".into(),
            repo_tags: vec!["example:latest".into()],
            ..ImageSummary::default()
        };
        let by_id = test_container("a".repeat(64), "one", &[]);
        let mut by_tag = test_container("b".repeat(64), "two", &[]);
        by_tag.image_id = "sha256:other".into();
        by_tag.image = "example:latest".into();
        let mut unrelated = test_container("c".repeat(64), "three", &[]);
        unrelated.image_id = "sha256:other".into();
        unrelated.image = "other:latest".into();

        assert!(image.is_used_by(&by_id));
        assert!(image.is_used_by(&by_tag));
        assert!(!image.is_used_by(&unrelated));
    }

    #[test]
    fn an_image_entry_reports_a_short_id_and_a_readable_size() {
        let image = ImageSummary {
            id: format!("sha256:{}", "a".repeat(64)),
            repo_tags: vec!["example:1".into()],
            created: 1_700_000_000,
            size: 150 * 1024 * 1024,
            ..ImageSummary::default()
        };

        let rendered = image.to_json(vec!["web".to_string()]);

        assert_eq!(rendered["id"], json!("aaaaaaaaaaaa"));
        assert_eq!(rendered["size"], json!("150.0 MiB"));
        assert_eq!(rendered["used_by_visible_containers"], json!(["web"]));
    }

    #[test]
    fn resource_limits_are_reported_only_when_they_are_set() {
        let mut inspect = ContainerInspect::default();
        let unlimited = inspect.to_json(EnvMode::NamesOnly, 32);
        assert_eq!(unlimited["resources"]["memory_limit_bytes"], json!(null));
        assert_eq!(unlimited["resources"]["cpu_limit"], json!(null));

        inspect.host_config.memory = 512 * 1024 * 1024;
        inspect.host_config.nano_cpus = 1_500_000_000;
        let limited = inspect.to_json(EnvMode::NamesOnly, 32);
        assert_eq!(limited["resources"]["memory_limit"], json!("512.0 MiB"));
        assert_eq!(limited["resources"]["cpu_limit"], json!(1.5));
    }
}
