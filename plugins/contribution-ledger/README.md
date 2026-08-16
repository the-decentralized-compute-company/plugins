# contribution-ledger

An honest local record of what this node gave to the mesh: requests it fronted
and answered on its own hardware, completion tokens the host observed, how long
it spent inside routing attempts, how long it was available to serve, and which
peers it shared a mesh with.

It writes one aggregated bucket per UTC hour to an append-only file on this
machine, exposes that file through five MCP tools and five HTTP routes, and puts
a **Contribution** page in the operator console.

---

## What this is not

**This is not a payment system, a currency, or a claim on anything.**

There is no balance, no credit, no token-as-money, no transfer, and nothing here
settles against anything. That is a design constraint, not an omission. The
moment a plugin implies value transfer it takes on a trust and compliance
problem that belongs to a product decision and a legal review, not to a
third-party binary running on someone else's hardware. If a payment layer ever
exists, it will need audited inputs and an adversarial threat model; this file
is neither.

**This is not proof of anything to anybody else.** A local record is
self-reported by definition. It is this node's own statement about its own work,
produced by this node from counters this node kept, stored on this node's own
disk, and trivially editable by whoever owns the machine. Every tool response
carries that sentence in a `disclaimer` field, and the console page prints it
above the fold, so a number from here cannot be quoted without it.

**It is deliberately not signed.** A signature over self-reported data would
attest only "this node said this", never "this is true" — and there is no
verifier, no key distribution, and no revocation story to make even that weak
claim useful. Shipping one would be security theatre that invites exactly the
misreading the rest of this document is written to prevent. If you want to know
whether the file changed since you last looked, hash it yourself.

---

## Where the numbers come from

Two independent sources, and nothing else.

### 1. `GET /api/status`, sampled over loopback

The host's management API exposes a `routing_metrics` block of monotonic,
process-lifetime counters. The ledger reads it every `--poll-secs` (30 by
default) and records the **difference** between consecutive readings, never the
running total.

| Ledger field | Status payload field | Meaning |
| --- | --- | --- |
| `requests_fronted` | `routing_metrics.request_count` | Requests this node accepted on its own API surface |
| `requests_succeeded` | `routing_metrics.successful_requests` | Of those, the ones that completed |
| `served_locally` | `routing_metrics.pressure.locally_served_request_count` | Answered by this machine's own hardware |
| `served_remotely` | `routing_metrics.pressure.remotely_served_request_count` | Answered by another mesh node — consumption, not contribution |
| `served_by_endpoint` | `routing_metrics.pressure.endpoint_request_count` | Answered by an attached inference endpoint |
| `local_attempts` / `remote_attempts` / `endpoint_attempts` | `routing_metrics.local_node.*_attempt_count` | Routing attempts by target kind |
| `completion_tokens` | `routing_metrics.completion_tokens_observed` | Completion tokens the host observed |
| `attempt_ms` | `avg_attempt_ms` × total attempts | Reconstructed sum of attempt wall time |
| peers, node, mesh | `peers[].id`, `node_id`, `mesh_id`, `node_state` | Identity and presence |

The management API is served on the **console port** (`--console`, 3131 by
default), not on the OpenAI-compatible API port 9337. `GET /api/status` is a
read-only route and does not require the elevated trusted-local check that
`/api/plugins/*` does, so a plain loopback GET is enough.

### 2. Mesh lifecycle events, pushed by the host

The plugin declares `peer_up`, `peer_down`, `local_accepting`, `local_standby`,
and `mesh_id_updated`. These give peer presence and the serving-availability
windows that `accepting_hours` is measured from. They need no configuration and
work even with sampling turned off.

It declares **no mesh channels**: a local record must not gossip, and the host's
allowlist means it could not receive one even if something tried to send it.

---

## What this cannot tell you

These are gaps in what a plugin can observe today, not gaps in this plugin.
Every one of them is surfaced in the `caveats` array of a `summary` response, so
the limitation travels with the number.

