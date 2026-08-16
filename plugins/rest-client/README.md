# rest-client

Let a model on your mesh call an API **you** declared — and nothing else.

Four MCP tools, projected by the host:

| Tool | On the MCP endpoint | What it does |
| --- | --- | --- |
| `endpoints` | `rest-client.endpoints` | Every API this node can reach, with its operations and their parameters. |
| `describe` | `rest-client.describe` | One operation in full, including a JSON Schema for its parameters. |
| `call` | `rest-client.call` | Invoke one declared operation with declared parameters. |
| `status` | `rest-client.status` | Configuration and credential state. Makes no network requests. |

Three of them are also mounted over HTTP by the host, at
`GET /api/plugins/rest-client/http/status`,
`GET /api/plugins/rest-client/http/endpoints`, and
`POST /api/plugins/rest-client/http/call`.

This plugin makes **outbound requests from your machine, with your
credentials**. The [Blast radius](#blast-radius) section says exactly what it
will and will not do, and every guard there is on by default.

---

## The whole point: the model never supplies a URL

An HTTP tool that takes a URL is a server-side request forgery primitive handed
to a language model. It can reach `http://127.0.0.1:9337/v1/models`, your
router's admin page, `http://169.254.169.254/latest/meta-data/`, and every
service on your LAN that trusted its network position instead of a password.
Prompt content decides where the request goes.

So there is no URL argument here. You write a file naming the APIs this node may
call, which methods and paths are permitted on each, which parameters each
operation takes, and where its credential comes from. A model then calls:

```jsonc
{ "endpoint": "github", "operation": "list_issues",
  "params": { "owner": "rust-lang", "repo": "rust", "state": "open" } }
```

`endpoint` and `operation` are names from your file. `params` are values for
parameters you declared. There is no fifth field — `CallArgs` is
`deny_unknown_fields`, so there is nowhere for a header, a method, a timeout, or
a host to land. That is the security model, and it is a type, not a convention.

---

## Declaring endpoints

The declaration is a TOML file at `~/.tdcc/rest-client.toml`
(`%USERPROFILE%\.tdcc\rest-client.toml` on Windows), or wherever `--config`
points.

### `[[endpoint]]`

| Key | Required | Meaning |
| --- | --- | --- |
| `name` | yes | The name a model uses. Letters, digits, `_`, `-`; at most 64 characters; unique. |
| `description` | yes | What this API is. A model reads it. |
| `base_url` | yes | Scheme, host, port, and optional path prefix. No query, no fragment, no `user:password@`. |
| `methods` | yes | Allowed methods, from `GET`, `HEAD`, `POST`, `PUT`, `PATCH`, `DELETE`. |
| `paths` | yes | Path patterns, relative to the `base_url` path. Every request is checked against these. |
| `headers` | no | Static headers sent on every request to this endpoint. |
| `timeout_secs` | no | 1–120. Default `20`. |
| `max_response_bytes` | no | 1024–8388608. Default `262144`. |
| `max_request_bytes` | no | 64–1048576. Default `65536`. |
| `max_calls_per_minute` | no | 1–6000. Default `60`. |
| `allow_private_base` | no | Default `false`. See [Blast radius](#blast-radius). |
| `allow_insecure_auth` | no | Default `false`. Required to send a credential over cleartext `http`. |

Path patterns use two wildcards: `*` matches any run of characters inside one
segment, and a whole segment of `**` matches any number of segments.

```toml
paths = ["/repos/*/*/issues", "/v1/models*", "/files/**"]
```

At most 32 endpoints, 64 operations each, 32 parameters each, 64 path patterns,
16 static headers. `Authorization`, `Cookie`, `Content-Type`, `Host`, and the
rest of the framing headers are refused in `headers` — credentials go in
`[endpoint.auth]` and a request content type goes on the operation's body.

### `[[endpoint.operation]]`

| Key | Required | Meaning |
| --- | --- | --- |
| `name` | yes | The name a model passes as `operation`. |
| `description` | yes | What this operation does. A model reads it. |
| `method` | yes | Must be in the endpoint's `methods`. |
| `path` | yes | A template such as `/repos/{owner}/{repo}/issues`, relative to the base URL path. |

### `[[endpoint.operation.parameter]]`

The OpenAPI-ish part. Each parameter becomes a property in the JSON Schema
`describe` returns and a line in the `call` tool's description.

| Key | Required | Meaning |
| --- | --- | --- |
| `name` | yes | Matches a `{placeholder}` for a path parameter. |
| `in` | yes | `path` or `query`. There is no `header` — see [Limitations](#limitations). |
| `type` | yes | `string`, `integer`, `number`, or `boolean`. |
| `description` | yes | Written for a model. An empty one fails to load. |
| `required` | no | Default `false`. Path parameters must set it to `true`. |
| `enum` | no | Allowed values. Anything else is refused before the request is built. |
| `default` | no | Used when the caller omits the parameter. Optional parameters only. |
| `min_length`, `max_length` | no | Strings only. |
| `minimum`, `maximum` | no | Numbers only, inclusive. |

### `[endpoint.operation.body]`

Declare this and the operation accepts a JSON `body`; omit it and supplying one
is an error.

| Key | Required | Meaning |
| --- | --- | --- |
| `description` | yes | What the body should contain. |
| `required` | no | Default `false`. |
| `content_type` | no | Default `application/json`. |

Only `POST`, `PUT`, `PATCH`, and `DELETE` may declare a body.

### A worked example

Read-only GitHub, two operations, one credential:

<!-- example:declaration -->
```toml
version = 1

[[endpoint]]
name = "github"
description = "GitHub REST API v3, read-only repository and issue queries."
base_url = "https://api.github.com"
methods = ["GET"]
paths = ["/repos/*/*", "/repos/*/*/issues"]
timeout_secs = 15
max_calls_per_minute = 30

[endpoint.auth]
kind = "bearer"
token_env = "TDCC_REST_CLIENT_GITHUB_TOKEN"

[endpoint.headers]
Accept = "application/vnd.github+json"
X-GitHub-Api-Version = "2022-11-28"

[[endpoint.operation]]
name = "get_repo"
description = "Fetch a repository's metadata: description, stars, default branch, topics."
method = "GET"
path = "/repos/{owner}/{repo}"

[[endpoint.operation.parameter]]
name = "owner"
in = "path"
type = "string"
required = true
description = "Account or organisation that owns the repository, for example `rust-lang`."

[[endpoint.operation.parameter]]
name = "repo"
in = "path"
type = "string"
required = true
description = "Repository name without the owner, for example `rust`."

[[endpoint.operation]]
name = "list_issues"
description = "List issues in a repository, newest first."
method = "GET"
path = "/repos/{owner}/{repo}/issues"

[[endpoint.operation.parameter]]
name = "owner"
in = "path"
type = "string"
required = true
description = "Account or organisation that owns the repository."

[[endpoint.operation.parameter]]
name = "repo"
in = "path"
type = "string"
required = true
description = "Repository name without the owner."

[[endpoint.operation.parameter]]
name = "state"
in = "query"
type = "string"
description = "Which issues to include."
enum = ["open", "closed", "all"]
default = "open"

[[endpoint.operation.parameter]]
name = "per_page"
in = "query"
type = "integer"
description = "How many issues to return in one page."
minimum = 1
maximum = 100
default = 30
```

**TOML ordering catches people out.** `[endpoint.auth]` and
`[endpoint.headers]` are tables on the endpoint, so they must appear *before*
the first `[[endpoint.operation]]`. Once an array-of-tables entry has started,
every later table belongs to it.

That declaration produces this, appended to the `call` tool's description, which
is what a model reads before it calls anything:

<!-- example:rendered -->
```text

github: GitHub REST API v3, read-only repository and issue queries.
  github.get_repo — GET /repos/{owner}/{repo}
      Fetch a repository's metadata: description, stars, default branch, topics.
      - owner (path, string, required) Account or organisation that owns the repository, for example `rust-lang`.
      - repo (path, string, required) Repository name without the owner, for example `rust`.
  github.list_issues — GET /repos/{owner}/{repo}/issues
      List issues in a repository, newest first.
      - owner (path, string, required) Account or organisation that owns the repository.
      - repo (path, string, required) Repository name without the owner.
      - state (query, string, optional, default open, one of: open, closed, all) Which issues to include.
      - per_page (query, integer, optional, default 30, 1–100) How many issues to return in one page.
```

A file that does not parse is a **startup failure** — the plugin refuses to
start and the host reports it, rather than coming up with an empty catalog and
leaving you to find out later that a restriction you wrote is not being applied.
A file that is simply *absent* is not a failure: the plugin starts inert, and
every tool says where the file was expected.

---

## Auth

Credentials are configuration and never a model argument. Each endpoint's
`[endpoint.auth]` table names an **environment variable**; the value is read
once, from the environment of the `tdcc` process, at startup.

| `kind` | Other keys | What is sent |
| --- | --- | --- |
| `none` | — | Nothing. This is the default when the table is absent. |
| `bearer` | `token_env` | `Authorization: Bearer <value>` |
| `basic` | `username`, `password_env` | `Authorization: Basic <base64(username:value)>` |
| `header` | `header`, `value_env` | `<header>: <value>` |
| `query` | `param`, `value_env` | `?<param>=<value>` |

```toml
[endpoint.auth]
kind = "header"
header = "X-Api-Key"
value_env = "TDCC_REST_CLIENT_WEATHER_KEY"
```

```bash
# in the environment of the tdcc process
export TDCC_REST_CLIENT_WEATHER_KEY='<your key>'
```

Four things this plugin does about credentials, each of them tested:

- **`token_env` takes a variable name, and a value is rejected.** The field must
  be a valid shell variable name — letters, digits, and `_`, not starting with a
  digit — which already refuses most credential shapes (`sk-…`, `xoxb-…`, a
  JWT's dots). A prefix check catches the ones that would slip through, so
  pasting `ghp_…` or `AKIA…` into this file fails at load rather than being
  committed to something you later share.
- **A credential never has a `Display`, a `Serialize`, or a derived `Debug`.**
  It lives in a `Secret` whose `Debug` prints `<redacted>`, so an accidental
  `{:?}` in a log line or a panic message cannot leak it.
- **Every string on the way out is redacted.** Tool results, error messages, and
  the URL echoed back all pass through a redactor built from the resolved
  values. When an API rejects a request and quotes the token back in the error
  body, the token does not reach the model.
- **A `query` credential is stripped from the reported URL** by rebuilding the
  query pair by pair, not by string replacement — so a different encoding cannot
  defeat the redaction.
- **A credential containing a control character disables its endpoint**, with a
  message naming the variable and not quoting the value. A `\r\n` inside a
  header value is the shape of a header-injection attempt, and an operator is
  better served by that diagnostic than by a transport error three layers down.

A missing variable disables **that endpoint only**. The plugin still starts,
`status` reports `auth_ready: false` and names the variable, and a `call` to it
fails with the same message. Other endpoints are unaffected.

**Cleartext plus a credential is refused.** An endpoint with auth and an `http`
base URL fails to load unless you set `allow_insecure_auth = true` on it. That
exists for a service on your own machine; it is not a general escape hatch.

---

## Using the tools

### `endpoints`

Start here. No arguments, no network.

```jsonc
{
  "count": 1,
  "endpoints": [
    {
      "name": "github",
      "description": "GitHub REST API v3, read-only repository and issue queries.",
      "base_url": "https://api.github.com",
      "methods": ["GET"],
      "allowed_paths": ["/repos/*/*", "/repos/*/*/issues"],
      "auth": { "kind": "bearer", "env": "TDCC_REST_CLIENT_GITHUB_TOKEN", "ready": true },
      "limits": { "timeout_secs": 15, "max_response_bytes": 262144,
                  "max_request_bytes": 65536, "max_calls_per_minute": 30 },
      "operations": [ { "name": "get_repo", "call_as": "github.get_repo", "method": "GET", "…": "…" } ]
    }
  ],
  "note": "Call one of these with `rest-client.call`, naming the endpoint and the operation. There is no way to pass a URL."
}
```

The auth block reports the variable **name** and whether it was present at
startup. It never reports a value.

### `describe`

```jsonc
{ "endpoint": "github", "operation": "list_issues" }
```

```jsonc
{
  "endpoint": "github",
  "operation": "list_issues",
  "call_as": "github.list_issues",
  "method": "GET",
  "path": "/repos/{owner}/{repo}/issues",
  "params_schema": {
    "type": "object",
    "properties": {
      "owner": { "type": "string", "description": "…", "x-in": "path" },
      "repo":  { "type": "string", "description": "…", "x-in": "path" },
      "state": { "type": "string", "description": "Which issues to include.",
                 "enum": ["open", "closed", "all"], "default": "open", "x-in": "query" },
      "per_page": { "type": "integer", "description": "…",
                    "minimum": 1.0, "maximum": 100.0, "default": 30, "x-in": "query" }
    },
    "required": ["owner", "repo"],
    "additionalProperties": false
  },
  "body": null,
  "path_parameters": ["owner", "repo"],
  "query_parameters": ["state", "per_page"]
}
```

`x-in` is not JSON Schema; it is the one thing a schema cannot say and a caller
needs to know, because a value bound for the path has stricter rules than one
bound for the query string.

### `call`

```jsonc
{ "endpoint": "github", "operation": "list_issues",
  "params": { "owner": "rust-lang", "repo": "rust", "state": "open" } }
```

```jsonc
{
  "endpoint": "github",
  "operation": "list_issues",
  "method": "GET",
  "url": "https://api.github.com/repos/rust-lang/rust/issues?state=open&per_page=30",
  "status": 200,
  "content_type": "application/json",
  "json": [ /* the API's own response */ ],
  "bytes": 48213,
  "truncated": false,
  "duration_ms": 214,
  "response_headers": { "content-type": "application/json; charset=utf-8", "etag": "\"…\"" },
  "budget": { "used_this_minute": 1, "max_calls_per_minute": 30 }
}
```

A JSON body is parsed into `json`. Other textual types come back in `text`. A
type that is not text at all — an image, an archive — is reported by name with
its status and size, rather than returned as lossily-decoded bytes.

### Failure

Every failure is an error naming its cause, never an empty success:

| What happened | What comes back |
| --- | --- |
| Endpoint or operation not declared | An error listing what *is* declared |
| A parameter is missing, mistyped, out of range, or outside its `enum` | An error naming the parameter and the constraint |
| A parameter would change the shape of the path | An error; the request is not sent |
| The credential's variable is unset | An error naming the variable |
| The per-minute budget is spent | An error saying how many seconds until the window resets |
| The base URL resolves into private space | An error naming `allow_private_base` |
| The request times out or cannot connect | An error naming which |
| The API answered 4xx or 5xx | An error carrying the status — see below |

**A non-2xx response keeps its status code**, because "the resource does not
exist" and "the API is broken" call for different reactions:

```text
github.get_repo answered 404 Not Found. The endpoint answered, so the request
was permitted; the resource itself does not exist. Response body: {"message":"Not Found"}
```

and, in the error's structured `data`:

```jsonc
{ "endpoint": "github", "operation": "get_repo", "method": "GET",
  "url": "https://api.github.com/repos/rust-lang/nope",
  "status": 404, "reason": "Not Found", "retryable": false,
  "body_excerpt": "{\"message\":\"Not Found\"}", "duration_ms": 96 }
```

`retryable` is true for `408`, `425`, `429`, `500`, `502`, `503`, and `504`.
`401` and `403` add a line pointing at the credential rather than at the
request.

### `status`

The tool to call when everything else is failing. Reports the declaration path,
whether it loaded, and per endpoint the auth kind, the variable name, whether
that variable was present, and how much of the call budget is spent. Makes no
network requests, so it answers even when nothing else does.

---

## Why the schema is in the description

The SDK builds a tool's `inputSchema` from a Rust type at compile time. A plugin
cannot hand the host a schema it computed at startup, so `call`'s `inputSchema`
is fixed: `endpoint`, `operation`, `params` (an object), `body`. It cannot grow
a per-operation shape.

Three things close that gap, and all three are generated from the same
declaration:

1. **The `call` tool's description is built at startup** and carries every
   declared operation — its signature, and each parameter's location, type,
   required-ness, default, `enum`, range, and your own words. Descriptions are
   runtime strings, so this part *can* vary per node. It is capped at 6000
   characters and, past that, points at the other tools rather than silently
   dropping half your catalog.
2. **`describe` returns a real JSON Schema object** per operation.
3. **`params` is validated against the same declaration**, and a wrong call gets
   an error naming the parameter and the constraint rather than a generic
   refusal.

A model that reads `call`'s description already knows the signature; one that
wants the machine-readable form calls `describe`.

---

## Configuration

There is no `[plugin.settings]` block for this plugin, and that is deliberate.

`[plugin.settings]` values are stored by the host and rendered by the console,
but they are **never delivered to the plugin process** — there is no settings
field in the launch contract or the initialize handshake, and only a web UI
bundle can read them back. This plugin ships no web UI, so declaring a config
schema would draw console controls that could not affect a single request. And
an endpoint declaration is a document with nested tables, not a handful of
scalars; it belongs in a file you can diff and review.

| Setting | `[[plugin]].args` | Environment | Default |
| --- | --- | --- | --- |
| Declaration file | `--config <path>` | `TDCC_REST_CLIENT_CONFIG` | `$HOME/.tdcc/rest-client.toml` |
| Contact in `User-Agent` | `--contact <email or url>` | `TDCC_REST_CLIENT_CONTACT` | none |
| Every credential | — *(environment only)* | named by each `[endpoint.auth]` | — |

Arguments win over the environment. An unrecognised flag is a **hard startup
error**, not a warning: a typo in `--config` that was quietly ignored would
leave a node running against the wrong declaration.

**`[[plugin]].url` is deliberately not read.** Seven plugins in this repository
use it and mean four different things by it; here it could only mean "the one
API", which would quietly contradict a file that declares several. Setting it
changes nothing.

```toml
version = 1

[[plugin]]
name = "rest-client"
enabled = true
args = ["--config", "/etc/tdcc/rest-client.toml", "--contact", "ops@example.org"]
```

---

## Blast radius

This runs on someone's own hardware, reaches the internet from their address,
and carries their credentials. Every guard below is on by default.

**Network.** Outbound HTTPS/HTTP only, to the endpoints declared in your file
and no others. No listener is opened — the host owns HTTP and MCP.

**The destination is never caller-controlled.** Scheme, host, port, path shape,
and method all come from the declaration. A caller supplies an endpoint name, an
operation name, parameter values, and optionally a JSON body. `CallArgs` is
`deny_unknown_fields`, so those four are the entire surface.

**Path parameters are confined twice.** Before the URL exists, a value is
refused if it is empty, `.`, `..`, or contains `/` or `\` or a control
character, and is then percent-encoded down to the RFC 3986 unreserved set — so
`?`, `#`, `&`, `:`, `@`, and `%` are all encoded and cannot start a query
string, add userinfo, or smuggle a second escape. After the URL exists, the
assembled `Url` is checked again: same scheme, host, and port as the base; path
still under the base path; no dot segments; method in the endpoint's list; and
the path relative to the base matching one of the endpoint's `paths` patterns.
The second check reads the finished `Url`, so it does not depend on the first
one being right.

**Private addresses are refused.** Before each call the base URL's host is
resolved and the answer is checked. Loopback, RFC 1918, link-local (including
the `169.254.169.254` cloud metadata endpoint), unique-local IPv6,
carrier-grade NAT, IETF protocol assignments, benchmarking and reserved space,
and IPv4-mapped IPv6 forms of all of those are refused. This is the same guard
`web-search` uses, and it re-resolves on every call rather than trusting a
startup check. `allow_private_base = true` opts one endpoint in, for a LAN
service you meant to declare.

This is a guard, not a sandbox. The connection re-resolves the name, so a DNS
answer that changes between the check and the connection (DNS rebinding) can
still get through. It stops the ordinary cases, which is what it is for.

**Redirects are never followed.** A client that chases a `Location` header is a
client whose destination is chosen by the server it just talked to, which is the
property this plugin exists to remove. A 3xx comes back as a status with
`location` among the reported headers.

**Everything a caller can grow is bounded.** Response bytes, request bytes,
per-request timeout, path length, and calls per minute are all per-endpoint with
ceilings an operator cannot raise past. The rate limiter's state is one window
per declared endpoint, seeded at startup and never inserted into. Response
headers are reported from a 12-entry allowlist — no `Set-Cookie` — each capped
at 512 characters.

**Filesystem:** one file read, once, at startup: the declaration. Nothing is
written. **Subprocesses:** none.

**Secrets:** covered in [Auth](#auth) above. Nothing key-shaped may appear in
the declaration file or in `[[plugin]].args`.

---

## Limitations

Stated here rather than left for you to find.

- **The address guard is not a sandbox.** DNS rebinding between the check and
  the connection can still reach a private address. Declaring an endpoint whose
  hostname you do not control is trusting whoever answers for that name.
- **There is no `header` parameter location.** A caller-controlled header is how
  request smuggling and credential overwriting start, and no declared API needs
  one badly enough. Static headers are yours to set in `[endpoint.headers]`.
- **There is no regex constraint on parameters.** `enum`, length, and numeric
  range only. If a value needs a stricter shape than that, express it as an
  `enum`, or accept that the remote API will reject it.
- **`call`'s `inputSchema` cannot vary per node.** See
  [Why the schema is in the description](#why-the-schema-is-in-the-description).
- **The path allowlist is checked against the operation's template at load
  time** by substituting each path parameter's first `enum` value, or `_` when
  it has none. An allowlist entry that pins a literal segment where the
  operation takes a parameter will be reported as unreachable — express the pin
  as an `enum` on that parameter instead. The authoritative check runs at
  request time on the concrete path, and that one has no such gap.
- **Nothing here rate-limits across restarts.** The per-minute window lives in
  memory. A node restarting in a loop can exceed a remote API's limit.
- **A declared endpoint is trusted with what you gave it.** This plugin controls
  where a request goes; it cannot control what the API does with a `PUT` you
  allowed. Declare the narrowest set of methods and paths that does the job, and
  prefer read-only endpoints.

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
    plugins/rest-client/
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
protocol-version match, so a host and a plugin built against mismatched protocol
versions refuse to connect at startup rather than misbehaving later.

```bash
cargo build --release
```

The first build downloads a vendored `protoc` for `tdcc-plugin`'s `prost-build`
step; no system protobuf compiler is needed. TLS is rustls with bundled roots,
so no OpenSSL headers are needed either. There is no `base64` dependency: HTTP
Basic needs one 20-line encoder, and it lives in `src/auth.rs` beside the RFC
4648 vectors that test it.

---

## Tests

```bash
cargo test
```

136 tests, no outbound network required. The pure logic is covered directly:
declaration parsing and every rule it enforces, path-pattern matching, template
expansion and the values it refuses, parameter validation and rendering, JSON
Schema generation, the generated `call` description, credential resolution and
redaction, base64, and the call budget with an injected clock.

The call path is covered end to end against a stub HTTP server on loopback that
records what it was asked for. That is what proves the `Authorization` header is
actually sent, that a `query` credential reaches the wire but not the result,
that a body is posted with its content type, that a `404` and a `500` come back
differently, that an over-cap response is truncated and says so, and that a
refused call sends nothing at all.

One test reads this README: `the_readme_example_renders_exactly_what_the_readme_shows`
parses the declaration under [A worked example](#a-worked-example) and asserts
that it renders exactly the block printed beneath it, so the two cannot drift.

Not covered without a host: the initialize handshake, the host's own schema
validation of `CallArgs` before a handler is entered, and the HTTP route
projections. Those rest on the checklist in the repository's
[CONTRIBUTING.md](../../CONTRIBUTING.md).

---

## Package and install locally

The archive needs one top-level directory named after the plugin, containing
`plugin.toml` and an executable named exactly `rest-client` (`rest-client.exe`
on Windows). This plugin declares neither a config schema nor a web UI, so its
`plugin-manifest.json` is `{}` and may be left out; `--print-package-manifest`
prints it if you want to include it anyway.

macOS and Linux:

```bash
rm -rf target/package
mkdir -p target/package/rest-client
cp target/release/rest-client target/package/rest-client/rest-client
cp plugin.toml README.md target/package/rest-client/
tar -C target/package -czf target/rest-client-0.1.0-local.tar.gz rest-client

tdcc plugins install --archive ./target/rest-client-0.1.0-local.tar.gz \
  --name rest-client --version 0.1.0
tdcc plugins info rest-client
```

Windows:

```powershell
Remove-Item -Recurse -Force target\package -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force target\package\rest-client | Out-Null
Copy-Item target\release\rest-client.exe target\package\rest-client\rest-client.exe
Copy-Item plugin.toml, README.md target\package\rest-client\
Compress-Archive -Path target\package\rest-client `
  -DestinationPath target\rest-client-0.1.0-local.zip -Force

tdcc plugins install --archive .\target\rest-client-0.1.0-local.zip `
  --name rest-client --version 0.1.0
```

Set `TDCC_PLUGIN_DIR` to an empty directory first if you do not want an
in-development build landing in your real plugin store.

Then write `~/.tdcc/rest-client.toml`, export the credentials it names, enable
the plugin, and start the node:

```bash
tdcc client --port 9337 --console 3131 --config ./config.toml
```

```bash
curl --fail http://127.0.0.1:3131/api/plugins/rest-client/http/status
curl --fail -X POST http://127.0.0.1:3131/api/plugins/rest-client/http/call \
  -H 'Content-Type: application/json' \
  -d '{"endpoint":"github","operation":"get_repo","params":{"owner":"rust-lang","repo":"rust"}}'
```

Running the binary directly, outside a host, fails immediately with
`TDCC_PLUGIN_ENDPOINT is not set for plugin process`. That is correct — the host
owns the control endpoint and passes it in through the launch contract.
`--help` and `--print-package-manifest` work anywhere.

---

## License

Apache-2.0.
