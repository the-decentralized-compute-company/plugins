//! Prometheus text exposition, version 0.0.4.
//!
//! The rules this module exists to enforce, because getting any of them wrong
//! turns a dashboard into a plausible-looking lie:
//!
//! - **Base units.** Seconds and bytes. The node reports milliseconds and
//!   decimal gigabytes; the conversion happens here, once, and the metric name
//!   states the unit it ended up in.
//! - **`_total` on counters, and only on counters.** A counter that resets on
//!   node restart is still a counter; `rate()` handles the reset. A gauge that
//!   happens to grow is not.
//! - **`# HELP` and `# TYPE` on every family**, emitted exactly once, with all
//!   samples of a family contiguous.
//! - **Bounded label sets.** No request id, no prompt, no free-form status
//!   string, no unbounded peer set. Every label value that comes from node data
//!   is escaped and length-capped, and every repeating group is capped by
//!   count. See README > "Cardinality".
//! - **Duplicate label sets are dropped.** Prometheus rejects an entire scrape
//!   that contains the same series twice, so the writer refuses to emit one
//!   even if the node reports two processes that collapse onto the same labels.

use std::collections::HashSet;

use crate::collector::CollectorSnapshot;
use crate::node::NodeStatus;
use crate::settings::Settings;

/// Content type Prometheus expects for this format.
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Node-derived series emitted regardless of how many models, GPUs or peers
/// the node has. This is what `tdcc_exporter_series` counts.
pub const FIXED_NODE_SERIES: usize = 39;

/// The exporter's own series. Always emitted, including when collection fails,
/// which is what lets a dashboard distinguish "the node is down" from "the
/// exporter is down".
pub const EXPORTER_SERIES: usize = 20;

/// Total series in a successful scrape before models, GPUs and peers are added.
///
/// Kept in step with `render_exposition` by `fixed_series_count_is_accurate`.
pub const FIXED_SERIES: usize = FIXED_NODE_SERIES + EXPORTER_SERIES;

/// Longest label value the exporter will emit, in characters.
///
/// Model names and hostnames are operator-controlled but not exporter-
/// controlled; capping keeps one pathological name from bloating every scrape.
const MAX_LABEL_VALUE_CHARS: usize = 128;

/// The states a node or a peer can be in. Emitting the full set every scrape,
/// with zeros, is what lets `tdcc_node_state` be graphed without gaps and keeps
/// the label set fixed instead of following whatever string the node sent.
const NODE_STATES: [&str; 4] = ["client", "standby", "loading", "serving"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetricKind {
    Counter,
    Gauge,
    Histogram,
    Summary,
}

impl MetricKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Gauge => "gauge",
            Self::Histogram => "histogram",
            Self::Summary => "summary",
        }
    }
}

/// Accumulates exposition text and guarantees it is well formed.
pub struct Exposition {
    out: String,
    families: HashSet<String>,
    series: HashSet<String>,
    written: usize,
    duplicates_dropped: usize,
}

impl Exposition {
    pub fn new() -> Self {
        Self {
            out: String::with_capacity(8 * 1024),
            families: HashSet::new(),
            series: HashSet::new(),
            written: 0,
            duplicates_dropped: 0,
        }
    }

    /// Open a metric family. A repeat call for the same name is ignored, since
    /// a second `# HELP` line for one family is a parse error downstream.
    pub fn family(&mut self, name: &str, kind: MetricKind, help: &str) {
        if !self.families.insert(name.to_string()) {
            return;
        }
        self.out.push_str("# HELP ");
        self.out.push_str(name);
        self.out.push(' ');
        self.out.push_str(&escape_help(help));
        self.out.push('\n');
        self.out.push_str("# TYPE ");
        self.out.push_str(name);
        self.out.push(' ');
        self.out.push_str(kind.as_str());
        self.out.push('\n');
    }

    /// Write one sample. Labels are emitted in the order given.
    pub fn sample(&mut self, name: &str, labels: &[(&str, &str)], value: f64) {
        let mut line = String::with_capacity(name.len() + 32);
        line.push_str(name);
        if !labels.is_empty() {
            line.push('{');
            for (index, (key, value)) in labels.iter().enumerate() {
                if index > 0 {
                    line.push(',');
                }
                line.push_str(key);
                line.push_str("=\"");
                line.push_str(&sanitize_label_value(value));
                line.push('"');
            }
            line.push('}');
        }
        if !self.series.insert(line.clone()) {
            self.duplicates_dropped += 1;
            return;
        }
        self.out.push_str(&line);
        self.out.push(' ');
        self.out.push_str(&format_value(value));
        self.out.push('\n');
        self.written += 1;
    }

    /// A free-form comment line. Prometheus ignores these; a human running
    /// `curl` does not, which is how a collection error reaches an operator
    /// without becoming a high-cardinality label.
    pub fn comment(&mut self, text: &str) {
        self.out.push_str("# ");
        self.out.push_str(&escape_help(text));
        self.out.push('\n');
    }

    pub fn written(&self) -> usize {
        self.written
    }

    pub fn duplicates_dropped(&self) -> usize {
        self.duplicates_dropped
    }

    pub fn finish(self) -> String {
        self.out
    }
}

impl Default for Exposition {
    fn default() -> Self {
        Self::new()
    }
}

/// Everything a scrape needs to render itself.
pub struct RenderInput<'a> {
    pub settings: &'a Settings,
    pub exporter_version: &'a str,
    /// The node state that was collected, or the reason collection failed.
    pub status: Result<&'a NodeStatus, &'a str>,
    pub collector: &'a CollectorSnapshot,
}

