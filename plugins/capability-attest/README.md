# capability-attest

Benchmarks the node it runs on against a **pinned, reproducible profile**,
signs the result with that node's own mesh key, and publishes it to peers.

Routing should rest on measured capability, not on what a node asserts about
itself. This plugin is the concrete form of that idea — and the first thing it
has to be honest about is exactly how far a signature gets you.

---

## What a signature proves, and what it does not

**A valid signature proves two things:** the record was produced by the holder
of that node's mesh key, and none of its bytes changed afterwards.

**It does not prove the benchmark was run, or run honestly.** Nothing in a
signature can. The numbers are produced on the node's own hardware, by software
the node's operator controls, and are then signed by that same operator's key.
A node that wants to publish a throughput it never achieved can do so, and the
signature will verify.

So: a signed capability record is an **attributable claim**. You know exactly
whose claim it is, you can compare it against what the node actually delivers,
and you can revoke or downweight that key when reality disagrees. That is worth
having. It is not a guarantee, and treating it as one would be worse than having
no records at all, because other nodes would route on a promise nobody made.

Both sentences ship in the data, not just in this file: every `verify`,
`record`, `peers`, and `status` response carries `what_a_signature_proves` and
`what_a_signature_does_not_prove` verbatim, so the caveat cannot be lost on the
way to whoever is deciding where to send a request.

### What verification does check

Within those limits, verification is not decorative. `verify` checks:

| Check | What a failure means |
| --- | --- |
| Ed25519 signature over the canonical claim | The record was altered, or did not come from `node_endpoint_id`. |
| `node_endpoint_id` is a well-formed 32-byte key | The record cannot be attributed to anything. |
| The pinned prompt rebuilds to its recorded `prompt_sha256` | The benchmark is not reproducible — the profile describes a prompt nobody can rebuild. |
| **The headline numbers follow from the samples beside them** | The record inflates its own median. This is the cheapest way to lie, and it is caught by arithmetic, not by trust. |
| Freshness against the record's expiry, and against a caller-supplied `max_age_seconds` | The measurement is too old to route on. |
| The attached owner certificate, via `tdcc_identity::verify_node_ownership` | Expired, revoked, untrusted, or issued for a different node. |
| `endpoint_locality` | Measured through a non-loopback endpoint, so it may describe a different machine. |

`usable_for_routing` is the single boolean a router wants. It is false whenever
any of the above except owner attribution fails — an unowned node can still
publish an honest, verifiable measurement.

---

## Reproducibility

An unqualified "42 tok/s" is not a measurement. Every input that can change the
number is pinned in the record's `profile`, and the prompt is *derived* from
those fields rather than stored, so anyone can rebuild the exact bytes that were
sent and check the hash:

- `model`, `context_tokens`, `max_output_tokens`
- `temperature_milli`, `top_p_milli`, `seed`
- `warmup_runs`, `measured_runs`
- `filler_sentence`, `chars_per_token_estimate_milli`, `prompt_chars`,
  `prompt_sha256`

`profile_fingerprint` (a SHA-256 of all of that) is the value to compare before
putting two nodes on the same scale. **A fingerprint mismatch means "different
benchmark", not "slower node."** The `peers` tool reports
`comparable_with_this_node` per peer so callers do not have to work that out.

### How the numbers are defined

- **`time_to_first_token_us`** — from sending the request to the first streamed
  chunk carrying non-empty content.
- **`output_tokens_per_second_milli`** — `(tokens - 1) × 10⁹ ÷ (total_us -
  ttft_us)`, in thousandths of a token per second. Prefill is reported
  separately as time to first token rather than being averaged into the
  generation rate.
- **`output_tokens`** — the server's `usage.completion_tokens` when it reports
  one (`token_count_source: "server-usage"`), otherwise a tally of streamed
  content deltas (`"stream-deltas"`), which is a proxy and is labelled as one.
- **`prompt_tokens`** — the server's count, which is the ground truth that
  `context_tokens` only estimates. A plugin cannot tokenize for an arbitrary
  remote model, so `context_tokens` is turned into a prompt length using the
  recorded `chars_per_token_estimate_milli`.

A response that is not streamed produces a clear error, not a zero: time to
first token is not observable from a buffered response, so there is no honest
number to report.

### Why every number in a record is an integer

`serde_json` does not round-trip `f64` exactly. A real run here produced
`31.165399999999998`, which came back from JSON as `31.1654` — a different
double. The record then failed to verify on the receiving node even though it
had been signed correctly. Timings are therefore microseconds, rates are
thousandths of a token per second, and sampling settings are thousandths. A test
(`record::tests::a_claim_contains_no_floating_point_number`) keeps it that way.

