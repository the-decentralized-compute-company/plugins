# TDCC Plugins

The plugin author guide, two teaching examples, and eleven working plugins for
[TDCC](https://decentralizedcompute.company) — Decentralized Compute.

Nothing in this repository is published to the plugin catalog or installable
with `tdcc plugins install <name>`. The plugins under
[`plugins/`](plugins/) are built from source and installed from a local
archive; the five first-party plugins listed further down live in their own
repositories and ship their own releases.

- **Building a plugin?** Read [The SDK is not on crates.io](#the-sdk-is-not-on-cratesio)
  first — it is the thing that will stop you in your first five minutes. Then
  [What a plugin is](#what-a-plugin-is), then copy
  [`examples/hello-plugin`](examples/hello-plugin).
- **Looking for a plugin?** Jump to the [plugin catalog](#plugin-catalog).
- **Contributing?** See [CONTRIBUTING.md](CONTRIBUTING.md).

---

## The SDK is not on crates.io

Every plugin in this repository depends on `tdcc-plugin`, and

```toml
tdcc-plugin = "0.72.1"
```

**does not resolve.** The crate is not published under that name: it was
renamed from `mesh-llm-plugin`, and the `tdcc-mesh` repository that holds it is
private. A fresh `cargo build` in a plugin directory fails at dependency
resolution, not at compile time, and the error does not explain why.

So every crate here points at a local checkout instead:

```toml
tdcc-plugin = { path = "../../../tdcc-mesh/crates/tdcc-plugin" }
```

That relative path assumes the two repositories are siblings:

```text
token/
  tdcc-mesh/          the main repository (private), providing crates/tdcc-plugin
  tdcc-plugins/       this repository
    plugins/web-search/
```

Two plugins need a second crate from the same checkout, for the same reason:

| Plugin | Also depends on | Why |
| --- | --- | --- |
| [`model-mirror`](plugins/model-mirror) | `model-ref` | So a mirror's artifact ids are the host's own canonical refs, derived by the host's own code |
| [`capability-attest`](plugins/capability-attest) | `tdcc-identity` (`host-io`) | Node key loading and owner-certificate verification, rather than a second implementation of either |

`capability-attest` also pins `ed25519-dalek = "=3.0.0-rc.0"` to match
`tdcc-identity`'s: the two crates exchange `VerifyingKey` values, so a second
copy in the graph is a type error rather than a silently working duplicate.

([`semantic-cache`](plugins/semantic-cache) writes the same thing as
`{ version = "0.72.1", path = "…" }`. Cargo uses the path and ignores the
version while the path is present; the version is there so that publishing day
is a one-key deletion.)

**If your checkout is laid out differently**, the simplest fix is to edit the
path in that plugin's `Cargo.toml`. A `[patch]` section is the alternative, but
it only rewrites *registry* dependencies — so you have to change the line to a
version requirement first, and then redirect it:

```toml
# Cargo.toml
tdcc-plugin = "0.72.1"

# .cargo/config.toml, above the crate
[patch.crates-io]
tdcc-plugin = { path = "/absolute/path/to/tdcc-mesh/crates/tdcc-plugin" }
```

Patching a dependency that is already a path dependency does nothing, and Cargo
reports it only as an unused-patch warning.

**Once the SDK is published**, replace the path dependency with a version pin
matching the `tdcc` release you target, and nothing else changes — no source
file in any of these plugins depends on the dependency being local:

```toml
tdcc-plugin = "0.72.1"
```

Pin it exactly. The initialize handshake requires an exact protocol-version
match, so a host and a plugin built against mismatched SDKs refuse to connect
loudly at startup rather than misbehaving later.

Every plugin here repeats this in its own README, under a heading like
*Building against the SDK*, with the exact lines for that crate.

---

## What a plugin is

A TDCC plugin is a **native process** that `tdcc` launches and supervises over
a single local control connection — a Unix socket on macOS and Linux, a named
pipe on Windows. The plugin declares what it contributes in a manifest; the
host projects that manifest onto its own MCP and HTTP surfaces.

The practical consequence: a plugin does not implement MCP JSON-RPC, does not
run an HTTP server, and does not negotiate a socket path. It declares
`tool("search")` and the host synthesizes `tools/list` and `tools/call` for it.

### Host owns / plugin owns

| The host (`tdcc`) owns | The plugin owns |
| --- | --- |
| Launching, supervising, restarting the plugin process | Its feature logic and handler implementations |
| The control connection and side-stream negotiation | Its local state and storage |
| Projecting the manifest onto MCP (`tools/*`, `resources/*`, `prompts/*`) | The contents of each operation, resource, and prompt response |
| HTTP serving, routing, and mounting under `/api/plugins/<name>/…` | Reading and writing stream payloads when invoked |
| Request validation against the declared schemas | Its own plugin-specific mesh channel semantics |
| Capability resolution and route collision detection | Whatever external service it supervises or attaches to |
| Serving web UI bundle assets, same-origin, from the installed package | The browser code inside that bundle |
| Storing and validating `[plugin.settings]` | Declaring the schema those settings are validated against |
| Owner identity, permissions, and policy enforcement | — |

Two boundaries are worth stating outright, because both are easy to assume
wrong:

- **`[plugin.settings]` never reaches the plugin process.** There is no
  settings field in the launch contract or the initialize handshake. The plugin
  *declares* the schema; the host stores the values and the console renders
  them; a web UI bundle reads them back through `host.config.visible.settings`.
  If the process itself needs a value, pass it through `[[plugin]].args`,
  `[[plugin]].url`, or the plugin's own state.
- **A plugin's web UI is a projection, not the plugin.** Turning the UI off
  leaves the process, its MCP tools, its HTTP routes, and its endpoints fully
  operational.

### Contribution surfaces

Everything a plugin adds is declared under one of these:

| Surface | Use it for | Typical declarations |
| --- | --- | --- |
| `provides` | A stable capability contract core or another plugin can depend on by name | `capability("object-store.v1")` |
| `config` | Operator-facing settings, rendered by the console's own controls | `config_schema`, `config_setting` |
| `web_ui` | Console pages and a Configuration section, served from the package | `web_ui`, `web_ui_bundle`, `web_ui_page`, `web_ui_config_section` |
| `mesh` | Plugin-specific peer-to-peer channels | `mesh::channel("notes.v1")` |
| `events` | Mesh lifecycle events the host may deliver | `events::peer_up()`, `events::peer_down()` |
| `mcp` | Tools, resources, prompts, completions, or an attached external MCP server | `mcp::tool`, `mcp::resource`, `mcp::external_stdio` |
| `http` | Plugin-owned HTTP operations mounted by the host | `http::get`, `http::post`, `.stream_request()`, `.sse()` |
| `inference` | An attached OpenAI-compatible endpoint or a plugin-hosted provider | `inference::openai_http`, `inference::provider` |

Delivery is allowlist-based: no `mesh` declaration means no channel messages,
and no `events` declaration means no mesh events. Declare the smallest set that
does the job.

### What the plugins in this repository demonstrate

The examples show the shape. The eleven plugins show what the shape is for.
Between them they exercise every surface in the table above except `resources`,
`prompts`, and `inference::provider`:

| Surface | Used by | Worth reading for |
| --- | --- | --- |
| `mcp` | all eleven, 53 declared `mcp::tool` handlers between them | Typed `Deserialize + JsonSchema` argument structs; the doc comment on a field becomes the description a model sees |
| `http` | seven plugins, 22 routes | Same handler, two projections — `workload-policy` mounts every tool as both, deliberately |
| `provides` | seven | `web-search.v1`, `semantic-cache.v1`, `workload-policy.v1`, `metrics.prometheus.v1`, `model-mirror.v1`, `contribution-ledger.v1`, `capability-attest.v1` |
| `events` | four | `event-webhook` declares all six kinds and filters in-process; `contribution-ledger` declares five and derives uptime windows from them |
| `mesh` | two | `model-mirror` and `capability-attest` each declare exactly one channel and treat every inbound message as untrusted |
| `inference` | one | `openai-endpoint` — a control-plane declaration, so chat traffic never enters the plugin process |
| `config` + `web_ui` | one | `contribution-ledger` — the only one that declares settings, and only because a console page reads them back |

Four patterns recur, and they are the ones worth copying:

**Configuration comes from `args` and the environment, not `[plugin.settings]`.**
Ten of the eleven declare no config schema at all, and each says why in its own
README: a setting the process cannot read is a console control that looks
authoritative and changes nothing. `sqlite-query`'s database list, `code-context`'s
root, and `model-mirror`'s disk cap are all limits that have to be enforced
*inside* the process, so all three are `[[plugin]].args`.

**Anything key-shaped is environment-only.** `[[plugin]].args` is written into
`config.toml` and echoed back by `tdcc plugins info`, and command lines are
visible to every process on the machine. So `web-search` reads its Brave key
from `TDCC_WEB_SEARCH_BRAVE_API_KEY` and nowhere else; `event-webhook` **refuses
to start** if you pass the webhook URL as an argument; `openai-endpoint`'s
`--api-key-env` takes a variable *name* and rejects anything shaped like a key.

**A tool that cannot reach its backend returns an error.** Not an empty
success, not a plausible zero. `semantic-cache` fails a `lookup` rather than
reporting a miss, because an outage and a cold cache look identical from the
outside. `contribution-ledger`'s `summary` refuses rather than returning a page
of zeroes. The one deliberate exception is documented where it happens:
`prometheus-exporter`'s scrape route always returns a parseable exposition with
`tdcc_up 0`, because Prometheus needs a body to alert on — while its `check`
tool errors like everything else.

**Every README states a blast radius.** These run on hardware somebody else
paid for. Each plugin names exactly what it touches — network, filesystem,
subprocesses, secrets — and each defaults to the narrowest useful permission:
`code-context` refuses to leave its root through two independent layers,
`sqlite-query` opens `SQLITE_OPEN_READ_ONLY` file handles rather than scanning
SQL for the word `DROP`, `web-search` refuses private addresses so a model
cannot reach `127.0.0.1:9337` through it.

One more thing worth reading for the mechanism rather than the feature:
`prometheus-exporter` declares `/metrics` with `.stream_response()`. A buffered
HTTP binding is a JSON operation, and Prometheus needs
`text/plain; version=0.0.4`. Declaring a streamed response makes the host
negotiate a short-lived side stream and copy the plugin's bytes through
verbatim, so the plugin writes a complete HTTP/1.1 response without ever opening
a socket of its own.

---

## Plugin catalog

Eleven plugins live in this repository, under [`plugins/`](plugins/). None of
them is in the catalog, none has a GitHub release, and none is installable by
name. Each is a standalone Rust crate you build and install from a local
archive.

### How to read these tables

**Install** is the same recipe for every one of them:

```bash
cd plugins/<name>
cargo build --release
# package one top-level directory named after the plugin, containing
# plugin.toml and an executable named exactly <name> (<name>.exe on Windows)
tdcc plugins install --archive <archive> --name <name>
```

Each plugin's README has the exact packaging commands for macOS, Linux, and
Windows, including the extra files that plugin needs in the archive.
`--version` defaults to `dev` when you leave it off. Set `TDCC_PLUGIN_DIR` to
an empty directory first if you do not want an in-development build landing in
your real plugin store.

**Tools** are the MCP tool names. On the host MCP endpoint they are
plugin-namespaced — `search` in `web-search` is `web-search.search`.

**Status** is honest about the gap between "it compiles" and "it does something
useful on your machine". Every plugin below builds and passes its own tests
(see [What was verified](#what-was-verified)); the status column names what
each one still needs before it is more than a process that starts.
[Prerequisites in full](#prerequisites-in-full) expands every entry.

### Capability tools — new abilities for a model

Install these when you want a model on your mesh to be able to do something it
cannot do on its own. All three are read-oriented by default and say exactly
what they can reach.

| Plugin | What it does | Tools | Install | Status |
| --- | --- | --- | --- | --- |
| [`web-search`](plugins/web-search) | Search the web and read a result as clean text, from your machine, in your name | `search`, `fetch` | `tdcc plugins install --archive <archive> --name web-search` | Builds · `fetch` works unconfigured; `search` needs a backend |
| [`code-context`](plugins/code-context) | Index one local directory and search, read, and draw it, with `path:line` citations | `search`, `read`, `tree`, `status`, `reindex` | `tdcc plugins install --archive <archive> --name code-context` | Builds · needs `--root`; nothing else |
| [`sqlite-query`](plugins/sqlite-query) | Read schema and run bounded read-only SQL against SQLite files an operator listed | `list_databases`, `list_tables`, `describe_table`, `query`, `execute` | `tdcc plugins install --archive <archive> --name sqlite-query` | Builds · needs `--db` paths; nothing else |

Notes a reader should have before installing any of them:

- `web-search` makes outbound requests **from your address**. It honours
  `robots.txt`, refuses private and link-local addresses (including the cloud
  metadata endpoint), caps response size, and re-checks both guards at every
  redirect hop. All of that is on by default.
- `code-context` filters credential-shaped files by name, extension, directory,
  and a `-----BEGIN … PRIVATE KEY` content check — and its own README says
  plainly that this will not catch a token pasted into `config/production.yaml`.
  Point it at repositories you would show the model anyway.
- `sqlite-query` is read-only through the file handle, not through SQL
  inspection, and denies `ATTACH` at statement-compile time. `execute` exists
  but is refused unless the operator opted a specific database into `--db-rw`.

### Operations — running a node

For the person whose node this is: measurement, alerting, cost, and attaching a
backend they already run.

| Plugin | What it does | Tools | Install | Status |
| --- | --- | --- | --- | --- |
| [`prometheus-exporter`](plugins/prometheus-exporter) | Publishes node, request, model, GPU, and peer state in Prometheus text exposition format, with a Grafana dashboard and alert rules | `check`, `metrics` | `tdcc plugins install --archive <archive> --name prometheus-exporter` | Builds · works as installed; Prometheus and Grafana are yours |
| [`event-webhook`](plugins/event-webhook) | Filters and coalesces mesh events and delivers them to one endpoint as JSON, Slack, or Discord | `status`, `test` | `tdcc plugins install --archive <archive> --name event-webhook` | Builds · inert until `TDCC_EVENT_WEBHOOK_URL` is set |
| [`semantic-cache`](plugins/semantic-cache) | Caches completions by meaning within an exact-match bucket, and reports the saving as a checkable number | `lookup`, `store`, `stats`, `status`, `purge` | `tdcc plugins install --archive <archive> --name semantic-cache` | Builds · **needs an embeddings server the node does not provide** |
| [`openai-endpoint`](plugins/openai-endpoint) | Registers an OpenAI-compatible server you already run as a mesh inference endpoint, and diagnoses whether it is actually routable | `status`, `models`, `health`, `verify_stream`, `compat` | `tdcc plugins install --archive <archive> --name openai-endpoint` | Builds · needs a running backend · **name clashes with the first-party plugin** |

Two of these carry a caveat too big for a table cell:

> **`semantic-cache` needs an embeddings endpoint, and the TDCC node is not
> one.** The node's OpenAI frontend serves `/v1/models`,
> `/v1/chat/completions`, `/v1/completions`, and `/v1/responses`; embeddings
> are out of scope for it. Point the plugin at a local server that exposes
> `POST /v1/embeddings` — Ollama, `llama-server --embeddings`, LM Studio, or
> vLLM serving an embedding model — and run its `status` tool, which sends one
> real probe and reports what it actually found. The default endpoint is the
> node itself, so that this works with no configuration on the day the node
> grows the route; until then that default fails loudly rather than behaving
> like a cache that never hits.

> **`openai-endpoint` here has the same name as the first-party
> [`openai-endpoint`](https://github.com/the-decentralized-compute-company/openai-endpoint).**
> That is not a coincidence — it does the same job — but it is a conflict. A
> plugin's install name, manifest id, and `[[plugin]].name` must all match, and
> an install replaces whatever directory is already at that name. So a node has
> one or the other, never both, and installing this archive over a
> catalog-installed copy replaces it. If you want both on one machine, rename
> the crate, `plugin.toml`, and manifest id in your fork.

Also worth knowing: `prometheus-exporter` is the **pull** side of monitoring.
The first-party [`metrics`](https://github.com/the-decentralized-compute-company/metrics)
plugin is the **push** side — it advertises metrics support so `tdcc` can send
telemetry to an OTLP collector configured in `tdcc` itself. They do not overlap
and can both be enabled.

### Contributing hardware to a mesh

The set that only makes sense when the machine is yours and the work is
somebody else's. Consent, measurement, evidence, and bandwidth.

| Plugin | What it does | Tools | Install | Status |
| --- | --- | --- | --- | --- |
| [`workload-policy`](plugins/workload-policy) | Lets the machine's owner write, in a file, what it will accept — models, peers, owners, sizes, hours, rate limits — and answers "should this run here?" | `check`, `report`, `policy`, `reload` | `tdcc plugins install --archive <archive> --name workload-policy` | Builds · **advisory: it decides, it does not intercept** |
| [`capability-attest`](plugins/capability-attest) | Benchmarks this node on a pinned, reproducible profile, signs the result with the node key, and publishes it to peers | `status`, `record`, `verify`, `benchmark`, `hold`, `peers` | `tdcc plugins install --archive <archive> --name capability-attest` | Builds · needs a node key and a loopback streaming endpoint |
| [`contribution-ledger`](plugins/contribution-ledger) | Keeps a local, hourly, append-only record of what this node gave the mesh, with a **Contribution** console page | `summary`, `epochs`, `peers`, `status`, `flush` | `tdcc plugins install --archive <archive> --name contribution-ledger` | Builds · needs `[[plugin]].url` pointing at the console port |
| [`model-mirror`](plugins/model-mirror) | Holds a model cache and serves it to peers, so a 20 GB artifact crosses the origin's link once instead of once per node | 13, from `status` and `find` to `begin_receive` / `finalize_receive` | `tdcc plugins install --archive <archive> --name model-mirror` | Builds · inert until `--max-cache-bytes` is set |

Three limits stated here rather than left for you to find:

> **`workload-policy` is not enforced by the host.** There is no hook in the
> plugin protocol today that lets a plugin veto an inference request on its way
> through — the host owns routing and policy enforcement. What ships is the
> *decision*, as a capability, an MCP tool, and an HTTP route. Enforcing it
> means putting something in front of the node's OpenAI-compatible port that
> calls `check` and honours the answer. That is a real deployment and it is
> honestly a wrapper. Its own README says so in its second section, which is
> the right place for it.

> **`contribution-ledger` cannot tell you which peers you served.** When a peer
> sends this node work, the host relays it into the local API proxy and counts
> it identically to a request typed on this machine; the requester's peer
> identity is known at the tunnel but is not carried into the routing metrics.
> So its `peers` tool reports **co-presence only**, and says so in its own
> response. It is also not a payment system, not a claim on anything, and not
> proof to anybody else — a local self-report is trivially editable by whoever
> owns the machine, and every tool response carries that sentence in a
> `disclaimer` field.

> **A `capability-attest` signature does not prove the benchmark was run
> honestly.** It proves the record came from the holder of that node's mesh key
> and has not changed since. The numbers are produced on the operator's own
> hardware by software the operator controls. That makes a record an
> *attributable claim* — worth having, revocable when reality disagrees — and
> the plugin ships both halves of that sentence inside every `verify`, `record`,
> `peers`, and `status` response so the caveat cannot be lost downstream.

### Prerequisites in full

Everything that must be true before a plugin does more than start. "None"
means it works as installed on a node that is already running.

| Plugin | Needs before it does anything | Optional |
| --- | --- | --- |
| `web-search` | For `search`: a SearXNG instance **with the JSON format enabled** (public instances almost universally do not), or a Brave Search API key in `TDCC_WEB_SEARCH_BRAVE_API_KEY`. `fetch` needs neither. | `--contact` for a truthful `User-Agent` |
| `code-context` | `--root <dir>`. The process exits at startup without one. | — |
| `sqlite-query` | At least one `--db <alias>=<path>` naming a database file that already exists. It never creates one. | `--db-rw` for write access, off per database by default |
| `prometheus-exporter` | None. Reads `GET /api/status` on loopback. | Prometheus, and the shipped Grafana dashboard and alert rules |
| `event-webhook` | `TDCC_EVENT_WEBHOOK_URL` (or `[[plugin]].url`). Without it the plugin runs and counts every event as `dropped_no_target`. | Slack or Discord; `--format json` posts to anything |
| `semantic-cache` | An OpenAI-compatible `POST /v1/embeddings`. **The TDCC node does not expose one.** | `TDCC_SEMANTIC_CACHE_API_KEY`; `--allow-remote-embeddings` for a non-loopback endpoint |
| `openai-endpoint` | An already-running OpenAI-compatible server reachable over cleartext `http`. Auth on `/v1/models` makes the endpoint permanently unroutable — the host's health probe cannot send a key. | `--api-key-env` for this plugin's own diagnostics |
| `workload-policy` | Nothing to start; a gateway that calls `check` to actually enforce. No policy file means permissive dry-run. | `~/.tdcc/workload-policy.toml`; `mode = "enforce"` once you trust the rules |
| `capability-attest` | `~/.tdcc/key` (running `tdcc` once creates it) and a **loopback** OpenAI-compatible endpoint honouring `"stream": true`. | `--busy-url` pointing at a real in-flight-request count — the plugin does not ship one, and the fallback is a latency proxy. `nvidia-smi` for measured VRAM; nothing else is probed |
| `contribution-ledger` | `[[plugin]].url` pointing at the **console** port (`--console`, 3131 by default), a writable state directory, and — for console configuration writes — a local owner identity. | `--no-host-api` to record presence only |
| `model-mirror` | `--max-cache-bytes`. It defaults to `0`, which means the node holds and serves nothing. | `--import-root`; `--no-advertise` |

### What was verified

This catalog was assembled by reading every plugin and running, in each of the
eleven directories, against a sibling `tdcc-mesh` checkout:

```text
cargo check --all-targets   exit 0 for all eleven, no warnings
cargo test                  exit 0 for all eleven — 865 passed, 0 failed
```

That is what "Builds" means in the tables above, and it is all it means. **No
plugin here was installed into a running host as part of assembling this
catalog**, so the claims that need a live node — the initialize handshake, side
streams, the web UI projection states, endpoint health transitions — rest on
each plugin's own README and on the checklist under
[Test before publishing](#test-before-publishing).

One test was not run: `web-search` marks a single test `#[ignore]` because it
makes real outbound requests. Run it with `cargo test -- --ignored` if you are
changing the fetch path.

Each plugin's own test section says what its suite does and does not cover.
Several go further than unit tests without needing a host: `web-search`,
`openai-endpoint`, `prometheus-exporter`, and `event-webhook` drive real HTTP
servers on loopback, and `semantic-cache` runs its full store-then-lookup path
against a stub embedder — which pins the cache's behaviour and says nothing
about how well any real embedding model paraphrases.

The test counts above come from that run, not from the READMEs.

### First-party plugins, in their own repositories

TDCC documents five first-party plugins. Each is a **separate repository** with
its own releases — none of them is vendored here, and they are the only entries
on this page that `tdcc plugins install <name>` resolves.

| Plugin | What it does | Install | Status |
| --- | --- | --- | --- |
| [`blackboard`](https://github.com/the-decentralized-compute-company/blackboard) | Shares short-lived status, findings, questions, and answers across a mesh. Also ships an Agent Skill. | `tdcc plugins install blackboard` | First-party · external repo |
| [`openai-endpoint`](https://github.com/the-decentralized-compute-company/openai-endpoint) | Attaches an already-running OpenAI-compatible server (vLLM, TGI, Ollama, Lemonade) to the mesh. | `tdcc plugins install openai-endpoint` | First-party · external repo · **name clash** with [`plugins/openai-endpoint`](plugins/openai-endpoint) |
| [`flash-moe`](https://github.com/the-decentralized-compute-company/flash-moe) | Attaches a Flash-MoE endpoint, or supervises a local Flash-MoE process for SSD expert streaming. | `tdcc plugins install flash-moe` | First-party · external repo |
| [`metrics`](https://github.com/the-decentralized-compute-company/metrics) | Advertises metrics support for TDCC telemetry. The OTLP destination is configured in `tdcc`, not in the plugin. | `tdcc plugins install metrics` | First-party · external repo |
| [`agents`](https://github.com/the-decentralized-compute-company/agents) | Runs mesh-native A2A agents and exposes their tools through the mesh MCP endpoint. | `tdcc plugins install agents` | First-party · external repo |

Whether a given first-party plugin publishes a prebuilt archive for your exact
platform is up to that repository's releases. If `tdcc plugins install` reports
no compatible asset for your target, build it from its repository and point
`[[plugin]].command` at the binary you built.

### Examples

Not plugins to install — plugins to read. Start here before writing your own.

| Example | Teaches | Status |
| --- | --- | --- |
| [`hello-plugin`](examples/hello-plugin) | The smallest complete plugin: manifest, one MCP tool, control connection. | Example · not in catalog |
| [`notes-console`](examples/notes-console) | Config schema, MCP tool, HTTP routes, a console page, and a Configuration section. | Example · not in catalog |

### Community plugins

The catalog can also carry third-party entries. Search it before installing
something unfamiliar:

```bash
tdcc plugins search
tdcc plugins search database
```

A catalog entry is a discovery aid, not an endorsement: it is metadata only,
and the binary still comes from that repository's GitHub releases. Read the
repository and check what its archives contain before installing.

---

## Quick start

### Install a published plugin

The catalog resolves a bare name to a repository, then follows that
repository's GitHub releases:

```bash
tdcc plugins install blackboard
tdcc plugins install blackboard@1.1.0
```

An explicit GitHub reference skips the catalog entirely. All four forms work:

```bash
tdcc plugins install the-decentralized-compute-company/openai-endpoint
tdcc plugins install the-decentralized-compute-company/openai-endpoint@0.1.2
tdcc plugins install https://github.com/the-decentralized-compute-company/openai-endpoint
tdcc plugins install https://github.com/the-decentralized-compute-company/openai-endpoint@v0.1.2
```

Both `1.1.0` and `v1.1.0` are accepted; the installer tries the tag both ways.
Without `@version`, it resolves the repository's latest GitHub release, which
excludes drafts and prereleases.

Then enable it in `~/.tdcc/config.toml` and restart `tdcc`:

```toml
[[plugin]]
name = "blackboard"
```

### Install a local build

This is how everything under [`plugins/`](plugins/) is installed, and how you
test a release candidate before publishing it:

```bash
tdcc plugins install --archive ./my-plugin-0.1.0-local.tar.gz \
  --name my-plugin --version 0.1.0
```

`--archive` takes `.tar.gz` or `.zip`, requires `--name`, conflicts with the
positional reference, and defaults `--version` to `dev`. It runs the same
validation as a downloaded release. Local installs are replaced by rebuilding
and reinstalling; `tdcc plugins update` only works on GitHub release sources.

### Manage what is installed

```bash
tdcc plugins list
tdcc plugins info blackboard
tdcc plugins update blackboard
tdcc plugins disable blackboard   # stays on disk, does not start
tdcc plugins enable blackboard
tdcc plugins delete blackboard    # removes files and metadata
```

Installed files live under `~/.tdcc/plugins/installed/<name>/`, with the
metadata record at `~/.tdcc/plugins/<name>/plugin-install.json`. Set
`TDCC_PLUGIN_DIR` to relocate the whole store — useful for keeping an
in-development plugin out of your real installation.

### Write a plugin

```bash
git clone https://github.com/the-decentralized-compute-company/plugins
cp -R plugins/examples/hello-plugin ./my-plugin
```

Then rename the crate, the `plugin.toml` `name`, and the id in
`PluginMetadata::new`. Keep one name everywhere. Two of those are enforced:

- the **executable filename** in the archive must be exactly the install name
  (`notes`, or `notes.exe` on Windows), or extraction fails;
- the **manifest id** must equal the configured `[[plugin]].name`, or the host
  rejects the initialize handshake with
  `Plugin 'x' identified itself as 'y'`.

The install name is the catalog `name` for a catalog install and the
**repository name** for an `owner/repo` install — so if you plan to publish on
GitHub, name the repository after the plugin too.

---

## The manifest contract

There are two manifests, and they do different jobs.

### `plugin.toml` — the package marker

A small file that marks the plugin directory inside the archive:

```toml
name = "notes"
version = "1.0.0"
```

Required in every archive — its presence is how the installer locates the
plugin root. Without it, extraction fails with
`plugin archive does not contain plugin.toml`. The installer does not currently
read the fields inside it, but keep them accurate: they are what a human reads
when inspecting an extracted package.

### `plugin-manifest.json` — the packaged metadata

Generated from the same `plugin!` declaration the runtime serves, so the
packaged metadata cannot drift from the running manifest. It carries exactly
two things: `config_schema` and `web_ui`. A plugin that declares neither
produces `{}` and can omit the file; a plugin that declares either **must**
ship it, or the console has no schema to render and no bundle root to validate.

Of the eleven plugins here, ten emit `{}` and leave the file out;
[`contribution-ledger`](plugins/contribution-ledger) declares both a config
schema and a web UI, so its archive requires the file and its README shows the
generation step on both platforms.

The maintained web UI exemplar keeps a checked-in copy of its expected output
as `plugin.package.json` so tests can diff against it. The file that goes in
the archive is always named `plugin-manifest.json`.

Generate it from your binary:

```rust
use anyhow::{Context, Result, bail};
use tdcc_plugin::{Plugin, PluginRuntime, package_manifest_json};

#[tokio::main]
async fn main() -> Result<()> {
    let plugin = build_plugin();
    match std::env::args().nth(1).as_deref() {
        Some("--print-package-manifest") => {
            let manifest = plugin.manifest().context("plugin manifest")?;
            println!("{}", package_manifest_json(&manifest)?);
            Ok(())
        }
        Some(argument) => bail!("unknown option: {argument}"),
        None => PluginRuntime::run(plugin).await,
    }
}
```

### A minimal runtime declaration

Taken from the maintained web UI exemplar, which is read directly by the host's
own tests:

```rust
use tdcc_plugin::{
    PluginMetadata, SimplePlugin, capability, config_integer, config_schema, config_setting,
    constraint_range, mcp, plugin, plugin_server_info, proto, web_ui, web_ui_bundle,
    web_ui_config_section, web_ui_page,
};

pub fn exemplar_plugin() -> SimplePlugin {
    plugin! {
        metadata: PluginMetadata::new(
            "web-ui-exemplar",
            "0.1.0",
            plugin_server_info(
                "web-ui-exemplar",
                "0.1.0",
                "Web UI exemplar",
                "Buildable reference plugin for host-projected web UI",
                None::<String>,
            ),
        ),
        provides: [capability("exemplar.notes.v1")],
        config: [config_schema("web-ui-exemplar")
            .setting(
                config_setting("retention_days", config_integer())
                    .default_value(&14)
                    .constraint(constraint_range(Some("1"), Some("365")))
                    .apply_mode(proto::PluginConfigApplyMode::DynamicValidationOnly)
                    .restart_scope(proto::PluginConfigRestartScope::PluginProcess)
                    .description("How long exemplar notes stay available.")
                    .label("Retention days")
                    .category("exemplar-retention", "Retention", "Exemplar retention settings", 10)
                    .unit("days")
                    .control_hint("number"),
            )],
        web_ui: [web_ui()
            .bundle(web_ui_bundle("main", "bundle"))
            .page(
                web_ui_page("overview", "Exemplar Notes", "overview", "register-mesh-plugin-ui.js")
                    .bundle_id("main"),
            )
            .config_section(
                web_ui_config_section("page-actions", "Exemplar page", "register-mesh-plugin-ui.js")
                    .parent_tab("integrations")
                    .bundle_id("main"),
            ),
        ],
        mcp: [
            mcp::tool("status")
                .description("Show that the exemplar's non-UI capability remains available.")
                .handle(|_args, _context| Box::pin(async {
                    Ok(serde_json::json!({
                        "capability": "exemplar.notes.v1",
                        "status": "available"
                    }))
                })),
        ],
    }
}
```

**Field order is fixed.** `metadata`, then optional `startup_policy`,
`provides`, `config`, `web_ui`, `mesh`, `events`, `mcp`, `http`, `inference`,
then the lifecycle hooks `health`, `on_initialized`, `on_channel_message`,
`on_mesh_event`. Any field may be omitted; none may move ahead of an earlier
one. Build one plugin with every surface it needs — do not build a second
plugin just to attach a schema or a UI.

### Naming

MCP identifiers are plugin-namespaced by default, so a tool named `feed` in the
`blackboard` plugin is `blackboard.feed` on the host MCP endpoint, and its
resource is `blackboard://snapshot`. Keep the canonical identity namespaced
even when a friendly alias exists.

Tool names still share one namespace per plugin, and the plugin names in the
catalog share one namespace across the mesh. See
[the naming section in CONTRIBUTING.md](CONTRIBUTING.md#naming-you-share-one-namespace)
before you pick either.

---

## The launch contract

The host spawns the plugin as a child process and passes connection details
through the environment. There is no configuration file for this, and no
handshake before the socket:

| Variable | Meaning | Always set? |
| --- | --- | --- |
| `TDCC_PLUGIN_ENDPOINT` | Local control endpoint the plugin connects to | Yes |
| `TDCC_PLUGIN_TRANSPORT` | Transport kind: `unix` or `pipe` | Yes |
| `TDCC_PLUGIN_NAME` | Configured plugin name | Yes |
| `TDCC_PLUGIN_URL` | The `[[plugin]].url` value | Only when `url` is configured |
| `TDCC_PLUGIN_WEB_UI_DIR` | Validated bundle asset root in the installed package | Only for a plugin with a valid installed web UI |

`PluginRuntime::run` consumes `TDCC_PLUGIN_ENDPOINT` and
`TDCC_PLUGIN_TRANSPORT` for you; you never read those two yourself. Its absence
is fatal — the runtime exits with
`TDCC_PLUGIN_ENDPOINT is not set for plugin process`, which is exactly what you
see when you run a plugin binary directly outside a host. `TDCC_PLUGIN_TRANSPORT`
falls back to `unix` on Unix and `pipe` on Windows.

The other three are yours to read with `std::env::var` if the plugin needs
them. Seven of the eleven plugins here read `TDCC_PLUGIN_URL`, and they mean
four different things by it: the backend to attach (`openai-endpoint`), the
node's own console API (`prometheus-exporter`, `contribution-ledger`), a
service to call (`web-search`'s SearXNG base, `semantic-cache`'s embeddings
endpoint, `capability-attest`'s benchmark target), and a delivery destination
(`event-webhook`). All seven also accept an explicit flag that overrides it,
and all seven validate it before use — `[[plugin]].url` is operator input, not
a trusted value.

### The legacy prefix

The host also exports every one of those variables under the pre-rename
`MESH_LLM_PLUGIN_*` prefix, with identical values. That mirror exists purely so
plugin binaries built before the TDCC rename keep starting; the host never
reads the legacy names back, and the current SDK prefers `TDCC_PLUGIN_*`
whenever both are present. Write new plugins against `TDCC_PLUGIN_*` only. The
shim disappears when the plugin protocol version moves past 2, because the
initialize handshake requires an exact protocol match and a version bump
already forces every plugin to be rebuilt.

### Everything else about lifecycle

- One long-lived control connection carries initialize, health, manifest
  registration, small RPCs, mesh events, stream negotiation, and cancellation.
- Large or streaming payloads never ride the control connection. Declare
  `.stream_request()`, `.stream_response()`, or `.sse()` on an HTTP binding and
  the host negotiates a short-lived side stream — a Unix socket or a named pipe
  — so health checks stay responsive during a 10 GB upload.
- Keep `health` fast and independent of long-running work. `capability-attest`
  is the clearest case: `benchmark` takes as long as the benchmark takes, and
  `health` reads one field and never waits on the benchmark lock.
- Plugin health and endpoint health are separate concerns. A registered
  inference endpoint can go unhealthy and drop out of routing while the plugin
  process stays loaded and enabled; when it recovers, it becomes routable again
  automatically.

---

## Configuration

One `[[plugin]]` table per plugin in `~/.tdcc/config.toml`:

```toml
version = 1

[[plugin]]
name = "openai-endpoint"
url = "http://127.0.0.1:8000/v1"

[[plugin]]
name = "notes"
enabled = true
web_ui_enabled = true
command = "/opt/notes/notes"
args = ["--verbose"]

[plugin.settings]
retention_days = 30

[plugin.startup]
connect_timeout_secs = 75
init_timeout_secs = 90
optional = true
lazy_start = true
```

| Field | Meaning |
| --- | --- |
| `name` | Installed plugin identifier. Required, and must match the plugin's own manifest id — the host rejects a plugin that identifies itself as something else. |
| `enabled` | Start this plugin with `tdcc`. Defaults to `true`. |
| `web_ui_enabled` | Show a declared web UI in the console. Defaults to `true`. Meaningful only for a plugin that declares one, and never starts or stops the process. |
| `command` | Explicit executable path. Needed only when running a locally built binary instead of an installed package. |
| `args` | Arguments passed to the plugin process. |
| `url` | Passed to the process as `TDCC_PLUGIN_URL`. |
| `settings` | Host-owned values validated against the plugin's declared schema. Written as `[plugin.settings]`. |
| `[plugin.startup]` | `connect_timeout_secs`, `init_timeout_secs`, `optional`, `lazy_start`. |

`optional = true` records a missing plugin as inactive instead of rejecting the
whole config. `lazy_start = true` defers the process launch until the plugin is
first used. Both are useful for slow machines and for integrations that should
not block startup.

Changing a plugin's configuration takes effect on the next start or reload, not
in an active session.

**`args` is not private.** It is written into `config.toml` and echoed back by
`tdcc plugins info`, and on most systems a process's command line is readable by
every other process on the machine. Nothing key-shaped belongs in it. Read
credentials from the environment of the `tdcc` process instead — that is what
every plugin in this repository does, and two of them refuse to start if you
try the other way.

---

## Web UI projection

A plugin may contribute console pages and Configuration sections. The host
serves the bundle from the installed package, same-origin, and imports the code
only once the projection is `ready`, enabled, available, has a same-origin
`asset_base_url`, and the requested page or section actually exists. There is no
iframe, no sandbox, no remote asset loading, and no generic event bus.

### Declaring it

```rust
web_ui: [
    web_ui()
        .bundle(web_ui_bundle("main", "bundle"))
        .page(
            web_ui_page("overview", "Overview", "overview", "register-mesh-plugin-ui.js")
                .bundle_id("main"),
        )
        .config_section(
            web_ui_config_section("settings", "Settings", "register-mesh-plugin-ui.js")
                .parent_tab("integrations")
                .bundle_id("main"),
        ),
],
```

The v1 rules the installer enforces:

- **Exactly one bundle**, with a non-empty id and a package-relative root below
  the package root. Not `""`, not `"."`, not absolute, no `..`, no URL scheme.
  Split files *inside* that root instead of declaring a second one.
- **Every page and config section** needs a non-empty id, a non-empty label or
  title, and a `bundle_id` matching the declared bundle.
- **Every `entry_script`** must be a relative path inside the bundle root, and
  must exist in the installed package.
- **`route` is a slug.** No `/`, no `\`, no protocol syntax, no `..`.
- **`parent_tab`** is either omitted or `"integrations"`.

Fail any of these and the projection state becomes `invalid` — the plugin keeps
running and keeps its non-UI capabilities.

### Projection states

| State | Meaning |
| --- | --- |
| `none` | The plugin declares no web UI. |
| `ready` | Manifest and installed bundle are valid; the host may mount it. |
| `disabled` | Valid and installed, but `web_ui_enabled` is off. |
| `invalid` | Manifest or bundle failed validation, or the bundle root is missing. |
| `plugin_not_running` | The process is stopped; installed metadata still carries the projection. |

### The bundle

Ship browser-importable JavaScript exporting `registerMeshPluginUi(host)`. The
host does not transpile TypeScript, JSX, CommonJS, or bare npm imports at
runtime — bundle those yourself before packaging.

```js
export async function registerMeshPluginUi(host) {
  return {
    pages: {
      overview({ element, page }) {
        const heading = document.createElement('h2')
        heading.textContent = page.label
        element.replaceChildren(heading)
        return { unmount() { element.replaceChildren() } }
      }
    }
  }
}
```

- Every mount handler returns an object with `unmount()`, which must tear down
  DOM content and detach host subscriptions.
- Read settings from `host.config.visible.settings`; write them with
  `host.config.requestMutation(...)`, never by touching `config.toml`.
  Mutations may only change plugin-owned `settings` keys — `enabled`,
  `web_ui_enabled`, `command`, `args`, `url`, and `startup` are host-owned and
  rejected. Malformed requests return `400`; values that fail the schema return
  `422`.
- Configuration writes require a local owner identity. For development,
  `tdcc auth init --no-passphrase` creates an unencrypted one.
- `host.network.fetchPlugin(path, init)` and `host.network.json(path, init)`
  take plugin-relative paths like `http/items?limit=2` and reject origins,
  fragments, backslashes, and `.` / `..` segments. `json(...)` also rejects
  non-2xx responses.
- Do not rebuild a schema-backed setting with raw DOM controls. The console
  already renders declared settings with its own validated controls; a config
  section should add actions and context around them.

Copy the exemplar's self-contained
[`host-contract.d.ts`](examples/notes-console/bundle/host-contract.d.ts) for
TypeScript authoring. Never import types from `tdcc-ui` — that is private
console source, not a plugin SDK.

For a second worked example beyond `notes-console`,
[`contribution-ledger/bundle/`](plugins/contribution-ledger/bundle) is a
shippable ES module with a real page and a Configuration section, and its README
includes the checks that prove the projection is independent of the process —
turning the UI off must leave the tools and routes answering.

### Routes

| Route | Purpose |
| --- | --- |
| `GET /api/plugins/:plugin/web-ui` | Projection state and `asset_base_url` |
| `PATCH /api/plugins/:plugin/web-ui/enabled` | Toggle the projection only |
| `GET /api/plugins/:plugin/web-ui/config` | Visible settings and schema |
| `PATCH /api/plugins/:plugin/web-ui/config` | Settings-only mutation |
| `GET /api/plugins/:plugin/web-ui/assets/*` | Validated bundle assets |

A ready page lives at the static console path
`/plugins/<plugin-name>/<page-id>`. A single ready page gets a direct
navigation item; several are grouped under a **Plugins** menu. Config sections
appear under Configuration → Plugins → Integrations.

---

## Build, package, and release

### Depend on the SDK

```toml
[dependencies]
anyhow = "1"
schemars = "1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }

# See "The SDK is not on crates.io" — this is the published form, and it does
# not resolve today. Until then, use a path dependency into a local checkout:
#   tdcc-plugin = { path = "../../../tdcc-mesh/crates/tdcc-plugin" }
tdcc-plugin = "0.72.1"
```

Pin `tdcc-plugin` to a version compatible with the `tdcc` release you target.
The initialize handshake requires an exact protocol-version match, so a host
and a plugin built against mismatched protocol versions refuse to connect —
loudly, at startup, not silently at first use.

Every plugin crate here is standalone: `edition = "2024"`,
`license = "Apache-2.0"`, `publish = false`, and an empty `[workspace]` table
so it is not swept into a surrounding workspace. Keep `Cargo.lock` committed —
a plugin is a binary that runs on other people's machines, and a locked
dependency set is what makes a release reproducible and reviewable.

The first build of any of these downloads a vendored `protoc` through
`tdcc-plugin`'s `prost-build` step. No system protobuf compiler is needed.
`sqlite-query` additionally needs a C compiler, because it statically links the
SQLite amalgamation rather than depending on whatever `libsqlite3` a
contributor happens to have.

### Archive layout

One directory named after the plugin, at the archive root:

```text
notes/
  plugin.toml            required
  notes                  required — notes.exe on Windows
  plugin-manifest.json   required if the plugin declares config_schema or web_ui
  bundle/
    register-mesh-plugin-ui.js
  README.md
  LICENSE
  skills/
    notes-workflow/
      SKILL.md
```

The installer first looks for a top-level directory named after the plugin that
contains `plugin.toml`; failing that, it accepts exactly one top-level directory
containing `plugin.toml` and rejects an archive with several. It then requires
an executable named exactly after the plugin, plus `.exe` on Windows. Name the
directory after the plugin and this stays boring.

### Asset names

Release assets are selected by plugin name, release tag, OS, and CPU
architecture only. Plugin archives carry no GPU backend flavors.

```text
<plugin-name>-<version>-<target-triple>.<ext>     versioned
<plugin-name>-<target-triple>.<ext>               stable alias
```

| Platform | Target triple | Extension |
| --- | --- | --- |
| macOS Apple Silicon | `aarch64-apple-darwin` | `.tar.gz` |
| macOS Intel | `x86_64-apple-darwin` | `.tar.gz` |
| Linux x86_64 | `x86_64-unknown-linux-gnu` | `.tar.gz` |
| Linux ARM64 | `aarch64-unknown-linux-gnu` | `.tar.gz` |
| Windows x86_64 | `x86_64-pc-windows-msvc` | `.zip` |
| Windows ARM64 | `aarch64-pc-windows-msvc` | `.zip` |

For a `v1.1.0` release of `notes`, publish:

```text
notes-v1.1.0-aarch64-apple-darwin.tar.gz
notes-v1.1.0-x86_64-apple-darwin.tar.gz
notes-v1.1.0-x86_64-unknown-linux-gnu.tar.gz
notes-v1.1.0-aarch64-unknown-linux-gnu.tar.gz
notes-v1.1.0-x86_64-pc-windows-msvc.zip
notes-v1.1.0-aarch64-pc-windows-msvc.zip
```

Selection order, per target:

1. `<name>-<tag>-<triple>.<ext>` using the resolved release tag verbatim.
2. The same name with the `v` added or removed — so a `v1.1.0` tag also matches
   `notes-1.1.0-…`, and a `1.1.0` tag also matches `notes-v1.1.0-…`.
3. `<name>-<triple>.<ext>`, the stable alias.

The version segment comes from the **release tag**, not from your `Cargo.toml`
version. Keep the tag and the asset names in step, or the stable alias silently
becomes the only thing that ever matches. The tag is also what gets recorded as
the installed version, and what `tdcc plugins update` compares against to decide
whether a newer release exists.

`<plugin-name>` here is the catalog `name` for a catalog install, and the
**repository name** for an `owner/repo` install. Keep the crate name,
`plugin.toml` name, manifest id, catalog name, and repository name all
identical and this never comes up.

### Test before publishing

Install the exact archive you are about to release through the same validation
boundary a download goes through:

```bash
tdcc plugins install --archive ./notes-1.1.0-local.tar.gz \
  --name notes --version 1.1.0
tdcc plugins info notes
```

Then, at minimum, confirm:

1. The plugin connects and completes initialization.
2. The manifest exposes every expected MCP, HTTP, inference, capability,
   channel, event, and web UI entry.
3. Invalid input returns a useful error without dropping the control session.
4. Health still responds while a handler is running.
5. Streaming and cancellation do not leak side streams.
6. A stopped endpoint goes unavailable without disabling the plugin.
7. Every published archive extracts and runs on its target.
8. A mismatched protocol version is rejected clearly.
9. If the plugin declares a web UI: `ready`, `disabled`, `invalid`, and
   `plugin_not_running` all behave, and non-UI capabilities survive all four.

### Publish to the catalog

The catalog is a Hugging Face dataset holding one JSONL line per plugin. It is
metadata only — it never serves binaries, and installing a catalog result just
redirects to that repository's GitHub releases.

```json
{"name":"notes","description":"Shared notes for a mesh.","github_url":"https://github.com/example/notes","author_email":"dev@example.com","author_name":"Example"}
```

All five fields are required, `name` must be unique, and unknown extra fields
are ignored for forward compatibility. Point `TDCC_PLUGIN_CATALOG_URL` at a
different JSONL file to use a private catalog.

### Ship Agent Skills

A plugin can carry skills under `skills/<skill-name>/SKILL.md`. Use lowercase
ASCII names with single hyphens, include `name` and `description` frontmatter,
and keep supporting file paths relative to the skill directory — no hard-coded
home directories or OS-specific absolute paths. Users install them with
`tdcc skills install`; the agent launchers (`tdcc claude`, `tdcc goose`,
`tdcc pi`, `tdcc opencode`) install them automatically before starting a
session. Existing user-owned skill directories are never overwritten without
`--force`.

---

## Versioning and compatibility

- **Protocol.** The host/plugin wire protocol is a single integer. Initialize
  requires an exact match, so a protocol bump means every plugin must be
  rebuilt against the matching SDK. This is deliberately strict: mismatches
  fail at startup with a clear message instead of misbehaving later.
- **SDK.** `tdcc-plugin` versions track `tdcc` releases. Pin an exact version
  and upgrade deliberately.
- **Manifests are additive.** Older hosts ignore unknown manifest fields.
  Declaring a new surface does not break an older node; it simply is not
  projected there.
- **Web UI is additive and projection-only.** `web_ui_enabled` stays
  independent of the process `enabled` flag, and invalid, disabled, missing,
  and stopped projections all remain visible in the summary and API state
  rather than disappearing.
- **Your own plugin.** Use semantic versioning, tag releases to match your
  asset names, and treat a change to a capability id, an MCP tool name, an HTTP
  path, or a settings key as a breaking change — those are the names other
  people wrote down. `workload-policy`'s compatibility section is a good model:
  it enumerates exactly which of its identifiers are load-bearing, including
  its stable outcome codes and its policy file keys.

---

## Security notes for plugin authors

- Treat plugin configuration, `url` values, and every request argument as
  untrusted input.
- Never put secrets in manifests, archives, logs, or MCP tool descriptions.
  Tool descriptions are shown to models and users.
- Prefer host-owned HTTP and MCP projections over opening your own listener.
- Declare the smallest possible set of mesh channels and events.
- Document the network access, files, subprocesses, and permissions your plugin
  needs.
- Pin or verify third-party dependencies in release builds.

Installing a plugin runs third-party native code on your machine with your user
account's privileges. There is no sandbox. Treat it exactly like installing any
other native binary.

The expectations for a plugin contributed here, and what a review looks for,
are in [CONTRIBUTING.md](CONTRIBUTING.md#security-expectations).

---

## Reference

| Topic | Where |
| --- | --- |
| Control sessions, side streams, host projections, ownership | [Plugin Architecture](https://decentralizedcompute.company/docs/pages/plugin-architecture/) |
| Author API, packaging, skills, testing | [Developing Plugins](https://decentralizedcompute.company/docs/pages/developing-plugins/) |
| Installing and configuring plugins | [Plugins](https://decentralizedcompute.company/docs/pages/plugins/) |
| MCP, HTTP, inference, capabilities, control messages | [Plugin Reference](https://decentralizedcompute.company/docs/pages/plugin-reference/) |
| CLI switches for `tdcc plugins` | `tdcc plugins --help` |
| The maintained web UI exemplar, read directly by host tests | `docs/plugins/exemplars/web-ui` in the main TDCC repository |

The main `tdcc` repository is not public, so paths into it are given as paths,
not links. Everything you need to build and publish a plugin is either on the
docs site above or in this repository — the exemplar's author type contract is
vendored here as
[`examples/notes-console/bundle/host-contract.d.ts`](examples/notes-console/bundle/host-contract.d.ts).

## Getting help

- **A bug in an example, in a plugin under `plugins/`, or something wrong or
  missing in this guide** — open an issue in this repository. For a plugin, say
  which one and paste the real output.
- **A bug in a first-party plugin** (`blackboard`, the external
  `openai-endpoint`, `flash-moe`, `metrics`, `agents`) — open it in that
  plugin's own repository.
- **A bug in the plugin SDK, the installer, the host projection, or the
  console** — open it against the main TDCC repository if you have access, or
  here if you do not. Include the output of `tdcc plugins info <name>` and your
  `[[plugin]]` block with any secrets removed.
- **`cargo build` cannot find `tdcc-plugin`** — that is expected. Read
  [The SDK is not on crates.io](#the-sdk-is-not-on-cratesio) before opening
  anything.

Contributions are welcome — see [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Apache-2.0. See [LICENSE](LICENSE).
