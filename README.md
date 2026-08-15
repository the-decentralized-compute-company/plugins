# TDCC Plugins

Reference material and working examples for writing plugins for
[TDCC](https://decentralizedcompute.company) — Decentralized Compute.

This repository is documentation and examples. The first-party plugins each
live in their own repository and ship their own release archives; nothing in
this repository is published to the plugin catalog or installable with
`tdcc plugins install <name>`.

- **Writing a plugin?** Start with [What a plugin is](#what-a-plugin-is), then
  copy [`examples/hello-plugin`](examples/hello-plugin).
- **Looking for a plugin to install?** Jump to the
  [plugin catalog](#plugin-catalog).

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

---

## Plugin catalog

TDCC documents five first-party plugins. Each is a **separate repository** with
its own releases — none of them is vendored here. The two examples at the
bottom of the table live in this repository, are not in the catalog, and have
no published releases at all.

| Plugin | What it does | Install | Status |
| --- | --- | --- | --- |
| [`blackboard`](https://github.com/the-decentralized-compute-company/blackboard) | Shares short-lived status, findings, questions, and answers across a mesh. Also ships an Agent Skill. | `tdcc plugins install blackboard` | First-party · external repo |
| [`openai-endpoint`](https://github.com/the-decentralized-compute-company/openai-endpoint) | Attaches an already-running OpenAI-compatible server (vLLM, TGI, Ollama, Lemonade) to the mesh. | `tdcc plugins install openai-endpoint` | First-party · external repo |
| [`flash-moe`](https://github.com/the-decentralized-compute-company/flash-moe) | Attaches a Flash-MoE endpoint, or supervises a local Flash-MoE process for SSD expert streaming. | `tdcc plugins install flash-moe` | First-party · external repo |
| [`metrics`](https://github.com/the-decentralized-compute-company/metrics) | Advertises metrics support for TDCC telemetry. The OTLP destination is configured in `tdcc`, not in the plugin. | `tdcc plugins install metrics` | First-party · external repo |
| [`agents`](https://github.com/the-decentralized-compute-company/agents) | Runs mesh-native A2A agents and exposes their tools through the mesh MCP endpoint. | `tdcc plugins install agents` | First-party · external repo |
| [`hello-plugin`](examples/hello-plugin) | The smallest complete plugin: manifest, one MCP tool, control connection. | build, then `tdcc plugins install --archive` | Example in this repo · not in catalog |
| [`notes-console`](examples/notes-console) | Config schema, MCP tool, HTTP routes, a console page, and a Configuration section. | build, then `tdcc plugins install --archive` | Example in this repo · not in catalog |

Whether a given first-party plugin publishes a prebuilt archive for your exact
platform is up to that repository's releases. If `tdcc plugins install` reports
no compatible asset for your target, build it from its repository and point
`[[plugin]].command` at the binary you built.

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

Authoring, or testing a release candidate before publishing it:

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
them, and only `openai-endpoint`-style plugins usually do.

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
- Keep `health` fast and independent of long-running work.
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
tdcc-plugin = "0.72.1"
schemars = "1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

Pin `tdcc-plugin` to a version compatible with the `tdcc` release you target.
The initialize handshake requires an exact protocol-version match, so a host
and a plugin built against mismatched protocol versions refuse to connect —
loudly, at startup, not silently at first use.

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
  people wrote down.

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

- **A bug in an example, or something wrong or missing in this guide** — open
  an issue in this repository.
- **A bug in a first-party plugin** (`blackboard`, `openai-endpoint`,
  `flash-moe`, `metrics`, `agents`) — open it in that plugin's own repository.
- **A bug in the plugin SDK, the installer, the host projection, or the
  console** — open it against the main TDCC repository if you have access, or
  here if you do not. Include the output of `tdcc plugins info <name>` and your
  `[[plugin]]` block with any secrets removed.

Contributions are welcome — see [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Apache-2.0. See [LICENSE](LICENSE).