/// Render one complete exposition.
pub fn render_exposition(input: &RenderInput<'_>) -> String {
    let mut out = Exposition::new();

    match input.status {
        Ok(status) => render_node(&mut out, input.settings, status),
        Err(error) => {
            // No node series at all: a stale value is worse than a gap, because
            // a gap is visible on a graph and a stale value is not.
            out.comment(&format!("collection failed: {}", one_line(error)));
        }
    }

    let node_series = out.written();
    render_exporter(&mut out, input, node_series);
    out.finish()
}

fn render_node(out: &mut Exposition, settings: &Settings, status: &NodeStatus) {
    render_identity(out, status);
    render_requests(out, status);
    render_tokens(out, status);
    render_models(out, settings, status);
    render_gpus(out, settings, status);
    render_peers(out, settings, status);
}

fn render_identity(out: &mut Exposition, status: &NodeStatus) {
    out.family(
        "tdcc_node_info",
        MetricKind::Gauge,
        "Identity of the node this exporter reads. Always 1; read the labels.",
    );
    out.sample(
        "tdcc_node_info",
        &[
            ("node_id", status.node_id.as_str()),
            ("version", status.version.as_str()),
            ("hostname", status.my_hostname.as_deref().unwrap_or("")),
            ("mesh_id", status.mesh_id.as_deref().unwrap_or("")),
        ],
        1.0,
    );

    out.family(
        "tdcc_node_state",
        MetricKind::Gauge,
        "Node state, 1 for the state the node reports and 0 for the others.",
    );
    for state in NODE_STATES {
        let active = status.node_state.eq_ignore_ascii_case(state);
        out.sample("tdcc_node_state", &[("state", state)], bool_value(active));
    }

    out.family(
        "tdcc_node_is_host",
        MetricKind::Gauge,
        "1 when this node serves inference for the mesh.",
    );
    out.sample("tdcc_node_is_host", &[], bool_value(status.is_host));

    out.family(
        "tdcc_node_is_client",
        MetricKind::Gauge,
        "1 when this node consumes inference from the mesh.",
    );
    out.sample("tdcc_node_is_client", &[], bool_value(status.is_client));

    out.family(
        "tdcc_node_runtime_ready",
        MetricKind::Gauge,
        "1 when the local serving runtime reports itself ready to take work.",
    );
    out.sample(
        "tdcc_node_runtime_ready",
        &[],
        bool_value(status.llama_ready),
    );

    out.family(
        "tdcc_node_vram_bytes",
        MetricKind::Gauge,
        "Video memory this node advertises to the mesh. Converted from the \
         node's decimal-gigabyte figure, so it is accurate to about a gigabyte.",
    );
    out.sample("tdcc_node_vram_bytes", &[], gb_to_bytes(status.my_vram_gb));
}

fn render_requests(out: &mut Exposition, status: &NodeStatus) {
    let routing = &status.routing_metrics;

    out.family(
        "tdcc_requests_in_flight",
        MetricKind::Gauge,
        "Requests this node is currently serving or routing.",
    );
    out.sample(
        "tdcc_requests_in_flight",
        &[],
        status.inflight_requests as f64,
    );

    out.family(
        "tdcc_requests_in_flight_peak",
        MetricKind::Gauge,
        "Highest concurrent request count seen since this node started.",
    );
    out.sample(
        "tdcc_requests_in_flight_peak",
        &[],
        routing.local_node.peak_inflight_requests as f64,
    );

    out.family(
        "tdcc_requests_total",
        MetricKind::Counter,
        "Requests fronted by this node since it started, by outcome. Local to \
         this node; it is not a mesh-wide total.",
    );
    let failures = routing
        .request_count
        .saturating_sub(routing.successful_requests);
    out.sample(
        "tdcc_requests_total",
        &[("outcome", "success")],
        routing.successful_requests as f64,
    );
    out.sample(
        "tdcc_requests_total",
        &[("outcome", "failure")],
        failures as f64,
    );

    out.family(
        "tdcc_requests_served_total",
        MetricKind::Counter,
        "Requests fronted by this node, by which service took them: this node, \
         a mesh peer, or an attached endpoint. A request that found no service \
         at all is counted in tdcc_requests_total but not here, so these three \
         do not sum to it.",
    );
    for (service, value) in [
        ("local", routing.pressure.locally_served_request_count),
        ("remote", routing.pressure.remotely_served_request_count),
        ("endpoint", routing.pressure.endpoint_request_count),
    ] {
        out.sample(
            "tdcc_requests_served_total",
            &[("service", service)],
            value as f64,
        );
    }

    out.family(
        "tdcc_request_retries_total",
        MetricKind::Counter,
        "Routing attempts beyond the first, summed across all requests.",
    );
    out.sample(
        "tdcc_request_retries_total",
        &[],
        routing.retry_count as f64,
    );

    out.family(
        "tdcc_request_failovers_total",
        MetricKind::Counter,
        "Requests that needed more than one routing attempt.",
    );
    out.sample(
        "tdcc_request_failovers_total",
        &[],
        routing.failover_count as f64,
    );

    out.family(
        "tdcc_request_attempts_total",
        MetricKind::Counter,
        "Routing attempts by target kind. Every attempt lands in exactly one \
         kind, so the sum is the total attempt count.",
    );
    for (target, value) in [
        ("local", routing.local_node.local_attempt_count),
        ("remote", routing.local_node.remote_attempt_count),
        ("endpoint", routing.local_node.endpoint_attempt_count),
    ] {
        out.sample(
            "tdcc_request_attempts_total",
            &[("target", target)],
            value as f64,
        );
    }

    out.family(
        "tdcc_request_attempt_failures_total",
        MetricKind::Counter,
        "Routing attempts that did not succeed, by reason.",
    );
    for (reason, value) in [
        ("timeout", routing.attempt_timeout_count),
        ("unavailable", routing.attempt_unavailable_count),
        ("context_overflow", routing.attempt_context_overflow_count),
        ("rejected", routing.attempt_reject_count),
    ] {
        out.sample(
            "tdcc_request_attempt_failures_total",
            &[("reason", reason)],
            value as f64,
        );
    }

    let attempts = routing.local_node.attempt_count();

    out.family(
        "tdcc_request_attempt_duration_seconds",
        MetricKind::Summary,
        "Time spent in routing attempts. The node exposes a cumulative count \
         and mean only, so this summary carries no quantiles: use \
         rate(_sum)/rate(_count) for the windowed mean. See README > Latency.",
    );
    out.sample(
        "tdcc_request_attempt_duration_seconds_sum",
        &[],
        mean_ms_to_total_seconds(routing.avg_attempt_ms, attempts),
    );
    out.sample(
        "tdcc_request_attempt_duration_seconds_count",
        &[],
        attempts as f64,
    );

    out.family(
        "tdcc_request_queue_wait_seconds",
        MetricKind::Summary,
        "Time attempts spent queued before execution. Count and mean only, for \
         the same reason as tdcc_request_attempt_duration_seconds.",
    );
    out.sample(
        "tdcc_request_queue_wait_seconds_sum",
        &[],
        mean_ms_to_total_seconds(routing.avg_queue_wait_ms, attempts),
    );
    out.sample(
        "tdcc_request_queue_wait_seconds_count",
        &[],
        attempts as f64,
    );
}

