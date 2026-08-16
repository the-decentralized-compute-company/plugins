# workload-policy

Lets the person who owns a machine say what it will and will not accept, in a
file, and refuses the work that does not match — before the node runs any of it.

On a mesh of donated hardware this is not a comfort feature. Someone plugging a
gaming PC into a shared pool has opinions — only overnight, nothing over 8k
context, not that model, not from that peer — and they need somewhere to put
them before they contribute, not after something surprising happens.

```toml
# ~/.tdcc/workload-policy.toml
version = 1
mode = "enforce"
default = "deny"

# This machine is mine during the day.
[[rule]]
id = "overnight-only"
action = "deny"
reason = "This node accepts work between 22:00 and 06:00 local time."
when.hours = ["06:00-22:00"]

# Big jobs belong somewhere with more VRAM.
[[rule]]
id = "context-cap"
action = "deny"
reason = "This node caps context at 8192 tokens."
when.context_tokens_over = 8192

# Everything else on the allow list, at a fair rate per peer.
[[rule]]
id = "allowed-models"
action = "limit"
when.models = ["qwen/*", "meta/llama-3*"]
limit = { requests = 60, per_seconds = 60, per = "peer" }
```

---

## What this is, exactly

This plugin **decides, and the host enforces the decision.**

It declares the host's admission capability, `admission-control.v1`. The host
calls it before it schedules any inbound inference request — from a local client
on `127.0.0.1:9337` and from mesh peers, whose requests are tunnelled to that
same listener — and acts on the answer. A `deny` stops the request there and
returns a `403` (or a `429` when a rate limit asked the caller to come back
later) carrying this plugin's outcome code and the operator's own sentence.
Nothing is scheduled, nothing is routed, nothing runs.

*Inference request* is doing work in that sentence. One inbound path is not one:
if this node hosts a skippy pipeline stage for a peer's run, those activations
arrive on a separate QUIC ALPN and never reach this hook. The host documents
that gap under "What Is Not Covered" in `docs/plugins/admission.md`. Rules here
govern the requests this node is asked to answer, which is most of what a
donated machine does, but not the whole of it.

> **This changed in 0.2.0.** Before it, the host had no hook to consume a
> plugin's decision, and this README said so: the plugin decided and something
> in front of the node had to honour the answer. `tdcc` now has a general
> admission hook, documented in `docs/plugins/admission.md`, and this plugin is
> its first consumer. If you deployed 0.1.0 behind a gateway that calls `check`,
> that still works — `check` is unchanged — but it is no longer the only way to
> get enforcement.

Four ways in, all running the same evaluator:

| Shape | Who calls it | Enforcing? |
| --- | --- | --- |
| Capability `admission-control.v1` → operation `admit` | The host, before it schedules anything | **Yes** |
| Capability `workload-policy.v1` | Anything that wants the richer `check`/`report`/`policy` surface | No — advisory |
| MCP tool `workload-policy.check` | An agent, an operator, or the mesh MCP endpoint | No — advisory |
| `POST /api/plugins/workload-policy/http/check` | A gateway in front of the node, or a script | No — advisory |

`check` remains the way to ask "what would this node do with a request like
*that*" without submitting one, and it is the only one that takes `explain`.

### What the host does when this plugin cannot answer

The host owns that decision, not this plugin, and it is worth knowing before you
install anything: a **declared** hook that is slow, crashed, or disabled fails
**closed** by default. Set `TDCC_ADMISSION_ON_UNAVAILABLE=allow` to keep serving
instead; it logs a warning on every request it lets through. The host's deadline
for one call is 2 seconds (`TDCC_ADMISSION_TIMEOUT_MS`), and it is hard — a
wedged plugin cannot hang the node.