**It cannot tell you which peers you served.** This is the biggest one, and it
is worth being precise about. When a peer sends this node work, the host relays
the request over a QUIC tunnel into the local API proxy, where it is counted the
same as a request typed on this machine. The requester's peer identity is known
at the tunnel — `handle_inbound_http_stream` has it — but it is not carried into
the routing metrics, and `record_request` has no field for it. So
`served_locally` mixes "work I did for myself" with "work I did for a peer", and
nothing downstream can separate them. The `peers` tool therefore reports
**co-presence only**: these peers shared a mesh with this node during this
bucket. It says so in its own `note` field. Do not read it as a served-for list.

**There is no per-request event for a plugin to subscribe to.** The host's
internal event taxonomy (`tdcc-events::OutputEvent`, including `RequestRouted`,
`ModelLoaded`, `PeerJoined`) is delivered through an in-process `OutputSink` that
terminal and TUI renderers install. It does not cross the plugin control
connection. The only events the protocol carries to a plugin are
`MeshEvent`, whose entire enum is `PEER_UP`, `PEER_DOWN`, `PEER_UPDATED`,
`LOCAL_ACCEPTING`, `LOCAL_STANDBY`, `MESH_ID_UPDATED`. Sampling is therefore not
a shortcut — it is the maximum resolution available.

**There is no GPU-time counter.** Nothing exposes GPU seconds, utilisation, or
occupancy to a plugin. `attempt_ms` is wall-clock time spent inside routing
attempts, reconstructed as `avg_attempt_ms × attempt_count`, summed across
local, remote, **and** endpoint targets — because the host publishes one mean,
not one per target kind. Treat it as a busy-time estimate. It is labelled that
way in the payload and in the console tile.

**Work is lost when the host restarts.** The counters are process-lifetime. When
a reading comes back lower than the previous one the ledger treats it as a
restart, takes the new reading as the delta, and increments `counter_resets` —
so the *existence* of the gap is recorded even though its size is not.

**Work finer than the poll interval is invisible in time, not in total.** Totals
are exact between successful polls; only the attribution to a particular hour is
approximate, bounded by one poll interval. Failed polls are counted in
`polls_failed` and raise a caveat, so under-counting is never silent.

**Per-model breakdown is deliberately absent.** `/api/models` carries per-model
routing metrics and the ledger does not read them. Which models were asked for
is information about what peers wanted, not about what this node gave.

**What would close these gaps.** A host-side per-request accounting surface for
plugins — even a coarse one — carrying the requester's peer id for tunnelled
requests and separating local from remote attempt time. Until that exists, no
plugin can honestly claim to know who it served or how much GPU it burned, and
this one does not.

---

## How it is stored

```text
<state dir>/journal.jsonl     append-only, one JSON object per line
<state dir>/cursor.json       disposable resume point
```

**Append-only on the hot path.** Sealing a bucket opens the file, writes one
line with a single `write_all`, flushes, fsyncs, and closes. Nothing rewrites
earlier bytes, so a crash can only truncate the newest line. On startup an
unparseable *final* line in a file that does not end in a newline is recognised
as exactly that, discarded, and reported — as `truncated_tail_at_startup` in
`status` and as a caveat in `summary`. A line that fails to parse anywhere else
is counted in `unreadable_lines_at_startup`, because that means something other
than this plugin is writing to the file and you should know.

**Aggregate, never per-request.** One record covers a whole UTC hour. A node
serving a million requests an hour writes the same 24 lines a day as an idle
one. There is no per-request row that could grow unbounded — and nothing in a
record could describe an individual request even if you wanted it to.

**Bounded forever.** Hourly rows older than `--compact-after-days` (14) are
merged into one row per UTC day. Daily rows older than `--retain-days` (400) are
dropped. Steady state is about 336 hourly rows plus 400 daily rows — under a
thousand lines, a few hundred kilobytes, for good.

One marker line is also written per plugin start, recording that the host's
counters went back to zero at that point. Those are capped at the most recent
128, because a plugin that crash-looped before its first bucket seal would add
one per restart and age alone would not bound them.

