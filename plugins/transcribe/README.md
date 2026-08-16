# transcribe

Turn an audio file into text, with timestamped segments, using a
Whisper-compatible backend you point it at.

Four MCP tools, projected by the host:

| Tool | On the MCP endpoint | What it does |
| --- | --- | --- |
| `transcribe` | `transcribe.transcribe` | One audio file in; text and timestamped segments out. Long WAV recordings are chunked and stitched automatically. |
| `list_audio` | `transcribe.list_audio` | The audio files this plugin may read, with size and — for WAV — duration. The `path` of each entry is what `transcribe` accepts. |
| `status` | `transcribe.status` | How this plugin is configured. Touches nothing. |
| `probe_backend` | `transcribe.probe_backend` | Sends 0.3 s of generated silence through the real request path and reports what came back. |

Two settings decide what this plugin can do, and **both start closed**:
`--root` (with none configured, no file is readable) and `--backend-url` (with
none configured, no request is made). The [Blast radius](#blast-radius) section
says exactly what it touches once you open them.

---

## Read this first: a TDCC node does not transcribe

`tdcc` serves an OpenAI-compatible API on `127.0.0.1:9337`, but not this part of
it. The routes a node actually serves are `/v1/models`,
`/v1/chat/completions`, `/v1/completions`, and `/v1/responses` — there is no
`/v1/audio/transcriptions`. Pointing this plugin at your own node gets you a
`404`, and the error it returns says so.

So the backend is configuration, and standing one up is a prerequisite. Two
kinds work.

### A local whisper.cpp server — nothing leaves your machine

The option for people who contribute their own hardware and would rather their
recordings stayed on it. Build [whisper.cpp](https://github.com/ggml-org/whisper.cpp),
fetch a model, and run its bundled server:

```bash
whisper-server --host 127.0.0.1 --port 8080 -m models/ggml-base.en.bin
```

**Check the route it prints on startup and use that.** Current builds serve
transcription at `/inference`; older and patched builds differ, and some ship an
OpenAI-compatible alias. This plugin does not guess — give it the full path:

```toml
[[plugin]]
name = "transcribe"
url = "http://127.0.0.1:8080/inference"
args = ["--root", "/srv/recordings"]
```

Then confirm it end to end, which is what `probe_backend` is for:

```bash
curl --fail -X POST http://127.0.0.1:3131/api/plugins/transcribe/tools/probe_backend \
  -H 'Content-Type: application/json' -d '{}'
```

A larger model transcribes better and slower. `base` is a reasonable place to
start; if a chunk takes longer than `--timeout-secs` (300 s by default), either
raise it or lower `--chunk-seconds`.

### An OpenAI-compatible transcription endpoint — hosted, key-based

```bash
# in the environment of the tdcc process
export TDCC_TRANSCRIBE_API_KEY='<your key>'
```

```toml
[[plugin]]
name = "transcribe"
url = "https://api.openai.com/v1"
args = ["--root", "/srv/recordings", "--model", "whisper-1"]
```

Your audio leaves your machine and reaches that provider; that is the trade for
not running a model. Note the 25 MB per-request limit OpenAI documents for this
endpoint — this plugin's `--max-upload-bytes` default of 24,000,000 bytes sits
just under it, and long WAV files are chunked to fit. That default is decimal on
purpose: 24 MiB is 25,165,824 bytes, which is *over* a decimal 25 MB.

### How the URL is completed

You may write an origin, an OpenAI-style prefix, or a complete path. Only the
last is left alone:

| What you write | What gets POSTed to |
| --- | --- |
| `http://127.0.0.1:8080` | `http://127.0.0.1:8080/v1/audio/transcriptions` |
| `https://api.example.com/v1` | `https://api.example.com/v1/audio/transcriptions` |
| `https://api.example.com/v1/audio` | `https://api.example.com/v1/audio/transcriptions` |
| `http://127.0.0.1:8080/inference` | `http://127.0.0.1:8080/inference` |

Any other path is taken as deliberate and used verbatim. `status` reports the
completed URL, so there is no need to guess which rule applied.

A missing backend is **not** a startup failure. The plugin starts, prints the
reason once to stderr, keeps `status` and `list_audio` working, and returns that
same message — naming the missing setting — from `transcribe` and
`probe_backend`.

---

## Configuration

There is no `[plugin.settings]` block for this plugin, and that is deliberate.

`[plugin.settings]` values are stored by the host and rendered by the console,
but they are **never delivered to the plugin process** — there is no settings
field in the launch contract or the initialize handshake, and only a web UI
bundle can read them back. This plugin ships no web UI, so a config schema would
draw console controls that could not change which directory it reads. Everything
therefore comes from the two channels a plugin process can actually receive:
`[[plugin]].args` and the environment of the `tdcc` process.

**The API key is environment-only, on purpose.** `args` is written into
`~/.tdcc/config.toml`, echoed back by `tdcc plugins info`, and visible in a
process listing. Passing `--api-key` is a **startup error** that tells you where
the key belongs, and a key embedded in `--backend-url` is refused the same way.

| Setting | `[[plugin]].args` | Environment | Default |
| --- | --- | --- | --- |
| Audio root (repeatable) | `--root <dir>` | `TDCC_TRANSCRIBE_ROOTS` (path-separated list) | none — nothing readable |
| Backend URL | `--backend-url <url>` | `TDCC_TRANSCRIBE_BACKEND_URL` | `[[plugin]].url` |
| API key | — *(environment only)* | `TDCC_TRANSCRIBE_API_KEY` | none |
| Model field | `--model <name>` | `TDCC_TRANSCRIBE_MODEL` | `whisper-1` |
| Default language hint | `--language <code>` | `TDCC_TRANSCRIBE_LANGUAGE` | none |
| Chunk length | `--chunk-seconds <10-1800>` | `TDCC_TRANSCRIBE_CHUNK_SECONDS` | `300` |
| Chunk overlap | `--overlap-seconds <0-60>` | `TDCC_TRANSCRIBE_OVERLAP_SECONDS` | `5` |
| Chunk budget per call | `--max-chunks <1-512>` | `TDCC_TRANSCRIBE_MAX_CHUNKS` | `64` |
| Max bytes read from disk | `--max-file-bytes <n>` | `TDCC_TRANSCRIBE_MAX_FILE_BYTES` | `268435456` (256 MiB) |
| Max bytes per request | `--max-upload-bytes <n>` | `TDCC_TRANSCRIBE_MAX_UPLOAD_BYTES` | `24000000` |
| Cap on `list_audio` | `--max-list-entries <n>` | `TDCC_TRANSCRIBE_MAX_LIST_ENTRIES` | `500` |
| Per-request timeout | `--timeout-secs <5-3600>` | `TDCC_TRANSCRIBE_TIMEOUT_SECS` | `300` |
| Descend into dot-directories | `--include-hidden` | `TDCC_TRANSCRIBE_INCLUDE_HIDDEN=true` | off |
| Omit `timestamp_granularities[]` | `--no-granularity-field` | `TDCC_TRANSCRIBE_NO_GRANULARITY_FIELD=true` | field is sent |

`args` wins over the environment, which wins over `[[plugin]].url`, which wins
over the built-in default.

`whisper-1` is the default model because OpenAI's endpoint requires a `model`
field and rejects the request without one; whisper.cpp's server accepts the
field and serves whatever model it was started with, so the default is harmless
there. Change it with `--model` when your backend cares.

An unrecognised flag, an out-of-range number, or an overlap that is not under
half the chunk length is a **hard startup error**, not a warning. A typo in
`--root` that was quietly ignored would leave you believing this plugin could
read a directory it cannot.

A fuller example:

```toml
version = 1

[[plugin]]
name = "transcribe"
enabled = true
url = "http://127.0.0.1:8080/inference"
args = [
  "--root", "/srv/podcasts",
  "--root", "/mnt/interviews",
  "--chunk-seconds", "240",
  "--overlap-seconds", "8",
  "--language", "en",
]
```

---

## Naming a file

Every root gets a **label**, taken from its own final path component and
deduplicated — `/srv/podcasts` becomes `podcasts`, and a second root also ending
in `audio` becomes `audio-2`. A file is named `<label>/<path below that root>`,
which is exactly the string `list_audio` returns:

```jsonc
{ "root": "podcasts" }
```

```jsonc
{
  "entries": [
    { "path": "podcasts/2024/ep-12.wav", "bytes": 331200044, "extension": "wav", "duration_seconds": 3450.0 },
    { "path": "podcasts/2024/ep-13.mp3", "bytes": 42317008, "extension": "mp3" }
  ],
  "truncated": false,
  "roots": ["podcasts"],
  "unavailable_roots": []
}
```

`duration_seconds` is present for WAV, whose header states it, and absent for
everything else — reading a duration out of a compressed container needs a
decoder this plugin does not have, and an invented number would be worse than
none.

A bare relative path (`2024/ep-12.wav`) also works when exactly one root
contains it; when two do, the error names both candidates rather than picking
one. Absolute paths and `..` are refused outright.

---

## Using `transcribe`

```jsonc
{ "path": "podcasts/2024/ep-12.wav", "language": "en", "segments": true }
```

```jsonc
{
  "path": "podcasts/2024/ep-12.wav",
  "format": "WAV (RIFF)",
  "bytes": 331200044,
  "duration_seconds": 3450.0,
  "backend": "http://127.0.0.1:8080/inference",
  "model": "whisper-1",
  "language_requested": "en",
  "language_detected": "english",
  "chunks": 12,
  "chunk_seconds": 300.0,
  "overlap_seconds": 5.0,
  "segments_available": true,
  "segments": [
    {
      "id": 0,
      "start": 0.0,
      "end": 4.32,
      "start_time": "00:00:00.000",
      "end_time": "00:00:04.320",
      "text": "Welcome back to the show."
    },
    {
      "id": 417,
      "start": 872.5,
      "end": 876.1,
      "start_time": "00:14:32.500",
      "end_time": "00:14:36.100",
      "text": "The budget was already spent."
    }
  ],
  "text": "Welcome back to the show. … The budget was already spent. …",
  "warnings": [],
  "elapsed_seconds": 96.412
}
```

**The segments are the point.** They carry both the seconds a player seeks to
and the clock string a person reads, so a model can answer "at 00:14:32 she said
the budget was already spent" instead of handing back an hour of prose. Set
`"segments": false` if you genuinely only want the words; it is on by default.

`language` takes a two-letter ISO-639-1 code (`en`, `de`, `ja`) or `auto`.
Anything else is refused before the file is opened, because a backend's answer
to a nonsense language code is usually a `400` with no hint that the language
field was the problem. `auto` means no hint is sent at all.

`prompt` passes a short vocabulary hint through to the backend —
`"Kubernetes, etcd, Anthropic"` — to bias names and jargon that are easy to
mishear. It is not transcribed.

---

## Long recordings

A recording longer than `--chunk-seconds`, or larger than
`--max-upload-bytes`, is cut into overlapping chunks, each sent as its own
complete WAV, and stitched back into one timeline.

**Why overlap.** A cut at a flat 300 s lands in the middle of a sentence. The
words before the cut are spoken into a silence the model never hears the end of,
and the words after it begin with no context, so both copies come back mangled.
With `--overlap-seconds 5`, chunk *n* runs to 300 s and chunk *n+1* starts at
295 s, so every moment near the boundary is heard in full by at least one chunk.

**Why that does not double the text.** Each chunk carries a *keep-window* whose
boundary sits at the middle of the overlap. A segment is reported by the single
chunk whose window contains its start — which is the chunk that heard more of
what surrounds it — so the windows partition the recording and every moment is
reported exactly once. Timestamps are shifted by the chunk's own offset first,
so a segment at local 4.0 s in the chunk cut at 295 s comes back at 299.0 s
absolute.

A last defensive pass drops a segment that repeats its predecessor verbatim
within 1.5 s, ignoring case and punctuation, for the case where a backend's own
timestamps disagree with the cut by a fraction of a second.

At the defaults — 300 s chunks, 5 s overlap, 64 chunks — one call covers a
little over five hours of audio. Past that it refuses, naming the number of
chunks the file actually needs, rather than transcribing the first five hours
and returning something that looks complete.

### Chunking works on WAV only, and that is a real limit

Cutting an MP3, an Ogg, an M4A or a FLAC at an arbitrary second means decoding
it, which means a codec library or an `ffmpeg` subprocess. **This plugin carries
neither, deliberately** — see [Blast radius](#blast-radius). WAV needs no
decoder: the header states the frame size, so a time range is byte arithmetic,
and the `fmt ` chunk is copied verbatim into each slice so channel layout, bit
depth and extensible headers survive the cut.

That means:

| File | Behaviour |
| --- | --- |
| PCM or IEEE-float WAV, any length | Chunked natively, timestamps stitched |
| Any compressed format, under `--max-upload-bytes` | Sent whole in one request |
| Any compressed format, over `--max-upload-bytes` | **Refused**, with a message saying it cannot be split and suggesting a conversion |
| A WAV wrapping a compressed payload (ADPCM, µ-law) | Same refusal, naming the format tag |

The conversion the refusal suggests is the one Whisper wants anyway:

```bash
ffmpeg -i interview.m4a -ar 16000 -ac 1 interview.wav
```

---

## When it fails

Failure is always reported, never returned as an empty transcript. An outage and
a genuinely silent recording would otherwise look identical, and the difference
is the whole value of the tool. Each of these gets its own message:

| What went wrong | What the message names |
| --- | --- |
| No backend configured | `--backend-url`, `TDCC_TRANSCRIBE_BACKEND_URL`, `[[plugin]].url`, and that a node does not serve this route |
| No root configured | `--root` and `TDCC_TRANSCRIBE_ROOTS` |
| Path outside a root, or a link pointing out of one | That it was refused — never where the root lives on disk |
| Path not found | The configured roots, and `list_audio` |
| The same path in two roots | Both candidates, spelled as you would disambiguate them |
| Not an audio file | What it actually is — "a PDF document", "a PNG image", "an RF64 container" |
| Corrupt WAV | Which part of the header is wrong |
| Over `--max-file-bytes` | That setting |
| Too large **and** not chunkable | That it cannot be split, why, and the `ffmpeg` line that fixes it |
| One chunk over `--max-upload-bytes` | The `--chunk-seconds` value that would fit, computed |
| Needs more than `--max-chunks` | The number of chunks it actually needs |
| Backend not running | `--backend-url`, and the transport error with any key scrubbed out |
| Backend timed out | `--timeout-secs` and `--chunk-seconds` |
| `401` / `403` | `TDCC_TRANSCRIBE_API_KEY`, and whether a key was sent at all |
| `404` / `405` | That a node does not serve this route, and both `/inference` and `/v1/audio/transcriptions` conventions |
| `413` | `--max-upload-bytes` and `--chunk-seconds` |
| `415` | That the backend does not decode this format |
| `400` / `422` | `--model`, and the model name that was sent |
| `429` | That it is rate limiting, and how to send fewer requests |
| `5xx` | That it is the backend's own error, not a configuration problem here |
| A 2xx that is not a transcript | An excerpt of what came back instead |

A chunked run that fails partway names **which** chunk and at what timestamp —
"chunk 9 of 12 (from 00:39:20.000 to 00:44:20.000) failed: …" — because a
backend that dies on chunk 9 is a different problem from one that never
answered.

Two things are reported as `warnings` rather than failures, because a partial
answer is still useful as long as you are told what is missing:

- Some chunks came back without segments, so those parts have text but no
  timeline. The backend may not implement `response_format=verbose_json`.
- `segments` was turned off on a chunked run, so a few words near each boundary
  may appear twice in `text` — there is no timeline to stitch on.

---

## Reply shapes this plugin understands

The canonical shape is OpenAI's `verbose_json`. Whisper implementations differ,
so the segment parser accepts four spellings of a timestamp rather than
returning an empty list against a server that was working fine:

| Fields on a segment | Units | Where it comes from |
| --- | --- | --- |
| `start`, `end` | fractional seconds | OpenAI `verbose_json` — the canonical shape |
| `offsets: { from, to }` | milliseconds | whisper.cpp's own JSON output |
| `timestamps: { from, to }` | `HH:MM:SS,mmm` strings | whisper.cpp's human-readable pair |
| `t0`, `t1` | centiseconds | whisper's internal token times |

A reply that is not JSON at all is taken as the transcript text, since that is
exactly what `response_format=text` looks like. A segment carrying no usable
timestamp is **skipped**, never given an invented one, and a negative timestamp
is refused rather than read as `0.0` — putting words at the opening second that
were never said there is worse than one fewer segment.

`{"text": ""}` is a success: silence transcribes to nothing, and nothing is the
right answer. Presence of the field decides, not its contents.

---

## Blast radius

This runs on somebody's own hardware and reads their recordings. Every guard
below is on by default.

**Filesystem: read-only, and only inside the roots you name.** With no `--root`,
nothing is readable at all. A path from a tool call is refused if it is
absolute, contains `..`, or contains `:`; then the joined path is canonicalized
and re-checked for containment, so a symlink or Windows junction inside a root
that points outside it is refused. Containment is tested component-wise, so
`/srv/audio-backup` does not count as being inside `/srv/audio`. `list_audio`
never follows a link and descends at most 12 directories. **Nothing is ever
written, moved, or deleted.**

Error messages deliberately carry no absolute path: telling a caller where the
root lives on the contributor's disk is a disclosure this plugin has no reason
to make. There is a test that asserts it.

**Network: one POST, to the backend you configured.** Nothing else is contacted,
no listener is opened — the host owns HTTP and MCP — and redirects are not
followed, because replaying a multipart body with an `Authorization` header to a
host you did not configure is not something to do silently. There is no
private-address guard, and it would be pointless here: the only URL this plugin
ever reaches is the one in your own configuration, never one a model supplied,
and for most people it *is* `127.0.0.1`.

**What leaves the machine:** the audio bytes, the model name, and — when you
pass them — a language code and a prompt. **Not** the filename: the multipart
part is always named `audio.<ext>`, with the extension derived from the file's
own leading bytes rather than its path.

**Subprocesses: none.** Nothing is spawned, and no `PATH` lookup happens. This
is why chunking is WAV-only, and the trade was made in that direction on
purpose: an `ffmpeg` this plugin invoked would be an attacker-chosen binary on a
`PATH` an attacker controls.

**Everything a caller can grow is bounded.** File size, request body size,
reply body size (8 MiB), chunk count per call, listing entries, listing depth,
and the request timeout all have ceilings, and every one of them is refused
loudly rather than silently truncated.

**Secrets:** the key is read only from `TDCC_TRANSCRIBE_API_KEY`. `Backend`'s
`Debug` implementation is hand-written to redact it so an accidental `{:?}`
cannot leak it; transport errors and backend error bodies are scrubbed of it
before they are returned; `status` reports whether a key is present, never what
it is. Passing it as an argument or embedding it in the URL is a startup error.

**Mesh:** no channels and no event subscriptions are declared. Delivery is
allowlist-based, so this plugin receives nothing from the mesh — which is the
right posture for something that reads people's recordings.

---

## What this cannot do

- **It cannot chunk anything but WAV.** A three-hour MP3 under the upload
  ceiling is sent as one request and may well exceed `--timeout-secs`; a
  three-hour MP3 over the ceiling is refused. Convert to WAV first.
- **It cannot tell you who is speaking.** No diarization. The segments are
  timestamped, not attributed.
- **It cannot verify a timestamp is right.** The times come from the backend;
  this plugin corrects them for the chunk offset and clamps them to the length
  of the recording, but a model that hallucinated a timestamp inside a chunk
  produces a wrong timestamp here too.
- **It cannot return word-level timings.** Only segments. OpenAI's
  `timestamp_granularities[]=word` is not requested and word arrays are not
  parsed.
- **It cannot resume.** A chunked run that fails on chunk 9 of 12 returns an
  error, not the first eight chunks. Re-running repeats the work.
- **`--max-file-bytes` is a ceiling, not a memory bound.** A file under it is
  read whole into memory, and a chunked WAV holds both the file and the current
  slice at once. At the 256 MiB default that is worth knowing before you raise
  it on a small node.
- **Duration is only known for WAV.** For everything else `duration_seconds` is
  whatever the backend reports, or absent.
- **The reply parser is tolerant by design.** It accepts four timestamp shapes
  because backends differ; which one *your* backend emits is a property of your
  backend, and `probe_backend` is how you find out.

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
    plugins/transcribe/
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
so no OpenSSL headers are needed either.

---

## Tests

```bash
cargo test
```

165 tests, no network and no backend required.

The pure logic is covered directly, beside the code it covers: configuration
precedence and every error message it can produce, root confinement including a
real symlink-out-of-root escape, WAV header parsing (odd-length chunks,
streamed sizes, truncated recordings, extensible headers, compressed payloads),
frame-aligned slicing, container sniffing, the chunk plan, and segment parsing
and stitching.

Two properties of the chunk planner are worth naming because they are what make
long-audio output trustworthy, and both are asserted over a swept range rather
than a single case:

- `every_moment_of_the_recording_belongs_to_exactly_one_chunk` — the keep-windows
  partition the timeline, so nothing is dropped and nothing is doubled.
- `a_kept_moment_is_always_inside_the_chunk_that_keeps_it` — no chunk is ever
  asked to report on audio it was not given.

The request path is covered end to end against a stub HTTP server on loopback,
which is what makes these answerable at all: how many requests a recording
actually produced, what multipart fields each carried, whether each uploaded
chunk parses back as a valid WAV of the expected duration, and whether the
`Authorization` header was set. A mocked client could answer none of them. The
headline case is
`a_long_recording_is_chunked_with_overlap_and_stitched_back_to_absolute_time`,
which sends 25 s of audio through three overlapping chunks and asserts the exact
stitched timestamps.

No test is ignored, and none makes an outbound request.

---

## Package and install locally

The archive needs one top-level directory named after the plugin, containing
`plugin.toml` and an executable named exactly `transcribe` (`transcribe.exe` on
Windows). This plugin declares neither a config schema nor a web UI, so its
`plugin-manifest.json` is `{}` and may be left out; `--print-package-manifest`
prints it if you want to include it anyway.

macOS and Linux:

```bash
rm -rf target/package
mkdir -p target/package/transcribe
cp target/release/transcribe target/package/transcribe/transcribe
cp plugin.toml README.md target/package/transcribe/
tar -C target/package -czf target/transcribe-0.1.0-local.tar.gz transcribe

tdcc plugins install --archive ./target/transcribe-0.1.0-local.tar.gz \
  --name transcribe --version 0.1.0
tdcc plugins info transcribe
```

Windows:

```powershell
Remove-Item -Recurse -Force target\package -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force target\package\transcribe | Out-Null
Copy-Item target\release\transcribe.exe target\package\transcribe\transcribe.exe
Copy-Item plugin.toml, README.md target\package\transcribe\

Compress-Archive -Path target\package\transcribe `
  -DestinationPath target\transcribe-0.1.0-local.zip -Force

tdcc plugins install --archive .\target\transcribe-0.1.0-local.zip `
  --name transcribe --version 0.1.0
```

Set `TDCC_PLUGIN_DIR` to an empty directory first if you do not want an
in-development build landing in your real plugin store.

Then enable it and start the node:

```bash
tdcc client --port 9337 --console 3131 --config ./config.toml
```

```bash
curl --fail -X POST http://127.0.0.1:3131/api/plugins/transcribe/tools/status \
  -H 'Content-Type: application/json' -d '{}'

curl --fail -X POST http://127.0.0.1:3131/api/plugins/transcribe/tools/list_audio \
  -H 'Content-Type: application/json' -d '{}'

curl --fail -X POST http://127.0.0.1:3131/api/plugins/transcribe/tools/transcribe \
  -H 'Content-Type: application/json' \
  -d '{"path":"recordings/standup.wav","language":"en"}'
```

Running the binary directly, outside a host, fails immediately with
`TDCC_PLUGIN_ENDPOINT is not set for plugin process`. That is correct — the host
owns the control endpoint and passes it in through the launch contract.
`--help` and `--print-package-manifest` are handled before that and work
standalone.

---

## License

Apache-2.0.
