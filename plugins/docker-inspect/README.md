# docker-inspect

Let a model on your mesh answer questions about what is running on this box.

Seven MCP tools, projected by the host:

| Tool | On the MCP endpoint | What it does |
| --- | --- | --- |
| `status` | `docker-inspect.status` | How this plugin is configured, and what it will show. Contacts nothing. |
| `daemon` | `docker-inspect.daemon` | Whether Docker answers, its version, and what it reports about the host. |
| `list_containers` | `docker-inspect.list_containers` | The containers you allowed, with state, ports, networks, and labels. |
| `inspect_container` | `docker-inspect.inspect_container` | One container in full: mounts, limits, health, privileges. |
| `container_logs` | `docker-inspect.container_logs` | A bounded tail of one container's logs, labelled stdout/stderr. |
| `container_stats` | `docker-inspect.container_stats` | One live sample: CPU, memory, network, block IO, processes. |
| `list_images` | `docker-inspect.list_images` | Local images, their size, and which of your containers use them. |

**Read this before installing.** Handing anything access to the Docker socket
is handing it root on the machine — see [What you are granting](#what-you-are-granting).
This plugin is built so that read-only is a property of the binary rather than
a promise, and so that you can restrict it to your own containers. Both are
explained below, and both are the point.

---

## What you are granting

The Docker daemon has no permission model. It is a single API, and anyone who
can reach it can create a container — which means bind-mounting `/` into that
container, which means being root on the host. That is true of `docker` itself,
of every Docker client library, and of any user you add to the `docker` group.

So the question worth asking before installing this is not "is it read-only",
it is "what could it become if it were wrong". Three things reduce that:

**1. The write verbs are not in the binary.**

No Docker client crate is linked. This plugin speaks the Engine API directly,
and there is exactly one function that writes to the socket
([`src/transport.rs`](src/transport.rs)), whose request line is:

```rust
format!("GET {} HTTP/1.1\r\n…", path.as_str(), …)
```

The method is a literal. There is no parameter for it and no second writer, so
`POST`, `PUT`, `PATCH`, and `DELETE` are absent from the compiled artifact
rather than merely uncalled. A test asserts on the bytes a request produces.

**2. The paths are an allowlist enforced by the module system.**

[`src/paths.rs`](src/paths.rs) owns an `ApiPath` newtype whose field is private
and whose constructors are the eight read paths this plugin uses. Nothing
elsewhere in the crate can build one, and no constructor takes a caller's
string.

**3. A caller's container reference never reaches the wire.**

A name or id from a tool call is matched against the containers the daemon
listed *after* your visibility filter has been applied, and the daemon's own
64-character id is what the next request uses — re-checked as hexadecimal
before it is spliced into a path. A hidden container cannot be reached by
naming it exactly, and a reference containing `/`, `?`, or `..` matches nothing
rather than redirecting a request.

What none of that changes: **this plugin still reads everything the socket can
see**, unless you restrict it. Container environments, host paths from bind
mounts, and log lines are all readable through it by design. Restrict it.

---

## Restrict what it can see

A node contributing hardware to a mesh usually wants to expose *its own*
services, not everything else on the machine. Two repeatable flags do that:

```toml
[[plugin]]
name = "docker-inspect"
args = ["--container", "tdcc-*", "--label", "com.example.expose=true"]
```

- `--container <pattern>` matches container names. `*` matches any run of
  characters; everything else is literal. Matching is **case sensitive**,
  because an allowlist that quietly matched more than it says would be the
  wrong kind of forgiving.
- `--label <key>` requires the label to be present; `--label <key>=<value>`
  requires that exact value.

The two are a **union**: a container is visible if it matches any pattern *or*
any label selector. Give neither and every container on the machine is visible
— and the plugin says so on stderr at startup.

The filter is applied before anything is reported, and there is no tool
argument that widens it. Every listing also reports `hidden_by_filter`, so a
caller can tell "nothing is running" from "nothing you may see is running"
without learning anything about what was hidden.

With a filter configured, `list_images` narrows too: it shows only the images
your visible containers use, so the image list does not become a catalogue of
everything else on the machine. `--all-images` turns that off.

---

## Logs hand secrets to a model

This is the part to think about hardest.

Applications print connection strings on startup, echo bearer tokens in debug
builds, and dump whole request headers when something fails. `container_logs`
takes those lines and puts them in a model's context, from where they go
wherever that conversation goes. There is no filter here that can fix that,
and this plugin does not pretend to have one.

What it does instead:

- **Caps the volume.** 100 lines by default (`--max-log-lines`, ceiling 5000),
  256 KiB per read (`--max-log-bytes`), and 2000 characters per line
  (`--max-line-chars`). A request for more lines than the cap is clamped, not
  refused, and the response reports both numbers.
- **Never follows.** One request, one bounded body. There is no code path that
  holds a log stream open.
- **Carries the warning with the data.** Every log response includes a
  `warning` field saying what these lines may contain, so the caveat survives
  being copied out of this README.
- **Can be turned off entirely.** `--no-logs` makes the tool refuse and name
  the flag; everything else keeps working.

If a container's logs must not be seen, do not make that container visible.

---

## Two more redactions, and their limits

**Environment variable values are hidden by default.** `inspect_container`
reports the *names* — which answers "is `DATABASE_URL` set at all" — and hides
the values behind `--show-env`. Container environments are where credentials
live.

```jsonc
"env": {
  "redacted": true,
  "count": 10,
  "names": ["MYSQL_USER", "MYSQL_PASSWORD", "MYSQL_ROOT_PASSWORD", "PATH", …],
  "note": "Values are hidden because container environments routinely hold credentials. …"
}
```

**Secret-shaped command arguments are masked**, always, and the response says
when it happened (`"command_redacted": true`). `--password=x`, `--token x`,
`KEY=value` where the key looks secret, and a `scheme://user:password@host` URL
are all caught.

**It will miss things.** The filter matches a short list of name fragments
(`password`, `token`, `secret`, `apikey`, `credential`, `private_key`,
`access_key`), and it cannot know that the third positional argument of a
bespoke binary is a token. It is a reduction in accidents, not a boundary. The
boundary is the visibility filter.

---

## Configuration

There is no `[plugin.settings]` block for this plugin, and that is deliberate.

`[plugin.settings]` values are stored by the host and rendered by the console,
but they are **never delivered to the plugin process** — there is no settings
field in the launch contract or the initialize handshake, and only a web UI
bundle can read them back. This plugin ships no web UI, and every setting it
has is a limit that must be enforced *inside* the process: which containers are
visible, how many log lines may leave the machine, whether a TCP endpoint may
be opened at all. A console control that looked authoritative and changed none
of that would be worse than no control.

Everything therefore comes from the two channels a plugin process can actually
receive: `[[plugin]].args` and the environment of the `tdcc` process.

| Setting | `[[plugin]].args` | Environment | Default |
| --- | --- | --- | --- |
| Docker endpoint | `--endpoint <value>` | `TDCC_DOCKER_INSPECT_ENDPOINT` | platform socket |
| Visible container names | `--container <pattern>` *(repeatable)* | `TDCC_DOCKER_INSPECT_CONTAINERS` *(comma separated)* | everything |
| Visible container labels | `--label <key>[=<value>]` *(repeatable)* | `TDCC_DOCKER_INSPECT_LABELS` *(comma separated)* | everything |
| Allow a TCP endpoint | `--allow-tcp` | `TDCC_DOCKER_INSPECT_ALLOW_TCP=true` | refused |
| Show environment values | `--show-env` | `TDCC_DOCKER_INSPECT_SHOW_ENV=true` | names only |
| Turn logs off | `--no-logs` | `TDCC_DOCKER_INSPECT_LOGS=false` | logs enabled |
| Log line cap | `--max-log-lines <1-5000>` | `TDCC_DOCKER_INSPECT_MAX_LOG_LINES` | `100` |
| Log byte cap | `--max-log-bytes <n>` | `TDCC_DOCKER_INSPECT_MAX_LOG_BYTES` | `262144` |
| Characters per log line | `--max-line-chars <80-20000>` | `TDCC_DOCKER_INSPECT_MAX_LINE_CHARS` | `2000` |
| Containers per listing | `--max-containers <1-1000>` | `TDCC_DOCKER_INSPECT_MAX_CONTAINERS` | `200` |
| Images per listing | `--max-images <1-1000>` | `TDCC_DOCKER_INSPECT_MAX_IMAGES` | `200` |
| Labels per container | `--max-labels <0-200>` | `TDCC_DOCKER_INSPECT_MAX_LABELS` | `32` |
| List every image | `--all-images` | `TDCC_DOCKER_INSPECT_ALL_IMAGES=true` | scoped to the filter |
| Response size cap | `--max-response-bytes <n>` | `TDCC_DOCKER_INSPECT_MAX_RESPONSE_BYTES` | `8388608` |
| Request timeout | `--timeout-secs <1-300>` | `TDCC_DOCKER_INSPECT_TIMEOUT_SECS` | `20` |
| Docker API version | `--api-version <vN.NN>` | `TDCC_DOCKER_INSPECT_API_VERSION` | `v1.41` |

**Endpoint precedence**, highest first: `--endpoint`, then
`TDCC_DOCKER_INSPECT_ENDPOINT`, then `[[plugin]].url`, then `DOCKER_HOST`, then
the platform default. `DOCKER_HOST` comes last on purpose — it is ambient and
may have been exported for something else entirely. There is a test for each
step of that order.

An unrecognised flag, an out-of-range number, or a malformed label selector is
a **hard startup error**, not a warning. A typo in `--container` that was
quietly ignored would widen an allowlist from "one service" to "every container
on this machine".

`--container` and `--label` may be repeated; every other flag may not, and
giving one twice is an error rather than a silent winner. Both `--flag value`
and `--flag=value` work.

### Endpoints

| Form | Notes |
| --- | --- |
| `unix:///var/run/docker.sock` | The default on macOS and Linux. A bare `/path` works too. |
| `npipe:////./pipe/docker_engine` | The default on Windows. `\\.\pipe\docker_engine` and `npipe://./pipe/…` are accepted and normalised. |
| `tcp://host:port` | Cleartext. **Requires `--allow-tcp`** — see below. `http://` is the same thing. |
| `https://…` | Refused. This binary links no TLS stack, so it can neither verify a certificate nor present a client one. |
| `ssh://…` | Refused. That form means running the `ssh` binary, and this plugin spawns no subprocesses. |

Rootless Docker (`unix:///run/user/<uid>/docker.sock`), Colima, and Rancher
Desktop all listen somewhere other than the default; point `--endpoint` at
whichever applies. Anything else speaking the Docker Engine API — Podman's
Docker-compatible socket, for instance — is the same wire protocol, but that
has not been tested here.

> **An unauthenticated TCP Docker endpoint is itself a serious
> misconfiguration.** `tcp://…` on port 2375 has no authentication of any kind:
> everyone who can reach that port can create containers on that machine, and
> anyone who can create a container can become root on it. That is true whether
> or not this plugin is involved. `--allow-tcp` exists so that enabling it is
> deliberate, and the plugin prints a warning on every start when it is set.
> Fix the endpoint; do not work around it.

---

## Using the tools

`list_containers`:

```jsonc
{ "all": true }
```

```jsonc
{
  "returned": 2,
  "matching": 2,
  "truncated": false,
  "hidden_by_filter": 3,
  "filter": "names matching tdcc-*",
  "includes_stopped": true,
  "results": [
    {
      "id": "9f3a1c2b7d40",
      "name": "tdcc-node",
      "all_names": ["tdcc-node"],
      "image": "ghcr.io/example/tdcc:0.72.1",
      "state": "running",
      "status": "Up 13 minutes",
      "created": "2026-08-16T15:53:26Z",
      "ports": [
        {
          "container_port": 9337,
          "protocol": "tcp",
          "host_port": 9337,
          "host_ip": "0.0.0.0",
          "published_to_all_interfaces": true
        }
      ],
      "networks": [{ "name": "bridge", "ip_address": "172.18.0.2" }],
      "labels": { "com.docker.compose.service": "node" },
      "labels_truncated": false
    }
    // … and one more, elided here
  ]
}
```

`published_to_all_interfaces` is called out because it is the difference
between a port reachable from this machine and one reachable from the network,
and it is the most common accidental exposure on a node somebody set up in a
hurry.

`inspect_container` adds mounts with their host paths, resource limits, health,
and a `security_notes` list naming anything that widens what the container can
do to its host:

```jsonc
{
  "security_notes": [
    "runs privileged: it has effectively full access to the host's devices and kernel interfaces",
    "mounts the Docker socket at /var/run/docker.sock: a process in this container can create further containers and is therefore root-equivalent on this host",
    "uses host networking: it shares this machine's network namespace, including loopback"
  ]
}
```

Those are read straight out of the inspect payload — privileged mode, a mounted
Docker socket, host network or PID namespace, added Linux capabilities, and
writable binds of sensitive host paths. **It is not a security audit.** It is
the handful of settings that change the blast radius of a container, stated so
that "what is running here" gets the answer that matters.

`container_logs`:

```jsonc
{ "container": "tdcc-node", "tail": 5, "timestamps": true }
```

```jsonc
{
  "container": { "id": "9f3a1c2b7d40", "name": "tdcc-node", "state": "running" },
  "lines": [
    { "stream": "stderr", "timestamp": "2026-08-16T16:06:36.626331079Z", "text": "…" }
  ],
  "returned_lines": 5,
  "tail_used": 5,
  "max_lines": 100,
  "dropped_older_lines": 0,
  "lines_cut_to_length": 0,
  "byte_cap_reached": false,
  "warning": "Container logs frequently contain credentials, tokens, personal data, …"
}
```

`container_stats` takes about a second, because the daemon needs two samples to
compute a CPU percentage:

```jsonc
{
  "container": { "id": "9f3a1c2b7d40", "name": "tdcc-node", "image": "…" },
  "cpu": { "percent": 94.42, "online_cpus": 12, "note": null },
  "memory": { "usage_bytes": 5817294848, "usage": "5.4 GiB", "limit": "58.9 GiB", "percent": 9.2 },
  "network": { "rx_bytes": 110285557, "tx_bytes": 854274469, "interfaces": 1 },
  "block_io": { "read_bytes": 135168, "write_bytes": 1433600 },
  "processes": { "current": 121, "limit": null }
}
```

`percent` is a share of one core times the number of cores, exactly as
`docker stats` reports it, so 200% means two cores saturated. It is `null`,
with a `note`, when the daemon does not report the system-wide counter the
calculation needs — Windows daemons never do. A zero would be a different
claim, and a model will repeat whichever it is given.

Failure is always an error, never an empty success: an unreachable socket, a
permission denial, a container that does not exist, a stopped container asked
for statistics, and a body over the size cap each come back as an error naming
the cause and, where one exists, the setting that changes it.

```text
docker-inspect could not reach the Docker daemon at unix:///var/run/docker.sock:
permission denied. The user running tdcc has no access to that socket. On Linux
that normally means adding the user to the `docker` group and restarting tdcc —
understand first that membership of that group is equivalent to root on this
machine, because anyone who can create a container can mount the host filesystem
into it. Point it somewhere else with `--endpoint <value>` in [[plugin]].args,
TDCC_DOCKER_INSPECT_ENDPOINT, [[plugin]].url, or DOCKER_HOST.
```

---

## Blast radius

**The Docker socket, read-only.** One local IPC connection per request — a Unix
socket, or a named pipe on Windows — carrying eight `GET` paths: `/_ping`,
`/version`, `/info`, `/containers/json`, `/containers/{id}/json`,
`/containers/{id}/logs`, `/containers/{id}/stats`, and `/images/json`. Nothing
else is reachable, for the three structural reasons in
[What you are granting](#what-you-are-granting).

**Network:** none, unless you configure a `tcp://` endpoint and pass
`--allow-tcp`. There is no TLS stack in the binary, so cleartext is the only
thing it *can* speak — which is enforced by the dependency graph rather than by
a comment. No listener is opened either: the host owns HTTP and MCP.

**Filesystem:** none. This plugin opens no files. It *reports* host paths that
containers have mounted, because that is what an operator is asking about, but
it never reads them.

**Subprocesses:** none. This is why `ssh://` endpoints are refused rather than
shelled out to.

**Memory:** every response is bounded before it is buffered — 8 MiB per API
response, 256 KiB per log read, and caps on containers, images, labels, lines,
and characters per line. A body past its cap stops the read and is reported as
truncated rather than growing without limit.

**What leaves the machine:** whatever a model does with the answers. Container
names, image names, host paths of bind mounts, network addresses, environment
variable names, and log lines all reach the caller. That is the plugin's
purpose, and the visibility filter is how you scope it.

---

## What this cannot do

Stated here rather than left to be discovered.

- **It only sees this machine.** There is no mesh channel and no peer
  awareness; it answers about the daemon it is pointed at and nothing else.
- **It cannot tell you whether a container is *doing* the right thing.** It
  reports configuration, state, and one resource sample. Whatever the
  application inside believes about itself is only in its logs.
- **`security_notes` is not an audit.** It names five specific settings from
  the inspect payload. A container can be dangerous in ways none of them cover.
- **The secret filter is best effort**, as described above. It will not catch a
  credential passed positionally, written into an image, or printed to stdout.
- **No streaming and no `follow`.** Logs are a bounded read. Events, `attach`,
  and `exec` are not implemented and are not going to be.
- **Image `Containers` counts are not requested.** Computing them makes the
  daemon walk every container's writable layer; the `used_by_visible_containers`
  field is derived locally from the container list instead.
- **`--api-version` is pinned to `v1.41`** so that a future API change cannot
  alter what this plugin sends. A daemon older than Docker 20.10 will refuse
  that; its own error names the maximum it supports, and the plugin's error
  repeats it and names the flag.
- **A Windows daemon reports no system-wide CPU counter**, so `cpu.percent` is
  `null` there rather than wrong.

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
    plugins/docker-inspect/
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
step, so no system protobuf compiler is needed. No TLS implementation appears
anywhere in `Cargo.lock` — no `rustls`, no `ring`, no `openssl-sys` — so no
OpenSSL headers are needed either, and the "cleartext only" statement above is
enforced by the dependency graph rather than by a comment.

---

## Tests

```bash
cargo test
```

140 tests, no Docker required. Covered directly: endpoint parsing in every form
`DOCKER_HOST` accepts and the refusals for `https` and `ssh`; configuration
precedence, ranges, and each error message; the visibility allowlist, its
wildcard matcher, and container reference resolution including the ambiguous
and malformed cases; the eight API paths and the rejection of a non-hexadecimal
id; the log demultiplexer against framed, TTY, partial, multi-byte, and
non-UTF-8 input; the stats arithmetic against captured cgroup v1, cgroup v2, and
Windows samples; environment and command-line redaction; label and line caps;
byte formatting and the calendar conversion.

The request path is covered end to end against a stub Docker daemon on
loopback, which is what exercises connecting, the exact `GET` this plugin
writes, `Content-Length`, chunked, and close-delimited framing, the size cap,
status handling, and the resolve-then-fetch sequence. Those last tests assert
on what the stub was *not* asked for as well: no inspect request follows a
reference to a hidden container, no stats request follows a stopped one, and
nothing at all is sent when `--no-logs` is set.

One test is ignored by default because it needs a running daemon:

```bash
cargo test -- --ignored --nocapture
```

It drives every tool against the Docker daemon on your machine over the
platform's own local transport and prints the results. That is what proves the
same code reaches a real daemon — the unit tests pin the behaviour, and this
proves the behaviour still fits what Docker actually sends.

---

## Package and install locally

The archive needs one top-level directory named after the plugin, containing
`plugin.toml` and an executable named exactly `docker-inspect`
(`docker-inspect.exe` on Windows). This plugin declares neither a config schema
nor a web UI, so its `plugin-manifest.json` is `{}` and may be left out;
`--print-package-manifest` prints it if you want to include it anyway.

macOS and Linux:

```bash
rm -rf target/package
mkdir -p target/package/docker-inspect
cp target/release/docker-inspect target/package/docker-inspect/docker-inspect
cp plugin.toml README.md target/package/docker-inspect/
tar -C target/package -czf target/docker-inspect-0.1.0-local.tar.gz docker-inspect

tdcc plugins install --archive ./target/docker-inspect-0.1.0-local.tar.gz \
  --name docker-inspect --version 0.1.0
tdcc plugins info docker-inspect
```

Windows:

```powershell
Remove-Item -Recurse -Force target\package -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force target\package\docker-inspect | Out-Null
Copy-Item target\release\docker-inspect.exe target\package\docker-inspect\docker-inspect.exe
Copy-Item plugin.toml, README.md target\package\docker-inspect\
Compress-Archive -Path target\package\docker-inspect `
  -DestinationPath target\docker-inspect-0.1.0-local.zip -Force

tdcc plugins install --archive .\target\docker-inspect-0.1.0-local.zip `
  --name docker-inspect --version 0.1.0
```

Set `TDCC_PLUGIN_DIR` to an empty directory first if you do not want an
in-development build landing in your real plugin store.

Then enable it and start the node:

```toml
version = 1

[[plugin]]
name = "docker-inspect"
enabled = true
args = ["--container", "tdcc-*", "--max-log-lines", "50"]
```

```bash
tdcc client --port 9337 --console 3131 --config ./config.toml
```

```bash
curl --fail -X POST http://127.0.0.1:3131/api/plugins/docker-inspect/tools/status \
  -H 'Content-Type: application/json' -d '{}'
```

Running the binary directly, outside a host, fails immediately with
`TDCC_PLUGIN_ENDPOINT is not set for plugin process` — after printing the
configuration banner, which is a quick way to check what a set of arguments
actually resolves to. That error is correct: the host owns the control endpoint
and passes it in through the launch contract.

---

## License

Apache-2.0.
