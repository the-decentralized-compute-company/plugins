# describe-image

Put the mesh's own vision models to work on a picture.

Five MCP tools, projected by the host:

| Tool | On the MCP endpoint | What it does |
| --- | --- | --- |
| `describe` | `describe-image.describe` | An image in, a description out. |
| `ask` | `describe-image.ask` | One question about an image, answered from what is visible. |
| `read_text` | `describe-image.read_text` | The text visible in an image, transcribed. |
| `status` | `describe-image.status` | How this plugin is configured. Makes no request. |
| `vision_models` | `describe-image.vision_models` | Which models the endpoint serves and which can see. |

All five are also mounted over HTTP by the host — `POST` for the three that
carry images, `GET` for the two diagnostics:

```text
POST /api/plugins/describe-image/http/describe
POST /api/plugins/describe-image/http/ask
POST /api/plugins/describe-image/http/read_text
GET  /api/plugins/describe-image/http/status
GET  /api/plugins/describe-image/http/vision_models
```

> **The answers come from a model, and models are wrong sometimes.**
> A description is not a measurement and a transcription is not OCR. See
> [What you are actually getting](#what-you-are-actually-getting) — it is the
> most important section here, and every tool result repeats it in a `caveat`
> field.

By default this plugin reads **no files**, fetches **no URLs**, and talks to
**nothing but `127.0.0.1:9337`**. Every widening is a flag. See
[Blast radius](#blast-radius).

---

## What it needs

**A vision-capable model, served on your mesh.** That is the one prerequisite,
and it is not optional. This plugin does not ship a model, does not download
one, and does not run inference itself — it sends an image to the
OpenAI-compatible API your node already exposes and reports what comes back.

Confirm you have one before anything else:

```bash
curl -s http://127.0.0.1:9337/v1/models | grep -o '"vision_status":"[a-z]*"' | sort | uniq -c
```

A TDCC node annotates every model in that list with what it inferred:

```jsonc
{ "id": "Qwen3-VL-4B-Instruct",
  "capabilities": ["text", "multimodal", "vision"],
  "vision_status": "supported" }
```

`supported` means the node has hard evidence — a projector (`mmproj`) file
beside the weights, or a `vision_config` in the model's `config.json`. `likely`
means it only recognised the name. If every entry says `none`, install a vision
model; nothing here can work around that, and `describe` will tell you so by
name rather than failing obscurely.

Once one is loaded, no configuration is needed at all:

```toml
version = 1

[[plugin]]
name = "describe-image"
enabled = true
```

`describe-image.vision_models` is the tool that answers "is this working" from
inside the node.

---

## Choosing the model

The model is **discovered, never hard-coded**. Contributors bring their own
hardware and their own weights and the set changes as peers come and go, so a
constant in the source would be wrong on almost every node.

| What the endpoint says | What happens | `selected_by` |
| --- | --- | --- |
| A model has `vision_status: "supported"` or a `vision` capability | It is used | `declared` |
| Only `vision_status: "likely"` | It is used, with a caveat | `declared-likely` |
| Capability metadata exists, none of it says vision | **Error**, listing what is served | — |
| No entry carries any metadata at all | Name heuristic, with a caveat | `name-heuristic` |
| `--model` is set, or a `model` argument is passed | That one, and it must be in the list | `configured` |

The last two rows are worth expanding.

**The name heuristic only ever runs against an endpoint that published nothing.**
Point `--api-base` at a bare llama.cpp or vLLM server and `/v1/models` is a list
of ids with no capabilities on them; refusing outright would be unhelpful and
guessing silently would be dishonest, so the plugin falls back to the same name
signals the host itself uses (`vl`, `vision`, `llava`, `internvl`, `paligemma`,
…), labels the result `name-heuristic`, and says in the `caveat` that it
guessed. If the node published an answer, the node wins — a model called
`something-vl-flavoured` that the node reports as `vision_status: "none"` is not
selected.

**A pinned model must be in the list.** A typo in `--model` fails immediately
with the served ids, rather than turning into an opaque `404` from the inference
server on every call. Pinning a model the endpoint says is text-only is allowed
— you may know something the node does not — and the `caveat` says so out loud.

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
`~/.tdcc/config.toml`, echoed back by `tdcc plugins info`, and visible in a
process listing. A credential belongs in none of those, so there is no
`--api-key` flag to misuse.

| Setting | `[[plugin]].args` | Environment | Default |
| --- | --- | --- | --- |
| API base URL | `--api-base <url>` | `TDCC_DESCRIBE_IMAGE_API_BASE` | `http://127.0.0.1:9337/v1`, or `[[plugin]].url` |
| API key | — *(environment only)* | `TDCC_DESCRIBE_IMAGE_API_KEY` | none |
| Pin a model | `--model <id>` | `TDCC_DESCRIBE_IMAGE_MODEL` | discovered |
| Readable directory | `--root <dir>` *(repeatable)* | `TDCC_DESCRIBE_IMAGE_ROOTS` *(path list)* | **none** |
| Allow a non-loopback API base | `--allow-remote-api` | `TDCC_DESCRIBE_IMAGE_ALLOW_REMOTE_API=true` | refused |
| Allow http(s) image URLs | `--allow-remote-images` | `TDCC_DESCRIBE_IMAGE_ALLOW_REMOTE_IMAGES=true` | refused |
| Allow private image addresses | `--allow-private-network` | `TDCC_DESCRIBE_IMAGE_ALLOW_PRIVATE_NETWORK=true` | refused |
| Longest edge sent, px | `--max-dimension <64-4096>` | `TDCC_DESCRIBE_IMAGE_MAX_DIMENSION` | `1024` |
| Max source bytes per image | `--max-image-bytes <n>` | `TDCC_DESCRIBE_IMAGE_MAX_IMAGE_BYTES` | `8388608` |
| Max decoded pixels per image | `--max-pixels <n>` | `TDCC_DESCRIBE_IMAGE_MAX_PIXELS` | `50000000` |
| Images per call | `--max-images <1-8>` | `TDCC_DESCRIBE_IMAGE_MAX_IMAGES` | `4` |
| Answer length ceiling, tokens | `--max-tokens <16-8192>` | `TDCC_DESCRIBE_IMAGE_MAX_TOKENS` | `512` |
| Request timeout, seconds | `--timeout-secs <5-900>` | `TDCC_DESCRIBE_IMAGE_TIMEOUT_SECS` | `120` |
| Encoding sent | `--image-format auto\|jpeg\|png` | `TDCC_DESCRIBE_IMAGE_IMAGE_FORMAT` | `auto` |
| JPEG quality | `--jpeg-quality <40-95>` | `TDCC_DESCRIBE_IMAGE_JPEG_QUALITY` | `82` |

`args` wins over the environment, which wins over `[[plugin]].url`, which wins
over the built-in default.

An unrecognised flag, an out-of-range number, a repeated flag (except `--root`),
or a `--root` that does not exist is a **hard startup error**, not a warning. A
typo in `--allow-remote-images` that was quietly ignored would leave you
believing a guard was off when it was on, and a mistyped root would silently
make every local path unreadable.

A fuller example:

```toml
version = 1

[[plugin]]
name = "describe-image"
enabled = true
args = [
  "--root", "/srv/screenshots",
  "--root", "/home/ops/scans",
  "--max-dimension", "1280",
  "--max-images", "2",
]
```

The timeout is 120 s rather than the 20 s a text plugin would use because the
first vision call after a restart pays for the projector load as well as the
inference. If you see timeouts only on the first call, that is what it is.

---

## Using the tools

### `describe`

```jsonc
{ "images": ["screenshots/build-failure.png"], "focus": "the error message" }
```

```jsonc
{
  "model": "Qwen3-VL-4B-Instruct",
  "selected_by": "declared",
  "text": "A terminal window showing a cargo build. The last lines are red…",
  "images": [
    {
      "source": "file",
      "label": "build-failure.png",
      "original": { "width": 2560, "height": 1440, "format": "png", "bytes": 486213 },
      "sent": { "width": 1024, "height": 576, "media_type": "image/png", "bytes": 118004 },
      "downscaled": true
    }
  ],
  "finish_reason": "stop",
  "usage": { "prompt_tokens": 812, "completion_tokens": 96 },
  "caveat": "This description was produced by a language model looking at the image…"
}
```

The `images` block is there so you can see what was actually sent rather than
infer it — the original size, the size after downscaling, and both byte counts.
The base64 that went over the wire is never echoed back.

### `ask`

```jsonc
{ "images": ["scans/invoice.jpg"], "question": "What is the total due?" }
```

`question` is required and cannot be empty. The model is instructed to answer
only from what is visible and to say plainly when the image does not settle the
question, rather than to speculate.

### `read_text`

```jsonc
{ "images": ["screenshots/dialog.png"], "max_tokens": 1500 }
```

Adds one field to the usual result:

```jsonc
{ "text": "Disk Utility\nErase “Untitled”?\n…", "no_text_found": false, … }
```

`no_text_found` is `true` when the model reported no legible text at all, which
is a different thing from the model failing to answer. Raise `max_tokens` for a
dense page — if the transcription stops mid-sentence, `finish_reason` will say
`length`.

### `status` and `vision_models`

`status` is local and cannot hang: it reports the endpoint, whether a key is
set, the configured roots, every limit, and any advisories. `vision_models`
makes a request, so it also tells you whether the endpoint is reachable:

```jsonc
{
  "endpoint": "http://127.0.0.1:9337/v1/",
  "count": 2,
  "publishes_capabilities": true,
  "would_use": "Qwen3-VL-4B-Instruct",
  "selected_by": "declared",
  "models": [
    { "id": "Llama-3.1-8B-Instruct", "vision": null, "vision_status": "none" },
    { "id": "Qwen3-VL-4B-Instruct", "vision": "declared", "vision_status": "supported" }
  ],
  "problem": null
}
```

`problem` is the deliberate exception to "failure is never an empty success":
listing models is a diagnostic, and "here is everything you have and none of it
can see" is the answer to that question, not an error. The other four tools do
error.

---

## Where an image can come from

Three forms, told apart before anything is opened.

**A local path.** Resolved inside the directories the operator listed with
`--root`, and refused entirely when none is configured — which is the default.
Both `album/holiday.png` (relative to a root) and the full path work; with a
single root the relative form reads exactly like a path inside that directory.

**A `data:` URI.** `data:image/png;base64,<...>`. Base64 only; a
percent-encoded data URI is refused. The oversized case is caught from the
encoded length before it is expanded into memory.

**An http/https URL**, if `--allow-remote-images` is on. Off by default,
because this node makes that request, from its own address, on behalf of
whoever called the tool.

Every other URL scheme is refused by name rather than falling through to the
path branch — `file:///etc/shadow` reaching a filesystem resolver is exactly
the accident that is designed out. On Windows, `C:\photos\a.png` is a path and
not a `c:` URL, because a one-character scheme is not treated as a scheme.

PNG, JPEG, GIF, WebP, BMP, and TIFF are accepted, and a test asserts that each
of those six really does have a decoder linked into the build rather than just
a line in this table. The format comes from sniffing the bytes, not from what
the caller claimed, so a PNG mislabelled `image/jpeg` still works.

---

## What happens to an image before it is sent

A vision encoder does not see pixels; it sees a fixed grid of patches cut out
of whatever you send. A 12 MP phone photo is roughly 16 MB of base64 in the
request body, costs thousands of image tokens, frequently trips a server's
request-size limit, and reaches the encoder as the same patch grid a 1 MP
version would have produced. So:

1. **Dimensions are read from the header and checked first**, before a decode
   is attempted. A 40 KiB PNG can declare 60000x60000, which is 14 GiB of RGBA
   the moment it is decoded; `--max-pixels` is that guard, not a preference.
2. **The image is decoded** with its own allocation limits as a second layer.
3. **It is resized so its longest edge is at most `--max-dimension`**, aspect
   ratio preserved, with a Lanczos3 filter — the cheaper filters are where
   `read_text` legibility goes. An image already inside the budget is not
   upscaled.
4. **It is re-encoded.** Under `auto`, a lossless source (PNG, BMP, TIFF, GIF)
   at 1.2 MP or smaller stays PNG, so a screenshot of a terminal keeps its hard
   edges; everything else becomes JPEG at `--jpeg-quality`. Transparency is
   composited onto white when the result is a JPEG, so a transparent icon does
   not arrive as a black rectangle. `--image-format jpeg|png` overrides the
   heuristic.
5. **It is inlined as a `data:` URI** in an `image_url` content part.

That last point is a choice worth naming: an `https://` URL in the request
would be fetched **by the inference server**, from a machine and a network
position this plugin does not control. Inlining the bytes keeps the whole
transfer inside the request this node made.

The message is a single `user` turn with the instruction first and the images
after it. There is deliberately no system message — multimodal chat templates
vary in whether they accept one — and no `detail` field on the image part,
because that is an OpenAI-hosted concept that local servers ignore.

Sampling temperature is fixed rather than configurable: `0.0` for `read_text`,
because a transcription has one correct output and any spread is corruption,
and `0.2` for `describe` and `ask`.

---

## What you are actually getting

**A description is a model's guess about pixels.** It can be wrong, and it can
be wrong confidently. Objects that are not there, counts that are off by one,
colours that are named from context rather than seen, text that is
plausible-looking rather than present. Every result carries a `caveat` field
saying so, and the caveat is not conditional on how well the call went.

**`read_text` is not OCR.** It is a vision language model reading a picture, so
it fails differently from an OCR engine: instead of returning garbage
characters it returns fluent, plausible, wrong ones. `0` and `O`, `1` and `l`,
`5` and `S` are the usual casualties; a long serial number, a licence key, an
IBAN, or a code fragment should be checked against the image before it is used.
Reading order in a multi-column layout is a guess. Illegible characters are
asked for as `[?]`, but nothing enforces that. If you need character-exact
output, use a real OCR engine and use this to describe what the page is.

**Model selection can be a guess too.** `selected_by` says which of `declared`,
`declared-likely`, `name-heuristic`, or `configured` produced the model, and
anything other than `declared` appends its own sentence to the `caveat`.

**What this plugin cannot do.** It cannot tell you whether an image has been
edited, cannot identify a person, cannot read text the downscale removed
(lower-resolution input is the trade `--max-dimension` makes — raise it for a
dense scan), and cannot see anything outside the frame. It has no memory
between calls: each call is one request with the images in it.

---

## Blast radius

This runs on hardware somebody contributed, with the operator's privileges and
no sandbox. Every guard below is on by default and has to be turned off
deliberately.

**Network — outbound only.** No listener is opened; the host owns HTTP and MCP.
Two destinations are possible:

- *The inference endpoint*, `http://127.0.0.1:9337/v1` by default. It receives
  the image bytes and the instruction. A non-loopback endpoint is **refused at
  startup** unless `--allow-remote-api` is passed, because every call ships
  pictures off the box and that has to be a deliberate choice. The loopback
  check is syntactic — a guard against pasting a public endpoint by accident,
  not a defence against a hostile DNS server.
- *An image URL*, only if `--allow-remote-images` is on.

**Image URLs are guarded, and the guard is the one `web-search` uses.** Before
each request the hostname is resolved and the answer is checked: loopback, RFC
1918, link-local (including the `169.254.169.254` cloud metadata endpoint),
unique-local IPv6, carrier-grade NAT, and the IPv4-mapped IPv6 forms of all of
those are refused. Redirects are followed by hand, up to three, and the guard
runs again at every hop — a permitted URL that redirects into private space is
not followed just because the first hop passed. `--allow-private-network` opts
out. The configured API base is exempt, the same way `web-search` exempts its
SearXNG base: it comes from your configuration, not from a model.

*This is a guard, not a sandbox.* The connection re-resolves the name, so a DNS
answer that changes between the check and the connection (DNS rebinding) can
still get through. It stops the ordinary cases, which is what it is for.

**Filesystem — read-only, inside `--root`, and closed by default.** With no
root configured, every local path is refused and only `data:` URIs work. With
roots configured, two independent layers apply: a lexical check refuses any
`..` before a syscall happens, and the resolved path is then canonicalized —
which follows symlinks and Windows junctions — and re-tested for containment.
A symlink inside a root pointing outside it fails at the second layer, which is
the only one that can catch it. Containment is component-wise, so
`/srv/photos-backup` does not count as being inside `/srv/photos`. Directories
and non-regular files are refused. Nothing is ever written.

Error messages from a failed lookup deliberately do not say where the roots
live or whether anything exists outside them. `status` does report them, to an
operator who configured them and needs to see them.

**Bounds, all of them enforced.** At most `--max-images` per call. At most
`--max-image-bytes` of source per image, checked in whichever way the source
allows: a data URI is bounded from its encoded length before it is expanded, a
file from the open handle's own metadata and then again by a capped read, and a
URL from `Content-Length` and then again while the body streams. At most
`--max-pixels` decoded, read from the image header before any decode is
attempted. A 4 MB cap on the endpoint's own responses. A `--timeout-secs`
deadline on every request. An oversized input is refused rather than silently
truncated.

**Subprocesses:** none. Nothing is spawned.

**Mesh:** no channels and no event subscriptions are declared. Delivery is
allowlist-based, so this plugin receives nothing from peers.

**Secrets:** no key is committed, logged, or printed. `ApiKey`'s `Debug`
implementation is hand-written to redact the value so an accidental `{:?}`
cannot leak it, there is no flag that could put a key in `config.toml`, a
credential embedded in `--api-base` is a startup error naming the environment
variable instead, and every error body is scrubbed of the key before it is
returned.

**Privacy, stated plainly:** the images you pass are sent to the configured
endpoint, and their content ends up in that server's request handling and
possibly its logs. By default that is your own node on loopback. It is not by
default anything else.

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
    plugins/describe-image/
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
so no OpenSSL headers are needed either. Image decoding is pure Rust — the
`image` crate with an explicit six-format allowlist rather than its default
feature set, because every linked decoder is attack surface reachable from a
tool argument.

---

## Tests

```bash
cargo test
```

147 tests, no network and no model required.

The pure logic is covered directly: configuration precedence and every one of
its error messages, key redaction, the private-address guard, data URI parsing,
path confinement, scaling arithmetic, the encoding heuristic, the `/v1/models`
parser and the selection rules, request building, and completion parsing. The
real request path is covered end to end against a stub HTTP server on loopback,
which is what exercises model discovery, the redirect guard, the size caps, and
the failure messages deterministically — including a redirect loop, where the
test asserts on the number of connections the stub actually served rather than
on a message.

The image tests generate their own images rather than shipping fixtures, so
they assert on real decoded pixels: that a 2400x1800 source lands at 1024x768
and gets smaller in bytes, that the re-encoded output decodes back to the
reported dimensions, that a fully transparent PNG becomes white and not black
when it is flattened into a JPEG, and that `--jpeg-quality` changes the output
size.

Two assertions are worth knowing about because they encode security decisions
rather than behaviour: `a_symlink_pointing_outside_a_root_is_refused` creates a
real directory link and skips itself with a printed note if the platform
refuses to make one (Windows needs Developer Mode), and
`an_api_key_never_appears_in_an_error` drives a stub that returns the key
inside a `500` body.

---

## Package and install locally

The archive needs one top-level directory named after the plugin, containing
`plugin.toml` and an executable named exactly `describe-image`
(`describe-image.exe` on Windows). This plugin declares neither a config schema
nor a web UI, so its `plugin-manifest.json` is `{}` and may be left out;
`--print-package-manifest` prints it if you want to include it anyway.

macOS and Linux:

```bash
rm -rf target/package
mkdir -p target/package/describe-image
cp target/release/describe-image target/package/describe-image/describe-image
cp plugin.toml README.md target/package/describe-image/
tar -C target/package -czf target/describe-image-0.1.0-local.tar.gz describe-image

tdcc plugins install --archive ./target/describe-image-0.1.0-local.tar.gz \
  --name describe-image --version 0.1.0
tdcc plugins info describe-image
```

Windows:

```powershell
Remove-Item -Recurse -Force target\package -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force target\package\describe-image | Out-Null
Copy-Item target\release\describe-image.exe target\package\describe-image\describe-image.exe
Copy-Item plugin.toml, README.md target\package\describe-image\
Compress-Archive -Path target\package\describe-image `
  -DestinationPath target\describe-image-0.1.0-local.zip -Force

tdcc plugins install --archive .\target\describe-image-0.1.0-local.zip `
  --name describe-image --version 0.1.0
```

Set `TDCC_PLUGIN_DIR` to an empty directory first if you do not want an
in-development build landing in your real plugin store.

Then enable it and start the node:

```bash
tdcc client --port 9337 --console 3131 --config ./config.toml
```

```bash
curl --fail -X POST http://127.0.0.1:3131/api/plugins/describe-image/tools/vision_models \
  -H 'Content-Type: application/json' -d '{}'
```

Running the binary directly, outside a host, prints its configuration and then
fails with `TDCC_PLUGIN_ENDPOINT is not set for plugin process`. That is
correct — the host owns the control endpoint and passes it in through the launch
contract.

---

## License

Apache-2.0.