fn render_tokens(out: &mut Exposition, status: &NodeStatus) {
    let routing = &status.routing_metrics;

    out.family(
        "tdcc_completion_tokens_total",
        MetricKind::Counter,
        "Completion tokens observed by this node's router. \
         rate(tdcc_completion_tokens_total[5m]) is tokens per second.",
    );
    out.sample(
        "tdcc_completion_tokens_total",
        &[],
        routing.completion_tokens_observed as f64,
    );

    out.family(
        "tdcc_completion_throughput_samples_total",
        MetricKind::Counter,
        "Attempts that produced a generation-throughput sample.",
    );
    out.sample(
        "tdcc_completion_throughput_samples_total",
        &[],
        routing.throughput_samples as f64,
    );

    // Absent rather than zero when the node has no sample yet: zero would read
    // as "throughput collapsed", which is a different and much louder claim.
    out.family(
        "tdcc_completion_tokens_per_second",
        MetricKind::Gauge,
        "Mean generation throughput across every sampled attempt since the node \
         started, as the node computes it. Absent until a sample exists. Prefer \
         rate(tdcc_completion_tokens_total[5m]) for anything time-windowed.",
    );
    if let Some(tokens_per_second) = routing.avg_tokens_per_second {
        out.sample("tdcc_completion_tokens_per_second", &[], tokens_per_second);
    }
}

fn render_models(out: &mut Exposition, settings: &Settings, status: &NodeStatus) {
    out.family(
        "tdcc_models_loaded",
        MetricKind::Gauge,
        "Model processes the local runtime currently has, ready or not.",
    );
    out.sample(
        "tdcc_models_loaded",
        &[],
        status.runtime.models.len() as f64,
    );

    out.family(
        "tdcc_primary_model_size_bytes",
        MetricKind::Gauge,
        "On-disk size of the model this node primarily serves. This is file \
         size, not resident VRAM: the node does not report per-model VRAM.",
    );
    out.sample(
        "tdcc_primary_model_size_bytes",
        &[],
        gb_to_bytes(status.model_size_gb),
    );

    // Collapse instances onto (model, backend). The runtime also carries an
    // `instance_id`, which is unique per launch and would therefore be an
    // unbounded label; counting instances instead keeps the information without
    // the cardinality.
    let mut groups: Vec<ModelGroup> = Vec::new();
    for model in &status.runtime.models {
        let key = (model.name.as_str(), model.backend.as_str());
        match groups.iter_mut().find(|group| group.key == key) {
            Some(group) => group.absorb(model),
            None => {
                let mut group = ModelGroup::new(key);
                group.absorb(model);
                groups.push(group);
            }
        }
    }
    groups.sort_by(|left, right| left.key.cmp(&right.key));
    groups.truncate(settings.max_model_series);

    out.family(
        "tdcc_model_processes",
        MetricKind::Gauge,
        "Local runtime processes per model and backend.",
    );
    for group in &groups {
        out.sample(
            "tdcc_model_processes",
            &[("model", group.key.0), ("backend", group.key.1)],
            group.processes as f64,
        );
    }

    out.family(
        "tdcc_model_processes_ready",
        MetricKind::Gauge,
        "Local runtime processes reporting status 'ready', per model and \
         backend. Any other status counts as not ready; the raw status string \
         is not a label because it is free-form.",
    );
    for group in &groups {
        out.sample(
            "tdcc_model_processes_ready",
            &[("model", group.key.0), ("backend", group.key.1)],
            group.ready as f64,
        );
    }

    out.family(
        "tdcc_model_context_length_tokens",
        MetricKind::Gauge,
        "Largest context window offered for a model on this node.",
    );
    for group in &groups {
        if let Some(context_length) = group.context_length {
            out.sample(
                "tdcc_model_context_length_tokens",
                &[("model", group.key.0), ("backend", group.key.1)],
                context_length as f64,
            );
        }
    }
}

