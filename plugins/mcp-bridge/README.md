# mcp-bridge

Every MCP server anybody has already written, usable from a TDCC mesh without
porting it.

You list the servers in a file. At startup the plugin launches or connects to
each one, asks what tools it has, and re-declares them on this node under the
alias you gave that server — so `read_file` on the server you called `files`
becomes `mcp-bridge.files__read_file` on the node's MCP endpoint, with the
server's own JSON Schema forwarded unchanged.

Three tools of its own, projected by the host:

| Tool | On the MCP endpoint | What it does |
| --- | --- | --- |
| `status` | `mcp-bridge.status` | Every bridged server, its state, its tool counts, and why the last attempt failed. Opens no connection. |
| `tools` | `mcp-bridge.tools` | Maps every bridged tool back to the server and upstream name it forwards to. |
| `reconnect` | `mcp-bridge.reconnect` | Drops and reopens the connection to one named server, now. |

All three are also mounted over HTTP by the host, at
`GET /api/plugins/mcp-bridge/http/status`,
`GET /api/plugins/mcp-bridge/http/tools`, and
`POST /api/plugins/mcp-bridge/http/reconnect`.

> **Read [Security](#security-what-you-are-actually-agreeing-to) before you
> install this.** It runs third-party programs on your machine with your
> privileges and hands them whatever a model asks for. Everything it does to
> narrow that is on by default, and none of it is a sandbox.

---

## Security: what you are actually agreeing to

A bridged MCP server is **a native binary running as you**. There is no
container, no seccomp filter, no capability drop. If you add
`@some/mcp-server-shell` to your server list, a model on your mesh can run
shell commands on your machine, because that is what you asked for.

So the rule this plugin is built around: **trust each entry in the server list
exactly as much as you would trust running that binary yourself, by hand, and
walking away.**

What the plugin does about it:

**Nothing is auto-discovered.** It does not read `~/.cursor/mcp.json`,
`claude_desktop_config.json`, `.vscode/mcp.json`, or anything else. It reads one
file, the one you point it at, and launches what is in it. An empty or missing
file bridges nothing and the plugin says so at startup.

**A child gets a small environment, not yours.** `tdcc`'s environment contains
`TDCC_PLUGIN_ENDPOINT` — the node's own plugin control endpoint — and whatever
keys you exported for your other plugins. A child launched from inside `tdcc`
would inherit all of it. Instead each server gets a platform baseline (`PATH`,
`HOME`, temp directories, `SystemRoot` on Windows) plus exactly the names its
own entry asked for. Everything under `TDCC_PLUGIN_*`, `MESH_LLM_PLUGIN_*`, and
`TDCC_MCP_BRIDGE_*` is stripped last, from whatever the rules produced, so no
setting — `inherit_env = true` included — can hand a third-party binary the
control connection to your node. See
[The child's environment](#the-childs-environment).

**Commands are executed, not interpreted.** `command` plus an `args` array,
handed to the OS. There is no shell, so nothing in the server list is subject to
word splitting, globbing, or `$(…)`.

**Only the tools you allow.** `allow_tools` and `deny_tools` per server, matched
against the upstream's own tool names, with **deny winning**. Three of a
server's forty tools is one line.

**Credentials are variable names, not values.** `env_from = ["GITHUB_TOKEN"]`
for a launched server, `bearer_token_env = "NOTES_TOKEN"` for an HTTP one. The
plugin refuses a `bearer_token_env` that does not look like a variable name, and
refuses a URL with a username or password in it outright rather than redacting
it later. Nothing key-shaped goes in the server list, in `[[plugin]].args`, or
in a log line.

**Everything is bounded.** Servers per node, tools per server, argument shape,
schema size, result size, connect time, call time, reconnect rate. Each has a
limit, each limit is in the table below, and going over one is an error naming
the setting rather than a silent truncation.

What it does **not** do, and cannot:

- It cannot stop a server you added from doing what that server does. A
  filesystem server with a write tool can write, unless you deny that tool.
- It does not sandbox, chroot, or drop privileges.
- It does not inspect or filter tool *arguments*. The upstream owns its own
  contract and enforces it; a second, approximate copy of somebody else's
  validation sitting in the middle causes disagreements, not safety.
- A bare `command` is resolved through the `PATH` of the `tdcc` process. A
  `PATH` an attacker controls means an attacker-chosen binary. Give an absolute
  path if that matters to you.

---

## Why this rather than `mcp::external_stdio`

The plugin SDK already lets a plugin declare an external MCP endpoint —
`mcp::external_stdio`, `mcp::external_http` — and the host connects to it and
namespaces its tools by itself. If that is all you need, use it: it is fewer
moving parts and it is in the host.

This plugin exists for the four things that declaration cannot express, because
it is compiled into a plugin's manifest rather than written by an operator:

| | `mcp::external_*` | `mcp-bridge` |
| --- | --- | --- |
| Adding a server | recompile a plugin | edit a file, restart `tdcc` |
| Which tools are exposed | all of them | `allow_tools` / `deny_tools` |
| Child environment | inherited from `tdcc` | a baseline plus what you named |
| Working directory, per-server timeouts, result caps | — | per server |
| Reconnect | on next use | supervised, with backoff, reported in `status` |

---

## The server list

One file. `$HOME/.tdcc/mcp-bridge.toml` by default (`%USERPROFILE%` on Windows),
or wherever `--servers` points.

```toml
# ~/.tdcc/mcp-bridge.toml
version = 1

# Optional; every server inherits these unless it overrides them.
[defaults]
connect_timeout_secs = 30
call_timeout_secs    = 120
max_result_bytes     = 4000000
max_tools_per_server = 128

# A server this node launches and supervises.
[[server]]
alias        = "files"
transport    = "stdio"
command      = "npx"
args         = ["-y", "@modelcontextprotocol/server-filesystem", "/srv/shared"]
allow_tools  = ["read_file", "read_text_file", "list_directory", "search_files"]
deny_tools   = ["write_*", "move_file", "edit_file"]
description  = "Read-only view of the shared directory"

# A server that is already running somewhere, reached over MCP Streamable HTTP.
[[server]]
alias            = "notes"
transport        = "http"
url              = "http://127.0.0.1:7777/mcp"
bearer_token_env = "NOTES_MCP_TOKEN"
deny_tools       = ["delete_*"]
```

Check it without launching anything:

```bash
mcp-bridge --check-config --servers ~/.tdcc/mcp-bridge.toml
```

```text
server list: /home/operator/.tdcc/mcp-bridge.toml
state: loaded
servers: 2

[files] enabled — stdio: npx
  tools prefixed  files__…
  timeouts        connect 30 s, call 120 s
  result cap      4000000 bytes
  restart         yes, with backoff
  allow_tools     read_file, read_text_file, list_directory, search_files
  deny_tools      write_*, move_file, edit_file

[notes] enabled — http: http://127.0.0.1:7777/mcp
  …
  from environment
    NOTES_MCP_TOKEN: NOT SET in the tdcc process — this server would refuse to start

Nothing above was launched and no connection was opened. Each entry runs with
the privileges of the tdcc process when it does.
```

### Every key

| Key | Applies to | Default | Meaning |
| --- | --- | --- | --- |
| `version` | document | required | Must be `1`. |
| `alias` | server | required | Your name for this server; prefixes every tool it publishes. Lowercase letters, digits, single underscores, starting with a letter, ≤ 32 characters. Unique across the file. |
| `transport` | server | required | `"stdio"` (this node launches it) or `"http"` (already running, MCP Streamable HTTP). |
| `enabled` | server | `true` | `false` keeps the entry and launches nothing. |
| `description` | server | — | Free text, shown in `status`. ≤ 500 characters. |
| `command` | stdio | required | Executable. A bare name is resolved through `PATH`; anything containing `/` or `\` is used as written. Never run through a shell. |
| `args` | stdio | `[]` | One argument per element. ≤ 64. |
| `cwd` | stdio | inherited | Working directory for the child. |
| `env` | stdio | `{}` | Literal `NAME = "value"` pairs. Not for credentials. ≤ 64. |
| `env_from` | stdio | `[]` | Names copied out of the `tdcc` process environment. ≤ 64. |
| `inherit_env` | stdio | `false` | Hand the child everything `tdcc` has. See the warning below. |
| `url` | http | required | `http` or `https`. A username or password in the URL is refused. |
| `bearer_token_env` | http | — | Name of the variable holding a bearer token. Never the token. |
| `allow_tools` | server | `[]` | Non-empty means only matching tools are candidates. `*` is the only wildcard. ≤ 256. |
| `deny_tools` | server | `[]` | Removes matches. **Wins over `allow_tools`.** ≤ 256. |
| `connect_timeout_secs` | server | `30` | 1–600. Bounds connecting *and* the first `tools/list`. |
| `call_timeout_secs` | server | `120` | 1–3600. Bounds every forwarded call. |
| `max_result_bytes` | server | `4000000` | 1024–67108864. A larger answer is refused, not truncated. |
| `max_tools` | server | `128` | 1–512. Tools beyond it are not bridged, and `status` says so. |
| `restart` | server | `true` | Reconnect with backoff when the link drops. |

At most 32 servers. An unknown key anywhere in the file is an **error**, not a
warning — a silently ignored `deny_tool = ["write_file"]`, singular, is a
denylist that does not exist on a machine whose owner believes it does.

Validation is **all-or-nothing** and reports every problem at once. One bad
server means no servers are launched: half a server list is a configuration you
never wrote.

### Why the server list is a file

`[plugin.settings]` values are stored by the host and rendered by the console,
but they are **never delivered to a plugin process** — there is no settings
field in the launch contract or the initialize handshake, and only a web UI
bundle can read them back. This plugin ships no web UI, so a config schema would
draw console controls whose values could not launch a single server.

`[[plugin]].args` does reach the process, but a server entry is a command, an
argument vector, a working directory, an environment plan, two pattern lists and
four numbers. That is a document, not a flag. So the only argument is where the
document lives:

| Setting | `[[plugin]].args` | Environment | Default |
| --- | --- | --- | --- |
| Server list path | `--servers <path>` | `TDCC_MCP_BRIDGE_SERVERS` | `$HOME/.tdcc/mcp-bridge.toml` |

Arguments win over the environment. `[[plugin]].url` is deliberately **not**
read: a server list is a file, not a URL.

```toml
# ~/.tdcc/config.toml
version = 1

[[plugin]]
name = "mcp-bridge"
args = ["--servers", "/etc/tdcc/mcp-bridge.toml"]

[plugin.startup]
# This plugin contacts every server in its list before it connects back to the
# host, because a plugin's tool list is fixed in the initialize response. The
# host's default here is 10 s, and a cold `npx -y …` server needs much longer.
connect_timeout_secs = 180

An unknown flag is a hard startup error rather than a warning: a typo in
`--servers` that was quietly ignored would bridge the wrong file, or nothing at
all, while looking configured.

---

## Namespacing: which server answered

Every bridged tool is `<alias>__<upstream name>` — a **double** underscore —
and the host adds its own plugin namespace on top, so the full name is
`mcp-bridge.<alias>__<tool>`.

```text
server list        alias "files",  tool "read_file"
in this plugin     files__read_file
on the endpoint    mcp-bridge.files__read_file
```

Three properties, each pinned by a test:

- **An alias may not contain `__`** (single underscores only, no trailing one),
  so splitting a bridged name at its *first* `__` always recovers the alias. The
  alias `a_b` with tool `c` and the alias `a` with tool `b_c` are different
  names, not the same one.
- **This plugin's own tools contain no `__`**, so no third-party server can
  publish a tool that shadows `status`, `tools`, or `reconnect`. A second,
  independent check refuses any bridged name that is already declared.
- **The alias is your word, not the upstream's.** A server never sees its own
  prefix and cannot choose or collide with another server's.

Two more things make the answer's origin readable rather than inferable:

- Every bridged tool's description starts with `[<alias>] `, carries the
  upstream's own description, and ends with a sentence naming the upstream tool
  and saying that it runs third-party code. The transport *kind* is named but
  the address is not — an internal hostname is not something a model needs.
- Every forwarded result carries `_meta` keys `tdcc.mcp-bridge/server` and
  `tdcc.mcp-bridge/tool`, written unconditionally, so a server cannot claim to
  be a different server.

An upstream name containing characters a tool name should not carry is
sanitized (`[A-Za-z0-9_-]` survives, everything else becomes `_`), and a name
over 48 characters is cut. Both are reported by `mcp-bridge.tools`, and **the
call still goes out under the upstream's own spelling** — the bridged name is a
label, never a rename. Two upstream names that sanitize to the same thing get a
numeric suffix rather than one of them disappearing.

---

## Schemas pass through

An upstream server owns its own contract, so its `inputSchema` is forwarded byte
for byte into the manifest — not regenerated from a Rust type, not
"normalised", not annotated. `$defs`, `$ref`, `unevaluatedProperties`, vendor
extensions, and keywords this plugin has never heard of all survive.

There are cases where there is nothing worth forwarding, and in each of them the
substitute is the permissive object schema the host would have produced anyway,
so no call is lost. `mcp-bridge.tools` reports which case each tool fell into:

| Upstream `inputSchema` | Declared | `tools` reports |
| --- | --- | --- |
| `{"type":"object", …}` | that object, verbatim | `forwarded` |
| has `properties`, no `type` | that object, verbatim | `forwarded-without-type` |
| `{"$ref": …}`, `allOf`, `anyOf`, `oneOf` | that object, verbatim | `forwarded-without-type` |
| `{}` | `{"type":"object","additionalProperties":true}` | `replaced-empty` |
| `{"type":"array"}` and similar | the same permissive object | `replaced-not-an-object` |
| over 128 KiB | the same permissive object | `replaced-too-large` |

Arguments are forwarded the same way: this plugin checks only that they are a
JSON object, and the upstream applies its own schema.

---

## Lifecycle

### Tools are discovered once, at startup

**This is the surprising part, so it is stated everywhere it matters.** A plugin
sends its manifest exactly once, in the initialize response, and the plugin
protocol has no message for adding a tool later. So `mcp-bridge` contacts every
server *before* `PluginRuntime::run`, and the set of bridged tools is fixed for
the life of the plugin process.

The consequences, all reported rather than left to be discovered:

- A server that is **unreachable at startup** contributes no tools. It still
  appears in `status` as `never-connected` with the reason, and the supervisor
  keeps trying — but its tools cannot appear until `tdcc` restarts the plugin.
- A server that **gains tools later** has them listed as `drift_added` in
  `status` after a reconnect, not projected.
- A server that **loses tools** has them listed as `drift_missing`. Calls to
  them fail with the upstream's own error.

Startup contacts every server concurrently, so it costs the slowest server's
`connect_timeout_secs` rather than the sum of all of them.

> **Raise the host's own plugin startup timeout.** Because discovery finishes
> *before* this plugin connects back to `tdcc`, the whole of it has to fit
> inside `[plugin.startup].connect_timeout_secs`, and the host's default for
> that is **10 seconds**. A cold `npx -y …` server takes considerably longer
> than that the first time. Set the host-side timeout generously — the
> `[[plugin]]` block below does — or the host will give up on the plugin before
> it has finished starting the servers you listed.

### A server that dies is restarted

Every 5 seconds the supervisor checks each link. A dead one is reconnected on a
fixed schedule — 1 s, 2 s, 4 s, 8 s, 16 s, 32 s, then 60 s for ever — so a
server that crashes on every start costs the machine at most one relaunch a
minute rather than a fork bomb. There is no jitter, deliberately: with at most
32 local processes, a schedule you can predict from `status` is worth more than
a smoother graph.

For a stdio server the old child is killed before a replacement is launched, so
a flapping server cannot accumulate processes. `restart = false` watches without
relaunching; the link still shows as `down` rather than vanishing.

`mcp-bridge.reconnect` forces an attempt immediately. It takes one alias and has
no "all": relaunching every third-party process on a machine should cost one
call per process.

### A slow server times out

`call_timeout_secs` bounds every forwarded call. A wedged upstream gets an error
naming the server, the tool, the elapsed limit, and the setting that raises it —
it does not hold a host request open. A timeout does not tear the connection
down; a slow tool is not a dead server.

### Failure is never an empty success

A call to a server that is down returns an error naming the server, its state,
the last reason, and both `mcp-bridge.status` and `mcp-bridge.reconnect`. It
never returns an empty result: an outage and a genuinely empty answer look
identical from the outside, and telling them apart is most of the value.

A result over `max_result_bytes` is **refused, not truncated**, and the message
says so — a file cut in half, or half a JSON document, is worse than an error,
because the caller cannot tell it happened.

---

## The child's environment

For a `transport = "stdio"` server, in this order:

| Source | Included by default | Notes |
| --- | --- | --- |
| Platform baseline | yes | Unix: `HOME`, `LANG`, `LC_ALL`, `LOGNAME`, `PATH`, `SHELL`, `TERM`, `TMPDIR`, `TZ`, `USER`. Windows: `APPDATA`, `COMSPEC`, `HOMEDRIVE`, `HOMEPATH`, `LOCALAPPDATA`, `NUMBER_OF_PROCESSORS`, `OS`, `PATH`, `PATHEXT`, `PROCESSOR_ARCHITECTURE`, `PROGRAMDATA`, `PROGRAMFILES`, `PROGRAMFILES(X86)`, `SYSTEMDRIVE`, `SYSTEMROOT`, `TEMP`, `TMP`, `USERPROFILE`, `WINDIR`. |
| `env_from = ["NAME"]` | copied from `tdcc` | How a key reaches a server without being written into the file. A name that is not set is a startup refusal naming it, not a server that fails obscurely later. |
| `env = { NAME = "value" }` | literal | Wins over both rows above. |
| everything else in `tdcc`'s environment | **dropped** | |
| `TDCC_PLUGIN_*`, `MESH_LLM_PLUGIN_*`, `TDCC_MCP_BRIDGE_*` | **always dropped** | Removed last, after every other rule, `inherit_env` included. |

> `inherit_env = true` hands the child every variable in the `tdcc` process
> except the reserved prefixes — including every API key you exported for your
> other plugins. It exists because some servers need a variable nobody thought
> to name, and it is off by default for the obvious reason.

A launched server's **stderr goes to this plugin's stderr**, which is where the
host's log picks it up. That is deliberate: when `npx` cannot find a package,
you want to read why.

---

## Prerequisites and known limits

**Needed before it does anything:** a server list with at least one entry, and
whatever that entry needs — `npx` or `uvx` on `PATH`, a Python environment, an
already-running HTTP server. The plugin itself needs nothing else and starts
either way.

Known limits, in the order you are likely to hit them:

- **Tools only.** MCP servers can also publish resources, prompts, and
  completions. This plugin bridges `tools/list` and `tools/call` and nothing
  else. A server's resources are not reachable through this node.
- **The tool set is frozen at startup.** See
  [Tools are discovered once](#tools-are-discovered-once-at-startup).
- **No sampling, no roots, no elicitation.** The plugin connects as a client
  that declares no capabilities, so a server that wants to ask the client for a
  completion, for a roots list, or for user input will not get one. Servers that
  use roots to scope themselves fall back to their command-line arguments —
  `@modelcontextprotocol/server-filesystem` logs exactly that and works.
- **HTTP means Streamable HTTP.** The current MCP HTTP transport, with an
  optional static bearer token. The deprecated two-endpoint HTTP+SSE transport
  is not supported, and neither is an OAuth authorization flow.
- **Cancellation and progress are not forwarded.** A host-side cancellation does
  not send `notifications/cancelled` upstream, and a server's progress
  notifications do not reach the caller. A call ends when it answers or when
  `call_timeout_secs` expires.
- **`PATH` resolution is a trust decision.** A bare `command` is resolved
  through the `PATH` of the `tdcc` process — which is what makes `npx` and
  `uvx` work on Windows, where they are `.cmd` shims `CreateProcess` will not
  find. It also means a `PATH` an attacker controls is an attacker-chosen
  binary.
- **No sandbox.** Stated again because it is the one that matters.

---

## Using the tools

`mcp-bridge.status` — no arguments, no network. Abbreviated; every server
line also carries `tools_dropped`, `tools_capped_at`, `drift_added`,
`drift_missing`, `restart`, both timeouts, `max_result_bytes`, and the
operator's `allow_tools` / `deny_tools`:

```jsonc
{
  "plugin": "mcp-bridge",
  "config": {
    "path": "/home/operator/.tdcc/mcp-bridge.toml",
    "state": "loaded",
    "servers_configured": 2
  },
  "totals": {
    "servers_configured": 2, "servers_enabled": 2, "servers_ready": 1,
    "servers_unavailable": 1, "tools_projected": 4, "tools_excluded": 10
  },
  "servers": [
    {
      "alias": "files", "transport": "stdio", "endpoint": "stdio: npx",
      "state": "ready", "tools_projected": 4, "tools_published": 14,
      "tools_excluded": 10, "failed_attempts": 0, "reconnects": 0,
      "server_name": "secure-filesystem-server", "server_version": "0.2.0"
    },
    {
      "alias": "notes", "transport": "http", "state": "never-connected",
      "tools_projected": 0,
      "last_error": "could not reach MCP server 'notes' at http://127.0.0.1:7777/mcp: …"
    }
  ],
  "management_tools": ["status", "tools", "reconnect"],
  "manifest_is_frozen": "The set of bridged tools is fixed when this plugin starts: …",
  "security": "Every server listed here runs third-party code with the privileges of …"
}
```

`mcp-bridge.tools` — `{ "server": "files" }`, optionally
`"include_excluded": false`:

```jsonc
{
  "tools": [
    {
      "tool": "files__read_file",
      "mcp_name": "mcp-bridge.files__read_file",
      "server": "files",
      "upstream_tool": "read_file",
      "renamed": false,
      "name_notes": ["verbatim"],
      "schema": "forwarded",
      "schema_explanation": "the upstream server's own schema, forwarded unchanged",
      "transport": "stdio"
    }
  ],
  "excluded": [
    { "server": "files", "upstream_tool": "write_file",
      "reason": "matched deny_tools pattern 'write_*'" }
  ]
}
```

`mcp-bridge.reconnect` — `{ "server": "files" }`:

```jsonc
{
  "server": "files",
  "reconnected": true,
  "state": "ready",
  "tools_projected": 4,
  "drift_added": ["read_media_file"],
  "drift_missing": [],
  "manifest_is_frozen": "…"
}
```

---

## Blast radius

**Subprocesses.** One per `transport = "stdio"` server, launched with the
command and arguments you wrote, executed directly with no shell, resolved
through `PATH` if the command is a bare name. Killed when the plugin exits and
before every reconnect. Its stderr goes to this plugin's stderr. **This is
arbitrary third-party code running as you.**

**Network.** Outbound only, and only to a `transport = "http"` server's URL. TLS
is rustls using the platform certificate store, so `https` works with no OpenSSL
headers. No listener is opened — the host owns HTTP and MCP. Nothing outbound
happens at all for a node whose server list is stdio-only or empty.

**Filesystem.** One file is read: the server list, at the path you configured.
The plugin writes nothing, ever. What a *bridged server* touches is entirely up
to that server.

**Secrets.** No key is committed, logged, or printed. Credentials are named,
never stored: `env_from` and `bearer_token_env` take variable names, a URL with
embedded credentials is refused at parse time rather than redacted later, and
`--check-config` reports a variable as `set` or `NOT SET` without printing the
value or its length.

**Mesh.** No channels and no events are declared. Delivery is allowlist-based,
so this plugin receives nothing unsolicited from the network — which is the
right posture for the component whose job is handing arguments to third-party
binaries.

**Memory.** Bounded everywhere a caller could grow it: 32 servers, 2048 raw
tools per `tools/list`, `max_tools` projected per server, 128 KiB per forwarded
schema, 4000 characters per forwarded description, `max_result_bytes` per
answer.

---

## Building against the SDK

`tdcc-plugin` is **not published on crates.io under that name** — it was renamed
from `mesh-llm-plugin` and its repository is private — so a version requirement
like `tdcc-plugin = "0.72.1"` will not resolve. `Cargo.toml` here uses a path
dependency into a local `tdcc-mesh` checkout instead:

```toml
tdcc-plugin = { path = "../../../tdcc-mesh/crates/tdcc-plugin" }
```

That path assumes `tdcc-plugins` and `tdcc-mesh` are siblings:

```text
token/
  tdcc-mesh/          # the private host repository, with crates/tdcc-plugin
  tdcc-plugins/       # this repository
    plugins/mcp-bridge/
```

If your checkout is laid out differently, change that one line, or add a
`[patch]` section instead of editing the dependency:

```toml
[patch.crates-io]
tdcc-plugin = { path = "/absolute/path/to/tdcc-mesh/crates/tdcc-plugin" }
```

**Once the SDK is published**, a public consumer replaces the path dependency
with a version pin matching the `tdcc` release they target:

```toml
tdcc-plugin = "0.72.1"
```

Nothing else changes — no code here depends on the dependency being local. Pin
an exact version: the initialize handshake requires an exact protocol-version
match, so a host and a plugin built against mismatched protocol versions refuse
to connect at startup rather than misbehaving later.

```bash
cargo build --release
```

The first build downloads a vendored `protoc` for `tdcc-plugin`'s `prost-build`
step; no system protobuf compiler is needed.

### The dependency list, and why

| Crate | Why this one |
| --- | --- |
| `rmcp` | The MCP client, at the same version the host itself links, so a bridged server talks to one MCP implementation rather than to a second one written here. Features: `client`, `transport-child-process`, `transport-streamable-http-client-reqwest`, `reqwest` (rustls, so `https` needs no OpenSSL headers), `which-command` (PATH resolution including Windows `PATHEXT`, without which `npx` cannot be launched at all). |
| `toml` | The server list. Parsing only; nothing is written back. |
| `url` | Already in the graph through `rmcp`'s reqwest transport, so it costs no extra compilation. Direct because deciding whether an upstream URL carries embedded credentials is a security check, and picking apart an authority by hand is the wrong way to make one. |
| `anyhow`, `schemars`, `serde`, `serde_json`, `tokio` | The SDK's own baseline. |

`Cargo.lock` is committed. This is a binary that runs on other people's
machines; a locked dependency set is what makes a release reproducible and a
review finite.

---

## Tests

```bash
cargo test
```

143 tests, no network and no child process required.

| Area | Covered by |
| --- | --- |
| Server list parsing, validation, every refusal message | `config` (20) |
| Discovery, filtering, naming, and error shapes over synthetic tool lists | `upstream` (18) |
| `<alias>__<tool>`, alias validation, sanitizing, collisions, stability | `naming` (14) |
| The whole path against a real MCP server | `upstream::end_to_end` (11) |
| Argument conversion, result bounding, provenance stamping | `forward` (11) |
| Startup messages, the `--check-config` plan, concurrent startup | `main` (10) |
| Manifest assembly, name reservation, declared surfaces | `manifest` (10) |
| The child's environment, on both platforms' baselines | `childenv` (10) |
| Schema forwarding and the three replacement cases | `schema` (9) |
| Allowlist, denylist, precedence, glob matching | `filter` (9) |
| The registry and the three tools' answers | `bridge` (9) |
| Option parsing and precedence | `cli` (7) |
| The reconnect schedule | `backoff` (5) |

The eleven `end_to_end` tests are the ones worth reading. They stand up a real
MCP server — [`src/testserver.rs`](src/testserver.rs), hand-written so it can
also misbehave on purpose — on an in-process pipe speaking the same
newline-delimited JSON-RPC an MCP stdio server speaks, and drive the whole path
through it: the initialize handshake, `tools/list`, discovery, namespacing,
schema forwarding, `tools/call` with the arguments arriving unchanged, an
upstream error result staying an error result, the size cap, the call timeout,
and the connection vanishing mid-call. Two servers are bridged at once and each
answer is checked for the right provenance stamp.

One test is **ignored by default**, because it reaches outside this repository:

```bash
cargo test -- --ignored --nocapture
```

It launches a genuine third-party MCP server —
`npx -y @modelcontextprotocol/server-filesystem` — as a real child process and
proves the point of the plugin against something nobody here wrote. Its actual
output on a Windows machine:

```text
Secure MCP Filesystem Server running on stdio
serverInfo: Some("secure-filesystem-server") Some("0.2.0"), protocol Some("2025-11-25")
bridged 14 tool(s) from a real MCP server:
  files__create_directory -> create_directory (Forwarded)
  files__directory_tree -> directory_tree (Forwarded)
  files__edit_file -> edit_file (Forwarded)
  files__get_file_info -> get_file_info (Forwarded)
  files__list_allowed_directories -> list_allowed_directories (Forwarded)
  files__list_directory -> list_directory (Forwarded)
  files__list_directory_with_sizes -> list_directory_with_sizes (Forwarded)
  files__move_file -> move_file (Forwarded)
  files__read_file -> read_file (Forwarded)
  files__read_media_file -> read_media_file (Forwarded)
  files__read_multiple_files -> read_multiple_files (Forwarded)
  files__read_text_file -> read_text_file (Forwarded)
  files__search_files -> search_files (Forwarded)
  files__write_file -> write_file (Forwarded)
```

It then calls `files__list_directory` and checks that the answer names a file it
created, and that the result carries `tdcc.mcp-bridge/server: "files"`. It needs
`npx` on `PATH` and downloads a package from the npm registry, which is why it
does not run by default.

**What no test here covers:** installation into a running host. The initialize
handshake, the host's projection of these operations onto its MCP and HTTP
surfaces, and the behaviour of a bridged tool called through the node's endpoint
rest on the checklist in
[CONTRIBUTING.md](../../CONTRIBUTING.md#5-run-it-and-watch-it-fail-correctly),
not on this suite.

---

## Package and install locally

The archive needs one top-level directory named after the plugin, containing
`plugin.toml` and an executable named exactly `mcp-bridge` (`mcp-bridge.exe` on
Windows). This plugin declares neither a config schema nor a web UI, so its
`plugin-manifest.json` is `{}` and may be left out; `--print-package-manifest`
prints it if you want to include it anyway.

macOS and Linux:

```bash
cargo build --release
rm -rf target/package
mkdir -p target/package/mcp-bridge
cp target/release/mcp-bridge target/package/mcp-bridge/mcp-bridge
cp plugin.toml README.md target/package/mcp-bridge/
tar -C target/package -czf target/mcp-bridge-0.1.0-local.tar.gz mcp-bridge

tdcc plugins install --archive ./target/mcp-bridge-0.1.0-local.tar.gz \
  --name mcp-bridge --version 0.1.0
tdcc plugins info mcp-bridge
```

Windows:

```powershell
cargo build --release
Remove-Item -Recurse -Force target\package -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force target\package\mcp-bridge | Out-Null
Copy-Item target\release\mcp-bridge.exe target\package\mcp-bridge\mcp-bridge.exe
Copy-Item plugin.toml, README.md target\package\mcp-bridge\
Compress-Archive -Path target\package\mcp-bridge `
  -DestinationPath target\mcp-bridge-0.1.0-local.zip -Force

tdcc plugins install --archive .\target\mcp-bridge-0.1.0-local.zip `
  --name mcp-bridge --version 0.1.0
```

Set `TDCC_PLUGIN_DIR` to an empty directory first if you do not want an
in-development build landing in your real plugin store.

Then write a server list, enable the plugin, and start the node:

```bash
tdcc client --port 9337 --console 3131 --config ./config.toml
```

```bash
curl --fail http://127.0.0.1:3131/api/plugins/mcp-bridge/http/status | jq '.totals'
curl --fail -X POST http://127.0.0.1:3131/api/plugins/mcp-bridge/tools/tools \
  -H 'Content-Type: application/json' -d '{"server":"files"}'
```

Running the binary directly, outside a host, fails immediately with
`TDCC_PLUGIN_ENDPOINT is not set for plugin process`. That is correct — the host
owns the control endpoint and passes it in through the launch contract.
`--help`, `--check-config`, and `--print-package-manifest` are handled before
the runtime starts and work anywhere.

---

## Compatibility

These identifiers are a public API. Changing one is a breaking change, because
they are names other people wrote down:

- the capability id `mcp-bridge.v1`;
- the tool names `status`, `tools`, `reconnect`, and the HTTP paths `/status`,
  `/tools`, `/reconnect`;
- the `__` separator and the shape `<alias>__<upstream tool>` — a change here
  renames every bridged tool on every node;
- the `_meta` keys `tdcc.mcp-bridge/server` and `tdcc.mcp-bridge/tool`;
- every key in the server list, and the document `version = 1`;
- the state strings `ready`, `down`, `never-connected`, `disabled`, and the
  config states `absent`, `loaded`, `invalid`, `unreadable`;
- the schema notes `forwarded`, `forwarded-without-type`, `replaced-empty`,
  `replaced-not-an-object`, `replaced-too-large`.

A new key in the server list is additive as long as the old spelling keeps
working; because unknown keys are rejected, an *older* build reading a *newer*
file fails loudly rather than ignoring the new setting.

---

## License

Apache-2.0.
