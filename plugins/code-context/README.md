# code-context

Give the model your repository without pasting it.

`code-context` indexes one local directory and exposes it as five MCP tools:
search it, read a line range out of it, and draw its tree. Every result carries
a root-relative path and a line number, so a model can cite `src/index.rs:142`
instead of paraphrasing what it thinks it saw. Every response is capped, so a
single answer cannot swallow a context window.

The plugin is confined to one configured root and refuses to read outside it.
That is the part to read carefully — see [Security](#security).

## Tools

All five are projected by the host as `code-context.<name>` on the MCP endpoint
and at `POST /api/plugins/code-context/tools/<name>`.

| Tool | What it does |
| --- | --- |
| `search` | `kind=content` scans indexed file text; `kind=symbol` matches declaration names from the index. Literal by default, `regex=true` for patterns, `path_glob` to narrow, `context_lines` for surrounding lines. |
| `read` | Returns a file or a line range, plus `start_line`, `end_line`, and `total_lines`. Only files the index accepted. |
| `tree` | Draws the directory tree, optionally scoped to a subdirectory. Shows exactly what `search` and `read` can reach. |
| `status` | File, byte, line, and symbol counts; why files were skipped; when the index last refreshed; the active limits. |
| `reindex` | Reindex now. Incremental by default; `force=true` rereads everything. |

A symbol search over this plugin's own source returns:

```json
{
  "kind": "symbol",
  "query": "resolve_within",
  "files_considered": 12,
  "files_read": 1,
  "matches": 1,
  "truncated": false,
  "results": [
    {
      "path": "src/paths.rs",
      "line": 134,
      "citation": "src/paths.rs:134",
      "text": "pub fn resolve_within(root: &Path, input: &str) -> Result<PathBuf, PathError> {",
      "symbol": "resolve_within",
      "symbol_kind": "fn"
    }
  ]
}
```

`files_read: 1` out of twelve candidates is the point of keeping symbols in the
index: only the file that actually declared something was opened. `citation` is
there so an answer can quote a location verbatim without the model having to
assemble one.

## Security

This plugin reads files on hardware that may not belong to the person asking
the question. Its blast radius is deliberately narrow, and stated here rather
than implied.

**What it can touch.** Regular files under one canonical root, chosen by the
operator. Nothing else. No network, no subprocess, no writes — it opens files
read-only and never creates, modifies, or deletes anything.

**How confinement is enforced.** Two independent layers, because either alone
is bypassable:

1. *Lexical*, in `sanitize_relative`. Absolute paths, rooted paths, Windows
   drive prefixes, NTFS alternate-data-stream syntax, and any `..` segment are
   refused before a syscall happens. `..` is rejected outright rather than
   normalized away, even when it would land back inside the root.
2. *Physical*, in `resolve_within`. The joined path is canonicalized — which
   resolves symlinks, junctions, and `.` — and containment is re-checked
   component-wise against the canonical root. A symlink inside the root that
   points outside it fails here.

The indexer adds a third layer: it walks with `follow_links(false)` and records
only regular files, so a link is never a candidate in the first place. And
`search` re-resolves each path at read time rather than trusting a path the
index recorded seconds ago.

`src/paths.rs` has the test that proves the escape is refused: it creates a real
symlink (or, on Windows, a directory junction) inside the root pointing at a
file outside it, asserts the link genuinely resolves, and then asserts
`resolve_within` still refuses it.

**Secret filtering is a heuristic, not a guarantee.** The plugin refuses to
index, search, or return files that look credential-shaped:

- names: `.env` and every `.env.*`, `.netrc`, `.npmrc`, `.pypirc`, `.htpasswd`,
  `.pgpass`, `.git-credentials`, `credentials`, `credentials.json`,
  `secrets.json`, `secrets.y[a]ml`, `kubeconfig`, `terraform.tfvars`, `id_rsa`
  and friends, and anything ending `_rsa` / `_dsa` / `_ecdsa` / `_ed25519`
- extensions: `.pem .key .p12 .pfx .jks .keystore .asc .gpg .pgp .ppk .kdbx
  .der .crt .cer`
- directories: `.ssh`, `.gnupg`, `.gpg`, `.aws`, `.azure`, `.kube`, `.docker`
- contents: any file with a line beginning `-----BEGIN … PRIVATE KEY`

**This will not catch a secret that does not look like one.** An API token
pasted into `config/production.yaml`, a password in a comment, a `.tf` file with
a hard-coded key — none of those match anything above, and all of them will be
indexed and returned. Point this plugin at repositories you would be willing to
show the model anyway. It reduces accidental disclosure; it does not prevent it.

**No sandbox.** Confinement is enforced by this plugin's own code, in this
plugin's own process, running with the operator's privileges. Installing any
plugin runs third-party native code on your machine. Read the source before you
trust the claim.

**What tool responses do not contain.** The absolute root. Paths in every
response are root-relative, and path errors quote the caller's own input rather
than the resolved location. The absolute root is printed once, to stderr, at
startup — for the operator, not for the model.

## Configuration

The root is required and arrives through `[[plugin]].args`:

```toml
# ~/.tdcc/config.toml
version = 1

[[plugin]]
name = "code-context"
enabled = true
command = "/opt/code-context/code-context"
args = ["--root", "/srv/repos/tdcc-mesh"]
```

| Flag | Environment fallback | Default | Meaning |
| --- | --- | --- | --- |
| `--root <dir>` | `CODE_CONTEXT_ROOT` | — | Required. The one directory the plugin may read. |
| `--max-file-bytes <n>` | `CODE_CONTEXT_MAX_FILE_BYTES` | `1048576` | Files larger than this are counted and skipped. Ceiling `16777216`. |
| `--refresh-secs <n>` | `CODE_CONTEXT_REFRESH_SECS` | `5` | Minimum index age before a tool call triggers an incremental refresh. `0` refreshes every call. Ceiling `3600`. |
| `--include-hidden` | `CODE_CONTEXT_INCLUDE_HIDDEN` | off | Index dot-files and dot-directories. Secret filtering still applies. |
| `--include-vendored` | `CODE_CONTEXT_INCLUDE_VENDORED` | off | Index `node_modules`, `target`, `dist`, and the rest of the vendored list. |

Flags win over the environment. Both `--root /x` and `--root=/x` work.

### Why there is no `[plugin.settings]` schema

`[plugin.settings]` never reaches a plugin process. The host stores those
values, the console renders them, and a web UI bundle reads them back — but
there is no settings field in the launch contract or the initialize handshake.
A `root` setting would look authoritative in the console and do nothing at all.
So this plugin declares no config schema and reads its root from `args`, which
is one of the two channels that does reach the process.

The practical consequence: **changing the root means editing `config.toml` and
restarting `tdcc`**, not clicking something in the console.

## What gets indexed

Included: regular files under the root that survive every filter below.

Excluded, and counted in `status` so you can see why:

| Filter | Rule |
| --- | --- |
| `.gitignore` | Honoured, along with `.ignore`, `.git/info/exclude`, and your global gitignore. Handled by the `ignore` crate — the same implementation ripgrep uses — not reimplemented here. Works on a plain directory too, not just a checkout. |
| Version control | `.git`, `.hg`, `.svn`, `.jj`, `.bzr`. Always skipped. |
| Secrets | The list under [Security](#security). Always skipped. |
| Vendored | `node_modules`, `vendor`, `third_party`, `target`, `dist`, `build`, `out`, `.next`, `__pycache__`, `.venv`, `site-packages`, `Pods`, `.terraform`, and similar. Skipped unless `--include-vendored`. |
| Hidden | Dot-files and dot-directories. Skipped unless `--include-hidden`. |
| Size | Larger than `--max-file-bytes`. |
| Binary | A NUL byte in the first 8 KiB, or not valid UTF-8. |
| Generated | `*.min.js`, `*.min.css`, `*.map`, `*.bundle.js`, or any file with a line longer than 2000 bytes. That last rule is the minified-bundle guard. |

### The index is an inventory, not a content cache

It holds paths, sizes, mtimes, line counts, and extracted symbols — not file
text. Symbol search therefore costs no file I/O at all; content search re-reads
the files it scans. That keeps memory bounded on a machine somebody lent you,
at the cost of re-reading on every content search, which is what ripgrep does
too.

### Incremental reindex

Every refresh re-walks the tree — that is how a deletion is noticed — but a file
whose size and mtime are both unchanged is never reopened, reread, or reparsed.
On a large repository that is the difference between a stat-bound walk and a
read-bound one.

The change signal is `(size, mtime)`. **A rewrite that preserves both is
invisible** until someone calls `reindex` with `force: true`. That is a real
limitation, stated here instead of hidden behind a hash of every file on every
refresh.

### Symbol extraction is a lexical scan

Not a parser, not a language server. It strips a language's declaration
modifiers and looks for a keyword followed by an identifier, for Rust, Python,
JavaScript/TypeScript, Go, Java/Kotlin/Scala/C#, C/C++, Ruby, PHP, Lua, and
shell. It will miss declarations written in unusual shapes and will occasionally
record one that appears inside a string or a comment. Files with any other
extension are still fully content-searchable; they just contribute no symbols.

## Limits

Every one of these is enforced in code and covered by a test.

| Limit | Value |
| --- | --- |
| `search` results | 1–200, default 40 |
| `search` context lines | 0–5, default 0 |
| Snippet length per line | 400 bytes, windowed around the match |
| `search` response payload | 96 KiB of snippets |
| `search` file bytes scanned | 64 MiB per call |
| `read` lines | 2000 per call, default window 400 |
| `read` bytes | 192 KiB per call |
| `tree` depth | 1–12, default 3 |
| `tree` entries | 1–2000, default 400 |
| Regex compiled size | 1 MiB (a pattern that would build a huge automaton is refused) |

`truncated: true` in a response means a cap stopped the work early, so more may
exist. In `read` it means specifically that a cap shortened the range you asked
for; compare `end_line` with `total_lines` to see whether the file continues.

## Building against the SDK

**This crate will not build from a fresh clone of this repository alone.**

`tdcc-plugin` is not published to crates.io under that name — it was renamed
from `mesh-llm-plugin` and its repository is private — so the line the guide
shows,

```toml
tdcc-plugin = "0.72.1"
```

does not resolve. `Cargo.toml` here uses a path dependency on a local `tdcc-mesh`
checkout instead:

```toml
tdcc-plugin = { path = "../../../tdcc-mesh/crates/tdcc-plugin" }
```

That path assumes `tdcc-mesh` and this repository sit side by side:

```text
token/
  tdcc-mesh/          the main repository, with crates/tdcc-plugin
  tdcc-plugins/       this repository
    plugins/code-context/
```

If your layout differs, change the path, or add a `[patch]` section pointing at
wherever your checkout lives. **Once the SDK is published, replace that line
with the version dependency and delete the path.** Pin it to a version
compatible with the `tdcc` release you target: the initialize handshake requires
an exact protocol-version match, so a host and a plugin built against different
protocol versions refuse to connect at startup.

The first build downloads a vendored `protoc` through `tdcc-plugin`'s
`prost-build` step. No system protobuf compiler is required.

### Dependencies

Beyond the SDK: `ignore` and `globset` for gitignore-aware walking and glob
matching, `regex` for search patterns, and the usual `anyhow` / `serde` /
`serde_json` / `schemars` / `tokio`. Nothing is pulled in for testing — the temp
directory and symlink helpers in `src/testsupport.rs` are hand-rolled so the
release dependency set stays as small as the job.

## Build and test

```bash
cargo test
cargo clippy --all-targets
cargo build --release
```

The tests cover the path confinement rules including the symlink escape, the
secret and binary and minified heuristics, option parsing precedence, symbol
extraction per language, matcher and glob construction, snippet windowing on
multi-byte text, tree rendering and its caps, read-range clamping, and the
incremental reindex accounting. One test indexes this crate's own directory —
`target/` and all — and asserts the build output stays out.

## Package and install locally

macOS or Linux, from this directory:

```bash
rm -rf target/package
mkdir -p target/package/code-context
cp target/release/code-context target/package/code-context/code-context
cp plugin.toml target/package/code-context/plugin.toml
cp README.md target/package/code-context/README.md
tar -C target/package -czf target/code-context-0.1.0-local.tar.gz code-context

tdcc plugins install --archive ./target/code-context-0.1.0-local.tar.gz \
  --name code-context --version 0.1.0
tdcc plugins info code-context
```

Windows uses `code-context.exe` and a `.zip` whose single top-level directory is
`code-context/`:

```powershell
Compress-Archive -Path target\package\code-context `
  -DestinationPath target\code-context-0.1.0-local.zip -Force
tdcc plugins install --archive .\target\code-context-0.1.0-local.zip `
  --name code-context --version 0.1.0
```

This plugin declares no config schema and no web UI, so
`--print-package-manifest` emits `{}` and `plugin-manifest.json` may be left out
of the archive entirely.

Set `TDCC_PLUGIN_DIR` to an empty directory first if you do not want a test
install landing in your real plugin store.

## Run and call it

```bash
tdcc client --port 9337 --console 3131 --config ./config.toml
```

In another terminal:

```bash
curl --fail -X POST http://127.0.0.1:3131/api/plugins/code-context/tools/status \
  -H 'Content-Type: application/json' -d '{}'

curl --fail -X POST http://127.0.0.1:3131/api/plugins/code-context/tools/search \
  -H 'Content-Type: application/json' \
  -d '{"query":"resolve_within","kind":"symbol"}'

curl --fail -X POST http://127.0.0.1:3131/api/plugins/code-context/tools/read \
  -H 'Content-Type: application/json' \
  -d '{"path":"src/paths.rs","start_line":100,"end_line":140}'

curl --fail -X POST http://127.0.0.1:3131/api/plugins/code-context/tools/tree \
  -H 'Content-Type: application/json' -d '{"depth":2}'
```

And the one that should fail:

```bash
curl -X POST http://127.0.0.1:3131/api/plugins/code-context/tools/read \
  -H 'Content-Type: application/json' -d '{"path":"../../../etc/passwd"}'
# → path must not contain a '..' segment
```

On the host MCP endpoint the same tools are namespaced `code-context.search`,
`code-context.read`, and so on.

### Running it directly

Running the binary with a root but no host fails immediately:

```text
code-context: confined to /srv/repos/tdcc-mesh
Error: TDCC_PLUGIN_ENDPOINT is not set for plugin process
```

That is correct. The host owns the control endpoint and passes it in through the
launch contract; a plugin must never invent a socket path of its own.

## Failure behaviour

A tool that cannot do its job says so; it never returns an empty success.

| Situation | Result |
| --- | --- |
| Path escapes the root | Error naming the rule that refused it |
| Malformed regex or glob | Error quoting the compiler's message |
| File not in the index | Error listing the reasons a file may be excluded, and pointing at `tree` |
| `start_line` past the end of the file | Error stating the file's actual length |
| File changed or vanished mid-search | That file is skipped; the search still returns its other results |
| No root configured | The process exits at startup with the flag and environment variable to set |

## License

Apache-2.0, matching this repository. See [LICENSE](../../LICENSE).