struct ModelGroup<'a> {
    key: (&'a str, &'a str),
    processes: u64,
    ready: u64,
    context_length: Option<u32>,
}

impl<'a> ModelGroup<'a> {
    fn new(key: (&'a str, &'a str)) -> Self {
        Self {
            key,
            processes: 0,
            ready: 0,
            context_length: None,
        }
    }

    fn absorb(&mut self, model: &crate::node::RuntimeModel) {
        self.processes += 1;
        if model.status.eq_ignore_ascii_case("ready") {
            self.ready += 1;
        }
        self.context_length = match (self.context_length, model.context_length) {
            (Some(current), Some(candidate)) => Some(current.max(candidate)),
            (current, candidate) => current.or(candidate),
        };
    }
}

fn render_gpus(out: &mut Exposition, settings: &Settings, status: &NodeStatus) {
    let gpus: Vec<(usize, &crate::node::Gpu)> = status
        .gpus
        .iter()
        .enumerate()
        .take(settings.max_gpu_series)
        .collect();

    out.family(
        "tdcc_gpu_vram_bytes",
        MetricKind::Gauge,
        "Total video memory on a local GPU, as the node detected it.",
    );
    for (index, gpu) in &gpus {
        out.sample(
            "tdcc_gpu_vram_bytes",
            &[("gpu", &index.to_string()), ("name", gpu.name.as_str())],
            gpu.vram_bytes as f64,
        );
    }

    out.family(
        "tdcc_gpu_vram_reserved_bytes",
        MetricKind::Gauge,
        "Video memory the node holds back from model placement on a local GPU.",
    );
    for (index, gpu) in &gpus {
        if let Some(reserved) = gpu.reserved_bytes {
            out.sample(
                "tdcc_gpu_vram_reserved_bytes",
                &[("gpu", &index.to_string()), ("name", gpu.name.as_str())],
                reserved as f64,
            );
        }
    }

    out.family(
        "tdcc_gpu_vram_allocatable_bytes",
        MetricKind::Gauge,
        "Video memory the node considers available for model placement on a \
         local GPU.",
    );
    for (index, gpu) in &gpus {
        if let Some(allocatable) = gpu.allocatable_vram_bytes {
            out.sample(
                "tdcc_gpu_vram_allocatable_bytes",
                &[("gpu", &index.to_string()), ("name", gpu.name.as_str())],
                allocatable as f64,
            );
        }
    }
}

fn render_peers(out: &mut Exposition, settings: &Settings, status: &NodeStatus) {
    // Aggregates first: they are always emitted, so a mesh too large for
    // per-peer series still gets a usable connectivity signal.
    out.family(
        "tdcc_peers",
        MetricKind::Gauge,
        "Mesh peers this node currently knows about, by state. Peers reporting \
         an unrecognised state are not counted here.",
    );
    for state in NODE_STATES {
        let count = status
            .peers
            .iter()
            .filter(|peer| peer.state.eq_ignore_ascii_case(state))
            .count();
        out.sample("tdcc_peers", &[("state", state)], count as f64);
    }

    let mut peers: Vec<&crate::node::Peer> = status.peers.iter().collect();
    peers.sort_by(|left, right| left.id.cmp(&right.id));
    peers.dedup_by(|left, right| left.id == right.id);
    let total = peers.len();
    peers.truncate(settings.max_peer_series);

    out.family(
        "tdcc_peers_truncated",
        MetricKind::Gauge,
        "Peers left out of the per-peer series below because the exporter's \
         peer cap was reached. Non-zero means --max-peer-series is too small \
         for this mesh, or was set to 0 on purpose.",
    );
    out.sample(
        "tdcc_peers_truncated",
        &[],
        total.saturating_sub(peers.len()) as f64,
    );

    out.family(
        "tdcc_peer_info",
        MetricKind::Gauge,
        "Identity of a mesh peer. Always 1; read the labels.",
    );
    for peer in &peers {
        out.sample(
            "tdcc_peer_info",
            &[
                ("peer", peer.id.as_str()),
                ("role", peer.role.as_str()),
                ("version", peer.version.as_deref().unwrap_or("")),
            ],
            1.0,
        );
    }

    out.family(
        "tdcc_peer_serving",
        MetricKind::Gauge,
        "1 when a mesh peer reports that it is serving inference.",
    );
    for peer in &peers {
        out.sample(
            "tdcc_peer_serving",
            &[("peer", peer.id.as_str())],
            bool_value(peer.state.eq_ignore_ascii_case("serving")),
        );
    }

    out.family(
        "tdcc_peer_vram_bytes",
        MetricKind::Gauge,
        "Video memory a mesh peer advertises. Converted from the peer's \
         decimal-gigabyte figure.",
    );
    for peer in &peers {
        out.sample(
            "tdcc_peer_vram_bytes",
            &[("peer", peer.id.as_str())],
            gb_to_bytes(peer.vram_gb),
        );
    }

    out.family(
        "tdcc_peer_rtt_seconds",
        MetricKind::Gauge,
        "Round-trip time this node last measured to a mesh peer. Absent when \
         no direct measurement exists.",
    );
    for peer in &peers {
        if let Some(rtt_ms) = peer.rtt_ms {
            out.sample(
                "tdcc_peer_rtt_seconds",
                &[("peer", peer.id.as_str())],
                f64::from(rtt_ms) / 1_000.0,
            );
        }
    }

    out.family(
        "tdcc_peer_latency_seconds",
        MetricKind::Gauge,
        "Latency the router uses for a mesh peer. May be an estimate rather \
         than a measurement; pair it with tdcc_peer_latency_age_seconds.",
    );
    for peer in &peers {
        if let Some(latency_ms) = peer.latency_ms {
            out.sample(
                "tdcc_peer_latency_seconds",
                &[("peer", peer.id.as_str())],
                f64::from(latency_ms) / 1_000.0,
            );
        }
    }

    out.family(
        "tdcc_peer_latency_age_seconds",
        MetricKind::Gauge,
        "Age of the latency reading for a mesh peer. A fresh 5 ms and an \
         hour-old 5 ms are not the same claim.",
    );
    for peer in &peers {
        if let Some(age_ms) = peer.latency_age_ms {
            out.sample(
                "tdcc_peer_latency_age_seconds",
                &[("peer", peer.id.as_str())],
                age_ms as f64 / 1_000.0,
            );
        }
    }
}

