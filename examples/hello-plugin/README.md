# hello-plugin

The smallest complete TDCC plugin: one manifest, one MCP tool, one runtime
entrypoint. Nothing here is optional scaffolding — remove any of it and the
plugin stops being installable or stops connecting.

## What it shows

- the four things every plugin needs: a `Cargo.toml` that depends on
  `tdcc-plugin`, a `plugin.toml` package marker, a `plugin!` manifest, and a
  `main` that hands the plugin to `PluginRuntime::run`
- a typed tool input (`GreetArgs`) that becomes the JSON Schema the host
  advertises in `tools/list` and validates arguments against
- the `--print-package-manifest` packaging option

It does **not** declare a config schema or a web UI, so its
`plugin-manifest.json` is `{}` and the file may be left out of the archive
entirely. Add it only when the plugin declares one of those two things.

## Build

```bash
cargo build --release
```

`tdcc-plugin` builds its protocol types with `prost-build`, so the first build
downloads a vendored `protoc`. No system protobuf compiler is required.

## Package and install locally

From this directory, on macOS or Linux:

```bash
rm -rf target/package
mkdir -p target/package/hello-plugin
cp target/release/hello-plugin target/package/hello-plugin/hello-plugin
cp plugin.toml target/package/hello-plugin/plugin.toml
tar -C target/package -czf target/hello-plugin-0.1.0-local.tar.gz hello-plugin

tdcc plugins install --archive ./target/hello-plugin-0.1.0-local.tar.gz \
  --name hello-plugin --version 0.1.0
tdcc plugins info hello-plugin
```

On Windows, copy `hello-plugin.exe` instead and build a `.zip` whose single
top-level directory is `hello-plugin/`:

```powershell
Compress-Archive -Path target\package\hello-plugin `
  -DestinationPath target\hello-plugin-0.1.0-local.zip -Force
tdcc plugins install --archive .\target\hello-plugin-0.1.0-local.zip `
  --name hello-plugin --version 0.1.0
```

Set `TDCC_PLUGIN_DIR` to an empty directory first if you do not want the
example landing in your real plugin store.

## Enable and call it

```toml
# config.toml
version = 1

[[plugin]]
name = "hello-plugin"
enabled = true
```

```bash
tdcc client --port 9337 --console 3131 --config ./config.toml
```

In another terminal:

```bash
curl --fail -X POST http://127.0.0.1:3131/api/plugins/hello-plugin/tools/greet \
  -H 'Content-Type: application/json' -d '{"name":"mesh"}'
```

On the host MCP endpoint the same tool is namespaced as `hello-plugin.greet`.

## Running it directly

Running the binary with no arguments outside a host fails immediately:

```text
Error: TDCC_PLUGIN_ENDPOINT is not set for plugin process
```

That is correct. The host owns the control endpoint and passes it in through
the launch contract; a plugin must never invent a socket path of its own.