Compaction is the one operation that rewrites the file. It writes a sibling temp
file, fsyncs it, and renames it over the journal, so an interrupted compaction
leaves the previous journal intact and the next attempt simply retries. It runs
at most every six hours, once at every startup, and only when it actually has
something to do.

**Restarts.** History is read from disk once at startup and kept in memory
afterwards, so reads never touch the disk. `cursor.json` holds the last
cumulative reading folded into a bucket that is now durable, so a plugin restart
neither discards work the host already counted nor counts a written bucket
twice. It is written atomically, and it moves on every path that writes a
bucket — an hour rolling over and an explicit `flush` alike. Losing it costs at
most the counts between the last write and the restart, never any recorded
history. Counters that accrued while the ledger was not running land in the
bucket where it next polled, and the unobserved time shows up in
`observed_fraction`.

A partial hour reaches disk when the UTC hour rolls over, or when you call
`flush`. Call it before reading the raw file or stopping the node.

---

## Privacy

Never written to disk, and never returned by any tool:

- prompts and completions
- model names
- request identifiers, sizes, or timings of individual requests
- anything at all about the **content** of the work

Peer identifiers are pseudonymous but still identifying, and they are treated
that way:

- only the first **16 characters** of a peer id are stored — enough to
  distinguish peers, and short enough that the file is not a tidy export of full
  mesh identities. This is readability and minimisation, **not** anonymisation:
  anyone holding the mesh's peer list can match a prefix back;
- ids are stored as a **set per bucket**, capped at 64. The finest timing
  resolution any peer sighting has is "was around during this hour", never
  "joined at 14:03:22". When the cap bites, the record says `peers_truncated`
  rather than silently dropping names;
- `--no-peer-ids` stops peer ids being recorded at all. `summary` then says so
  in a caveat instead of reporting an empty list as if it were a real zero.

