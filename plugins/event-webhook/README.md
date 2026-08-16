# event-webhook

Gets mesh events out of a TDCC node and into wherever your team already talks.

Subscribes to the node's mesh events, filters and coalesces them, and delivers
them to one HTTP endpoint as a generic JSON payload, a Slack message, or a
Discord embed. Delivery is queued, bounded, and retried; the node never waits on
it.

| Surface | Declaration | Where it lands |
| --- | --- | --- |
| `events` | all six `proto::mesh_event::Kind` values | `on_mesh_event`, which enqueues and returns |
| `mcp` | `tool("status")`, `tool("test")` | `event-webhook.status`, `event-webhook.test` |
| `health` | queue and delivery state | the plugin row in the console |

No web UI, no HTTP routes, no config schema, no mesh channels — see
[Why there is no config schema](#why-there-is-no-config-schema).

---

## What the node actually sends

This matters more than it usually would, because the obvious expectation is
wrong. The host delivers exactly **six** mesh event kinds to a plugin over the
control connection, and none of them is a model event:

| Host kind | Emitted as | When |
| --- | --- | --- |
| `PEER_UP` | `peer.up` | a peer becomes known, including the replay of every existing peer at startup |
| `PEER_DOWN` | `peer.down` | a peer is pruned |
| `PEER_UPDATED` | `peer.updated` | a peer's announcement changed |
| `LOCAL_ACCEPTING` | `node.accepting` | this node started accepting inbound connections |
| `LOCAL_STANDBY` | `node.standby` | this node is not accepting |
| `MESH_ID_UPDATED` | `mesh.id_updated` | this node's mesh id changed |

Two more are **derived**, not received:

| Derived event | How |
| --- | --- |
| `model.loaded` | a model appeared in a peer's `serving_models` between two updates |
| `model.unloaded` | a model disappeared from `serving_models`, or the peer went down |

The first sighting of a peer establishes a baseline and emits **no** model
events. On startup the host replays a `peer.up` for every peer it already knows,
and announcing forty already-running models as "just loaded" would be a lie.
Model events only ever describe a transition this process actually observed.

Three honest gaps, so nothing here is mistaken for something it is not:

- **There is no runtime-error or shutdown event for plugins.** The node's
  internal `Error` / `Fatal` / `Shutdown` output events are terminal-console
  events (`tdcc-events`), not part of the plugin protocol. `node.standby` is the
  closest available signal that this node stopped serving.
- **`MeshEvent.detail_json` is empty for every kind the host emits today.** The
  `detail` field is carried through if that ever changes; it is `null` now.
- **`capabilities` and `available_models` are always empty on the wire**, so
  they are not put in the payload. An always-empty `"capabilities": []` reads as
  "this peer has no capabilities", which is not what it means.

All six kinds are declared in the manifest even when your filter is narrower.
Delivery is allowlist-based — an undeclared kind never reaches this process at
all — and declaring statically keeps `tdcc plugins info` stable when you edit a
filter. Filtering happens in the plugin.

---

## Configuration

There is no `[plugin.settings]` block. Everything is an environment variable,
and the non-secret knobs are also `[[plugin]].args`, with args winning.

### The webhook URL is a credential

Anyone holding a Slack or Discord incoming-webhook URL can post into that
channel as you. So:

- It is read **only** from the environment: `TDCC_EVENT_WEBHOOK_URL`, falling
  back to `TDCC_PLUGIN_URL` (i.e. `[[plugin]].url`).
- Passing `--url`, `--webhook-url`, or `--webhook` is **refused at startup**
  with a pointer to the variable. Command-line arguments are visible to every
  process on the machine and are recorded in the console.
- It is never logged. Startup lines, health text, error messages, the `status`
  tool, and the `test` tool all show `https://hooks.slack.com/[redacted]` —
  scheme, host, and port only.
- Failure text borrowed from `reqwest` or from a response body is scrubbed: the
  full URL, the path, the query, and any path segment or query value of eight
  characters or more are replaced before the string is stored or printed.

The plugin process inherits the environment of `tdcc`, so the URL can live in a
systemd unit, a launchd plist, or a shell profile and stay out of
`config.toml` entirely.

```bash
export TDCC_EVENT_WEBHOOK_URL='https://hooks.slack.com/services/T000/B111/…'
tdcc client --port 9337 --console 3131 --config ./config.toml
```

### Everything else

| Argument | Environment variable | Default | Meaning |
| --- | --- | --- | --- |
| `--format` | `TDCC_EVENT_WEBHOOK_FORMAT` | `json` | `json`, `slack`, or `discord` |
| `--events` | `TDCC_EVENT_WEBHOOK_EVENTS` | `all` | comma-separated event names, or `all` |
| `--queue-capacity` | `TDCC_EVENT_WEBHOOK_QUEUE_CAPACITY` | `512` | bounded queue depth, 1–100000 |
| `--coalesce-secs` | `TDCC_EVENT_WEBHOOK_COALESCE_SECS` | `15` | flood window; `0` disables coalescing |
| `--timeout-secs` | `TDCC_EVENT_WEBHOOK_TIMEOUT_SECS` | `10` | per-request timeout, 1–120 |
| `--max-attempts` | `TDCC_EVENT_WEBHOOK_MAX_ATTEMPTS` | `5` | attempts per event, 1–10 |
| `--allow-insecure-url` | `TDCC_EVENT_WEBHOOK_ALLOW_INSECURE_URL` | off | permit non-loopback `http://` |

Filter names accept both spellings — `peer.up` and the host's own `peer_up` —
because you will read both. An unknown name **fails at startup**: a typo that
silently means "send nothing" is the worst possible failure for an alerting
plugin.

```toml
# ~/.tdcc/config.toml
version = 1

[[plugin]]
name = "event-webhook"
enabled = true
args = ["--format", "slack", "--events", "peer.up,peer.down,model.loaded,model.unloaded"]
```

### Why there is no config schema

`[plugin.settings]` values never reach the plugin process — there is no settings
field in the launch contract or the initialize handshake. Declaring a config
schema would render controls in Configuration → Plugins that this process could
never read. Rather than ship dead controls, this plugin reads its own
configuration and says so.

---

## Reliability

The rule the design is built around: **the node must never wait on a webhook.**

- `on_mesh_event` does no I/O. It normalizes, filters, coalesces, and hands the
  result to a bounded queue with a non-blocking `try_send`. A webhook endpoint
  that has been down for an hour costs the node a counter, not a stalled control
  connection.
- **Coalescing** guarantees at most one delivery per key per window, where a key
  is `(event, peer, model)`. A peer flapping once a second for ten minutes
  produces about 40 messages on the default 15-second window, not 600. The number
  swallowed rides along on the next delivery for that key as `coalesced: N` and
  in the summary line, so the count is late but never lost. The tradeoff is
  stated plainly: a final suppressed run is only reported when that key next
  fires.
- **The queue is capped.** When it is full the newest event is dropped and
  counted, and a log line is written on the first drop and every hundredth
  after. The alternative — an unbounded queue — turns a webhook outage into
  memory growth on somebody else's machine.
- **Retries** are exponential with full jitter, from 500 ms up to a 30-second
  cap, bounded by `--max-attempts`. `Retry-After` is honoured in its
  delta-seconds form, clamped to 30 seconds.
- **Retry classification** is narrow on purpose: `2xx` succeeds; `408`, `425`,
  `429`, and `5xx` are retried; every other `4xx` is permanent and is **not**
  retried. A revoked or mistyped webhook URL returns `401`/`403`/`404`, and
  hammering it would look like an attack from the endpoint's side.
- Delivery is **serial**. Chat webhooks are rate limited, and a burst of
  parallel POSTs is the fastest way to get an integration throttled.
- Every discard path has its own counter: `filtered_out`, `coalesced`,
  `dropped_queue_full`, `dropped_no_target`, `failed`. Nothing vanishes
  uncounted.

With no destination configured the plugin still starts, counts every event as
`dropped_no_target`, and says so in `health` and `status`. It does not report
itself unhealthy — an unhealthy plugin invites a restart loop, and restarting
will not conjure an environment variable.

---

## Payloads

`--format json` — a generic envelope for your own receiver:

```json
{
  "source": "tdcc",
  "event": "model.unloaded",
  "severity": "warn",
  "summary": "qwen3-8b is no longer served by peer 0123456789ab…",
  "timestamp": "2023-11-14T22:13:20.000Z",
  "timestamp_ms": 1700000000000,
  "node_id": "fedcba98…",
  "mesh_id": "mesh-7",
  "model": "qwen3-8b",
  "peer": { "peer_id": "…", "short_id": "0123456789ab…", "role": "host", "vram_bytes": 25769803776, "rtt_ms": 12, "serving_models": [], "models": [] },
  "coalesced": 0,
  "detail": null
}
```

`--format slack` sends `text` plus one coloured attachment with fields.
`--format discord` sends a single embed with an ISO 8601 timestamp. Both cap
list fields (a peer serving 500 models still fits inside Discord's ~6000
character embed limit) and elide with `+N more` rather than silently cutting.

---

## Tools

Both are projected on the host MCP endpoint under the plugin namespace and are
callable over HTTP.

```bash
# What is configured, and is it working?
curl --fail -X POST http://127.0.0.1:3131/api/plugins/event-webhook/tools/status \
  -H 'Content-Type: application/json' -d '{}'

# Send one synthetic event and report the real HTTP result.
curl --fail -X POST http://127.0.0.1:3131/api/plugins/event-webhook/tools/test \
  -H 'Content-Type: application/json' -d '{"note":"checking the channel"}'
```

`status` returns the redacted destination, which variable it came from, the
active filter, queue depth, and every counter. `test` bypasses the filter and
the coalescer, POSTs a `webhook.test` event through the same client and retry
policy, and **returns an error** if no destination is configured or delivery
does not succeed — never an empty success.

---

## Security and blast radius

- **Outbound network only**, to the single configured URL. No listener, no
  filesystem access, no subprocesses, no shell.
- **Redirects are not followed.** A webhook endpoint that answers `302` is a
  misconfiguration, and following it would post mesh data somewhere the operator
  never named.
- **Cleartext is refused by default.** Plain `http://` is allowed only to a
  loopback host, or with the explicit `--allow-insecure-url` opt-in.
- **TLS is rustls with webpki roots**, so the plugin behaves identically on
  every target it is published for and does not depend on a system OpenSSL.
- **What leaves the machine**: peer ids, this node's id, the mesh id, peer role,
  VRAM, RTT, and model names. If those are sensitive in your deployment, narrow
  `--events` — `node.accepting,node.standby` carries no peer or model data at
  all.
- Bounded memory everywhere: the queue, the coalescing key map (4096 keys), and
  the per-peer model tracker (1024 peers) all have hard caps and shed rather
  than grow.

---

## Building against the SDK

`tdcc-plugin` is **not** published to crates.io under that name — it was renamed
from `mesh-llm-plugin` and its repository is private. A dependency line of
`tdcc-plugin = "0.72.1"` does not resolve. This crate therefore points at a
local checkout:

```toml
tdcc-plugin = { path = "../../../tdcc-mesh/crates/tdcc-plugin" }
```

That path assumes the main TDCC repository is checked out beside this one:

```text
token/
  tdcc-mesh/          the main repository (private)
  tdcc-plugins/       this repository
    plugins/event-webhook/
```

If your layout differs, either fix the path or add a `[patch]` section. **Once
the SDK is published, replace the whole line with a pinned version**:

```toml
tdcc-plugin = "0.72.1"
```

Pin it to a version compatible with the `tdcc` release you target: the
initialize handshake requires an exact protocol-version match, so a host and a
plugin built against mismatched protocol versions refuse to connect at startup.

`tdcc-plugin` builds its protocol types with `prost-build`, so the first build
pulls a vendored `protoc`. No system protobuf compiler is required.

---

## Build and test

```bash
cargo test
cargo build --release
```

The tests cover everything testable without a running host: event
normalization and the model-tracker diff, the coalescer's per-window guarantee
and its memory bound, argument/environment precedence, URL validation, URL
redaction and scrubbing, retry classification, the backoff jitter band,
`Retry-After` parsing, all three payload shapes, the UTC timestamp formatter
(including a leap day), and the queue's drop accounting.

The retry policy is also proven against a real socket: a scripted TCP listener
answers one status per connection, so "was it retried?" is answered by how many
connections it actually served.

---

## Package and install locally

macOS or Linux, from this directory:

```bash
rm -rf target/package
mkdir -p target/package/event-webhook
cp target/release/event-webhook target/package/event-webhook/event-webhook
cp plugin.toml target/package/event-webhook/plugin.toml
cp README.md target/package/event-webhook/README.md
tar -C target/package -czf target/event-webhook-0.1.0-local.tar.gz event-webhook

tdcc plugins install --archive ./target/event-webhook-0.1.0-local.tar.gz \
  --name event-webhook --version 0.1.0
tdcc plugins info event-webhook
```

Windows uses `event-webhook.exe` and a `.zip` whose single top-level directory
is `event-webhook/`:

```powershell
Compress-Archive -Path target\package\event-webhook `
  -DestinationPath target\event-webhook-0.1.0-local.zip -Force
tdcc plugins install --archive .\target\event-webhook-0.1.0-local.zip `
  --name event-webhook --version 0.1.0
```

This plugin declares neither a config schema nor a web UI, so
`--print-package-manifest` emits `{}` and `plugin-manifest.json` may be left out
of the archive.

Set `TDCC_PLUGIN_DIR` to an empty directory first if you do not want a local
build landing in your real plugin store.

---

## Try it without Slack

Any process that accepts a POST works. With Python:

```bash
python3 -c "
from http.server import BaseHTTPRequestHandler, HTTPServer
class H(BaseHTTPRequestHandler):
    def do_POST(self):
        n = int(self.headers.get('content-length', 0))
        print(self.rfile.read(n).decode())
        self.send_response(204); self.end_headers()
HTTPServer(('127.0.0.1', 9099), H).serve_forever()
"
```

```bash
export TDCC_EVENT_WEBHOOK_URL='http://127.0.0.1:9099/hook'   # loopback: no opt-in needed
tdcc client --port 9337 --console 3131 --config ./config.toml
```

Then fire a test delivery with the `test` tool, and watch peers arrive as you
start a second node.

---

## Troubleshooting

| Symptom | What it means |
| --- | --- |
| `Error: TDCC_PLUGIN_ENDPOINT is not set for plugin process` | You ran the binary directly. The host owns the control endpoint; this is correct behaviour. |
| `health` says `no webhook target configured` | Neither `TDCC_EVENT_WEBHOOK_URL` nor `[[plugin]].url` is set in the environment `tdcc` actually runs with. |
| `status` shows `dropped_no_target` climbing | Same cause; events are arriving and being counted, with nowhere to go. |
| `status` shows `dropped_queue_full` climbing | The endpoint is slower than the event rate. Raise `--queue-capacity`, raise `--coalesce-secs`, or narrow `--events`. |
| `last_error` shows `HTTP 403` or `HTTP 404` | The webhook URL is wrong or revoked. It is not retried by design; fix the URL and restart the plugin. |
| Nothing arrives and every counter is zero | The filter excluded everything, or the mesh is genuinely quiet. `status` prints the parsed filter. |
| A flapping peer produces one message every 15 seconds | Working as intended. The `coalesced` count says how many were folded in. |

Configuration changes take effect on the next plugin start, not in an active
session.

## License

Apache-2.0.