Human-readable floats appear in `status` and `peers`, which are not signed.

---

## Never benchmarking over live traffic

Benchmarking a node that is serving somebody's request does two kinds of damage:
it degrades that request, and it measures a machine under unknown load, which is
not the number the record claims to carry.

So every attempt passes two gates, and the load gate **fails closed** — if the
plugin cannot tell whether the node is busy, it defers.

| Gate | Deferral reason |
| --- | --- |
| An operator hold (`hold` tool) | `hold` |
| Cooldown since the last completed attempt | `cooldown` |
| Exponential backoff after failures (1 min, doubling, capped at 1 h) | `backoff` |
| Node is serving traffic | `node_busy` |
| Load could not be determined | `activity_unknown` |

The `benchmark` tool's `ignore_cooldown` skips cooldown and backoff — an
operator asking for a run now has better information than a timer does. It never
skips a hold, and it never skips the load gate, because those two protect
somebody else's request rather than the schedule.

### Telling whether the node is busy

**Configure `--busy-url`.** Point it at a loopback URL on this node that returns
JSON with a count of in-flight requests, and set `--busy-pointer` to the JSON
pointer for that count (default `/active_requests`). Anything other than a
non-negative number at that pointer — missing key, string, unreachable endpoint —
is `activity_unknown`, and the run is skipped. Never an optimistic zero.

> **Prerequisite, stated plainly:** this plugin does not ship a source for that
> number. It is whatever your inference server or your own sidecar exposes;
> vLLM's `/metrics` scraped into JSON, an in-house status endpoint, anything with
> a count in it. If you have nothing to point at, leave it unset and read the
> next paragraph.

**Without `--busy-url`**, the fallback is a one-token request whose time to first
token is compared against `--max-guard-ttft-ms` (default 750). This is a latency
proxy, not a queue depth: a cold model or a slow disk looks exactly like a busy
node. It errs towards deferring, it is labelled as a proxy in the deferral
detail, and it is strictly weaker than a real signal. Configure `--busy-url` if
you can.

---

## Which key signs, and why it is the small one

Records are signed with the **node key** (`~/.tdcc/key`), not the owner key.

- The subject of a capability record is a node, and the node key's public half
  *is* the endpoint id peers route to. A verifier needs nothing beyond the record
  and the peer id it already has.
- The owner key can sign node ownership certificates. A plugin holding it could
  mint those. **This plugin never opens the owner keystore.**

Attribution still works. When the host has written a
`~/.tdcc/node-ownership.json`, the record carries that host-produced,
owner-signed certificate unchanged, and verification checks it against
`~/.tdcc/trusted-owners.json` with `tdcc_identity::verify_node_ownership` —
expiry, revoked owners, revoked certificates, revoked node ids, and the local
trust policy all included. This plugin never issues an ownership claim.

All key handling, path resolution (`TDCC_HOME` included), and certificate
verification come from `tdcc-identity`. Nothing here defines a key file, a key
format, or a key location of its own.

---

## Prerequisites

1. **A node identity.** `~/.tdcc/key` must exist. Starting `tdcc` once creates
   it. Without it the plugin starts, reports itself unhealthy, and `status`
   explains what to run.
2. **A node-local OpenAI-compatible endpoint** that honours `"stream": true`.
   vLLM, TGI, Ollama, llama.cpp's server, or whatever the `openai-endpoint`
   plugin is attached to.
3. **Optionally** `nvidia-smi` on `PATH` for a measured VRAM reading, or
   `--vram-total-mib` to declare one.

### The endpoint must be loopback

`--endpoint` is refused unless it is `http://` and its host is a loopback
address or `localhost`. This is both the narrowest useful permission and a
correctness rule: `tdcc` pools GPUs across machines, so a request sent to a
non-local address **may be served by a peer** — the numbers would describe
someone else's hardware while carrying this node's signature. That is exactly the
failure this plugin exists to prevent.

`--allow-remote-endpoint` overrides it and stamps the record
`endpoint_locality: "remote"`, which `verify` reports as a problem so callers can
discount it.

Plain HTTP only: the target is a process on the same machine, so the build links
no TLS stack at all.

---

## Build

`tdcc-plugin` and `tdcc-identity` are **not on crates.io under those names** —
the SDK was renamed from `mesh-llm-plugin` and the repository is private, so
`tdcc-plugin = "0.72.1"` does not resolve. `Cargo.toml` therefore uses path
dependencies that assume `tdcc-mesh` and `tdcc-plugins` are siblings:

```text
token/
  tdcc-mesh/
  tdcc-plugins/plugins/capability-attest/
```

```toml
tdcc-identity = { path = "../../../tdcc-mesh/crates/tdcc-identity", features = ["host-io"] }
tdcc-plugin   = { path = "../../../tdcc-mesh/crates/tdcc-plugin" }
```

If your checkout is elsewhere, edit those two paths or add a `[patch]` section.

**What a public consumer will have to change once the SDK is published:**
replace both path dependencies with plain version requirements matching the
`tdcc` release you target —

```toml
tdcc-identity = { version = "0.72.1", features = ["host-io"] }
tdcc-plugin   = "0.72.1"
```

— and nothing else. No source file imports anything by path. The
`ed25519-dalek = "=3.0.0-rc.0"` pin must keep matching `tdcc-identity`'s, since
the two crates exchange `VerifyingKey` values.

```bash
cargo build --release
cargo test
```

`tdcc-plugin` builds its protocol types with `prost-build`, so the first build
downloads a vendored `protoc`. No system protobuf compiler is required.

---

## Configure

`[plugin.settings]` values never reach a plugin process — the host stores them
and the console renders them, but nothing delivers them across the control
connection. So this plugin declares **no config schema** (a schema would render
controls the process could not read) and takes everything from
`[[plugin]].args`, `[[plugin]].url`, or the environment.

```toml
# ~/.tdcc/config.toml
version = 1

[[plugin]]
name = "capability-attest"
enabled = true
url = "http://127.0.0.1:8000/v1"     # arrives as TDCC_PLUGIN_URL
args = [
  "--model", "Qwen3-8B-Instruct",
  "--context-tokens", "2048",
  "--max-output-tokens", "256",
  "--interval-secs", "3600",
  "--busy-url", "http://127.0.0.1:8000/stats",
  "--busy-pointer", "/running_requests",
]
```

Every flag is also readable as `TDCC_ATTEST_<FLAG>` with hyphens as underscores
(`--max-guard-ttft-ms` → `TDCC_ATTEST_MAX_GUARD_TTFT_MS`). Precedence, highest
first: flag, `TDCC_ATTEST_*`, `TDCC_PLUGIN_URL` (endpoint only), built-in
default. Unknown flags are an error rather than being ignored — a typo that
silently reverted a setting would produce records pinned to something other than
what you wrote down.

Run `capability-attest --help` for the generated option list. The essentials:

| Flag | Default | Meaning |
| --- | --- | --- |
| `--endpoint <url>` | `TDCC_PLUGIN_URL` | Loopback OpenAI-compatible base URL. |
| `--model <id>` | *required* | Pinned into every record. |
| `--allow-remote-endpoint` | off | Permit a non-loopback endpoint; records it as `remote`. |
| `--api-key-env <name>` | `TDCC_ATTEST_API_KEY` | Env var holding a bearer token. |
| `--interval-secs <n>` | `3600` | How often the loop attempts a run (min 60). |
| `--min-interval-secs <n>` | `300` | Cooldown between completed runs (min 30). |
| `--record-ttl-secs <n>` | `7200` | Declared lifetime of a record. |
| `--context-tokens <n>` | `1024` | Approximate prompt length. |
| `--max-output-tokens <n>` | `128` | `max_tokens`, and the generation window. |
| `--temperature <f>` / `--top-p <f>` / `--seed <n>` | `0` / `1` / `42` | Sampling, pinned to 3 decimal places. |
| `--warmup-runs <n>` / `--measured-runs <n>` | `1` / `3` | Discarded and recorded runs. |
| `--request-timeout-secs <n>` | `120` | Per-request timeout. |
| `--busy-url <url>` / `--busy-pointer <ptr>` / `--busy-threshold <n>` | unset / `/active_requests` / `0` | The real load signal. |
| `--max-guard-ttft-ms <n>` | `750` | Fallback latency proxy limit. |
| `--vram-probe nvidia-smi\|off` | `nvidia-smi` | VRAM probe. |
| `--vram-total-mib <n>` | unset | Operator-declared VRAM. |
| `--node-key-path <path>` | `<TDCC_HOME>/.tdcc/key` | Node key override. |
| `--filler-sentence <text>` | built-in | Changes the profile fingerprint, by design. |

### Secrets

There is no `--api-key` flag, deliberately: a token on a process command line is
visible to every user on the machine. The bearer token is read from the
environment variable *named* by `--api-key-env`, never logged, and never written
into a record. `status` also strips any `user:password@` from the endpoint URL
before showing it.

