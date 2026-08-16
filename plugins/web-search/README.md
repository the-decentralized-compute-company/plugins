# web-search

Give a model on your mesh the ability to search the web and read a result.

Two MCP tools, projected by the host:

| Tool | On the MCP endpoint | What it does |
| --- | --- | --- |
| `search` | `web-search.search` | Ranked results — title, URL, snippet — from a search backend you choose. |
| `fetch` | `web-search.fetch` | One URL in, readable text out. Navigation, scripts, and cookie banners removed. |

Both are also mounted over HTTP by the host, at
`GET /api/plugins/web-search/http/search` and
`GET /api/plugins/web-search/http/fetch`.

This plugin makes **outbound requests from your machine, in your name**. The
[Blast radius](#blast-radius) section says exactly what it will and will not do,
and every guard there is on by default.

---

## Pick a backend

`search` needs one. `fetch` needs none and works either way.

### SearXNG — self-hosted, nothing leaves your control

The option for people who run their own compute and would rather not hand their
queries to a third party. Point the plugin at your own
[SearXNG](https://docs.searxng.org/) instance:

```toml
[[plugin]]
name = "web-search"
url = "http://searxng.internal:8080"
```

**Prerequisite, and it is not optional.** SearXNG serves HTML only unless JSON
is enabled. In your instance's `settings.yml`:

```yaml
search:
  formats:
    - html
    - json
```

Restart SearXNG, then confirm it from a shell:

```bash
curl -s 'http://searxng.internal:8080/search?q=test&format=json' | head -c 200
```

If that prints HTML, the plugin will report exactly that and name the setting.
Public SearXNG instances almost universally leave JSON off, so this backend is
for an instance you run.

If your instance also has `server.limiter` enabled, allow the plugin's address
or you will get `403`s.

### Brave Search API — hosted, key-based

```bash
# in the environment of the tdcc process
export TDCC_WEB_SEARCH_BRAVE_API_KEY='<your key>'
```

```toml
[[plugin]]
name = "web-search"
```

Get a key from [Brave Search API](https://brave.com/search/api/). Queries leave
your machine and reach Brave; that is the trade for not running an index.

### Which one gets used

The plugin infers when the answer is unambiguous, and refuses to guess when it
is not:

| What is configured | Backend |
| --- | --- |
| Only a Brave key | `brave` |
| Only a SearXNG URL | `searxng` |
| Both | error — set `--backend` explicitly |
| Neither | `search` is unavailable; `fetch` still works |

Set it outright with `--backend brave` or `--backend searxng` in
`[[plugin]].args`.

A missing backend is **not** a startup failure. The plugin starts, prints the
reason once to stderr, keeps `fetch` working, and returns that same message —
naming the missing setting — from every `search` call.

---

## Configuration

There is no `[plugin.settings]` block for this plugin, and that is deliberate.

`[plugin.settings]` values are stored by the host and rendered by the console,
but they are **never delivered to the plugin process** — there is no settings
field in the launch contract or the initialize handshake, and only a web UI
bundle can read them back. This plugin ships no web UI, so declaring a config
schema would draw console controls whose values could not affect a single
request. Everything therefore comes from the two channels a plugin process can
actually receive: `[[plugin]].args` and the environment of the `tdcc` process.

**The API key is environment-only, on purpose.** `args` is written into
`~/.tdcc/config.toml` and echoed back by `tdcc plugins info`. A credential
belongs in neither.

| Setting | `[[plugin]].args` | Environment | Default |
| --- | --- | --- | --- |
| Backend selection | `--backend brave\|searxng` | `TDCC_WEB_SEARCH_BACKEND` | inferred |
| Brave API key | — *(environment only)* | `TDCC_WEB_SEARCH_BRAVE_API_KEY` | — |
| Brave endpoint | `--brave-endpoint <url>` | `TDCC_WEB_SEARCH_BRAVE_ENDPOINT` | Brave's public API |
| SearXNG base URL | `--searxng-url <url>` | `TDCC_WEB_SEARCH_SEARXNG_URL` | `[[plugin]].url` |
| Default result count | `--results <1-20>` | `TDCC_WEB_SEARCH_RESULTS` | `8` |
| Request timeout | `--timeout-secs <1-600>` | `TDCC_WEB_SEARCH_TIMEOUT_SECS` | `20` |
| Max bytes per fetch | `--max-fetch-bytes <n>` | `TDCC_WEB_SEARCH_MAX_FETCH_BYTES` | `2000000` |
| Max characters returned | `--max-text-chars <n>` | `TDCC_WEB_SEARCH_MAX_TEXT_CHARS` | `40000` |
| Redirect budget | `--max-redirects <0-10>` | `TDCC_WEB_SEARCH_MAX_REDIRECTS` | `5` |
| Contact in `User-Agent` | `--contact <email or url>` | `TDCC_WEB_SEARCH_CONTACT` | none |
| Stop honouring robots.txt | `--ignore-robots` | `TDCC_WEB_SEARCH_RESPECT_ROBOTS=false` | honoured |
| Allow private addresses | `--allow-private-network` | `TDCC_WEB_SEARCH_ALLOW_PRIVATE_NETWORK=true` | refused |

`args` wins over the environment, which wins over `[[plugin]].url`.

An unrecognised flag or an out-of-range number is a **hard startup error**, not
a warning. A typo in `--allow-private-network` that was quietly ignored would
leave you believing a guard was off when it was on, or worse.

A fuller example:

```toml
version = 1

[[plugin]]
name = "web-search"
enabled = true
url = "http://searxng.internal:8080"
args = ["--results", "10", "--contact", "ops@example.org", "--max-text-chars", "20000"]
```

---

## Using the tools

`search`:

```jsonc
{ "query": "cargo workspace inheritance", "site": "doc.rust-lang.org", "count": 5 }
```

```jsonc
{
  "backend": "searxng",
  "query": "cargo workspace inheritance site:doc.rust-lang.org",
  "count": 5,
  "results": [
    { "rank": 1, "title": "…", "url": "https://…", "snippet": "…" }
  ]
}
```

`site` takes a bare hostname. Duplicate URLs — routine from a metasearch
backend aggregating several engines — are collapsed, so a model does not pay
context for three copies of one page. Zero results is an honest empty list, not
an error.

`fetch`:

```jsonc
{ "url": "https://doc.rust-lang.org/cargo/reference/workspaces.html", "max_chars": 8000 }
```

```jsonc
{
  "url": "https://doc.rust-lang.org/cargo/reference/workspaces.html",
  "final_url": "https://doc.rust-lang.org/cargo/reference/workspaces.html",
  "status": 200,
  "content_type": "text/html",
  "title": "Workspaces - The Cargo Book",
  "text": "# Workspaces\n\nA *workspace* is a collection of one or more packages…",
  "chars": 7984,
  "truncated": true,
  "redirects": 0,
  "robots_checked": true
}
```

The `text` is lightly structured rather than raw HTML: headings keep `#`
markers, list items keep a bullet, block elements keep their line breaks, link
text is kept while the URL is dropped, and `<pre>` blocks keep their
whitespace. Scripts, styles, `<nav>`, `<aside>`, `<footer>`, and containers
whose `class`/`id` reads like chrome (`cookie-banner`, `sidebar`, `navbar`,
`newsletter`, …) are removed with their contents. When the page marks its
content with `<main>` or `<article>`, only that region is rendered — unless
doing so would return almost nothing, in which case the whole body is used.

Failure is always reported, never returned as an empty success: an unreachable
backend, a rejected key, a `robots.txt` refusal, a `404`, a PDF, or a body over
the size cap each come back as an error naming the cause.

---

## Blast radius

This runs on someone's own hardware and reaches the internet from their
address. Every guard below is on by default and has to be turned off
deliberately.

**Network.** Outbound HTTPS/HTTP only, to the search backend you configured and
to URLs passed to `fetch`. No listener is opened — the host owns HTTP and MCP.

**Private addresses are refused.** Before each request the hostname is resolved
and the answer is checked. Loopback, RFC 1918, link-local (including the
`169.254.169.254` cloud metadata endpoint), unique-local IPv6, carrier-grade
NAT, and IPv4-mapped IPv6 forms of all of those are refused, so a model cannot
talk to `127.0.0.1:9337` or your internal services through this tool.
`--allow-private-network` opts out; the SearXNG base URL is exempt because it
comes from your configuration, not from a model.

This is a guard, not a sandbox. The connection re-resolves the name, so a DNS
answer that changes between the check and the connection (DNS rebinding) can
still get through. It stops the ordinary cases, which is what it is for.

**Redirects are followed by hand**, and the address guard and `robots.txt`
check run again at every hop — a permitted URL that redirects into private
space is not followed just because the first hop passed.

**robots.txt is honoured**, per RFC 9309: longest-match wins, `Allow` breaks a
tie, `*` and `$` work, a group naming `tdcc-web-search` overrides the `*` group,
and redirects to the `robots.txt` itself are followed. A `404` means no rules
were published and the fetch proceeds; a `5xx` or an unreadable `robots.txt`
means the fetch is refused rather than assuming consent. Rules are cached per
origin for an hour.

**The `User-Agent` is truthful** — it names this software and its version, and
you can append a contact:

```text
tdcc-web-search/0.1.0 (+https://github.com/the-decentralized-compute-company/tdcc-plugins) contact/ops@example.org
```

**Responses are capped and timed out.** 2 MB per fetch and a 20 s timeout by
default; an oversized body is refused rather than silently truncated. Only
HTML, plain text, XML, and JSON are accepted — a PDF or an image is refused by
name rather than returned as lossily-decoded bytes.

**Filesystem and subprocesses:** none. This plugin reads no files, writes no
files, and spawns nothing.

**Secrets:** no key is committed, logged, or printed. `SearchBackend`'s `Debug`
implementation is hand-written to redact the key so an accidental `{:?}` cannot
leak it, and transport errors are scrubbed of it before they are returned.

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
    plugins/web-search/
```

If your checkout is laid out differently, change that one line to point at your
`crates/tdcc-plugin`, or add a `[patch]` section instead of editing the
dependency:

```toml
[patch.crates-io]
tdcc-plugin = { path = "/absolute/path/to/tdcc-mesh/crates/tdcc-plugin" }
```

**Once the SDK is published**, a public consumer replaces the path dependency
with a version pin matching the `tdcc` release they target:

```toml
tdcc-plugin = "0.72.1"
```

Nothing else changes — no code in this plugin depends on the dependency being
local. Pin an exact version: the initialize handshake requires an exact
protocol-version match, so a host and a plugin built against mismatched
protocol versions refuse to connect at startup rather than misbehaving later.

```bash
cargo build --release
```

The first build downloads a vendored `protoc` for `tdcc-plugin`'s `prost-build`
step; no system protobuf compiler is needed. TLS is rustls with bundled roots,
so no OpenSSL headers are needed either.

---

## Tests

```bash
cargo test
```

88 tests, no network required. The pure logic — configuration precedence and
its error messages, `robots.txt` parsing and matching, the private-address
guard, HTML-to-text extraction, entity decoding, truncation, both backends'
request URLs and response shapes — is covered directly. The request path is
covered end to end against a stub HTTP server on loopback, which is what
exercises redirect following, the size cap, the media-type refusal, the
`robots.txt` fetch, and the disallow path deterministically.

One test is ignored by default because it makes real outbound requests:

```bash
cargo test -- --ignored --nocapture
```

It fetches two live pages and prints the extracted text. Run it after changing
anything in the fetch path — the unit tests prove the extractor's rules, and
this proves the rules still produce good output on real templates.

---

## Package and install locally

The archive needs one top-level directory named after the plugin, containing
`plugin.toml` and an executable named exactly `web-search` (`web-search.exe` on
Windows). This plugin declares neither a config schema nor a web UI, so its
`plugin-manifest.json` is `{}` and may be left out; `--print-package-manifest`
prints it if you want to include it anyway.

macOS and Linux:

```bash
rm -rf target/package
mkdir -p target/package/web-search
cp target/release/web-search target/package/web-search/web-search
cp plugin.toml README.md target/package/web-search/
tar -C target/package -czf target/web-search-0.1.0-local.tar.gz web-search

tdcc plugins install --archive ./target/web-search-0.1.0-local.tar.gz \
  --name web-search --version 0.1.0
tdcc plugins info web-search
```

Windows:

```powershell
Remove-Item -Recurse -Force target\package -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force target\package\web-search | Out-Null
Copy-Item target\release\web-search.exe target\package\web-search\web-search.exe
Copy-Item plugin.toml, README.md target\package\web-search\
Compress-Archive -Path target\package\web-search `
  -DestinationPath target\web-search-0.1.0-local.zip -Force

tdcc plugins install --archive .\target\web-search-0.1.0-local.zip `
  --name web-search --version 0.1.0
```

Set `TDCC_PLUGIN_DIR` to an empty directory first if you do not want an
in-development build landing in your real plugin store.

Then enable it and start the node:

```bash
tdcc client --port 9337 --console 3131 --config ./config.toml
```

```bash
curl --fail -X POST http://127.0.0.1:3131/api/plugins/web-search/tools/fetch \
  -H 'Content-Type: application/json' \
  -d '{"url":"https://example.com/"}'
```

Running the binary directly, outside a host, fails immediately with
`TDCC_PLUGIN_ENDPOINT is not set for plugin process`. That is correct — the host
owns the control endpoint and passes it in through the launch contract.

---

## License

Apache-2.0.
