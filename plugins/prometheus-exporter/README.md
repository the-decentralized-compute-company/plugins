# prometheus-exporter

Publishes a TDCC node's state in [Prometheus text exposition
format](https://prometheus.io/docs/instrumenting/exposition_formats/) so it can
be scraped into monitoring you already run.

```text
http://127.0.0.1:3131/api/plugins/prometheus-exporter/http/metrics
```

Ships with a [Grafana dashboard](dashboards/tdcc-node.json), an [example scrape
config](prometheus/scrape-config.yml) and [alerting
rules](prometheus/alerts.yml).

It reads. It does not write, does not push, does not touch the filesystem, does
not run a subprocess, and only ever talks to loopback — see
[Blast radius](#blast-radius).

> This is **pull**-based scraping. The first-party [`metrics`
> plugin](https://github.com/the-decentralized-compute-company/metrics) is the
> **push** side: it advertises metrics support so `tdcc` can send telemetry to
> an OTLP collector configured in `tdcc` itself. The two do not overlap and can
> both be enabled.

---

## Quick start

Build it, package it, install it, enable it, scrape it.

```bash
cd plugins/prometheus-exporter
cargo build --release
```

```bash
# Package (macOS / Linux)
rm -rf target/package && mkdir -p target/package/prometheus-exporter
cp target/release/prometheus-exporter target/package/prometheus-exporter/prometheus-exporter
cp plugin.toml README.md target/package/prometheus-exporter/
cp -R dashboards prometheus target/package/prometheus-exporter/
cp ../../LICENSE target/package/prometheus-exporter/
tar -C target/package -czf target/prometheus-exporter-0.1.0-local.tar.gz prometheus-exporter

tdcc plugins install --archive ./target/prometheus-exporter-0.1.0-local.tar.gz \
  --name prometheus-exporter --version 0.1.0
```

```powershell
# Package (Windows) — the executable must be named exactly prometheus-exporter.exe
Remove-Item -Recurse -Force target\package -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force target\package\prometheus-exporter | Out-Null
Copy-Item target\release\prometheus-exporter.exe target\package\prometheus-exporter\
Copy-Item plugin.toml, README.md target\package\prometheus-exporter\
Copy-Item -Recurse dashboards, prometheus target\package\prometheus-exporter\
Copy-Item ..\..\LICENSE target\package\prometheus-exporter\
Compress-Archive -Path target\package\prometheus-exporter `
  -DestinationPath target\prometheus-exporter-0.1.0-local.zip -Force

tdcc plugins install --archive .\target\prometheus-exporter-0.1.0-local.zip `
  --name prometheus-exporter --version 0.1.0
```

Enable it in `~/.tdcc/config.toml`:

```toml
version = 1

[[plugin]]
name = "prometheus-exporter"
```

Restart `tdcc`, then check the wiring from the outside:

```bash
curl --fail http://127.0.0.1:3131/api/plugins/prometheus-exporter/http/metrics
```

or from an agent, using the MCP tool:

```bash
curl --fail -X POST \
  http://127.0.0.1:3131/api/plugins/prometheus-exporter/tools/check \
  -H 'Content-Type: application/json' -d '{}'
```

```json
{
  "up": true,
  "node_api": "http://127.0.0.1:3131",
  "scrape_url": "http://127.0.0.1:3131/api/plugins/prometheus-exporter/http/metrics",
  "collect_seconds": 0.0031,
  "node_series": 51,
  "max_series": 635,
  "limits": { "max_peer_series": 64, "max_model_series": 32, "max_gpu_series": 32, "collect_timeout_ms": 2000 }
}
```

Finally, point Prometheus at it with
[`prometheus/scrape-config.yml`](prometheus/scrape-config.yml) and import
[`dashboards/tdcc-node.json`](dashboards/tdcc-node.json) into Grafana.

---

## Scraping from another machine

You cannot, directly, and that is `tdcc`'s decision rather than this plugin's.
Every `/api/plugins/…` route is **trusted-local**: the host requires a loopback
peer address *and* a loopback `Host` header, so a remote Prometheus is refused
before this plugin is ever consulted.

Two shapes that do work:

1. **A Prometheus agent per node**, scraping its own `127.0.0.1:3131` and
   `remote_write`-ing to a central store. This is the normal answer for a mesh
   of machines owned by different people, because nobody has to open a port.
2. **An authenticated reverse proxy** on the node, listening on a real
   interface and forwarding to `127.0.0.1:3131` with a loopback `Host` header.
   You own the authentication in that case; the node API has none of its own on
   this path.

---

## What it exposes

All metric names are prefixed `tdcc_`, all durations are seconds, all sizes are
bytes, and every counter ends in `_total`. Every family carries `# HELP` and
`# TYPE`.

### Exporter and node

| Metric | Type | Labels | Meaning |
| --- | --- | --- | --- |
| `tdcc_up` | gauge | — | 1 when the exporter read node state on this scrape. |
| `tdcc_exporter_build_info` | gauge | `version` | Plugin version. Always 1. |
| `tdcc_exporter_collections_total` | counter | `outcome` | Collections attempted, `success` or `error`. |
| `tdcc_exporter_collect_duration_seconds` | histogram | — | How long reading node state took. |
| `tdcc_exporter_series` | gauge | — | Node-derived series in this response. |
| `tdcc_exporter_duplicate_series_dropped` | gauge | — | Samples suppressed because their label set repeated. |
| `tdcc_node_info` | gauge | `node_id`, `version`, `hostname`, `mesh_id` | Node identity. Always 1. |
| `tdcc_node_state` | gauge | `state` | 1 for the current state, 0 for the other three. |
| `tdcc_node_is_host` / `tdcc_node_is_client` | gauge | — | Which side of the mesh this node is on. |
| `tdcc_node_runtime_ready` | gauge | — | 1 when the local serving runtime is ready. |
| `tdcc_node_vram_bytes` | gauge | — | VRAM this node advertises to the mesh. |

### Requests

| Metric | Type | Labels | Meaning |
| --- | --- | --- | --- |
| `tdcc_requests_in_flight` | gauge | — | Requests being served or routed right now. |
| `tdcc_requests_in_flight_peak` | gauge | — | Highest concurrency since the node started. |
| `tdcc_requests_total` | counter | `outcome` | Requests fronted by this node: `success`, `failure`. |
| `tdcc_requests_served_total` | counter | `service` | Which service took them: `local`, `remote`, `endpoint`. A request that found no service is not counted here, so these do not sum to `tdcc_requests_total`. |
| `tdcc_request_retries_total` | counter | — | Attempts beyond the first, across all requests. |
| `tdcc_request_failovers_total` | counter | — | Requests that needed more than one attempt. |
| `tdcc_request_attempts_total` | counter | `target` | Attempts by target: `local`, `remote`, `endpoint`. |
| `tdcc_request_attempt_failures_total` | counter | `reason` | `timeout`, `unavailable`, `context_overflow`, `rejected`. |
| `tdcc_request_attempt_duration_seconds` | summary | — | `_sum` and `_count` only. See [Latency](#latency). |
| `tdcc_request_queue_wait_seconds` | summary | — | `_sum` and `_count` only. |

### Throughput

| Metric | Type | Labels | Meaning |
| --- | --- | --- | --- |
| `tdcc_completion_tokens_total` | counter | — | Completion tokens the router observed. `rate()` this for tokens/second. |
| `tdcc_completion_throughput_samples_total` | counter | — | Attempts that produced a throughput sample. |
| `tdcc_completion_tokens_per_second` | gauge | — | The node's own lifetime mean. Absent until a sample exists. |

### Models and VRAM

| Metric | Type | Labels | Meaning |
| --- | --- | --- | --- |
| `tdcc_models_loaded` | gauge | — | Local model processes, ready or not. |
| `tdcc_model_processes` | gauge | `model`, `backend` | Processes per model. |
| `tdcc_model_processes_ready` | gauge | `model`, `backend` | How many of them report `ready`. |
| `tdcc_model_context_length_tokens` | gauge | `model`, `backend` | Largest context window offered. |
| `tdcc_primary_model_size_bytes` | gauge | — | On-disk size of the primary model. |
| `tdcc_gpu_vram_bytes` | gauge | `gpu`, `name` | Detected VRAM per local GPU. |
| `tdcc_gpu_vram_reserved_bytes` | gauge | `gpu`, `name` | VRAM held back from model placement. |
| `tdcc_gpu_vram_allocatable_bytes` | gauge | `gpu`, `name` | VRAM available for model placement. |

### Peers

| Metric | Type | Labels | Meaning |
| --- | --- | --- | --- |
| `tdcc_peers` | gauge | `state` | Peer count by state. Always emitted. |
| `tdcc_peers_truncated` | gauge | — | Peers dropped from the per-peer series below. |
| `tdcc_peer_info` | gauge | `peer`, `role`, `version` | Peer identity. Always 1. |
| `tdcc_peer_serving` | gauge | `peer` | 1 when the peer reports it is serving. |
| `tdcc_peer_vram_bytes` | gauge | `peer` | VRAM the peer advertises. |
| `tdcc_peer_rtt_seconds` | gauge | `peer` | Last measured round-trip time. Absent when unmeasured. |
| `tdcc_peer_latency_seconds` | gauge | `peer` | Latency the router uses, possibly an estimate. |
| `tdcc_peer_latency_age_seconds` | gauge | `peer` | How stale that reading is. |

Everything in the request, throughput and peer tables is measured **by this
node, about this node**. The node API is explicit that these are local
observations, not mesh-wide aggregates, and the `HELP` strings repeat it. Sum
across instances in PromQL if you want a fleet figure.

---

## Cardinality

Series count is bounded, and the bound is:

```text
  39  fixed node series
+ 20  exporter self-series  (the only ones emitted when tdcc_up is 0)
+  3 x models   (default cap 32)
+  3 x GPUs     (default cap 32)
+  6 x peers    (default cap 64)
= 635 series at the default caps
```

Both constants are asserted by a test that renders an empty node and counts the
result, so this table cannot drift away from the code.

The things that make Prometheus fall over are not in any label:

- **No request ids, prompts, response bodies, or user identifiers.** The
  exporter never sees them; the node API does not expose them and the exporter
  reads nothing else.
- **No free-form status strings.** The node's model status is an open-ended
  string, so it is folded into `tdcc_model_processes_ready` (0/1 per process)
  instead of becoming a `status` label. Same for peer latency source.
- **Fixed enum label sets.** `tdcc_node_state` and `tdcc_peers` always emit all
  four states, including zeros. A node reporting an unrecognised state produces
  four zeros, not a fifth series.
- **Capped repeating groups.** Peers, models and GPUs each have a hard ceiling;
  entries beyond it are dropped deterministically (peers sorted by id) and the
  drop is visible in `tdcc_peers_truncated`. `--max-peer-series 0` turns the
  per-peer detail off entirely and keeps the aggregates.
- **Escaped, length-capped label values.** Model names, GPU names and hostnames
  come from the operator's machine, so quotes, backslashes and newlines are
  escaped, and values are cut at 128 characters. A GPU renamed to
  `evil" } 999\ntdcc_up{forged="` cannot forge a series; there is a test for
  exactly that.
- **No duplicate label sets, ever.** Prometheus rejects a whole scrape that
  contains one. If node data collapses two things onto one label set the
  exporter drops the second and counts it in
  `tdcc_exporter_duplicate_series_dropped`.

`tdcc_exporter_series` reports what this node is actually contributing, so you
can watch the number rather than trust this section.

---

## Latency

**Request latency is a summary with no quantiles, not a histogram.** That is a
deliberate limitation and worth understanding before you build an SLO on it.

The node tracks latency as a cumulative attempt count and a cumulative *mean*.
It publishes no buckets, no quantiles, and no per-request events — there is no
endpoint, event stream or plugin surface that carries an individual request
duration. So the exporter can honestly publish:

```promql
rate(tdcc_request_attempt_duration_seconds_sum[10m])
  / rate(tdcc_request_attempt_duration_seconds_count[10m])
```

which is a correct **windowed mean**, reconstructed exactly: `_count` is the sum
of the three per-target attempt counters, and `_sum` is that count times the
node's mean, rounded back to the whole millisecond the node originally
accumulated. What it cannot publish is a p99, because nothing upstream knows
one.

Fabricating buckets from a mean would produce a graph that looks like a tail
latency and is not one. That is worse than an admitted gap, so it is not done.

What would fix it: the node's `routing_metrics` snapshot growing a bucketed
latency field (the collector already records every attempt's duration in
`record_attempt`, it just folds it into a running total). The moment it does,
this exporter can publish a real `tdcc_request_duration_seconds` histogram and
the summary can be deprecated.

The one genuine distribution here is
`tdcc_exporter_collect_duration_seconds`, because the exporter measures those
durations itself. It is a good proxy for "is the node's API loop blocked?" —
a loopback read that leaves the millisecond buckets usually means the node is
in trouble in ways users are also feeling.

---

## What this cannot reach

Written down so nobody goes looking for a metric that was never there.

| Wanted | Why it is missing |
| --- | --- |
| Per-request latency distribution | The node exposes a cumulative mean only. See [Latency](#latency). |
| Per-model VRAM residency | The node reports device VRAM and model *file* size, never how much of a model is resident. `tdcc_primary_model_size_bytes` is the closest honest proxy and is named as file size. |
| GPU utilisation, temperature, power | Not in the node API. Use `node_exporter` plus `dcgm-exporter` or `nvidia_gpu_exporter` alongside this one; they are the right tools and they already exist. |
| llama.cpp KV-cache usage and slot occupancy | Available at `/api/runtime/llama`, but it is a second request per scrape, exists only for one backend, and its contents are whatever the upstream server happens to expose. Deliberately out of scope. |
| Mesh-wide totals | Every counter here is this node's own view. Aggregate across instances in PromQL. |
| Model catalogue size, download progress | `/api/models` is a catalogue, not a loaded set; turning it into series would be an unbounded-cardinality mistake. |
| Per-peer bandwidth or error counts | The node publishes peer latency and state, not link counters. |

---

## Configuration

`[plugin.settings]` **never reaches a plugin process** — the host stores those
values and the console renders them, but nothing delivers them here. So this
plugin declares no settings schema (which would put dead knobs in the console)
and takes its configuration from `[[plugin]].url` and `[[plugin]].args`.

```toml
[[plugin]]
name = "prometheus-exporter"

# Node API base URL. Defaults to http://127.0.0.1:3131. Must be loopback.
url = "http://127.0.0.1:3131"

args = [
  "--max-peer-series", "64",     # 0 disables per-peer series, keeps aggregates
  "--max-model-series", "32",
  "--max-gpu-series", "32",
  "--collect-timeout-ms", "2000",
]
```

| Flag | Default | Notes |
| --- | --- | --- |
| `--node-url` | `TDCC_PLUGIN_URL`, else `http://127.0.0.1:3131` | Overrides `[[plugin]].url`. Loopback and `http://` only. |
| `--max-peer-series` | `64` | `0` keeps only the `tdcc_peers` aggregates. Ceiling 4096. |
| `--max-model-series` | `32` | Ceiling 4096. |
| `--max-gpu-series` | `32` | Ceiling 4096. |
| `--collect-timeout-ms` | `2000` | 1–60000. A timeout produces `tdcc_up 0`, not a failed scrape. |

Both `--flag value` and `--flag=value` work. Bad configuration fails at
startup, naming the offending value — not silently on a scrape nobody is
watching.

Changing any of this takes effect on the next `tdcc` start or reload.

---

## Failure behaviour

The two surfaces fail differently on purpose.

**The scrape endpoint always returns `200` with a valid exposition.** When node
state cannot be read it emits `tdcc_up 0`, drops every node series (a gap is
visible on a graph; a stale value is not), keeps the exporter's own series, and
puts the reason on a comment line:

```text
# collection failed: cannot reach http://127.0.0.1:3131: connection refused
# HELP tdcc_up 1 when the exporter read node state successfully on this scrape…
# TYPE tdcc_up gauge
tdcc_up 0
```

That is what lets you alert on *the node* being down (`tdcc_up == 0`) separately
from *the scrape* being broken (`up == 0`), and it keeps the reason in front of
anyone who curls the endpoint without putting an unbounded error string into a
label.

**The `check` tool returns a hard error instead.** A tool that cannot reach its
backend should say so, not return an empty success.

Plugin health is independent of both: it reports cached collection counters and
never touches the network. A node that is down is the thing this plugin exists
to report, not a reason for the host to restart the reporter.

---

## Blast radius

Installing a plugin runs native code on your machine with your privileges.
Here is precisely what this one does.

- **Network: one outbound TCP connection per scrape, to loopback only.** The
  configured URL is rejected at startup unless its host is `localhost`, an IPv4
  loopback address or `[::1]`, and after connecting the exporter checks that the
  address it actually resolved to is loopback before sending anything. A
  hosts-file entry pointing `localhost` somewhere else does not turn this into
  an outbound network client.
- **No TLS stack is linked at all.** An `https://` URL is refused at startup
  with an explanation rather than accepted and then failed.
- **Read-only.** One `GET /api/status`. It never calls a mutating route, and
  `/api/status` is a non-privileged read on the node API.
- **No filesystem access, no subprocess, no shell.** The only file the process
  opens is the control socket the host hands it.
- **No listener of its own.** The `/metrics` route is served over a host-
  negotiated side stream that exists for exactly one scrape.
- **No secrets.** Nothing key-shaped is read or stored. `GET /api/status`
  includes a `token` field; it has no field to deserialize into and can never
  reach the exposition. Do not add one.
- **Dependencies:** `tdcc-plugin`, `tokio`, `serde`, `serde_json`, `schemars`,
  `anyhow`, and `httparse` — the last of which has no transitive dependencies
  and is why the exporter parses HTTP itself instead of pulling in a client
  stack. `Cargo.lock` is committed, so a release build resolves to exactly the
  125 crates that were reviewed rather than to whatever is newest that day.

---

## Building against the SDK

`tdcc-plugin` is **not published on crates.io under that name** — it was
renamed from `mesh-llm-plugin` and its repository is private — so a plain
version dependency does not resolve. `Cargo.toml` points at a local checkout:

```toml
tdcc-plugin = { path = "../../../tdcc-mesh/crates/tdcc-plugin" }
```

That path assumes `tdcc-mesh` sits beside `tdcc-plugins`:

```text
token/
  tdcc-mesh/        <- the main repository
  tdcc-plugins/     <- this repository
```

If your checkout is elsewhere, change the path, or leave it alone and add a
patch to `.cargo/config.toml`. **Once the SDK is published to a registry**,
replace the path dependency with a pinned version matching the `tdcc` release
you target:

```toml
tdcc-plugin = "0.72.1"
```

The initialize handshake requires an exact protocol-version match, so a host
and a plugin built against mismatched SDKs refuse to connect loudly at startup.
Pin, do not float.

`tdcc-plugin` builds its protocol types with `prost-build`, which downloads a
vendored `protoc`. No system protobuf compiler is needed.

---

## How it works

```text
Prometheus                tdcc host                        this plugin
    |                        |                                  |
    |  GET /api/plugins/prometheus-exporter/http/metrics         |
    |----------------------->|                                  |
    |                        |  OpenStreamRequest (control)      |
    |                        |--------------------------------->|  bind side stream
    |                        |<---------------------------------|  endpoint
    |                        |  raw HTTP request (side stream)   |
    |                        |--------------------------------->|
    |                        |                                  |  GET /api/status
    |                        |<---------------------------------|  raw HTTP response
    |<-----------------------|  bytes copied verbatim            |
```

The interesting part is why `/metrics` is declared with `.stream_response()`.

A **buffered** plugin HTTP binding is a JSON operation: the host invokes the
handler, takes the JSON it returns, and writes it out as
`Content-Type: application/json`. Prometheus cannot scrape that — it needs
`text/plain; version=0.0.4`, and the plugin has to be the one to say so.

Declaring `.stream_response()` changes what the host does: instead of invoking
the operation it negotiates a short-lived side stream, forwards the raw HTTP
request down it, and copies whatever comes back straight through to the client.
So this plugin writes a complete HTTP/1.1 response, status line and headers
included, and Prometheus receives it verbatim.

The plugin still opens no socket of its own. The side stream is a local socket
(or a named pipe on Windows) that the *host* connects to, negotiated through the
control connection, alive for one scrape. The request is read only as far as
`\r\n\r\n`, because the host's half-close after forwarding is a real EOF on a
Unix socket and nothing at all on a Windows named pipe — reading to EOF would
work on one platform and hang on the other.

The same handler is also projected as the MCP tool
`prometheus-exporter.metrics`, which returns the identical exposition text
wrapped in JSON. That is the path an agent or a `curl` user takes; Prometheus
takes the HTTP route.

---

## Tests

```bash
cargo test
```

57 tests, covering everything that is testable without a running host:

- **`settings`** — URL parsing and every rule it enforces: non-loopback hosts,
  `https://`, credentials, paths, queries, unbracketed IPv6, bad ports, flag
  ceilings, both flag spellings.
- **`node`** — HTTP response head parsing (partial, case-insensitive headers,
  chunked refusal, malformed `Content-Length`), status deserialization from both
  a sparse and a full payload, and the attempt-count derivation.
- **`render`** — structural rules re-parsed out of the rendered text: every
  sample has a `HELP` and a `TYPE`, no family is declared twice, no series is
  emitted twice, counters end in `_total` and gauges do not, no metric name
  carries a non-base unit. Plus unit conversion, label escaping against a
  forged-series attempt, length capping, absent-versus-zero handling, unknown
  state handling, cumulative histogram buckets, the peer cap, and the exact
  fixed series count the cardinality bound is built on.
- **`collector`** — histogram bucketing including the `+Inf` edge and rejection
  of NaN/negative observations, plus three end-to-end scrapes over a real
  loopback socket: against a stub node API (asserting the metrics come out and
  that the API token in the payload does not), against a stub returning `503`
  (asserting the status code and body reach the caller), and against a dead port
  (asserting an unreachable node still yields a valid exposition).
- **`serve`** — the HTTP response bytes, header-terminator detection, that
  reading stops at the terminator without waiting for EOF, that a truncated
  request is an error, and that a stream for another binding is rejected.
- **`main`** — the manifest declares `/metrics` as a `GET` with a streamed
  response, the names match everywhere, and the package manifest is `{}`.

Not covered here, because it needs a live host: the side-stream handshake
itself. Verify that with the checklist in the repository README under *Test
before publishing*, at minimum items 1, 2, 3, 4 and 5.

---

## License

Apache-2.0, same as the rest of this repository.
