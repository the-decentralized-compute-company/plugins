# notes-console

A plugin that contributes more than one surface: an operator setting, an MCP
tool, two HTTP routes, a console page, and a Configuration → Plugins section —
all from a single `plugin!` declaration.

Use it as the working reference for the parts of the contract that are easy to
get subtly wrong: bundle roots, entry scripts, plugin-relative fetch paths, and
who actually owns `[plugin.settings]`.

## What it shows

| Surface | Declaration | Where it lands |
| --- | --- | --- |
| `provides` | `capability("notes-console.v1")` | capability resolution and `tdcc plugins info` |
| `config` | `config_schema("notes-console")` with `max_notes` | Configuration → Plugins, rendered by the host's own numeric control |
| `web_ui` | one bundle, one page, one config section | `/plugins/notes-console/notes` and the Integrations section |
| `mcp` | `tool("status")` | `notes-console.status` on the host MCP endpoint |
| `http` | `get("/notes")`, `post("/notes")` | `/api/plugins/notes-console/http/notes` |

## Layout

```text
notes-console/
  Cargo.toml
  plugin.toml                        package marker read by the installer
  src/main.rs                        entrypoint + --print-package-manifest
  src/manifest.rs                    the single plugin! declaration
  src/notes.rs                       plugin-owned state
  bundle/
    register-mesh-plugin-ui.js       shippable browser ES module
    host-contract.d.ts               author types, copied from the exemplar
```

`bundle/` is the one declared bundle root. v1 permits exactly one, every page
and config section must reference it by `bundle_id`, and every declared
`entry_script` must exist inside it in the installed package.

## Who owns what

The distinction the example is built to make obvious:

- **The plugin owns the notes.** They live in `src/notes.rs`, in the plugin
  process. The host never touches them; it only invokes the declared
  operations.
- **The host owns the settings.** `max_notes` is declared by the plugin but
  stored in the host's `[plugin.settings]`. It is *not* delivered to the plugin
  process — there is no settings field in the launch contract or the
  initialize handshake. The console bundle reads it from
  `host.config.visible.settings` and passes it to the plugin as a query
  parameter. The plugin keeps its own independent safety limit.

If you need a value inside the plugin process itself, pass it through
`[[plugin]].args`, `[[plugin]].url`, or the plugin's own state — not through
`[plugin.settings]`.

## Build and test

```bash
cargo test
cargo build --release
```

The tests cover the note store's ordering and retention, the request-limit
clamp, and a manifest assertion that the web UI declaration keeps exactly one
bundle root with matching `bundle_id` values — the rule the installer rejects a
package for.

## Package and install locally

macOS or Linux, from this directory:

```bash
rm -rf target/package
mkdir -p target/package/notes-console
cp target/release/notes-console target/package/notes-console/notes-console
cp plugin.toml target/package/notes-console/plugin.toml
cp -R bundle target/package/notes-console/bundle
target/release/notes-console --print-package-manifest \
  > target/package/notes-console/plugin-manifest.json
tar -C target/package -czf target/notes-console-0.1.0-local.tar.gz notes-console

tdcc plugins install --archive ./target/notes-console-0.1.0-local.tar.gz \
  --name notes-console --version 0.1.0
```

Windows uses `notes-console.exe` and a `.zip`:

```powershell
Compress-Archive -Path target\package\notes-console `
  -DestinationPath target\notes-console-0.1.0-local.zip -Force
tdcc plugins install --archive .\target\notes-console-0.1.0-local.zip `
  --name notes-console --version 0.1.0
```

Because this plugin declares a config schema and a web UI, `plugin-manifest.json`
is required in the archive. The installer parses it, resolves the bundle root
against the installed package, and records the result. Confirm it landed as
valid:

```bash
tdcc plugins info notes-console
cat ~/.tdcc/plugins/notes-console/plugin-install.json
```

The stored record should contain `"validation": { "status": "valid" }` under
`manifest.web_ui`. A `status` of `invalid` carries a `reason` naming the rule
that failed. With `TDCC_PLUGIN_DIR` set, the record is at
`$TDCC_PLUGIN_DIR/notes-console/plugin-install.json` instead.

## Run it

```toml
# config.toml
version = 1

[[plugin]]
name = "notes-console"
enabled = true
web_ui_enabled = true

[plugin.settings]
max_notes = 10
```

Configuration writes from the console require a local owner identity:

```bash
tdcc auth status
# only if none exists, for local development:
tdcc auth init --no-passphrase
tdcc client --port 9337 --console 3131 --config ./config.toml
```

Then check each surface:

```bash
curl --fail http://127.0.0.1:3131/api/plugins/notes-console/web-ui
curl --fail -X POST http://127.0.0.1:3131/api/plugins/notes-console/http/notes \
  -H 'Content-Type: application/json' -d '{"text":"first note"}'
curl --fail 'http://127.0.0.1:3131/api/plugins/notes-console/http/notes?limit=5'
curl --fail -X POST http://127.0.0.1:3131/api/plugins/notes-console/tools/status \
  -H 'Content-Type: application/json' -d '{}'
```

Open `http://127.0.0.1:3131/`, use the **Notes** navigation item, and add a
note from the page. Then open Configuration → Plugins, change **Notes per
page**, save, and reload the page to see the new page size take effect.

## Prove the projection is independent of the process

Disabling the web UI hides the assets and the navigation item but must leave
the MCP tool and HTTP routes working:

```bash
curl --fail -X PATCH http://127.0.0.1:3131/api/plugins/notes-console/web-ui/enabled \
  -H 'Content-Type: application/json' -d '{"enabled":false}'

# still 200
curl --fail -X POST http://127.0.0.1:3131/api/plugins/notes-console/tools/status \
  -H 'Content-Type: application/json' -d '{}'

# now 404
curl -s -o /dev/null -w '%{http_code}\n' \
  http://127.0.0.1:3131/api/plugins/notes-console/web-ui/assets/register-mesh-plugin-ui.js
```

Re-enable with the same request and `{"enabled":true}`.

## Bundle notes

- The bundle ships as plain browser JavaScript. The host imports it as an ES
  module and does not transpile TypeScript, JSX, CommonJS, or bare npm
  specifiers.
- `host.network.json("http/notes?limit=10")` resolves to
  `/api/plugins/notes-console/http/notes?limit=10`. The helper rejects origins,
  fragments, backslashes, and `.` / `..` segments, so a bundle cannot reach
  outside its own plugin namespace.
- `host.network.json(...)` rejects non-2xx responses. Use
  `host.network.fetchPlugin(...)` when you need to inspect the status yourself.
- Every mount handler returns `{ unmount() }`, and `unmount` must remove DOM
  content and detach listeners. The page also flips a `disposed` flag so an
  in-flight request cannot write into a torn-down element.
- The config section deliberately does not re-implement the `max_notes` input.
  Schema-backed settings belong to the host's own control, which handles
  validation and the owner-authorized save path.