---

## What it contributes

Capability `capability-attest.v1`. Mesh channel `capability-attest.v1`, message
kinds `record` and `request`. Mesh events `peer_up` and `peer_down` — nothing
else is declared, so nothing else is delivered.

### MCP tools

Namespaced on the host endpoint as `capability-attest.<name>`.

| Tool | Does |
| --- | --- |
| `status` | Profile, current record, schedule, and why the last attempt did or did not run. |
| `record` | The latest signed record, with this node's own verification of it. |
| `verify` | Verify any node's record. `max_age_seconds` applies your freshness policy over the record's own. |
| `benchmark` | Run now. `ignore_cooldown` skips the timer, not the hold or the load gate. |
| `hold` | Pause attestation for `seconds` (0 clears) before a driver update or a rebuild. |
| `peers` | Peer records, re-verified at read time so freshness is current. |

`benchmark` takes as long as the benchmark takes. Health stays responsive
throughout — it reads one field and never waits on the benchmark lock.

### HTTP routes

Mounted by the host under `/api/plugins/capability-attest/http/`:

```bash
curl --fail http://127.0.0.1:3131/api/plugins/capability-attest/http/status
curl --fail http://127.0.0.1:3131/api/plugins/capability-attest/http/record
curl --fail http://127.0.0.1:3131/api/plugins/capability-attest/http/peers
curl --fail -X POST http://127.0.0.1:3131/api/plugins/capability-attest/http/verify \
  -H 'Content-Type: application/json' \
  -d '{"record": { … }, "max_age_seconds": 3600}'
```

### Mesh exchange

On `peer_up`, this node asks the peer for its record and offers its own. On a
`request` message it replies with its latest record; on a `record` message it
verifies and stores. A record that fails signature verification is **not
stored** — keeping it would only give a hostile peer a way to fill the map. A
record claiming to be from this node is refused. At most 256 peer records are
retained, oldest-received evicted first.

`transport_peer_id` — the id the sending host stamped on the frame — is reported
next to `transport_peer_id_matches_signing_key`, but it is *not* covered by the
record's signature. A mismatch means the record was relayed, not that it is fake.

---

## A record

Produced by the test suite against a stub endpoint, so the numbers are the
stub's, not a real GPU's:

```json
{
  "claim": {
    "version": 1,
    "record_id": "3bd389e4e5f7dd2a5a5f5333e0455e1c",
    "node_endpoint_id": "e28a8970753332bd72fef413e6b0b2ef1b4aadda7aa2c141f233712a6876b351",
    "attester": "capability-attest",
    "attester_version": "0.1.0",
    "measured_at_unix_ms": 1786873991444,
    "expires_at_unix_ms": 1786881191444,
    "endpoint_locality": "loopback",
    "profile": {
      "model": "demo-model",
      "context_tokens": 16,
      "max_output_tokens": 8,
      "temperature_milli": 0,
      "top_p_milli": 1000,
      "seed": 42,
      "warmup_runs": 0,
      "measured_runs": 3,
      "chars_per_token_estimate_milli": 4000,
      "filler_sentence": "This paragraph is deterministic benchmark filler used to reach a fixed context size.",
      "prompt_chars": 64,
      "prompt_sha256": "ca60975ac77c6241af4647467ff518bb3757bae424dfbf4800af22b438803ed2"
    },
    "measurement": {
      "runs": [
        {
          "run": 1,
          "time_to_first_token_us": 30249,
          "total_us": 77575,
          "output_tokens": 9,
          "output_tokens_per_second_milli": 169040,
          "token_count_source": "server-usage",
          "prompt_tokens": 260
        }
      ],
      "median_output_tokens_per_second_milli": 169040,
      "median_time_to_first_token_us": 31201,
      "warmup_runs_discarded": 0,
      "vram": {
        "source": "unavailable",
        "total_mib": null,
        "free_mib": null,
        "devices": [],
        "detail": "no VRAM reading: --vram-probe off. Set --vram-total-mib to declare one, knowing it will be recorded as operator-declared"
      }
    },
    "ownership": null
  },
  "signature": "71593e08a160920ba65357c1e0c02ffef4d4f21d1e246aa44b7e9b25c3522bbf982cfdeee86278e6da2af2cf37133268c0dc94f38fc0a920d8ce369b9e3de002"
}
```