fn render_exporter(out: &mut Exposition, input: &RenderInput<'_>, node_series: usize) {
    out.family(
        "tdcc_up",
        MetricKind::Gauge,
        "1 when the exporter read node state successfully on this scrape. When \
         it is 0 every tdcc_* node series is absent and the reason is on a \
         comment line at the top of this response.",
    );
    out.sample("tdcc_up", &[], bool_value(input.status.is_ok()));

    out.family(
        "tdcc_exporter_build_info",
        MetricKind::Gauge,
        "Version of the prometheus-exporter plugin. Always 1.",
    );
    out.sample(
        "tdcc_exporter_build_info",
        &[("version", input.exporter_version)],
        1.0,
    );

    out.family(
        "tdcc_exporter_collections_total",
        MetricKind::Counter,
        "Node-state collections the exporter has attempted, by outcome.",
    );
    out.sample(
        "tdcc_exporter_collections_total",
        &[("outcome", "success")],
        input.collector.successes as f64,
    );
    out.sample(
        "tdcc_exporter_collections_total",
        &[("outcome", "error")],
        input.collector.failures as f64,
    );

    out.family(
        "tdcc_exporter_collect_duration_seconds",
        MetricKind::Histogram,
        "Time the exporter spent reading node state, including the failed \
         attempts. This is the one true distribution in this exposition: the \
         exporter times these itself.",
    );
    let mut cumulative = 0u64;
    for (bound, count) in &input.collector.buckets {
        cumulative += count;
        out.sample(
            "tdcc_exporter_collect_duration_seconds_bucket",
            &[("le", &format_value(*bound))],
            cumulative as f64,
        );
    }
    out.sample(
        "tdcc_exporter_collect_duration_seconds_bucket",
        &[("le", "+Inf")],
        input.collector.count as f64,
    );
    out.sample(
        "tdcc_exporter_collect_duration_seconds_sum",
        &[],
        input.collector.sum_seconds,
    );
    out.sample(
        "tdcc_exporter_collect_duration_seconds_count",
        &[],
        input.collector.count as f64,
    );

    out.family(
        "tdcc_exporter_series",
        MetricKind::Gauge,
        "Series produced from node state in this response, excluding the \
         exporter's own. Compare with the ceiling in README > Cardinality.",
    );
    out.sample("tdcc_exporter_series", &[], node_series as f64);

    out.family(
        "tdcc_exporter_duplicate_series_dropped",
        MetricKind::Gauge,
        "Samples the exporter refused to emit in this response because their \
         label set was already used. Non-zero means node data collapsed onto \
         one label set; the scrape stays valid.",
    );
    let duplicates = out.duplicates_dropped();
    out.sample(
        "tdcc_exporter_duplicate_series_dropped",
        &[],
        duplicates as f64,
    );
}

/// Reconstruct a cumulative total from a cumulative mean and its denominator.
///
/// The node tracks integer milliseconds and divides on the way out, so
/// multiplying back and rounding recovers the original integer exactly, which
/// keeps `_sum` monotonic across scrapes instead of jittering by float noise.
pub fn mean_ms_to_total_seconds(mean_ms: f64, count: u64) -> f64 {
    if count == 0 || !mean_ms.is_finite() || mean_ms <= 0.0 {
        return 0.0;
    }
    (mean_ms * count as f64).round() / 1_000.0
}

/// The node reports decimal gigabytes; Prometheus wants bytes.
fn gb_to_bytes(gigabytes: f64) -> f64 {
    if !gigabytes.is_finite() || gigabytes <= 0.0 {
        return 0.0;
    }
    (gigabytes * 1e9).round()
}

fn bool_value(value: bool) -> f64 {
    if value { 1.0 } else { 0.0 }
}

/// Format a value the way the text format expects.
pub fn format_value(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value.is_infinite() {
        return if value.is_sign_positive() {
            "+Inf".to_string()
        } else {
            "-Inf".to_string()
        };
    }
    format!("{value}")
}