The console setting **Show peer ids** hides them in the UI. It does not affect
what is written — settings never reach the plugin process (see
[Who owns what](#who-owns-what)), so the recording switch has to be a process
argument.

---

## Security and blast radius

**Filesystem.** The ledger reads and writes exactly two file names —
`journal.jsonl` and `cursor.json`, plus their transient `.tmp` siblings — inside
exactly one directory, fixed at startup. No tool argument names a path, so no
caller can steer it. Point `--state-dir` somewhere and that is the whole
footprint. Escaping the configured root is not prevented by a check; it is
prevented by there being no input that could cause it.

**Network.** Outbound: one plain HTTP/1.1 `GET /api/status`, to a **loopback
address only**. The base URL is validated at startup and rejected unless it is
`http://` (no TLS stack is linked, so `https://` is refused rather than silently
downgraded) with a host that is a literal loopback IP or the exact word
`localhost` — which is mapped to `127.0.0.1` in-process rather than resolved, so
a hosts-file entry cannot redirect it off the machine. Paths, queries,
fragments, and credentials in the base URL are rejected; the ledger appends
`/api/status` itself. Responses are capped at 4 MiB and the whole request times
out after 10 seconds. No inbound listener, no subprocess, no shell.

**Secrets.** None. Nothing key-shaped is read, stored, logged, or accepted.

**Failure.** A ledger that cannot write is not a ledger, so an unusable state
directory fails the process at startup with a clear error rather than running
and quietly dropping history. A `summary` with nothing measured behind it
returns an **error naming the reason**, never a page of zeroes.

---

## Surfaces

| Surface | Declaration | Where it lands |
| --- | --- | --- |
| `provides` | `capability("contribution-ledger.v1")` | capability resolution and `tdcc plugins info` |
| `config` | `default_window_days`, `show_peer_ids` | Configuration → Plugins, rendered by the host's own controls |
| `web_ui` | one bundle, one page, one config section | `/plugins/contribution-ledger/contribution` |
| `events` | 5 mesh lifecycle events | presence and serving windows |
| `mcp` | `summary`, `epochs`, `peers`, `status`, `flush` | `contribution-ledger.summary` on the host MCP endpoint |
| `http` | `GET /summary`, `/epochs`, `/peers`, `/status`; `POST /flush` | `/api/plugins/contribution-ledger/http/...` |

### Tools

| Tool | Arguments | Answers |
| --- | --- | --- |
| `summary` | `days` (1–3650, default 7) | Aggregated totals, coverage, and caveats. **Errors** when nothing was measured. |
| `epochs` | `days`, `limit` (1–1000, default 50) | The raw buckets behind the summary, newest first. |
| `peers` | `days`, `limit` | Peers co-present during the window, by id prefix. |
| `status` | none | Ledger diagnostics. Always answers — this is what you call when `summary` refuses. |
| `flush` | none | Writes the in-progress bucket to the journal now. |

Out-of-range `days` and `limit` are clamped rather than rejected, so a wide
request degrades to the widest supported window instead of failing.

---

## Who owns what

- **The plugin owns the record.** The journal lives in the plugin's state
  directory, in the plugin process. The host never touches it; it only invokes
  the declared operations.
- **The host owns the settings.** `default_window_days` and `show_peer_ids` are
  declared by this plugin but stored in the host's `[plugin.settings]` and are
  *not* delivered to the plugin process — there is no settings field in the
  launch contract or the initialize handshake. The console bundle reads them
  from `host.config.visible.settings`. That is why every knob the *process*
  needs (state directory, poll interval, retention, peer recording) is a
  `[[plugin]].args` argument and not a setting.

---

## Configuration

```toml
version = 1

[[plugin]]
name = "contribution-ledger"
enabled = true
web_ui_enabled = true

# Passed to the process as TDCC_PLUGIN_URL. This is the management API — the
# same port the operator console is served on (--console, 3131 by default).
# Without it, the ledger records presence and uptime but measures no counters,
# and `summary` says so by failing rather than by returning zeroes.
url = "http://127.0.0.1:3131"

args = ["--poll-secs", "30"]

[plugin.settings]
default_window_days = 7
show_peer_ids = true
```

### Arguments

| Argument | Default | Meaning |
| --- | --- | --- |
| `--state-dir <PATH>` | `<plugin store>/contribution-ledger/ledger` | Where `journal.jsonl` and `cursor.json` live. The plugin store follows `TDCC_PLUGIN_DIR`, falling back to `~/.tdcc/plugins`. |
| `--host-api <URL>` | value of `[[plugin]].url` | Overrides the launch-contract URL. Loopback `http://` only. |
| `--no-host-api` | off | Deliberately record presence and uptime with no counter sampling. `summary` then answers with `measured: false` and a caveat instead of failing. |
| `--poll-secs <N>` | `30` | Sampling interval, 5–900. |
| `--no-peer-ids` | off | Never write peer ids to disk. |
| `--compact-after-days <N>` | `14` | Age at which hourly rows merge into daily rows. |
| `--retain-days <N>` | `400` | Age at which daily rows are dropped, 1–3650. |

An unknown argument is fatal. A typo that silently disabled the sampler would
produce a ledger full of honest-looking zeroes, which is the one failure mode
this plugin exists to prevent.

---

## Build

```bash
cargo test
cargo build --release
```

### The SDK dependency

`Cargo.toml` points `tdcc-plugin` at a **local path**:

```toml
tdcc-plugin = { path = "../../../tdcc-mesh/crates/tdcc-plugin" }
```

`tdcc-plugin` is not published to crates.io under that name — it was renamed
from `mesh-llm-plugin` and the `tdcc-mesh` repository is private — so
`tdcc-plugin = "0.72.1"` **will not resolve**. The path above assumes a sibling
checkout laid out as:

```text
<parent>/
  tdcc-mesh/crates/tdcc-plugin/
  tdcc-plugins/plugins/contribution-ledger/
```

Adjust it if yours differs, or add a `[patch]` section pointing at your
checkout.

**Once the SDK is published**, a public consumer replaces that one line with the
registry form and pins the exact version matching the `tdcc` release they
target:

```toml
tdcc-plugin = "0.72.1"
```

Nothing else in this crate changes. The initialize handshake requires an exact
protocol-version match, so host and plugin must be built against compatible SDKs
either way — a mismatch fails loudly at startup, not quietly at first use.

`tdcc-plugin` builds its protocol types with `prost-build`, so the first build
downloads a vendored `protoc`. No system protobuf compiler is required.

### Tests

`cargo test` covers the logic that is testable without a running host:

- UTC bucketing and date formatting, including leap days and the century rule
- argument parsing, range validation, and state-directory resolution
- loopback URL validation, in every accepted spelling and every refused one
- status-payload parsing, including missing fields and malformed payloads
- counter differencing, including host-restart detection
- journal parsing with a torn final line and with a corrupt middle line
- retention: hourly→daily compaction, expiry, idempotence, peer-set capping
- window aggregation, peer presence, argument clamping, observed-time capping
- the ledger refusing to summarise unmeasured work, and answering `status`
  anyway
- manifest assertions: one bundle root with matching `bundle_id` values, five
  event subscriptions, no mesh channels, no endpoints

---

## Package and install locally

macOS or Linux, from this directory:

```bash
rm -rf target/package
mkdir -p target/package/contribution-ledger
cp target/release/contribution-ledger target/package/contribution-ledger/contribution-ledger
cp plugin.toml target/package/contribution-ledger/plugin.toml
cp README.md target/package/contribution-ledger/README.md
cp -R bundle target/package/contribution-ledger/bundle
target/release/contribution-ledger --print-package-manifest \
  > target/package/contribution-ledger/plugin-manifest.json
tar -C target/package -czf target/contribution-ledger-0.1.0-local.tar.gz contribution-ledger

tdcc plugins install --archive ./target/contribution-ledger-0.1.0-local.tar.gz \
  --name contribution-ledger --version 0.1.0
tdcc plugins info contribution-ledger
```

Windows uses `contribution-ledger.exe` and a `.zip` whose single top-level
directory is `contribution-ledger/`:

```powershell
Remove-Item -Recurse -Force target\package -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force target\package\contribution-ledger | Out-Null
Copy-Item target\release\contribution-ledger.exe target\package\contribution-ledger\
Copy-Item plugin.toml, README.md target\package\contribution-ledger\
Copy-Item -Recurse bundle target\package\contribution-ledger\bundle
target\release\contribution-ledger.exe --print-package-manifest `
  | Out-File -Encoding utf8 target\package\contribution-ledger\plugin-manifest.json
Compress-Archive -Path target\package\contribution-ledger `
  -DestinationPath target\contribution-ledger-0.1.0-local.zip -Force

tdcc plugins install --archive .\target\contribution-ledger-0.1.0-local.zip `
  --name contribution-ledger --version 0.1.0
```

Because this plugin declares both a config schema and a web UI,
`plugin-manifest.json` is **required** in the archive. Confirm it landed valid:

```bash
tdcc plugins info contribution-ledger
```

The stored record at `~/.tdcc/plugins/contribution-ledger/plugin-install.json`
should carry `"validation": { "status": "valid" }` under `manifest.web_ui`.

Set `TDCC_PLUGIN_DIR` to an empty directory first if you do not want this
landing in your real plugin store — the ledger's default state directory follows
it, so a test run will not touch your real history either.

---

## Run it

```bash
tdcc auth status
# only if none exists, for local development:
tdcc auth init --no-passphrase
tdcc client --port 9337 --console 3131 --config ./config.toml
```

Then check each surface:

```bash
# Diagnostics first: this one always answers.
curl --fail -X POST http://127.0.0.1:3131/api/plugins/contribution-ledger/tools/status \
  -H 'Content-Type: application/json' -d '{}'

# Aggregated contribution.
curl --fail 'http://127.0.0.1:3131/api/plugins/contribution-ledger/http/summary?days=1'

# The buckets behind it, and who was around.
curl --fail 'http://127.0.0.1:3131/api/plugins/contribution-ledger/http/epochs?days=1&limit=5'
curl --fail 'http://127.0.0.1:3131/api/plugins/contribution-ledger/http/peers?days=1'

# Force the in-progress hour to disk, then read the raw file.
curl --fail -X POST http://127.0.0.1:3131/api/plugins/contribution-ledger/http/flush \
  -H 'Content-Type: application/json' -d '{}'
cat ~/.tdcc/plugins/contribution-ledger/ledger/journal.jsonl
```

On the host MCP endpoint the same tools are namespaced
`contribution-ledger.summary`, `contribution-ledger.status`, and so on.

Open `http://127.0.0.1:3131/`, use the **Contribution** navigation item, then
open Configuration → Plugins → Integrations to see the ledger's own status line
and change the page's window.

### Check that it fails honestly

Remove `url` from the `[[plugin]]` table and restart. `summary` must refuse:

```bash
curl -s http://127.0.0.1:3131/api/plugins/contribution-ledger/http/summary | head -c 400
```

It should name the missing configuration and tell you what to add — not return a
tidy object full of zeroes. `status` must still answer, with
`source.mode: "unset"`. The console page shows the refusal instead of an empty
dashboard.

### Check that the projection is independent of the process

Turning the web UI off must leave the tools and routes working:

```bash
curl --fail -X PATCH http://127.0.0.1:3131/api/plugins/contribution-ledger/web-ui/enabled \
  -H 'Content-Type: application/json' -d '{"enabled":false}'

# still 200
curl --fail -X POST http://127.0.0.1:3131/api/plugins/contribution-ledger/tools/status \
  -H 'Content-Type: application/json' -d '{}'

# now 404
curl -s -o /dev/null -w '%{http_code}\n' \
  http://127.0.0.1:3131/api/plugins/contribution-ledger/web-ui/assets/register-mesh-plugin-ui.js
```

---

## Prerequisites

1. A `tdcc` host built against a compatible plugin protocol version, and a
   checkout of `tdcc-mesh` to build against until the SDK is published.
2. **A management API to poll.** It is served on the console port, so `tdcc`
   must be running with a console port (`--console`, 3131 by default). Run
   without one and there is nothing to sample; the ledger will record presence
   and uptime and `summary` will fail with the reason.
3. `[[plugin]].url` (or `--host-api`) pointing at that port. There is no
   discovery for it: the launch contract passes `TDCC_PLUGIN_ENDPOINT`,
   `TDCC_PLUGIN_TRANSPORT`, `TDCC_PLUGIN_NAME`, `TDCC_PLUGIN_URL`, and
   `TDCC_PLUGIN_WEB_UI_DIR`, and none of those carries the API port unless you
   configure `url`.
4. A writable state directory. The default follows `TDCC_PLUGIN_DIR` and falls
   back to `~/.tdcc/plugins`; override it with `--state-dir`.
5. For console configuration writes, a local owner identity
   (`tdcc auth init --no-passphrase` for development).

Not required: a GPU, an inference backend, network access beyond loopback, or
any external service.

---

## Layout

```text
contribution-ledger/
  Cargo.toml
  plugin.toml                        package marker read by the installer
  README.md
  src/
    main.rs                          entrypoint + --print-package-manifest
    manifest.rs                      the single plugin! declaration
    config.rs                        args, launch-contract env, loopback URL validation
    clock.rs                         UTC bucketing and formatting
    journal.rs                       on-disk records, tolerant reads, retention
    ledger.rs                        accumulator, aggregation, tool payloads
    source.rs                        loopback HTTP GET and /api/status parsing
    sampler.rs                       the background poll loop
  bundle/
    register-mesh-plugin-ui.js       shippable browser ES module
    host-contract.d.ts               author types, copied from the exemplar
```

## License

Apache-2.0.
