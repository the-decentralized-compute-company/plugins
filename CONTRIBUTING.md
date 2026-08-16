# Contributing

This repository holds the plugin author guide, two teaching examples, and
twenty-one working plugins. Fixes and clarifications are welcome, and so is a
new plugin — read [What belongs here](#what-belongs-here) first, because the
answer is "sometimes".

Before anything else: **`tdcc-plugin` is not on crates.io**, so a fresh
`cargo build` fails at dependency resolution. See
[The SDK is not on crates.io](README.md#the-sdk-is-not-on-cratesio) in the
README. It is the single most common way to lose twenty minutes here. The crate
is being prepared for publication; until a version actually appears, the path
dependency is the only thing that builds.

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

With twenty-one plugins already here, a second question now applies: **does one
of them already own this problem?** `code-context` reads a checked-out tree and
`git-tools` reads its history; `vector-store` holds passages and never reads a
file while `code-context` reads files and holds no vectors; `prometheus-exporter`
is the pull side of metrics and the first-party `metrics` plugin is the push
side. Those splits are deliberate, and a proposal that blurs one needs to say
why. A plugin that adds a second way to do something the catalog already does is
a name collision waiting to happen and a maintenance cost forever.

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

You do **not** need an `http` declaration to make a tool reachable over HTTP.
The host already mounts every declared MCP tool at
`POST /api/plugins/<plugin>/tools/<tool>`. Declare `http` when you want a
specific method, a `GET` for something a browser or a scraper will poll, a
streamed body, or SSE. Thirteen of the twenty-one plugins here declare one, for
45 routes between them; eight declare none and lose nothing.

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

`deny_unknown_fields` has gone from a curiosity to the house default: eleven of
the twenty-one plugins use it, and all ten of the newer ones do. It is worth the
line for a reason stronger than tidiness — it turns "this tool has no way to
take a URL / a header / a host" from a documented boundary into a type-system
one. `rest-client` is the clearest case: its `CallArgs` has exactly four fields
and `deny_unknown_fields`, so there is provably nowhere for prompt content to
smuggle in a destination. `workload-policy` uses it so there is nowhere for
request content to land in a policy check, which is also why there is nothing
about request content in its decision log.

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
`--print-package-manifest`, a `--check-config` that validates a declaration file
without launching anything — has to be handled before `PluginRuntime::run`.

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

There are now **105 declared tools across twenty-one plugins**, all of them
visible in one `tools/list` on a node that installs everything. That is past the
point where a name only has to make sense inside its own README: a model reading
that list sees `query`, `search`, `call`, `read`, `list`, and `status` many
times over, distinguished only by the plugin prefix. The conventions below exist
so the prefix is enough.

- **lowercase `snake_case`**: `query`, `read_chunk`, `describe_table`,
  `verify_stream`, `list_databases`, `preview_chunks`, `container_logs`. Never
  camelCase, never a dot — the dot is the host's separator and it is not yours
  to use.
- **A bare verb when the object is obvious** from the plugin name (`web-search`
  has `search` and `fetch`; `transcribe` has `transcribe`), and **`verb_noun`
  when it is not** (`sqlite-query` has `list_tables` and `describe_table`,
  because `list` alone would be ambiguous with `list_databases`;
  `docker-inspect` has `list_containers` and `list_images` for the same reason).
- **`status` means "what is this plugin configured as and what is it doing,
  without touching the network."** Seventeen of the twenty-one have one, and it
  is the tool an operator calls when everything else is failing. Keep it cheap,
  keep it answering, and keep it separate from a tool that probes a backend —
  `semantic-cache`, `vector-store`, `openai-endpoint`, `transcribe`, and
  `describe-image` all split `status` (no network) from a probe tool
  (`probe_backend`, `health`, `vision_models`) that does reach out and can fail.
  The four plugins without a `status` predate the convention or have a
  domain-specific equivalent (`prometheus-exporter`'s `check`,
  `workload-policy`'s `report`); a new plugin should have one.
- **When the domain word collides with the convention, the convention keeps the
  bare name.** `git-tools` has both `status` (this plugin's configuration) and
  `repo_status` (git's own working-tree status), rather than overloading one
  name with two meanings. That is the precedent: qualify the domain concept, not
  the operator's diagnostic.
- **Name the destructive one plainly and give it no default scope.**
  `sqlite-query` has `execute` rather than a `write: true` flag on `query`;
  `semantic-cache`'s `purge` refuses to run unless the caller names `expired`,
  `model`, or `all`; `vector-store`'s `delete` takes a `scope` with **no
  default** — its doc comment says why, "deleting an index is cheap to do and
  impossible to undo" — and requires explicit `document_ids` when the scope is
  `documents`. A model will call your tools. Make the dangerous one require a
  sentence.
- **A tool you decide not to ship is a naming decision too, and it deserves a
  test.** `git-tools` has `tools::tests::no_tool_name_suggests_a_write`, which
  fails if a future tool is called `commit`, `push`, `checkout`, `reset`, or a
  dozen other write-shaped words. `scheduled-prompts` asserts that no tool name
  contains `create`, `add`, `edit`, or `delete`, because a model scheduling its
  own future execution is the thing that plugin exists to prevent. If your
  README makes a promise about what a plugin will never do, write the test that
  keeps a later contributor honest.
- **If you re-export somebody else's tools, invent a sub-namespace and prove it
  is unambiguous.** `mcp-bridge` names a bridged tool `<alias>__<tool>` with a
  double underscore, forbids `__` inside an alias so splitting at the first `__`
  always recovers the alias, and refuses any bridged name that collides with one
  of its own three tools. A dot would have been more natural and is unavailable;
  a single underscore would have made `a_b` + `c` and `a` + `b_c` the same name.

### Capability and channel ids

`<plugin-name>.v<N>`, kebab-case, versioned from the start:
`web-search.v1`, `model-mirror.v1`, `capability-attest.v1`, `node-notes.v1`,
`rest-client.v1`. Sixteen of the twenty-one declare a capability and all but one
follow that form exactly. The one deliberate exception is
`metrics.prometheus.v1`, which names the *contract* rather than the plugin, so a
second exporter implementation could provide it instead. That is the right
reason to deviate; "it read better" is not.

Three plugins declare a mesh channel, and each uses the same string as its
capability id. Keep that: a channel and a capability with the same name are one
contract seen from two sides, and a reader should not have to check.

Treat the id as a public API. Bump the `vN` rather than changing what the old
one means.

### Flags, environment variables, and settings keys

- Flags: `--kebab-case`, accepting both `--flag value` and `--flag=value`.
  Every plugin here accepts both, and there is a test for it.
- Environment variables: `TDCC_<PLUGIN_NAME>_<SETTING>`, screaming snake case —
  `TDCC_WEB_SEARCH_BRAVE_API_KEY`, `TDCC_MODEL_MIRROR_MAX_CACHE_BYTES`,
  `TDCC_SCHEDULED_PROMPTS_JOBS_FILE`. Eighteen of the twenty-one plugins read
  their own environment variables; sixteen use that form. Two legacy deviations
  remain — `code-context` uses a bare `CODE_CONTEXT_*` and `capability-attest`
  abbreviates to `TDCC_ATTEST_*` — and every plugin added since uses the full
  form. **Use the full form.** The prefix is what stops your variable name from
  meaning something else in an operator's shell profile.
- Precedence, stated in your README and covered by a test: **flag beats
  environment beats `[[plugin]].url` beats built-in default**.
- **An unknown flag or an out-of-range value is a startup error, not a
  warning.** Every plugin here does this, and the reason is the same each time:
  a typo in `--allow-private-network` that was quietly ignored leaves an
  operator believing a guard is off when it is on, or worse. The same rule
  applies to a configuration *file*: `workload-policy`, `rest-client`,
  `mcp-bridge`, and `scheduled-prompts` all refuse to load a file with an
  unknown key rather than ignoring it, because `scheduel = "0 3 * * *"` silently
  doing nothing is indistinguishable from a node that was never busy.
- **When configuration grows past a handful of scalars, put it in a file and
  take the file's path in `args`.** An API declaration, an MCP server list, a
  policy, and a schedule are documents an operator wants to diff, review, and
  revert. Four plugins here do it, all four ship a way to validate the file
  without starting anything, and all four keep credentials out of it — a file
  names an environment variable, never a value.

---

## What a review looks for

In roughly the order a reviewer will hit them.

**Does it build and test from a clean checkout?**

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

All three, with the real exit codes pasted into the pull request. All
twenty-one plugins here pass all three today, so a regression is visible.

**Is every claim in the README verifiable?** This is the ground rule for the
whole repository. If you cannot point at the code, the CLI, or a test that
makes a statement true, do not write it. No invented version numbers, no
benchmarks, no planned features described in the present tense. The strongest
pattern in this repository is a README example that a test asserts against:
`git-tools`, `pdf-extract`, `vector-store`, `rest-client`, and
`scheduled-prompts` each have a test that parses or renders the exact block
printed in their README and fails when the two drift. Copy that. It costs one
test and it removes an entire category of document rot.

**Are the pure functions tested?** Parsing, scoring, formatting, policy
decisions, path resolution, retry classification, truncation — anything that
does not need a running host should be covered directly, with the tests beside
the code they cover. Where behaviour only shows up over a socket, stand up a
stub server on loopback rather than mocking the client: `event-webhook` proves
its retry policy by counting how many connections a scripted TCP listener
actually served, `transcribe` proves a long recording produced exactly three
requests each carrying a valid WAV, and `scheduled-prompts` proves a run does
not overlap itself by having the stub *hold* its response until the test
releases it rather than by sleeping and hoping. Those are answers a mock cannot
give.

**Does it fail honestly?** A tool that cannot reach its backend must return an
error naming the cause. Not an empty list, not a zero, not a cache miss, not no
search results. An outage and a genuinely empty result look identical to a
caller, and the difference is the whole value of the tool. If you have a
deliberate exception — `prometheus-exporter`'s scrape route returns `tdcc_up 0`
with a `200` because Prometheus needs a parseable body — document it where it
happens and keep the MCP tool erroring normally.

**Does a caveat travel with the answer?** If your plugin's results can be wrong
(a model's guess), can be someone else's (a peer's claim), or can contain more
than the caller asked for (a log line), the warning belongs in the payload, not
only in the README. `describe-image` puts a `caveat` on every result,
`node-notes` stamps `origin` / `untrusted` / `trust` on every peer note,
`docker-inspect` puts a `warning` on every log response, and
`contribution-ledger` and `capability-attest` carry a `disclaimer` in every
response. A caveat that lives only in a document is lost the first time somebody
copies the answer out of it.

**Is the configuration in a channel that actually reaches the process?**
`[plugin.settings]` never does. Declaring a `config_schema` for something your
process needs draws a control in the console that looks authoritative and
changes nothing. Twenty of the twenty-one plugins here declare no schema at all,
and each explains why in a short section — copy that. A schema is right only
when a web UI bundle reads the value back, which is exactly `contribution-ledger`'s
case.

**Is the blast radius written down?** Every plugin README here has a section
naming what it touches: network, filesystem, subprocesses, secrets, mesh. A
reviewer will look for it, and will compare it against the dependency list.
Several plugins make the claim *checkable from the dependency graph* rather than
from prose, which is stronger: `openai-endpoint` links no TLS backend at all, so
its "cleartext http only" claim cannot be violated; `git-tools` builds `git2`
with `default-features = false` so clone, fetch, and push are unavailable at the
library level; `node-notes` ships no HTTP client at all and its README tells you
to run `cargo tree | grep -iE "reqwest|hyper|rustls"` and see nothing.

**Is there a second section for what your answers hand to a model?** That is a
different list from what your plugin touches, and it is often the bigger one.
`docker-inspect` can read the socket and *chooses* to hide environment variable
values, cap log volume, and warn on every log response, because logs are where
applications print credentials. `git-tools` can read a repository and says
outright that history contains secrets a working tree does not. `describe-image`
sends pictures somewhere. Write that section.

**Are the limitations stated, rather than left to be discovered?** The
strongest sections in this repository are the ones admitting what a plugin
cannot do: `contribution-ledger` cannot tell you which peers it served,
`workload-policy` cannot intercept a request, a `capability-attest` signature
does not prove a benchmark was honest, `code-context`'s secret filter will miss
a token in a YAML file, `transcribe` cannot chunk anything but WAV,
`vector-store` is the wrong design past a few tens of thousands of passages and
says so with an enforced cap, `node-notes` has not been run on a live two-node
mesh and has a *What is not verified* section saying which of its claims that
leaves resting on reading the host's source. Write those. A reviewer who finds
an undisclosed limitation will trust the rest of the document less.

**Is `Cargo.lock` committed, and is the dependency list defensible?** Every
crate you add runs on someone else's machine. Prefer the smaller option and say
why in a comment when the choice is not obvious — `prometheus-exporter` parses
HTTP with `httparse` rather than pulling in a client stack, and its `Cargo.toml`
says so; `docker-inspect` speaks the Engine API directly rather than linking a
Docker client crate, which is what makes "the write verbs are not in the binary"
true.

---

## Security expectations

Installing a plugin runs third-party native code with the operator's
privileges. There is no sandbox. A plugin here will be read with that in mind,
and with the fact that the machine is frequently not the machine of the person
whose question the plugin is answering.

**Default to the narrowest useful permission, and make widening deliberate.**
The safe state is what you get by doing nothing:

- `code-context`, `pdf-extract`, `transcribe`, and `describe-image` read
  **nothing** until an operator names a root.
- `model-mirror` contributes **zero disk** until an operator sets
  `--max-cache-bytes`.
- `sqlite-query` opens every database read-only until a specific one is named in
  `--db-rw`.
- `node-notes` shares nothing, and refuses inbound notes, until `--share`.
- `web-search`, `rest-client`, `describe-image`, `vector-store`,
  `semantic-cache`, and `scheduled-prompts` refuse private or non-loopback
  destinations until a flag opts one in.
- `docker-inspect` refuses a `tcp://` endpoint until `--allow-tcp`, and hides
  environment variable values until `--show-env`.

The one place this repository has a default that is *not* the narrow one is
`docker-inspect`'s visibility filter — with no `--container` and no `--label`,
every container on the machine is visible. That was judged less bad than a
plugin that shows nothing and looks broken, and the cost is paid in a startup
warning on stderr, a line in `status`, and a paragraph in the README. If you
make a call like that, pay for it the same way.

**Enforce confinement with a mechanism, not a check on a string.** A read-only
file descriptor beats scanning SQL for `DROP`, which a view or a trigger
defeats. Canonicalizing a path and re-checking containment beats normalizing
`..` away — and doing *both*, lexically before any syscall and physically after
canonicalization, is what catches a symlink inside a root that points out of it.
The four root-confined plugins — `code-context`, `pdf-extract`, `transcribe`,
and `describe-image` — do exactly that pair, and each has a test that creates a
real symlink and proves it resolves before proving it is refused. Better still,
remove the input: `model-mirror` names blobs `sha256(canonical_ref)` so there is
no caller string that could become a path; `docker-inspect`'s API paths are a
newtype with a private field and eight constructors, none of which takes a
caller's string, so the allowlist is enforced by the module system; `rest-client`
has no URL argument at all, and then checks the *assembled* `Url` a second time
against the base so the check does not depend on the first one being right;
`vector-store` never opens a file, so its `source` field cannot traverse
anything. If a caller-supplied string can become a filesystem path, a URL host,
or a command-line argument, expect that to be the first thing a reviewer looks
at.

**If you launch or attach to somebody else's code, say so in the first
paragraph and strip the environment.** `mcp-bridge` is the only plugin here that
runs third-party binaries, and the rules it follows are the ones a reviewer will
apply to the next one: nothing is auto-discovered from another tool's config
file, the command and its arguments are handed to the OS with no shell so
nothing is word-split or globbed, each child gets a platform baseline plus only
the variable names its own entry asked for, and everything under
`TDCC_PLUGIN_*` and `MESH_LLM_PLUGIN_*` is stripped **last**, after every other
rule, so no setting — not even an explicit `inherit_env = true` — can hand a
third-party process the control connection to the node. It also states plainly
that it is not a sandbox and cannot stop a server you added from doing what that
server does.

**Nothing key-shaped goes in `args` or `plugin.toml`.** `[[plugin]].args` is
written into `config.toml`, echoed back by `tdcc plugins info`, and visible in
a process listing. Read credentials from the environment of the `tdcc` process,
and take the *variable name* rather than the value wherever configuration names
a credential — `rest-client`'s `token_env`, `mcp-bridge`'s `bearer_token_env`
and `env_from`, `openai-endpoint`'s `--api-key-env`, `scheduled-prompts`'
`url_env`. Refuse the wrong path loudly: `event-webhook` and `transcribe` both
fail at startup if you pass a URL or key as an argument, `vector-store` and
`mcp-bridge` refuse a URL with credentials embedded in it at parse time rather
than redacting it later, and `vector-store` has a test asserting that no flag
will ever take a key.

Then keep the value out of logs, errors, tool results, and `Debug` output.
Hand-write the `Debug` implementation so an accidental `{:?}` cannot leak it —
eight plugins do, including `web-search`, `transcribe`, `describe-image`,
`rest-client`, `semantic-cache`, `vector-store`, and `scheduled-prompts` — and
scrub error bodies you quote back, because the remote end may quote your
credential at you. `scheduled-prompts` has the test worth copying here: Slack
answers `invalid_token for /services/T0/B0/…`, so the plugin scrubs the URL, its
path, its query, and any path segment long enough to be a token out of a failure
body before that body is stored or returned.

**Treat everything from outside the process as hostile.** Configuration values,
`[[plugin]].url`, every tool argument, and above all anything arriving on a
mesh channel or from a bridged server. `model-mirror` re-parses every advertised
ref with the same validator it uses locally, requires 64 hex characters for a
digest, and truncates an inbound list at 512 entries. `capability-attest` drops
a peer record whose signature does not verify rather than storing it, because
storing it is how a hostile peer fills your map. `node-notes` goes furthest: an
inbound note is not validated, it is **re-derived** — text sanitized and
truncated against *this* node's limits, control characters and ANSI escapes
replaced, an unknown kind folded to a known one, a creation date in the future
pulled back to now, and the expiry recomputed from this node's own TTL ceiling
rather than believed. A sending peer id is self-declared and is used for
grouping and rate limiting and nothing else. That is the standard for anything a
stranger's machine can put in front of you.

**Bound everything that a caller can grow.** Queues, caches, retained
decisions, rate-limit buckets, peer maps, response sizes, result counts, log
lines, passages per collection, jobs per file, servers per node, schema bytes
per forwarded tool. Every plugin here has explicit caps and sheds rather than
growing, because unbounded memory growth on hardware somebody lent you is a
denial of service you shipped. A cap that truncates must *say* it truncated —
`truncated`, `more_available`, `hidden_by_filter`, `truncated_reason` — because
a silently shortened answer that looks complete is worse than an error.

**Think about what a tool spends, not only what it reads.** `scheduled-prompts`
is the case that forced this: `run_now` costs GPU time on somebody else's
machine, and a model can call it. The answer was to make it as narrow as it can
be while still being useful — one named job, no "run everything" form, no
control over what that job says, and every guard including the operator's hours
still applies, with only the *file* able to opt a job out of its window. And the
tool that would have been genuinely dangerous, creating a job, simply does not
exist: there is no code path from a tool call to a new job, which is a property
of the function signature rather than a permission check that could be
misconfigured. If your plugin can spend a stranger's electricity or fill their
disk, decide deliberately how much a single tool call may commit them to.

**Declare the smallest set of mesh channels and events.** Delivery is
allowlist-based, so declaring nothing means receiving nothing. Eighteen of the
twenty-one plugins declare no mesh channel at all, and sixteen subscribe to no
events.

**Say what you cannot protect against.** `web-search`'s and `rest-client`'s
address guards re-resolve at connection time, so DNS rebinding can still get
through, and both READMEs say so. `capability-attest` resolves `nvidia-smi`
through `PATH`, and `mcp-bridge` resolves a bare `command` the same way, so a
`PATH` an attacker controls means an attacker-chosen binary — both say so.
`docker-inspect` says its secret filter will not catch a credential passed
positionally or printed to stdout. `describe-image` says a transcription from a
vision model returns fluent, plausible, wrong characters rather than obvious
garbage, and that a serial number should be checked against the image. An honest
limitation is a feature of the document; a missing one is a trap.

---

## Pull requests

One topic per pull request. In the description, say what you verified and paste
the real output — including exit codes — rather than describing what should
happen. If something is untested on your platform, say so. If a claim in your
README rests on a live host rather than a test, say which claim, and prefer
putting that list in the README itself under a heading like *What is not
verified* so it survives past the pull request.

By contributing you agree that your contribution is licensed under
[Apache-2.0](LICENSE).