That is separate from what *this plugin* does about a policy file it cannot
read, described under [When things go wrong](#when-things-go-wrong). Both fail
closed, for the same reason, at different layers.

Everything else in here — the policy language, the precedence rules, the
dry-run ledger, the failure behaviour — is the part that is hard to get right,
and it is complete.

### What this deliberately does not do

**It does not read prompts.** Every condition is a structural property the node
can evaluate reliably: which model, which peer, which owner, what kind of
request, how many tokens, what time it is. There is no condition about what a
request *says*.

That is a design boundary, not an omission. Judging the meaning of a prompt is a
different and much harder problem, and its failure mode is worse: a classifier
that is 95% right is a machine that refuses one in twenty legitimate jobs for
reasons its operator cannot reproduce, while still passing the things someone
actually meant to sneak through. A rule about a token count is either right or
wrong, and you can tell which by reading it.

The boundary is enforced in the type system, not just in the docs: both the
`check` arguments and the host's `AdmissionRequest` set `deny_unknown_fields`, so
a caller that sends `prompt` or `messages` gets an invalid-arguments error rather
than having it quietly ignored. There is nowhere for content to land, which also
means there is nothing about content in the decision log.

**It does not modify requests.** A hook answers allow or deny. It cannot rewrite
the model, trim the context, or send the work somewhere else.

### What the host can and cannot tell it

At the OpenAI ingress two descriptor fields come with caveats, and a rule that
depends on them should be written knowing which:

- **`context_tokens` is an estimate.** There is no tokenizer in front of the
  ingress, so the host derives the figure from the request body size at four
  bytes per token and marks it `context_tokens_estimated`. A
  `when.context_tokens_over = 8192` rule is still your line; it is drawn against
  an approximation. Leave headroom if the exact boundary matters.
- **`peer` and `owner` arrive empty.** Mesh traffic reaches the ingress through a
  byte-level tunnel that terminates on loopback, and the submitting peer's
  identity is not carried across it. The host leaves the fields unset rather than
  guessing.

The second one has teeth: a policy containing `when.peers` or `when.owners`
declares those as required request fields, and a request that omits a required
field is refused with `policy.incomplete_request` before any rule runs. **A peer
allow-list therefore refuses everything at the OpenAI ingress today.** That is
the correct direction to fail — the alternative is a rule that quietly matches
nothing — but it means peer and owner rules are not usable there yet. Use model,
kind, size, and time conditions until the host can attribute a peer, and use
`check` if you need to evaluate peer rules from a component that does know the
identity.

---

## Install and configure

Build it (see [Building against the SDK](#building-against-the-sdk) first —
the dependency needs one edit or one sibling checkout), package it, and install
the archive:

```bash
cargo build --release
tdcc plugins install --archive ./target/workload-policy-0.2.0-local.tar.gz \
  --name workload-policy --version 0.2.0
```

Then in `~/.tdcc/config.toml`:

```toml
[[plugin]]
name = "workload-policy"
args = ["--policy", "/home/you/.tdcc/workload-policy.toml"]
```

`args` is optional. Without it the plugin reads
`$HOME/.tdcc/workload-policy.toml` (`%USERPROFILE%` on Windows).

| Option | Environment variable | Default |
| --- | --- | --- |
| `--policy <path>` | `TDCC_WORKLOAD_POLICY_FILE` | `$HOME/.tdcc/workload-policy.toml` |
| `--on-invalid-policy <deny\|allow>` | `TDCC_WORKLOAD_POLICY_ON_INVALID` | `deny` |

Arguments win over the environment. `--help` prints the same table.

### Why the policy is a file

`[plugin.settings]` never reaches a plugin process. The host stores those values
and the console renders them; there is no settings field in the launch contract
or the initialize handshake. A plugin that declared a `config_schema` for its
rules would get a nice-looking settings page that changed nothing.

So this plugin declares no config schema, and the policy lives in a file whose
path arrives through `args` or the environment — the two channels that do reach
the process. The file also happens to be the right shape for the job: it is
reviewable, diffable, and can be managed by whatever configuration tooling the
operator already uses.

---

## The policy file

```toml
version = 1                 # required; this build understands version 1

mode = "dry-run"            # "dry-run" (default) | "enforce"
default = "allow"           # "allow" (default) | "deny" — used when no rule matches
timezone = "local"          # "local" (default) | "utc" — for `hours` and `days`
observe = 500               # decisions retained for the report; 0 keeps counters only

[[rule]]
id = "a-stable-id"          # required, unique, [A-Za-z0-9-_.:], max 64 chars
action = "deny"             # "allow" | "deny" | "limit"
reason = "..."              # optional; shown to whoever was refused
when.models = ["qwen/*"]    # conditions — see below
```

**Unknown keys are errors.** `modle = "enforce"` does not silently do nothing;
it stops the file loading, and the error names the key and the line.

### Conditions

A rule matches when **every** condition it declares holds. A rule with no `when`
block matches every request.

| Condition | Matches when | Notes |
| --- | --- | --- |
| `when.models` | the requested model matches any pattern | `*` wildcard; ASCII case-insensitive |
| `when.peers` | the submitting peer id matches any pattern | `*` wildcard; **case-sensitive** |
| `when.owners` | the owner identity matches any pattern | `*` wildcard; **case-sensitive** |
| `when.kinds` | the request kind is exactly one of these | case-insensitive; e.g. `["chat", "embedding"]` |
| `when.context_tokens_over` | context size is **strictly greater** than this | |
| `when.max_output_tokens_over` | requested output size is strictly greater than this | |
| `when.hours` | node-local time is inside any window | `"22:00-06:00"`; start inclusive, end exclusive, wraps past midnight |
| `when.days` | node-local weekday is one of these | `["sat", "sun"]`, short or long names |

Peer and owner ids are matched case-sensitively on purpose. They are opaque
identifiers, and base58 and hex-cased alphabets contain distinct values that
differ only in case — folding them together would quietly widen an allow list
past what was written. Model ids are typed by hand and are matched
case-insensitively.

`when.hours` and `when.days` read the node's clock, not the caller's, so a peer
cannot claim it is 3 a.m.

### Rate limits

```toml
[[rule]]
id = "fair-share"
action = "limit"
limit = { requests = 60, per_seconds = 60, per = "peer" }
```

A `limit` rule allows the request while its budget lasts and denies it after.
`per` is `node` (one budget for the machine, the default), `peer`, `owner`, or
`model`. The implementation is a token bucket: `requests` is both the burst
capacity and the number that refills over `per_seconds`. A refusal carries
`retry_after_ms`.

Each rule has its own budget — two `limit` rules keyed on the same peer do not
share one — and budgets are dropped on reload, since the rules they belonged to
may no longer exist.

Bucket cardinality is capped at 4096. `per = "peer"` and `per = "owner"` key on
caller-supplied identifiers, so an uncapped map would be a memory-exhaustion
vector on hardware someone lent you. Past the cap the tracker drops idle buckets
first; if every bucket is still live, a new one is refused with
`policy.rate_limit_capacity` rather than allocated. `report` exposes
`rate_limit_buckets` so a number pinned at 4096 is visible rather than
mysterious.

### Precedence

**The first matching rule wins, in file order.** That is the whole rule. There
is no specificity scoring and no implicit "deny beats allow" — a policy does
what it reads like it does, top to bottom.

```toml
# Allowed, because "first" matches first.
[[rule]]
id = "first"
action = "allow"
when.models = ["qwen/*"]

[[rule]]
id = "second"
action = "deny"
when.models = ["qwen/*"]
```

If no rule matches, `default` decides. The allow-list idiom is an `allow` rule
followed by `default = "deny"`; the deny-list idiom is a `deny` rule and
`default = "allow"`.

### Requests that leave a field out

A condition over a field the request did not supply is **not** satisfied. On its
own that would be a hole: a caller could walk past `when.models = ["banned/*"]`
by simply not mentioning a model.

So the loaded policy computes which request fields it depends on, and a request
missing any of them is refused with `policy.incomplete_request` before any rule
runs. `workload-policy.policy` lists those fields as
`required_request_fields`, and the refusal names the ones that were missing. An
empty string counts as missing — `peer = ""` is not an identity, and accepting
it would create one anonymous rate-limit bucket that every caller could hide in.

Conditions that read the node's own clock (`hours`, `days`) require nothing from
the caller.

---

## Start in dry-run, then enforce

`mode` defaults to `dry-run`, and so does a node with no policy file at all.
That is deliberate: installing this plugin must never be the reason a machine
stops working, and an operator should be able to build a policy from their own
traffic instead of guessing at it.

In dry-run every request is evaluated, recorded, and then **served anyway**. The
response says so explicitly:

```json
{
  "decision": "allow",
  "would_deny": true,
  "enforced": false,
  "reason": "dry-run: this request was served, but an enforcing policy would refuse it — This node caps context at 8192 tokens.",
  "error": null
}
```

`error` is null because nothing was refused; there is no refusal to hand back.
The admission hook reads `decision`, and only `decision` — so a dry-run policy
stays dry-run at the enforcement point too. Wiring this plugin to the host does
not turn a dry run into an enforcing one; changing `mode` does.

The workflow:

1. Install with no policy file. Everything is served and recorded.
2. `workload-policy.report` — see which models, peers, and sizes actually turn
   up on this machine.
3. Write rules. Keep `mode = "dry-run"`.
4. `workload-policy.reload`, then `report` again. `would_deny` is what you would
   have refused; check that number is the number you meant.
5. Set `mode = "enforce"` and reload.

A policy with rules loaded in dry-run is reported as a warning by the `policy`
tool and logged at startup, because "I wrote the rules and nothing happened" is
the single most likely way to misunderstand this plugin.

---

## When things go wrong

| Situation | What happens | Why |
| --- | --- | --- |
| No policy file | Permissive dry-run, everything recorded | Installing an unconfigured plugin must not take a node out of service. An absent file is an unambiguous "I have not configured this yet". |
| Policy file does not parse or does not validate | **Every request is refused** with `policy.unavailable`, and the errors go to the log at startup | The file existing is evidence the operator wanted rules. Serving wide open while their rules sit unparsed is the worst of the three outcomes — it is the one where someone is wrong about what their machine is doing. |
| File exists but cannot be read (permissions) | Same as above — fail closed | "There is a policy I cannot read" is not the same as "there is no policy". |
| One rule in the file is invalid | The whole file fails to load | A partially applied policy is worse than none. By Murphy, the rule that failed to parse is the one that was holding the door shut. |
| `reload` of a bad file over a good one | Reload returns an error listing everything wrong; **the previously loaded policy stays in force** | A hot reload must never be able to change what a live node accepts because of a typo. |
| `reload` when the file has been deleted | Error; the loaded policy stays in force | Otherwise deleting a file would be the easiest way to remove a node's rules. |
| `reload` of a bad file when nothing good was loaded | The node moves to fail-closed | There is nothing to fall back to, and continuing to look unconfigured would be misleading. |

Fail-closed on an unreadable policy is the sharp edge here, and it is the right
default — but it is escapable. `--on-invalid-policy allow` keeps the node
serving, and says loudly, in the startup log and in every `policy` response,
that your rules are **not** being applied. It is a recovery hatch for someone
whose node is refusing everything at 3 a.m., not a posture to run in.

Validation reports every complaint at once, so a broken file can be fixed in one
pass. TOML syntax errors are the exception: those come from the parser one at a
time, with a line number and a caret.

One more layer sits above all of this. If the *plugin process itself* is not
answering — it crashed, it is disabled, it is wedged — the host applies its own
unavailable policy, which also fails closed. So there are two independent
fail-closed switches, and they escape independently:

| Broken thing | Who decides | Escape hatch |
| --- | --- | --- |
| The policy file will not load | This plugin | `--on-invalid-policy allow` |
| This plugin will not answer | The host | `TDCC_ADMISSION_ON_UNAVAILABLE=allow` |

---

## Tools and routes

The admission hook is not in this table: it is not a tool, it has no HTTP route,
and nothing but the host calls it. Everything below is for humans and gateways,
projected twice — once as an MCP tool, once as an HTTP route — with both running
the same function.

| MCP tool | HTTP | Does |
| --- | --- | --- |
| `workload-policy.check` | `POST …/http/check` | Evaluate one request |
| `workload-policy.report` | `GET …/http/report?limit=100` | Counters, recent decisions, top models and peers |
| `workload-policy.policy` | `GET …/http/policy` | What is loaded, with warnings and load errors |
| `workload-policy.reload` | `POST …/http/reload` | Re-read the policy file |

HTTP routes are mounted at `/api/plugins/workload-policy/http/…` on the console
port.

```bash
curl --fail -X POST http://127.0.0.1:3131/api/plugins/workload-policy/http/check \
  -H 'Content-Type: application/json' \
  -d '{"model":"qwen/qwen3-8b","peer":"12D3KooW…","context_tokens":4096,"explain":true}'
```

`reload` takes no path argument, on purpose. A reload that accepted a
caller-supplied path would be a "read any file on this machine and tell me what
is wrong with it" oracle, since load errors quote the file. The path is fixed at
startup.

### Reading a refusal

There are two shapes, because there are two callers.

**Through the host's admission hook**, a refusal is the response to the original
inference request: a `403` (or `429`, when `retry_after_ms` is set), with a
`Retry-After` header on the `429`. The body is the OpenAI error envelope every
client already parses, with the structured detail beside it:

```json
{
  "error": {
    "message": "Local workload policy on this node declined the request: This node caps context at 8192 tokens.",
    "type": "permission_error",
    "param": null,
    "code": "policy.deny_rule"
  },
  "admission": {
    "plugin": "workload-policy",
    "code": "policy.deny_rule",
    "message": "Local workload policy on this node declined the request: This node caps context at 8192 tokens.",
    "rule_id": "context-cap",
    "unavailable": false
  }
}
```

`"unavailable": true` means something different and important: the hook did not
answer at all and the node failed closed. That is a breakage to investigate, not
a rule that fired. See `docs/plugins/admission.md`.

**Through `check`**, the caller is a gateway or a script asking a question, and:

> **A refusal is an HTTP `200` with `"decision": "deny"`.** The status code
> describes whether the evaluation worked, not what it decided. A gateway that
> checks only the status code will fail open. Branch on `decision`.

A refusal carries a ready-made envelope to hand back to whoever submitted the
work:

```json
{
  "decision": "deny",
  "enforced": true,
  "would_deny": true,
  "code": "policy.deny_rule",
  "rule_id": "context-cap",
  "reason": "This node caps context at 8192 tokens.",
  "error": {
    "type": "workload_policy_denied",
    "code": "policy.deny_rule",
    "message": "Local workload policy on this node declined the request: This node caps context at 8192 tokens.",
    "rule_id": "context-cap",
    "retry_after_ms": null,
    "node_policy_source": "/home/you/.tdcc/workload-policy.toml"
  }
}
```

`error` is present if and only if `decision` is `deny`. Never drop a request
without surfacing it — a silent drop looks like a network fault, and the whole
point of this plugin is that a refusal is legible.

Outcome codes are stable identifiers; treat a change to one as breaking.

| Code | Meaning |
| --- | --- |
| `policy.allow_rule` | An `allow` rule matched, or a `limit` rule had budget left |
| `policy.deny_rule` | A `deny` rule matched |
| `policy.default_allow` / `policy.default_deny` | No rule matched; the policy default decided |
| `policy.rate_limited` | A `limit` rule is out of budget; see `retry_after_ms` |
| `policy.rate_limit_capacity` | Rate-limit bucket cap reached; a new bucket could not be created |
| `policy.incomplete_request` | The request omitted a field the policy needs |
| `policy.unavailable` | The policy file could not be loaded and the node is failing closed |

`"explain": true` adds a `trace`: every rule considered, in order, whether it
matched, and the first condition that stopped it. Invaluable when a rule you
expected to fire did not.

---

## Blast radius

These run on other people's hardware, so:

- **Filesystem.** Reads exactly one file — the policy — at a path fixed when the
  process starts. Never writes anything. `reload` re-reads that same path and
  takes no path from a caller.
- **Network.** None. No sockets, no outbound requests, no listeners. The host
  owns the control connection and every projection.
- **Subprocesses.** None.
- **Mesh.** Declares no channels and no events, so the host delivers neither.
  Delivery is allowlist-based; a policy engine is a poor place to accept
  unsolicited input.
- **Secrets.** None, and nothing key-shaped belongs in a policy file. It holds
  identifiers, sizes, and times.
- **Memory.** Every unbounded input is capped: 512 rules, 4096 rate-limit
  buckets, 10 000 retained decisions, 1024 counter keys.
- **State.** All in memory. Counters and the decision ring reset when the plugin
  process restarts; the policy file is the only durable thing.

Three things worth knowing before you point this at anything important:

**It is in the request path.** Every inbound request now waits on this plugin
before the node serves it. The hook does no I/O — it takes one mutex and runs a
pure evaluation over at most 512 rules — and the host caps one call at 2 seconds
and treats a miss as a failure. But a policy engine that gets slow makes the
whole node slow, so keep rule counts sane and do not add anything here that
blocks.

**It trusts its caller.** `peer` and `owner` are strings the calling component
supplies. This plugin has no way to verify that the peer that submitted the work
is the peer named in the descriptor — that is the host's identity layer's job.
A peer allow list is exactly as trustworthy as whatever fills in that field. At
the OpenAI ingress the host supplies neither, which is why peer and owner rules
refuse everything there; see
[What the host can and cannot tell it](#what-the-host-can-and-cannot-tell-it).

**Load errors quote your file.** A TOML parse error includes the offending line.
That is your own configuration, returned over a local API to a local operator,
and it is what makes the errors usable — but if you put something private in a
policy file, it can come back out in an error message.

---

## Building against the SDK

`tdcc-plugin` is **not** on crates.io under that name — it was renamed from
`mesh-llm-plugin` and the repository is private — so the dependency line in the
published docs, `tdcc-plugin = "0.72.1"`, does not resolve. This crate points at
a local checkout instead:

```toml
tdcc-plugin = { path = "../../../tdcc-mesh/crates/tdcc-plugin" }
```

That path assumes `tdcc-mesh` and `tdcc-plugins` are checked out as siblings:

```text
token/
  tdcc-mesh/         # the private host repository, providing crates/tdcc-plugin
  tdcc-plugins/      # this repository
    plugins/workload-policy/
```

If your layout differs, either change that one path, or leave the version
dependency in place and redirect it from the workspace root:

```toml
[patch.crates-io]
tdcc-plugin = { path = "/absolute/path/to/tdcc-mesh/crates/tdcc-plugin" }
```

**Once the SDK is published**, a public consumer replaces the path with a
version pinned to the `tdcc` release they target:

```toml
tdcc-plugin = "0.72.1"
```

Nothing else changes — no code in this crate depends on the dependency being
local. The initialize handshake requires an exact protocol-version match, so a
plugin built against a mismatched SDK refuses to connect at startup with a clear
message rather than misbehaving later.

Two dependencies beyond the SDK's usual set:

- **`toml`** — the policy file format.
- **`chrono`** (`clock` feature only) — `hours` and `days` need to know what
  "22:00" means on this machine, including daylight saving. That single call is
  isolated in `src/clock.rs`; everything downstream of it takes a timestamp as
  an argument, which is why the time-of-day tests do not have to wait for
  Tuesday.

The first build downloads a vendored `protoc` (the SDK builds its protocol types
with `prost-build`); no system protobuf compiler is required.

```bash
cargo check
cargo test
```

---

## Package and install locally

```bash
cargo build --release

rm -rf target/package
mkdir -p target/package/workload-policy
cp target/release/workload-policy target/package/workload-policy/workload-policy
cp plugin.toml README.md target/package/workload-policy/
cp ../../LICENSE target/package/workload-policy/
tar -C target/package -czf target/workload-policy-0.2.0-local.tar.gz workload-policy

tdcc plugins install --archive ./target/workload-policy-0.2.0-local.tar.gz \
  --name workload-policy --version 0.2.0
tdcc plugins info workload-policy
```

On Windows, copy `workload-policy.exe` instead and build a `.zip` whose single
top-level directory is `workload-policy\`:

```powershell
Compress-Archive -Path target\package\workload-policy `
  -DestinationPath target\workload-policy-0.2.0-local.zip -Force
```

Set `TDCC_PLUGIN_DIR` to an empty directory first if you do not want this
landing in your real plugin store.

This plugin declares neither a config schema nor a web UI, so
`--print-package-manifest` emits `{}` and `plugin-manifest.json` may be left out
of the archive.

Running the binary directly, outside a host, fails immediately:

```text
Error: TDCC_PLUGIN_ENDPOINT is not set for plugin process
```

That is correct. The host owns the control endpoint and passes it in through the
launch contract.

---

## Tests

```bash
cargo test
```

87 unit tests, covering the parts that are pure and therefore worth pinning
down: window parsing and midnight wrap-around, wildcard matching and the
case-sensitivity split, every validation rejection, conflict precedence, the
missing-field refusal, token-bucket refill and bucket-cap exhaustion, ledger
retention and counter caps, argument parsing, and the load/reload state machine
including "a bad reload never replaces a good policy".

`src/admit.rs` covers the enforcement path specifically: an enforcing deny rule
refuses, a permitted request is admitted, dry-run stays dry-run at the hook, a
rule keyed on a field the host cannot supply refuses rather than matching
nothing, a rate-limited refusal carries `retry_after_ms`, and every descriptor
field the policy understands survives the crossing.

The host side — the veto being consumed, the deadline, the plugin-absent case —
is tested in the `tdcc` repository under
`crates/tdcc-host-runtime/src/network/openai/admission.rs`.

---

## Compatibility

Semantic versioning. Treat these as breaking changes, because they are the names
other people wrote down: the capability id `workload-policy.v1`, the four MCP
tool names, the four HTTP paths, the outcome codes, the shape of the `error`
envelope, and the policy file keys.

The admission surface — the capability `admission-control.v1` and the operation
`admit` — belongs to the host, not to this plugin. It is defined in
`docs/plugins/admission.md` and versioned with `tdcc`. Dropping it here would be
a breaking change to this plugin, because the whole point of installing this is
that the host consults it.

`version = 1` in the policy file is the document version and is checked on load;
a future format change bumps it and says so rather than silently reinterpreting
a file.

## License

Apache-2.0, matching the repository.