The signature is Ed25519 over a length-prefixed canonical encoding of the claim,
domain-separated with `tdcc-capability-attest-v1:` so it can never be replayed as
a signature over anything else this key signs. A test enumerates the claim's
fields and asserts that changing **any** of them changes the signed bytes, so a
field cannot later be added outside the signature.

### VRAM sourcing

`vram.source` is one of three things, and a verifier that treats them alike is
not verifying anything:

- `"nvidia-smi"` — measured. `total_mib` and `free_mib` are both real.
- `"operator-declared"` — from `--vram-total-mib`. A statement of intent, not a
  measurement; `free_mib` is null because a declaration cannot know it.
- `"unavailable"` — nothing could be measured and nothing was declared.

There is no ROCm, Metal, or Level Zero probe. Their output formats are not
something this plugin can claim to parse correctly without being able to test
against them, and a wrong VRAM number in a signed record is worse than an absent
one. Those platforms use `--vram-total-mib`.

---

## Blast radius

Installing a plugin runs third-party native code with your user account's
privileges. There is no sandbox. What this one does:

| Does | Detail |
| --- | --- |
| Network | Outbound HTTP to `--endpoint` and `--busy-url` only, both refused unless loopback (`--allow-remote-endpoint` widens the first, and labels the record). No TLS stack is linked. |
| Subprocess | Exactly `nvidia-smi --query-gpu=memory.total,memory.free --format=csv,noheader,nounits`. Fixed argument list, no shell, so no configuration value or request argument reaches a command line. Killed if it exceeds 10 s. Disabled with `--vram-probe off`. |
| Files read | `~/.tdcc/key`, `~/.tdcc/node-ownership.json`, `~/.tdcc/trusted-owners.json`, all through `tdcc-identity`. |
| Files written | None. Records live in memory; a restart re-benchmarks. |
| Keys | The node key only. The owner keystore is never opened. |
| Mesh | One declared channel, two declared events. |
| Untrusted input | Peer records are parsed, verified, and dropped unless the signature checks out. Retention is capped at 256. |

Because `nvidia-smi` is resolved through `PATH`, a `PATH` an attacker controls
means an attacker-chosen `nvidia-smi`. Worth writing down even though a plugin
already runs as you.

---

## Package and install locally

From this directory, on macOS or Linux:

```bash
cargo build --release
rm -rf target/package
mkdir -p target/package/capability-attest
cp target/release/capability-attest target/package/capability-attest/capability-attest
cp plugin.toml README.md target/package/capability-attest/
cp ../../LICENSE target/package/capability-attest/     # Apache-2.0, from the repo root
tar -C target/package -czf target/capability-attest-0.1.0-local.tar.gz capability-attest

tdcc plugins install --archive ./target/capability-attest-0.1.0-local.tar.gz \
  --name capability-attest --version 0.1.0
tdcc plugins info capability-attest
```

On Windows, copy `capability-attest.exe` instead and build a `.zip` whose single
top-level directory is `capability-attest/`:

```powershell
Compress-Archive -Path target\package\capability-attest `
  -DestinationPath target\capability-attest-0.1.0-local.zip -Force
tdcc plugins install --archive .\target\capability-attest-0.1.0-local.zip `
  --name capability-attest --version 0.1.0
```

Set `TDCC_PLUGIN_DIR` to an empty directory first to keep it out of your real
plugin store.

The plugin declares neither a config schema nor a web UI, so
`--print-package-manifest` emits `{}` and `plugin-manifest.json` may be left out
of the archive.

Running the binary directly, outside a host, fails immediately with
`TDCC_PLUGIN_ENDPOINT is not set for plugin process`. That is correct: the host
owns the control endpoint.

---

## Limitations

Stated so nobody has to discover them the hard way.

- **A signature does not make a measurement true.** See the top of this file.
- **`context_tokens` is an estimate.** A plugin cannot tokenize for an arbitrary
  remote model. The record carries the estimate *and* the server's real
  `prompt_tokens` when the server reports one.
- **The fallback load signal is a latency proxy.** Without `--busy-url`, a cold
  model looks like a busy node. It errs towards deferring.
- **No VRAM probe outside NVIDIA.** Everything else is operator-declared and
  labelled as such.
- **Records are not persisted.** A restart means no record until the first run
  completes (two minutes after start-up, then subject to the gates).
- **`comparable_with_this_node` is fingerprint equality**, so a peer running a
  different model or context size is reported as not comparable rather than
  being scaled somehow. There is no honest way to scale across profiles.
- **The plugin cannot see the mesh transport's key material**, so it reports
  whether `transport_peer_id` matches the signing key rather than asserting it.

---

## License

Apache-2.0.
