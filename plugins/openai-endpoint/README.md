# openai-endpoint

Attach hardware TDCC does not manage itself.

If a machine is already running vLLM, TGI, Ollama, LM Studio, or
`llama-server`, this plugin registers that server as an OpenAI-compatible
inference endpoint on this node. The models it serves join the mesh, and `tdcc`
routes matching requests straight to it. Nothing has to be re-tooled,
re-quantized, or restarted — the person who set the machine up keeps owning the
model runtime, and TDCC only learns how to reach it.

It also answers the three questions that decide whether that actually works:

- **Is the endpoint routable?** — `health` reproduces the host's own probe.
- **What does it really serve?** — `models` reads `/v1/models` instead of
  trusting configuration.
- **Does it stream?** — `verify_stream` sends one small streaming completion and
  reports whether tokens arrived progressively or in one buffered blob.

## This plugin is not a proxy

The single load-bearing line is the manifest's `inference` declaration:

```rust
inference::openai_http(endpoint_id, address)
    .managed_by_plugin(false)
    .supports_streaming(true)
```

That is a **control-plane declaration**. The host reads the address, opens its
own connection to the backend, and relays bytes. Chat traffic never enters this
process, which is exactly why streaming survives: the host rewrites the request
line and the `Host` header, forwards every other header unchanged, and copies
the response body straight through as it arrives. A server-sent-event body has
no `Content-Length`, so it takes the host's incremental relay path and is never
accumulated before being forwarded.

The practical consequence is worth stating plainly, because it is the opposite
of what "endpoint plugin" usually implies: **this plugin cannot break your token
stream, and it cannot fix one either.** If tokens arrive in a lump, the cause is
the backend or something between it and this node. `verify_stream` exists to
tell you which.

## Prerequisites

An OpenAI-compatible server you are already running, reachable over cleartext
`http` from the node running `tdcc`. This plugin does not install, launch,
supervise, stop, or configure that server, and it never downloads a model. That
is deliberate: `managed_by_plugin` is `false`, so TDCC has no lifecycle claim
over someone else's process.