/// Escape a label value: backslash, newline and double quote, in that order.
fn escape_label_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '"' => out.push_str("\\\""),
            // A bare carriage return would end the line early. It is not part
            // of the escape set, so drop it rather than emit it raw.
            '\r' => {}
            other => out.push(other),
        }
    }
    out
}

/// Cap then escape. Capping first keeps the limit about the operator's string
/// rather than about how many escapes it happens to contain.
fn sanitize_label_value(value: &str) -> String {
    let capped: String = value.chars().take(MAX_LABEL_VALUE_CHARS).collect();
    escape_label_value(&capped)
}

/// `# HELP` and comment text is terminated by a newline, and backslash is an
/// escape character there too.
fn escape_help(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            other => out.push(other),
        }
    }
    out
}

fn one_line(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(300)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::CollectorSnapshot;
    use crate::node::{
        Gpu, LocalNodePressure, Peer, RoutingMetrics, RoutingPressure, RuntimeModel,
    };
    use crate::settings::settings_from;

    fn collector_snapshot() -> CollectorSnapshot {
        // Four observations: one under 5 ms, two under 10 ms, and one slower
        // than the last bound so it only appears in +Inf.
        let mut buckets: Vec<(f64, u64)> = crate::collector::DEFAULT_BUCKET_BOUNDS
            .iter()
            .map(|bound| (*bound, 0))
            .collect();
        buckets[0].1 = 1;
        buckets[1].1 = 2;
        CollectorSnapshot {
            successes: 3,
            failures: 1,
            buckets,
            count: 4,
            sum_seconds: 0.042,
        }
    }

    fn settings() -> Settings {
        settings_from(&[], None).expect("defaults parse")
    }

    fn sample_status() -> NodeStatus {
        NodeStatus {
            version: "0.72.1".into(),
            node_id: "node-1".into(),
            node_state: "serving".into(),
            is_host: true,
            is_client: false,
            llama_ready: true,
            my_hostname: Some("workstation".into()),
            mesh_id: Some("mesh-7".into()),
            my_vram_gb: 24.0,
            model_size_gb: 4.5,
            inflight_requests: 2,
            runtime: crate::node::RuntimeStatus {
                models: vec![
                    RuntimeModel {
                        name: "qwen3".into(),
                        backend: "cuda".into(),
                        status: "ready".into(),
                        context_length: Some(8192),
                    },
                    RuntimeModel {
                        name: "qwen3".into(),
                        backend: "cuda".into(),
                        status: "starting".into(),
                        context_length: Some(16384),
                    },
                ],
            },
            gpus: vec![Gpu {
                name: "RTX 4090".into(),
                vram_bytes: 25_769_803_776,
                reserved_bytes: Some(1_073_741_824),
                allocatable_vram_bytes: Some(24_696_061_952),
            }],
            peers: vec![Peer {
                id: "peer-a".into(),
                role: "host".into(),
                state: "serving".into(),
                vram_gb: 48.0,
                version: Some("0.72.1".into()),
                rtt_ms: Some(12),
                latency_ms: Some(14),
                latency_age_ms: Some(3_000),
            }],
            routing_metrics: RoutingMetrics {
                request_count: 10,
                successful_requests: 9,
                retry_count: 2,
                failover_count: 1,
                attempt_timeout_count: 1,
                attempt_unavailable_count: 0,
                attempt_context_overflow_count: 0,
                attempt_reject_count: 1,
                avg_queue_wait_ms: 5.0,
                avg_attempt_ms: 250.0,
                avg_tokens_per_second: Some(42.5),
                completion_tokens_observed: 1_234,
                throughput_samples: 9,
                local_node: LocalNodePressure {
                    peak_inflight_requests: 6,
                    local_attempt_count: 8,
                    remote_attempt_count: 3,
                    endpoint_attempt_count: 1,
                },
                pressure: RoutingPressure {
                    locally_served_request_count: 7,
                    remotely_served_request_count: 2,
                    endpoint_request_count: 1,
                },
            },
        }
    }

    fn render(status: Result<&NodeStatus, &str>, settings: &Settings) -> String {
        let collector = collector_snapshot();
        render_exposition(&RenderInput {
            settings,
            exporter_version: "0.1.0",
            status,
            collector: &collector,
        })
    }

    /// Minimal re-parse of the text format, used to assert structural rules
    /// rather than hand-comparing strings.
    struct Parsed {
        helped: HashSet<String>,
        typed: Vec<(String, String)>,
        samples: Vec<(String, String)>,
    }

    fn parse(text: &str) -> Parsed {
        let mut helped = HashSet::new();
        let mut typed = Vec::new();
        let mut samples = Vec::new();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("# HELP ") {
                let (name, _) = rest.split_once(' ').expect("HELP has help text");
                assert!(helped.insert(name.to_string()), "duplicate HELP for {name}");
            } else if let Some(rest) = line.strip_prefix("# TYPE ") {
                let (name, kind) = rest.split_once(' ').expect("TYPE has a kind");
                typed.push((name.to_string(), kind.to_string()));
            } else if line.starts_with('#') || line.is_empty() {
                continue;
            } else {
                let (series, value) = line.rsplit_once(' ').expect("sample has a value");
                samples.push((series.to_string(), value.to_string()));
            }
        }
        Parsed {
            helped,
            typed,
            samples,
        }
    }

    fn family_of(series: &str) -> String {
        let base = series.split('{').next().unwrap_or(series);
        for suffix in ["_bucket", "_sum", "_count"] {
            if let Some(stripped) = base.strip_suffix(suffix) {
                return stripped.to_string();
            }
        }
        base.to_string()
    }

    #[test]
    fn every_sample_belongs_to_a_declared_family() {
        let status = sample_status();
        let text = render(Ok(&status), &settings());
        let parsed = parse(&text);
        let declared: HashSet<String> = parsed.typed.iter().map(|(name, _)| name.clone()).collect();

        for (series, _) in &parsed.samples {
            let family = family_of(series);
            assert!(
                declared.contains(&family),
                "sample {series} has no TYPE line"
            );
            assert!(
                parsed.helped.contains(&family),
                "sample {series} has no HELP line"
            );
        }
    }

    #[test]
    fn no_series_is_emitted_twice() {
        let mut status = sample_status();
        // Two GPUs with the same name must still be distinct series, and two
        // model processes that collapse onto one label set must not be.
        status.gpus.push(status.gpus[0].clone());
        let text = render(Ok(&status), &settings());
        let parsed = parse(&text);
        let mut seen = HashSet::new();
        for (series, _) in &parsed.samples {
            assert!(seen.insert(series.clone()), "duplicate series {series}");
        }
        assert!(text.contains("tdcc_gpu_vram_bytes{gpu=\"0\","));
        assert!(text.contains("tdcc_gpu_vram_bytes{gpu=\"1\","));
    }

    #[test]
    fn counters_end_in_total_and_gauges_do_not() {
        let status = sample_status();
        let text = render(Ok(&status), &settings());
        for (name, kind) in parse(&text).typed {
            match kind.as_str() {
                "counter" => assert!(name.ends_with("_total"), "counter {name} lacks _total"),
                "gauge" => assert!(!name.ends_with("_total"), "gauge {name} claims _total"),
                _ => {}
            }
        }
    }

    #[test]
    fn every_metric_uses_a_base_unit_or_no_unit_at_all() {
        let status = sample_status();
        let text = render(Ok(&status), &settings());
        for (name, _) in parse(&text).typed {
            for banned in ["_ms", "_millis", "_milliseconds", "_gb", "_mb", "_kb"] {
                assert!(
                    !name.ends_with(banned),
                    "{name} exposes a non-base unit ({banned})"
                );
            }
        }
    }

    #[test]
    fn model_instances_collapse_onto_one_label_set_and_are_counted() {
        let status = sample_status();
        let text = render(Ok(&status), &settings());
        assert!(text.contains("tdcc_model_processes{model=\"qwen3\",backend=\"cuda\"} 2"));
        assert!(text.contains("tdcc_model_processes_ready{model=\"qwen3\",backend=\"cuda\"} 1"));
        // The larger of the two advertised context windows wins.
        assert!(
            text.contains(
                "tdcc_model_context_length_tokens{model=\"qwen3\",backend=\"cuda\"} 16384"
            )
        );
    }

    #[test]
    fn milliseconds_and_gigabytes_are_converted_to_base_units() {
        let status = sample_status();
        let text = render(Ok(&status), &settings());
        // 12 attempts x 250 ms = 3 s.
        assert!(text.contains("tdcc_request_attempt_duration_seconds_sum 3"));
        assert!(text.contains("tdcc_request_attempt_duration_seconds_count 12"));
        // 12 attempts x 5 ms = 0.06 s.
        assert!(text.contains("tdcc_request_queue_wait_seconds_sum 0.06"));
        assert!(text.contains("tdcc_node_vram_bytes 24000000000"));
        assert!(text.contains("tdcc_peer_rtt_seconds{peer=\"peer-a\"} 0.012"));
        assert!(text.contains("tdcc_peer_latency_age_seconds{peer=\"peer-a\"} 3"));
    }

    #[test]
    fn a_failed_collection_drops_node_series_and_says_why() {
        let text = render(
            Err("cannot reach http://127.0.0.1:3131: connection refused"),
            &settings(),
        );
        assert!(text.contains("# collection failed: cannot reach"));
        assert!(text.contains("tdcc_up 0"));
        assert!(!text.contains("tdcc_requests_in_flight "));
        assert!(text.contains("tdcc_exporter_series 0"));
        // The exporter's own series survive, so a dashboard can still show that
        // the exporter is alive and the node is not.
        assert!(text.contains("tdcc_exporter_collections_total{outcome=\"error\"} 1"));
    }

    #[test]
    fn histogram_buckets_are_cumulative_and_end_at_inf() {
        let status = sample_status();
        let text = render(Ok(&status), &settings());
        assert!(text.contains("tdcc_exporter_collect_duration_seconds_bucket{le=\"0.005\"} 1"));
        assert!(text.contains("tdcc_exporter_collect_duration_seconds_bucket{le=\"0.01\"} 3"));
        assert!(text.contains("tdcc_exporter_collect_duration_seconds_bucket{le=\"0.025\"} 3"));
        assert!(text.contains("tdcc_exporter_collect_duration_seconds_bucket{le=\"+Inf\"} 4"));
        assert!(text.contains("tdcc_exporter_collect_duration_seconds_count 4"));
        assert!(text.contains("tdcc_exporter_collect_duration_seconds_sum 0.042"));
    }

    #[test]
    fn peer_cap_of_zero_keeps_the_aggregates_and_drops_the_detail() {
        let mut settings = settings();
        settings.max_peer_series = 0;
        let status = sample_status();
        let text = render(Ok(&status), &settings);
        assert!(text.contains("tdcc_peers{state=\"serving\"} 1"));
        assert!(text.contains("tdcc_peers_truncated 1"));
        assert!(!text.contains("tdcc_peer_rtt_seconds{"));
    }

    #[test]
    fn hostile_label_values_cannot_forge_a_series() {
        let mut status = sample_status();
        status.gpus[0].name = "evil\" } 999\ntdcc_up{forged=\"".into();
        let text = render(Ok(&status), &settings());
        // The quote and the newline are escaped, so the whole thing stays one
        // label value on one line.
        assert!(text.contains(r#"name="evil\" } 999\ntdcc_up{forged=\"""#));
        // No forged series appears, and tdcc_up keeps exactly one sample.
        let samples = parse(&text).samples;
        for (series, _) in &samples {
            assert!(!series.starts_with("tdcc_up{"), "forged series {series}");
        }
        assert_eq!(
            samples
                .iter()
                .filter(|(series, _)| series == "tdcc_up")
                .count(),
            1
        );
    }

    #[test]
    fn label_values_are_length_capped() {
        let mut status = sample_status();
        status.gpus[0].name = "g".repeat(4_096);
        let text = render(Ok(&status), &settings());
        assert!(text.contains(&format!("name=\"{}\"", "g".repeat(MAX_LABEL_VALUE_CHARS))));
        assert!(!text.contains(&"g".repeat(MAX_LABEL_VALUE_CHARS + 1)));
    }

    #[test]
    fn absent_optional_readings_are_omitted_not_zeroed() {
        let mut status = sample_status();
        status.routing_metrics.avg_tokens_per_second = None;
        status.peers[0].rtt_ms = None;
        let text = render(Ok(&status), &settings());
        // Match a sample line, not the HELP/TYPE lines that still declare it.
        assert!(!text.contains("\ntdcc_completion_tokens_per_second "));
        assert!(!text.contains("tdcc_peer_rtt_seconds{"));
        // The families are still declared, so the metric is documented even
        // when this scrape has nothing to say about it.
        assert!(text.contains("# TYPE tdcc_completion_tokens_per_second gauge"));
    }

    #[test]
    fn unknown_node_and_peer_states_report_all_zeroes_rather_than_a_new_label() {
        let mut status = sample_status();
        status.node_state = "hibernating".into();
        status.peers[0].state = "hibernating".into();
        let text = render(Ok(&status), &settings());
        assert!(!text.contains("hibernating"));
        for state in NODE_STATES {
            assert!(text.contains(&format!("tdcc_node_state{{state=\"{state}\"}} 0")));
            assert!(text.contains(&format!("tdcc_peers{{state=\"{state}\"}} 0")));
        }
    }

    #[test]
    fn fixed_series_count_is_accurate() {
        // A node with no models, no GPUs and no peers produces exactly the
        // fixed part of the exposition. Keep FIXED_SERIES honest — the README's
        // cardinality bound is built on it.
        let status = NodeStatus {
            routing_metrics: RoutingMetrics {
                avg_tokens_per_second: Some(0.0),
                ..RoutingMetrics::default()
            },
            ..NodeStatus::default()
        };
        let text = render(Ok(&status), &settings());
        assert_eq!(parse(&text).samples.len(), FIXED_SERIES);
        assert!(
            text.contains(&format!("tdcc_exporter_series {FIXED_NODE_SERIES}")),
            "the exporter's own count of node series must match FIXED_NODE_SERIES"
        );
    }

    #[test]
    fn mean_reconstruction_recovers_the_original_integer_total() {
        // 7 attempts averaging 142.857142857 ms came from a 1000 ms total.
        let total = mean_ms_to_total_seconds(1_000.0 / 7.0, 7);
        assert_eq!(total, 1.0);
        assert_eq!(mean_ms_to_total_seconds(0.0, 5), 0.0);
        assert_eq!(mean_ms_to_total_seconds(10.0, 0), 0.0);
        assert_eq!(mean_ms_to_total_seconds(f64::NAN, 5), 0.0);
    }

    #[test]
    fn special_float_values_use_prometheus_spellings() {
        assert_eq!(format_value(f64::NAN), "NaN");
        assert_eq!(format_value(f64::INFINITY), "+Inf");
        assert_eq!(format_value(f64::NEG_INFINITY), "-Inf");
        assert_eq!(format_value(1.0), "1");
        assert_eq!(format_value(0.5), "0.5");
        assert_eq!(format_value(25_769_803_776.0), "25769803776");
    }

    #[test]
    fn a_repeated_family_declaration_is_ignored() {
        let mut out = Exposition::new();
        out.family("tdcc_demo", MetricKind::Gauge, "first");
        out.family("tdcc_demo", MetricKind::Counter, "second");
        let text = out.finish();
        assert_eq!(text.matches("# HELP tdcc_demo").count(), 1);
        assert!(text.contains("# TYPE tdcc_demo gauge"));
    }

    #[test]
    fn duplicate_samples_are_counted_and_dropped() {
        let mut out = Exposition::new();
        out.family("tdcc_demo", MetricKind::Gauge, "help");
        out.sample("tdcc_demo", &[("a", "1")], 1.0);
        out.sample("tdcc_demo", &[("a", "1")], 2.0);
        assert_eq!(out.written(), 1);
        assert_eq!(out.duplicates_dropped(), 1);
        assert_eq!(out.finish().matches("tdcc_demo{a=\"1\"}").count(), 1);
    }
}
