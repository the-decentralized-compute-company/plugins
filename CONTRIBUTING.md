# Contributing

This repository holds the plugin author guide, two teaching examples, and
eleven working plugins. Fixes and clarifications are welcome, and so is a new
plugin — read [What belongs here](#what-belongs-here) first, because the answer
is "sometimes".

Before anything else: **`tdcc-plugin` is not on crates.io**, so a fresh
`cargo build` fails at dependency resolution. See
[The SDK is not on crates.io](README.md#the-sdk-is-not-on-cratesio) in the
README. It is the single most common way to lose twenty minutes here.

---

## What belongs here

- Corrections to `README.md` when it disagrees with the code.
- Fixes and improvements to anything under `plugins/` or `examples/`.
- A new example, if it demonstrates a surface the existing two do not.
- A new plugin, **if you open an issue first**. The bar is high and the
  question is not "is it good" but "does someone running a node or contributing
  hardware need it, and is this repository where they will look for it". Say
  what it does, what it touches, and what it needs at runtime, and we will tell
  you whether it belongs here or in your own repository before you write it.

## What belongs elsewhere

- A bug in a first-party plugin — open it in that plugin's own repository
  (`blackboard`, the external `openai-endpoint`, `flash-moe`, `metrics`,
  `agents`). Note that this repository *also* has a plugin called
  `openai-endpoint`; say which one you mean.
- A bug in the SDK, installer, host projection, or console — open it against
  the main TDCC repository.
- A plugin you want to ship on your own schedule, under your own release
  cadence, or with dependencies you do not want reviewed here — publish it in
  your own repository and add a catalog entry. That is the normal path, it is
  fully supported, and nothing about it is second-class.

---

## Scaffolding a plugin

```bash
cp -R examples/hello-plugin plugins/my-plugin
cd plugins/my-plugin
```

### 1. Fix the five names

They must all be the same string, and two of them are enforced by the host:

| Where | File | Enforced? |
| --- | --- | --- |
| Crate name | `Cargo.toml` `[package] name` | No, but keep it aligned |
| Package marker | `plugin.toml` `name` | No — but it is what a human reads in an extracted archive |
| Manifest id | `PluginMetadata::new("my-plugin", …)` | **Yes** — a mismatch with `[[plugin]].name` fails the initialize handshake with `Plugin 'x' identified itself as 'y'` |
| Executable filename in the archive | packaging step | **Yes** — extraction fails without it (`my-plugin.exe` on Windows) |
| `[[plugin]].name` in `config.toml` | the operator's config | The other end of the manifest-id check |

Pick a name nobody has taken. Plugin names are global across the catalog and
across a node's install store, and an `--archive` install replaces whatever
directory already sits at that name. This repository already has one live
collision — `plugins/openai-endpoint` versus the first-party repository of the
same name — and it means a node can install one or the other, never both.

### 2. Point at the SDK

```toml
tdcc-plugin = { path = "../../../tdcc-mesh/crates/tdcc-plugin" }
```

Relative from `plugins/<name>/`, assuming `tdcc-mesh` and `tdcc-plugins` are
siblings. Commit that form, not a path that only works on your machine — if
your checkout is elsewhere, use a local `[patch.crates-io]` (which means
declaring the dependency as a version requirement, since a patch does not
rewrite a path dependency) and leave the committed line alone.

Say in your plugin's README what a public consumer replaces this with once the
SDK is published. Every existing plugin has that paragraph, it is the first
thing a stranger needs, and it is the difference between a plugin someone can
adopt and a plugin only we can build.

### 3. Keep the crate standalone

```toml
[package]
name = "my-plugin"
version = "0.1.0"
edition = "2024"
license = "Apache-2.0"
publish = false

# Not a member of any surrounding workspace.
[workspace]
```

Commit `Cargo.lock`. A plugin is a binary that runs on someone else's machine;
a locked dependency set is what makes a release reproducible and a review
finite.

### 4. Declare, do not implement

One `plugin!` macro, one manifest, every surface the plugin needs. Do not open
a socket, do not serve HTTP, do not speak MCP JSON-RPC — the host projects all
of that from your declaration. Field order in the macro is fixed: `metadata`,
`startup_policy`, `provides`, `config`, `web_ui`, `mesh`, `events`, `mcp`,
`http`, `inference`, then the lifecycle hooks.

Tool arguments are a struct deriving `Deserialize + JsonSchema`. **The doc
comment on each field becomes the description a model reads**, so write it for
the model, not for the compiler:

```rust
/// Arguments for the `greet` tool.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GreetArgs {
    /// Who to greet. Used verbatim in the reply.
    name: String,
    /// Repeat the greeting this many times. 1–10, default 1.
    #[serde(default)]
    times: Option<u32>,
}
```

`deny_unknown_fields` is worth the line. `workload-policy` is the only plugin
here that uses it today, and it uses it to guarantee there is nowhere for
prompt content to land in a policy check — which turns a documented boundary
into a type-system one, and means there is nothing about request content in its
decision log either.

### 5. Run it and watch it fail correctly

```bash
cargo test
cargo build --release
./target/release/my-plugin
# Error: TDCC_PLUGIN_ENDPOINT is not set for plugin process
```

That error is the correct outcome. The host owns the control endpoint and
passes it in through the launch contract; a plugin never invents a socket path.
Anything you want to work outside a host — `--help`,
`--print-package-manifest` — has to be handled before `PluginRuntime::run`.

Then package and install through the real validation boundary, with an isolated
store so you do not disturb your own installation:

```bash
TDCC_PLUGIN_DIR=/tmp/plugin-store tdcc plugins install \
  --archive ./target/my-plugin-0.1.0-local.tar.gz \
  --name my-plugin --version 0.1.0
TDCC_PLUGIN_DIR=/tmp/plugin-store tdcc plugins info my-plugin
```

A plugin that declares a web UI must report
`"validation": { "status": "valid" }` in its stored install record.

---

## Naming: you share one namespace

Four different namespaces are in play, with three different scopes. Getting
this wrong is not a style problem — an MCP tool name, an HTTP path, a
capability id, and a settings key are all names other people write down, and
changing one later is a breaking change.

| Name | Scope | Collision consequence |
| --- | --- | --- |
| Plugin name | **Global** — the catalog, and every node's install store | An install replaces the other one |
| Capability id, mesh channel id | **Global** — resolved by name across the mesh | Two plugins claiming one contract |
| MCP tool name, HTTP path, binding id | **Per plugin** — the host namespaces them as `<plugin>.<tool>` | Flat within your plugin; the host detects route collisions |
| Settings key | **Per plugin**, but a public API the console and any bundle read | Silent behaviour change on upgrade |

### Tools

The host namespaces MCP identifiers, so `search` in `web-search` is
`web-search.search` on the endpoint and does not collide with `search` in
`code-context`. Within one plugin the namespace is flat — there is no
sub-grouping — so names have to carry their own context.

The convention across the eleven plugins here:

- **lowercase `snake_case`**: `query`, `read_chunk`, `describe_table`,
  `verify_stream`, `list_databases`. Never camelCase, never a dot.
- **A bare verb when the object is obvious** from the plugin name (`web-search`
  has `search` and `fetch`), and **`verb_noun` when it is not**
  (`sqlite-query` has `list_tables` and `describe_table`, because `list` alone
  would be ambiguous with `list_databases`).
- **`status` means "what is this plugin configured as and what is it doing,
  without touching the network."** Seven of the eleven have one, and it is the
  tool an operator calls when everything else is failing. Keep it cheap, keep
  it answering, and keep it separate from a tool that probes a backend —
  `semantic-cache` and `openai-endpoint` both split `status` (no network) from
  a probe tool that does reach out and can fail.
- **Name the destructive one plainly and give it no default scope.**
  `sqlite-query` has `execute` rather than a `write: true` flag on `query`;
  `semantic-cache`'s `purge` refuses to run unless the caller names `expired`,
  `model`, or `all`. A model will call your tools. Make the dangerous one
  require a sentence.

### Capability and channel ids

`<plugin-name>.v<N>`, kebab-case, versioned from the start:
`web-search.v1`, `model-mirror.v1`, `capability-attest.v1`. The one deliberate
exception here is `metrics.prometheus.v1`, which names the *contract* rather
than the plugin, so a second exporter implementation could provide it instead.
That is the right reason to deviate; "it read better" is not.

Treat the id as a public API. Bump the `vN` rather than changing what the old
one means.

### Flags, environment variables, and settings keys

- Flags: `--kebab-case`, accepting both `--flag value` and `--flag=value`.
- Environment variables: `TDCC_<PLUGIN_NAME>_<SETTING>`, screaming snake case —
  `TDCC_WEB_SEARCH_BRAVE_API_KEY`, `TDCC_MODEL_MIRROR_MAX_CACHE_BYTES`. Eight
  plugins here read their own environment variables; six use that form,
  `code-context` uses a bare `CODE_CONTEXT_*`, and `capability-attest`
  abbreviates to `TDCC_ATTEST_*`. New plugins should use the full form.
- Precedence, stated in your README and covered by a test: **flag beats
  environment beats `[[plugin]].url` beats built-in default**.
- **An unknown flag or an out-of-range value is a startup error, not a
  warning.** Every plugin here does this, and the reason is the same each time:
  a typo in `--allow-private-network` that was quietly ignored leaves an
  operator believing a guard is off when it is on, or worse.

---

## What a review looks for

In roughly the order a reviewer will hit them.

**Does it build and test from a clean checkout?**

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

All three, with the real exit codes pasted into the pull request.

**Is every claim in the README verifiable?** This is the ground rule for the
whole repository. If you cannot point at the code, the CLI, or a test that
makes a statement true, do not write it. No invented version numbers, no
benchmarks, no planned features described in the present tense. A number that
came from a test should say which test — `semantic-cache`'s README does this,
so its example `stats` payload cannot drift away from the assertions that
produce it.

**Are the pure functions tested?** Parsing, scoring, formatting, policy
decisions, path resolution, retry classification, truncation — anything that
does not need a running host should be covered directly, with the tests beside
the code they cover. Where behaviour only shows up over a socket, stand up a
stub server on loopback rather than mocking the client: `event-webhook` proves
its retry policy by counting how many connections a scripted TCP listener
actually served, which is an answer a mock cannot give.

**Does it fail honestly?** A tool that cannot reach its backend must return an
error naming the cause. Not an empty list, not a zero, not a cache miss. An
outage and a genuinely empty result look identical to a caller, and the
difference is the whole value of the tool. If you have a deliberate exception —
`prometheus-exporter`'s scrape route returns `tdcc_up 0` with a `200` because
Prometheus needs a parseable body — document it where it happens and keep the
MCP tool erroring normally.

**Is the configuration in a channel that actually reaches the process?**
`[plugin.settings]` never does. Declaring a `config_schema` for something your
process needs draws a control in the console that looks authoritative and
changes nothing. Ten of the eleven plugins here declare no schema at all, and
each explains why in a short section — copy that. A schema is right only when a
web UI bundle reads the value back, which is exactly `contribution-ledger`'s
case.

**Is the blast radius written down?** Every plugin README here has a section
naming what it touches: network, filesystem, subprocesses, secrets, mesh. A
reviewer will look for it, and will compare it against the dependency list —
`openai-endpoint` links no TLS backend at all, so its "cleartext http only"
claim is enforced by the dependency graph rather than by a comment.

**Are the limitations stated, rather than left to be discovered?** The
strongest sections in this repository are the ones admitting what a plugin
cannot do: `contribution-ledger` cannot tell you which peers it served,
`workload-policy` cannot intercept a request, a `capability-attest` signature
does not prove a benchmark was honest, `code-context`'s secret filter will miss
a token in a YAML file. Write those. A reviewer who finds an undisclosed
limitation will trust the rest of the document less.

**Is `Cargo.lock` committed, and is the dependency list defensible?** Every
crate you add runs on someone else's machine. Prefer the smaller option and say
why in a comment when the choice is not obvious — `prometheus-exporter` parses
HTTP with `httparse` rather than pulling in a client stack, and its `Cargo.toml`
says so.

---

## Security expectations

Installing a plugin runs third-party native code with the operator's
privileges. There is no sandbox. A plugin here will be read with that in mind.

**Default to the narrowest useful permission, and make widening deliberate.**
`model-mirror` contributes zero disk until an operator sets
`--max-cache-bytes`. `sqlite-query` opens every database read-only until a
specific one is named in `--db-rw`. `web-search` refuses private addresses
until `--allow-private-network` is passed. In each case the safe state is what
you get by doing nothing.

**Enforce confinement with a mechanism, not a check on a string.** A read-only
file descriptor beats scanning SQL for `DROP`, which a view or a trigger
defeats. Canonicalizing a path and re-checking containment beats normalizing
`..` away. A derived on-disk key — `model-mirror` names blobs
`sha256(canonical_ref)` — beats validating a caller's path, because there is no
input that could escape. If a caller-supplied string can become a filesystem
path, a URL host, or a command-line argument, expect that to be the first thing
a reviewer looks at.

**Nothing key-shaped goes in `args` or `plugin.toml`.** `[[plugin]].args` is
written into `config.toml`, echoed back by `tdcc plugins info`, and visible in
a process listing. Read credentials from the environment of the `tdcc` process.
Refuse the wrong path loudly: `event-webhook` fails at startup if you pass its
webhook URL as an argument, and `openai-endpoint`'s `--api-key-env` takes a
variable *name* and rejects anything shaped like a key. Then keep the value out
of logs, errors, tool results, and `Debug` output — `web-search` hand-writes a
`Debug` implementation so an accidental `{:?}` cannot leak the key, and scrubs
transport errors before returning them.

**Treat everything from outside the process as hostile.** Configuration values,
`[[plugin]].url`, every tool argument, and above all anything arriving on a
mesh channel. `model-mirror` re-parses every advertised ref with the same
validator it uses locally, requires 64 hex characters for a digest, and
truncates an inbound list at 512 entries. `capability-attest` drops a peer
record whose signature does not verify rather than storing it, because storing
it is how a hostile peer fills your map.

**Bound everything that a caller can grow.** Queues, caches, retained
decisions, rate-limit buckets, peer maps, response sizes, result counts. Every
plugin here has explicit caps and sheds rather than growing, because unbounded
memory growth on hardware somebody lent you is a denial of service you shipped.

**Declare the smallest set of mesh channels and events.** Delivery is
allowlist-based, so declaring nothing means receiving nothing. Seven of the
eleven plugins declare no mesh surface at all.

**Say what you cannot protect against.** `web-search`'s address guard
re-resolves at connection time, so DNS rebinding can still get through, and its
README says so. `capability-attest` resolves `nvidia-smi` through `PATH`, so a
`PATH` an attacker controls means an attacker-chosen binary, and its README says
that too. An honest limitation is a feature of the document; a missing one is a
trap.

---

## Pull requests

One topic per pull request. In the description, say what you verified and paste
the real output — including exit codes — rather than describing what should
happen. If something is untested on your platform, say so. If a claim in your
README rests on a live host rather than a test, say which claim.

By contributing you agree that your contribution is licensed under
[Apache-2.0](LICENSE).