Anything that answers `GET /v1/models` and `POST /v1/chat/completions` works.
Verified shapes are described under
[OpenAI compatibility](#openai-compatibility-what-is-normalised-and-what-is-not);
the plugin reports what your server actually does rather than assuming.

## Configure

```toml
# ~/.tdcc/config.toml
version = 1

[[plugin]]
name = "openai-endpoint"
url  = "http://127.0.0.1:8000/v1"
```

That is the whole minimum. `url` is delivered to the process as
`TDCC_PLUGIN_URL`. Everything else is optional and goes in `args`:

```toml
[[plugin]]
name = "openai-endpoint"
url  = "http://127.0.0.1:8000/v1"
args = ["--endpoint-id", "vllm", "--model", "Qwen/Qwen3-8B"]
```

| Argument | Meaning | Default |
| --- | --- | --- |
| `--url <base>` | API base URL. Overrides `[[plugin]].url`. | — |
| `--endpoint-id <id>` | Endpoint id within this plugin. Lowercase letters, digits, `.`, `_`, `-`. | `upstream` |
| `--api-key-env <NAME>` | **Name** of an environment variable holding a bearer token for this plugin's own probes. | none |
| `--timeout-secs <n>` | Per-probe timeout, 1–120. | `10` |
| `--model <name>` | Model used by `verify_stream` and `compat` when the caller names none. | first discovered |

Both `--flag value` and `--flag=value` are accepted. Run the binary with
`--help` for the same table.

One node attaches one endpoint. `[[plugin]].name` must match the plugin's own
manifest id, and this plugin's id is fixed, so a second `[[plugin]]` table for
it is not possible. A machine with several backends either puts them behind one
OpenAI-compatible front door or runs a node each.

### There is no `[plugin.settings]` block

Deliberately. Host-owned settings are stored by the host and rendered by the
console, but they are **never delivered to the plugin process** — there is no
settings field in the launch contract or the initialize handshake. Declaring a
`config_schema` here would draw controls in the console that this process could
not read. Configuration therefore lives in `url` and `args`, which do reach it.

## The base URL rules, and why they are strict

The plugin refuses to start on a URL the host could not route to. Starting
successfully while advertising an unreachable endpoint is the worst outcome: the
node looks joined and silently is not.

| Rejected | Reason |
| --- | --- |
| `https://…` | The host's external-endpoint relay only dials cleartext `http` and drops every other scheme before connecting. An https endpoint would pass this plugin's probes and still never receive routed traffic. |
| `http://…/v1/chat/completions` | That is an operation, not a base. Routing appends the caller's path to the base, so an operation URL produces a doubled path. |
| `http://…/v1?key=…` | Query strings and fragments are not part of an API base. |
| `http://` | No host. |

### Why http only

This is a host constraint, not a preference, so the plugin is built without a
TLS backend at all — the dependency graph enforces what the comment claims. To
attach a TLS-fronted server, terminate TLS locally and point `url` at the local
cleartext listener.

## The API-key trap

Read this before configuring an authenticated backend, because it is the one
failure that looks like a healthy setup.

The host decides whether an endpoint is routable by issuing an
**unauthenticated** `GET` to the endpoint's models URL every 15 seconds. It
cannot send a key, and there is no manifest field to give it one.

- **Auth on `/v1/models` → fatal.** The host's probe gets a 401, the endpoint
  never becomes routable, and no amount of client-side credentials helps,
  because routing never selects the endpoint in the first place.
- **Auth only on `/v1/chat/completions` → workable.** The host forwards every
  client header except `Host` unchanged, so a caller supplying its own
  `Authorization` header reaches the backend normally.

`--api-key-env` exists for this plugin's own diagnostics: `models`, `compat`,
and `verify_stream` will authenticate so you can tell "the backend is broken"
apart from "the backend wants a key". `health` runs the probe **both** ways and
says so outright:

```text
NOT routable: the endpoint answers only with an API key, but the host's endpoint
health probe is unauthenticated and cannot send one. …
```

If you hit this, put an unauthenticated local listener in front of the backend,
or drop auth from the models route.

### Keys are never stored here

`--api-key-env` takes a variable **name**, and refuses anything not shaped like
one — pasting a real key into `config.toml` fails at startup with an
explanation. The value is read from the environment at request time and is never
logged, never returned by a tool, and never written into the manifest or the
archive. `status` reports the variable's name and whether it resolved, nothing
more.

## Tools

Namespaced on the host MCP endpoint as `openai-endpoint.<tool>`, and reachable
over HTTP at `POST /api/plugins/openai-endpoint/tools/<tool>`.

| Tool | Network | What it answers |
| --- | --- | --- |
| `status` | none | Effective configuration, the exact URL the host health-checks, how request paths are rewritten, and the last observation. Safe when the backend is down. |
| `models` | `GET /v1/models` | What the endpoint actually serves, read as `data[].id` — the only place the host looks. |
| `health` | `GET /v1/models` ×1–2 | Whether the endpoint is routable, diagnosing the API-key trap explicitly. |
| `verify_stream` | one small streaming completion | Whether tokens arrive progressively, with the finish reason and usage the backend emitted. |
| `compat` | one small completion, plus one more if `check_error_shape` | Where this backend diverges from OpenAI on usage and finish reasons, optionally including its error envelope. |

```bash
curl --fail -X POST http://127.0.0.1:3131/api/plugins/openai-endpoint/tools/health \
  -H 'Content-Type: application/json' -d '{}'

curl --fail -X POST http://127.0.0.1:3131/api/plugins/openai-endpoint/tools/verify_stream \
  -H 'Content-Type: application/json' -d '{"max_tokens":32}'
```

`verify_stream` and `compat` generate real tokens on someone else's hardware, so
they are bounded on purpose: a fixed one-line prompt, `temperature: 0`, and
`max_tokens` clamped to 1–128 regardless of what the caller asks for.

### Reading a streaming verdict

```json
{
  "streaming_ok": true,
  "explanation": "tokens arrived progressively across several network reads; …",
  "observed": { "verdict": "incremental", "reads": 14, "events": 13,
                "first_event_ms": 41, "total_ms": 380, "done_sentinel": true }
}
```

The verdict is structural, not timing-based — timing thresholds are flaky on a
loaded machine, but the number of distinct network reads is not.

| Verdict | Meaning |
| --- | --- |
| `incremental` | Several events across several reads. The chat surface will stream normally. |
| `buffered` | Every event arrived in **one** read. Something buffered the whole response; clients see a long pause and then the entire answer at once. Look for a reverse proxy with response buffering between here and the backend. |
| `single_event` | Only one event — too short to tell. Re-run with a larger `max_tokens`. |
| `no_events` | The backend answered but produced no stream. |

`buffered` is the failure this plugin exists to catch, and it is invisible to
any check that waits for the body before looking at it: the content is all
there, it just arrived at once.

## OpenAI compatibility: what is normalised, and what is not

Because the host relays bytes, **successful responses reach the client exactly
as the backend wrote them.** Nothing here rewrites usage, finish reasons, or
content. The tools report divergence so you know what your clients will see.

| Aspect | What happens |
| --- | --- |
| Response body (2xx) | Passed through unchanged, byte for byte, including SSE frames. |
| `usage` | Unchanged. Absent stays absent — `compat` says so, and `verify_stream` reports whether `stream_options.include_usage` produced one. |
| `finish_reason` | Unchanged. `compat` reports the raw value, whether it is an OpenAI value, and the closest OpenAI equivalent (`eos_token` → `stop`, `max_tokens` → `length`, `tool_use` → `tool_calls`, …). |
| Error body (non-2xx) | **Rewritten by the host** into `{"error":{"message","type","param","code"}}` — unless the body already is an `error` object carrying both a string `message` and a string `type`, which passes through untouched. |
| Error frames inside a 200 stream | Unchanged; a 200 is never rewritten. `verify_stream` surfaces them as `in_stream_error`. |
| Request path | Rewritten onto the configured base. `status` shows the mapping for your configuration. |
| Request headers | Unchanged except `Host`. |

`compat` with `{"check_error_shape": true}` sends one extra request naming a
model that cannot exist — cheap, it never reaches a GPU — and classifies the
envelope your backend uses (`open_ai_object`, `open_ai_object_without_type`,
`open_ai_string_error`, `fast_api_detail`, `bare_message`, `unknown_json`,
`plain_text`, `empty`), then states whether the host will rewrite it.

## Health, and what happens when the backend dies

Plugin health and endpoint health are separate concerns, and this plugin keeps
them separate:

- The plugin's own `health` hook returns immediately from a cached observation.
  It never makes a network request, so a hung backend cannot stall the control
  connection.
- The host probes the endpoint on its own 15-second schedule. A failing endpoint
  drops out of routing while this plugin stays loaded, enabled, and healthy.
  When the backend recovers, the endpoint becomes routable again automatically —
  nothing needs restarting.

The host allows a 30-second startup grace before calling a never-yet-healthy
endpoint unhealthy, and lets an already-healthy endpoint fail once (reported as
`degraded`, still routable) before dropping it. A backend reloading a model
usually rides that out without leaving the mesh.

At startup this plugin runs one discovery probe and logs the result to stderr.
It is spawned rather than awaited: the initialize handshake has a timeout, and a
slow or dead backend must not stop the plugin from coming up.

## Security

These run on other people's hardware, so the blast radius is stated exactly:

- **Network.** Outbound cleartext HTTP to the single base URL validated at
  startup, and nowhere else. Every operation path is a literal in
  `src/upstream.rs`; no tool argument ever reaches a URL, so no caller can steer
  a probe at another host or walk out of the configured path. The only
  caller-supplied values are a model name (which travels in a JSON body,
  validated for length and control characters) and bounded integers and booleans.
- **No listener.** The plugin opens no socket. The host owns the control
  connection, MCP, and HTTP.
- **No filesystem, no subprocess.** It reads one environment variable if you
  configure one, and writes nothing.
- **No secrets at rest.** See [Keys are never stored here](#keys-are-never-stored-here).
- **Bounded work.** Every request has a 1–120 second timeout; generation probes
  are clamped to 128 tokens with a fixed prompt.

The endpoint address you configure is treated as untrusted input and validated
before use, as is every tool argument.

## Building

```bash
cargo test
cargo build --release
```

`tdcc-plugin` builds its protocol types with `prost-build`, so the first build
downloads a vendored `protoc`. No system protobuf compiler is required.

### The SDK dependency

`Cargo.toml` points at a local checkout:

```toml
tdcc-plugin = { path = "../../../tdcc-mesh/crates/tdcc-plugin" }
```

**This is not the line a public consumer will use.** The SDK is not published to
crates.io under this name — it was renamed from `mesh-llm-plugin` and lives in
the `tdcc-mesh` repository, which is private — so `tdcc-plugin = "0.72.1"` does
not resolve today. The path above assumes `tdcc-mesh` and `tdcc-plugins` are
checked out as siblings.

When the SDK is published, replace that one line with:

```toml
tdcc-plugin = "0.72.1"
```

and nothing else changes. Pin the version to match the `tdcc` release you
target: the initialize handshake requires an exact protocol-version match, so a
host and a plugin built against mismatched protocol versions refuse to connect
loudly at startup rather than misbehaving later.

### Tests

81 tests, no backend required (the live tests bind a loopback socket):

- `src/config.rs` — every configuration rule: URL validation, the https refusal,
  operation-URL detection, the pasted-key guard, endpoint-id and timeout bounds.
- `src/openai.rs` — the compatibility matrix as pure functions: SSE decoding
  (including an event split across reads and a CRLF split across reads), model
  discovery, finish-reason mapping, usage normalisation, and error-shape
  classification. The two functions that mirror host behaviour are tested with
  the host's own cases so drift shows up as a failure.
- `src/upstream.rs` — live tests against a real OpenAI-compatible SSE server on
  a loopback socket, driving the actual HTTP client. A streaming server is
  verified as `incremental`; a server that writes the byte-identical body in one
  write is caught as `buffered`; an unreachable backend produces an error rather
  than an empty success; and an API-key-gated endpoint is diagnosed as
  unroutable rather than merely down.
- `src/manifest.rs` — the manifest declares exactly one streaming, unmanaged
  inference endpoint, every tool is present and described, and `status` reports
  the key's variable name but never its value.

## Package and install locally

macOS or Linux, from this directory:

```bash
cargo build --release
rm -rf target/package
mkdir -p target/package/openai-endpoint
cp target/release/openai-endpoint target/package/openai-endpoint/openai-endpoint
cp plugin.toml target/package/openai-endpoint/plugin.toml
cp README.md target/package/openai-endpoint/README.md
tar -C target/package -czf target/openai-endpoint-0.1.0-local.tar.gz openai-endpoint

tdcc plugins install --archive ./target/openai-endpoint-0.1.0-local.tar.gz \
  --name openai-endpoint --version 0.1.0
tdcc plugins info openai-endpoint
```

Windows uses `openai-endpoint.exe` and a `.zip`:

```powershell
Compress-Archive -Path target\package\openai-endpoint `
  -DestinationPath target\openai-endpoint-0.1.0-local.zip -Force
tdcc plugins install --archive .\target\openai-endpoint-0.1.0-local.zip `
  --name openai-endpoint --version 0.1.0
```

The archive must have exactly one top-level directory named after the plugin,
containing `plugin.toml` and an executable named exactly after the plugin
(`.exe` on Windows). This plugin declares neither a `config_schema` nor a
`web_ui`, so its `plugin-manifest.json` is `{}` and may be left out of the
archive entirely — `--print-package-manifest` still emits it if you want the
file for symmetry with other plugins.

Set `TDCC_PLUGIN_DIR` to an empty directory first if you do not want this
landing in your real plugin store.

## Verify it worked

```bash
tdcc client --port 9337 --console 3131 --config ./config.toml
```

Then, in order:

```bash
# 1. The plugin came up and the endpoint is attached.
tdcc plugins info openai-endpoint

# 2. The endpoint is routable — this is the one that catches the API-key trap.
curl --fail -X POST http://127.0.0.1:3131/api/plugins/openai-endpoint/tools/health \
  -H 'Content-Type: application/json' -d '{}'

# 3. The models the host will route on.
curl --fail -X POST http://127.0.0.1:3131/api/plugins/openai-endpoint/tools/models \
  -H 'Content-Type: application/json' -d '{}'

# 4. Streaming survives end to end.
curl --fail -X POST http://127.0.0.1:3131/api/plugins/openai-endpoint/tools/verify_stream \
  -H 'Content-Type: application/json' -d '{"max_tokens":48}'

# 5. The real thing: a streaming request through the mesh API.
curl -N -X POST http://127.0.0.1:9337/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"<id from step 3>","stream":true,
       "messages":[{"role":"user","content":"Count to twenty."}]}'
```

Step 5 should print tokens as they are generated. If step 4 said `incremental`
and step 5 does not stream, the problem is between `tdcc` and your client — not
in the backend and not here.

Finally, confirm the two lifecycles are independent: stop the backend and check
that `tdcc plugins info openai-endpoint` still shows the plugin running while
the endpoint goes unavailable. Start the backend again and it becomes routable
on its own within about 15 seconds.

## Running it directly

Running the binary with a valid configuration but no host fails immediately:

```text
[openai-endpoint] attaching http://127.0.0.1:8000/v1 as endpoint 'vllm'
Error: TDCC_PLUGIN_ENDPOINT is not set for plugin process
```

That is correct. The host owns the control endpoint and passes it in through the
launch contract; a plugin must never invent one. `--help` and
`--print-package-manifest` are the only things that work outside a host.

## License

Apache-2.0.
