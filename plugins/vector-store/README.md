# vector-store

The retrieval half of RAG, as local infrastructure: store documents as
passages, find them by meaning, and get back enough to **cite** the answer.

```text
upsert  docs/manual.md  ──▶  split on structure ──▶ embed ──▶ 12 passages on disk
query   "how do I set up the service"
        ──▶ docs/manual.md:14-22  "To install the service, unpack the archive…"
                                   Operations Manual > Install        score 0.91
```

That third line is the point. A retriever that returns text without a location
makes a model that cannot cite; this one keeps the source label, the line span,
and the heading breadcrumb on every passage, so a citation survives the whole
way through.

---

## Before you start: this plugin needs an embeddings endpoint

To compare passages by meaning, something has to turn them into vectors. This
plugin does not ship a model; it calls an **OpenAI-compatible
`POST /v1/embeddings`**.

**The TDCC node does not currently expose one.** Its OpenAI frontend router
declares exactly four routes — `/v1/models`, `/v1/chat/completions`,
`/v1/completions`, `/v1/responses` — and embeddings are listed as out of scope
in that component's own documentation. That was checked against the SDK
checkout this crate builds against, not assumed. The sibling
[`semantic-cache`](../semantic-cache) plugin has the same unmet prerequisite for
the same reason.

So out of the box you must point this plugin at an embeddings server:

```toml
[[plugin]]
name = "vector-store"
args = ["--embeddings-url", "http://127.0.0.1:11434/v1",
        "--embedding-model", "nomic-embed-text"]
```

Any local server exposing an OpenAI-compatible `/v1/embeddings` works — Ollama,
`llama-server --embeddings`, LM Studio, and vLLM serving an embedding model are
the usual choices. Do not take that list as verified for your version; **run the
`status` tool, which sends one real probe and tells you the truth**:

```bash
curl --fail -X POST http://127.0.0.1:3131/api/plugins/vector-store/tools/status \
  -H 'Content-Type: application/json' -d '{}'
```

The default endpoint is `http://127.0.0.1:9337/v1` — the node itself — so that
the day the node grows an embeddings route this plugin works with no
configuration. Until then that default fails loudly and legibly rather than
behaving like an index that never matches.

One tool needs no backend at all: **`preview_chunks`** shows exactly how a
document would be split, with no embedding and no storage. Use it to tune the
chunk sizes before you have an embedder running.

---

## Chunking, which matters more than the index

A brilliant index over passages that cut a sentence in half, or that separated a
heading from the paragraph it introduces, returns confident nonsense. A plain
brute-force scan over well-formed passages works. So the splitting here is
**structural first and length-bounded second**.

1. **Blocks come from the document.** Fenced code blocks are kept whole — blank
   lines and all, so a retrieved snippet never loses its closing brace — and
   their contents are never scanned for headings, so a `#` shell comment does
   not become a section. Markdown headings (ATX and setext) become breadcrumb
   entries. Everything else is a blank-line-separated paragraph.
2. **A block over the ceiling is cut at the least damaging boundary
   available**: sentences, then words, then — for an unbroken run like a base64
   blob or a minified bundle — mid-string. Each of those is reported as a
   `split_reason`, so "we cut your 40 000-character paragraph mid-thought" is
   something you are told rather than something you infer from bad retrieval
   three weeks later.
3. **Blocks are packed greedily** up to the target size, and **a heading never
   ends a chunk** — a trailing heading is pushed into the next one, because a
   heading's entire job is to introduce what follows it.
4. **Consecutive chunks overlap by whole blocks**, so a fact that straddles a
   boundary appears intact in one of them. Whole blocks rather than a character
   count, so the overlap is never half a sentence.

Every passage carries the 1-based inclusive line span it came from and the
heading path it sits under.

### A worked example

`preview_chunks` on this document, at `--chunk-chars 200
--chunk-overlap-chars 60 --max-chunk-chars 400`:

```markdown
# Operations Manual            ← line 1

## Install                     ← line 3

To install the service, unpack the archive and run the setup script. It will
ask you for a data directory.

The installer needs about two gigabytes of free space.   ← line 8

## Backup and restore          ← line 10

Take a backup before every upgrade.                      ← line 12
```

produces exactly two passages:

```jsonc
{
  "chunks": [
    {
      "index": 0, "chars": 195,
      "line_start": 1, "line_end": 8,
      "heading_path": ["Operations Manual"],
      "citation": "docs/manual.md:1-8",
      "text": "# Operations Manual\n\n## Install\n\nTo install the service, unpack…"
    },
    {
      "index": 1, "chars": 114,
      "line_start": 8, "line_end": 12,
      "heading_path": ["Operations Manual", "Install"],
      "citation": "docs/manual.md:8-12",
      "text": "The installer needs about two gigabytes of free space.\n\n## Backup and restore\n\nTake a backup before every upgrade."
    }
  ],
  "chunk_count": 2, "target_chars": 200, "overlap_chars": 60, "max_chars": 400
}
```

Three things to read out of it:

- **The spans overlap.** Chunk 1 starts on line 8, which chunk 0 ended on: the
  "two gigabytes" paragraph appears whole in both.
- **`## Backup and restore` travelled with the sentence it introduces**, rather
  than being stranded at the end of chunk 1.
- **The citations are usable as-is** — `docs/manual.md:8-12` is a real span in a
  real file.

Every number above is asserted by
`the_worked_example_in_the_readme_is_what_the_splitter_actually_produces` in
`src/manifest.rs`, so this example cannot drift away from the code.

### Sizes are characters, not tokens

There is no tokenizer here and no dependency that would supply one. Every bound
counts Unicode scalar values. For English prose a token is roughly four
characters, so the 1200-character default is *very* roughly 300 tokens — treat
that as an order of magnitude, not a measurement, and leave headroom against
your embedding model's real input limit.

---

## Storage: local, durable, and honest about scale

One **append-only JSONL log per collection**, under
`~/.tdcc/vector-store/collections/<name>.jsonl`. The first line is a header
pinning the collection's embedding model and dimensions; every line after it is
a `put` or a `delete`. Replaying the log rebuilds the collection, so the store
survives a restart, survives being copied to another machine, and survives a
crash mid-write — a torn final line is discarded, never guessed at. Every
mutation is one append plus one `fsync`. Deletes leave tombstones, and the log
is compacted (temp file, then rename) once it passes 512 lines and 30% dead
weight.

It is JSON, deliberately, with the vectors as plain arrays of numbers. A packed
binary format would be about half the size, but the person whose machine this
runs on can read a JSONL file with `head` and see exactly what was stored about
them. **The cost is roughly 10 KB per passage on disk at 768 dimensions**, so a
50 000-passage collection is about 500 MB of log.

### Search is an exact brute-force cosine scan

Every query compares against every live passage in one collection. At 768
dimensions a 50 000-passage scan is about 38 million multiply-adds — tens of
milliseconds — and it is **exact**: the nearest neighbour is the nearest
neighbour, not the nearest one an index happened to visit. An approximate index
would be faster and would introduce a recall parameter that silently drops
results, which is much harder to trust and much harder to debug when retrieval
goes wrong.

**Past a few tens of thousands of passages per collection this is the wrong
design and you want a real vector database.** That is not left as advice: the
default cap is 50 000 passages per collection and it is enforced, with an error
that says exactly this. Raising `--max-chunks-per-collection` is allowed, warns
at startup, and makes queries linearly slower.

Filtering runs before scoring, so narrowing by metadata or source prefix is
cheaper than the dot products it avoids.

---

## Embedding spaces are never mixed

A collection **pins the embedding model that created it**, in its header. An
`upsert` or a `query` using a different model is refused:

```text
collection "docs" holds vectors from embedding model "nomic-embed-text"
(768 dimensions) and this process is configured for "text-embedding-3-small".
Vectors from two models are not comparable, so this is refused rather than
answered with a confident wrong ranking. Either set --embedding-model back to
"nomic-embed-text", or delete the collection and rebuild it with the new model.
```

This is the single most expensive mistake a store like this can make, because it
fails *silently* and looks like a quality problem with the model. Two embedders
produce coordinates in unrelated spaces; the cosine between them is a real
number in `[-1, 1]` that means nothing.

Four layers enforce it:

| Layer | Catches |
| --- | --- |
| The collection header, checked **before** anything is embedded | An operator who changed `--embedding-model` and restarted — and it costs zero embedding calls to find out |
| A per-record model id, checked at load | A hand-edited log, or two collections concatenated |
| A dimension check on the header | The same model id served at a different width — a server that quietly swapped models |
| `cosine_similarity` returning `Option` | Anything that got past the other three: mismatched widths are skipped, never scored |

An unconfigured model pins to the literal identity `<unset>`, which no real
model id can collide with, so passages created before a model was configured are
never compared against passages created after.

`status` lists every collection with its pinned model and a
`usable_with_current_model` flag, so "why does this return nothing" has an
answer before you go looking for one.

---

## Collections are namespaces

A query names exactly one collection and can see nothing else. `delete` with
`scope: "collection"` removes exactly one file and touches no other collection's
data. Both are covered by tests
(`collections_are_namespaces_that_cannot_see_each_other`,
`deleting_one_collection_leaves_the_others_untouched`).

A collection name is **not** a path. It must be letters, digits, `-` and `_`,
1–64 characters, starting alphanumeric — so there is no character available to
build a traversal out of. Names are case-folded, so `Docs` and `docs` are one
collection on every platform rather than two on Linux and one everywhere else.
Windows device names (`con`, `nul`, `com1`…) are refused on every platform. The
resolved file path is then re-checked for containment against the canonical data
root, which catches a `collections` directory that has been replaced with a
symlink or junction.

---

## Tools

All six are on the host MCP endpoint namespaced as `vector-store.<tool>`, and
callable over HTTP at `POST /api/plugins/vector-store/tools/<tool>`.

| Tool | Does | Network |
| --- | --- | --- |
| `upsert` | chunk, embed and store documents | one embedding call per batch of chunks |
| `query` | find passages by meaning, with filters | one embedding call |
| `delete` | remove documents, or a whole collection | none |
| `stats` | what is stored: counts, models, sizes | **none** |
| `status` | is the embeddings backend actually reachable? | one probe embedding call |
| `preview_chunks` | how would this text be split? | **none** |

`upsert`, `query` and `stats` are also mounted as HTTP routes at
`/api/plugins/vector-store/http/{upsert,query,stats}` — a retrieval gateway in
front of `:9337` is the natural place to use this, and a gateway speaks HTTP,
not MCP. `delete` deliberately is not a route.

### `upsert`

```bash
curl --fail -X POST http://127.0.0.1:3131/api/plugins/vector-store/http/upsert \
  -H 'Content-Type: application/json' -d '{
    "collection": "handbook",
    "documents": [{
      "id": "docs/manual.md",
      "source": "docs/manual.md",
      "text": "# Operations Manual\n\n## Install\n\nTo install the service…",
      "metadata": {"team": "platform", "kind": "runbook"}
    }]
  }'
```

**This plugin never opens `source`.** It is a label, recorded and returned;
you pass the text you already read. That is deliberate — see
[Security](#security).

Re-sending a document id **replaces** every passage of the previous version, so
re-ingesting an edited file leaves no stale passages behind. A shortened
document does not leave its old tail retrievable
(`re_ingesting_a_shortened_document_leaves_no_stale_passages`).

Every document is chunked and validated *before* anything is embedded, so a bad
argument costs no embedding calls. If the embedder fails part-way, **nothing is
stored** — a document indexed with half its passages missing is worse than one
not indexed at all, because nothing later reports the gap
(`a_failed_ingest_stores_nothing`).

One thing to know about batches: **each document is written as its own durable
transaction**, so if a batch trips a bound part-way through — a full collection,
the byte limit — the documents already written stay written. The error names
them:

```text
collection "handbook" is at its limit of 50000 chunks … — document "docs/c.md"
failed after 2 earlier document(s) in this batch had already been stored
(docs/a.md, docs/b.md). Each document is written separately, so those remain;
re-sending the whole batch is safe once the cause is fixed, because an upsert
replaces.
```

### `query`

```bash
curl --fail -X POST http://127.0.0.1:3131/api/plugins/vector-store/http/query \
  -H 'Content-Type: application/json' -d '{
    "collection": "handbook",
    "query": "how do I set up the service",
    "top_k": 5,
    "min_score": 0.3,
    "filter": {"team": "platform"},
    "source_prefix": "docs/"
  }'
```

```jsonc
{
  "collection": "handbook",
  "embedding_model": "nomic-embed-text",
  "returned": 1,
  "collection_chunks": 12,
  "filtered": true,
  "results": [{
    "score": 0.91,
    "id": "docs/manual.md#1",
    "document_id": "docs/manual.md",
    "source": "docs/manual.md",
    "line_start": 14, "line_end": 22,
    "heading_path": ["Operations Manual", "Install"],
    "citation": "docs/manual.md:14-22",
    "text": "To install the service, unpack the archive…",
    "metadata": {"team": "platform", "kind": "runbook"}
  }],
  "notes": ["…"]
}
```

`filter` is exact string equality on every key given — an AND, never an OR — and
a passage missing the key is excluded rather than assumed to match. There is no
expression language, deliberately: a filter a model can compose from a document
it just read is a filter that can be made to do something surprising.

**An empty result says why.** `collection_chunks` tells you whether the
collection was empty, and `notes` distinguishes "nothing passed the filter" from
"nothing scored above `min_score`". A missing collection is an *error*, not an
empty list — a caller cannot tell a typo'd collection name from a genuine miss
unless the plugin says so.

Scores are cosine similarities in `[-1, 1]` and are **not comparable between
embedding models**. Tune `min_score` against your own results; do not copy a
number from anywhere, including this README.

### `delete`

Scope is required and has no default, because deleting an index is cheap to do
and impossible to undo:

```bash
# every passage of two documents
-d '{"collection":"handbook","scope":"documents","document_ids":["docs/manual.md"]}'
# the whole collection, file included
-d '{"collection":"handbook","scope":"collection"}'
```

`scope: "documents"` with no `document_ids` is an error, not a
delete-everything.

### `stats` and `status`

`stats` touches no network and reports what is actually stored — per collection:
chunk and document counts, the pinned embedding model and its dimensions,
estimated memory, real on-disk log size, live and dead log lines, and the
sources held (capped at 100). `approx_memory_bytes` estimates the payload —
text, metadata, vectors — and is **not** process memory; a note inside every
response says so, so the figure cannot be quoted without its caveat.

`status` sends one probe embedding and reports the endpoint's real state, the
effective configuration, and every collection's model pin. An unreachable
backend is a *result*, not a failure — reporting it is this tool's job.

---

## Configuration

`[plugin.settings]` is **not** used, and this plugin deliberately declares no
`config_schema`. The host contract is explicit that settings are stored and
validated by the host and never delivered to the plugin process — so a
schema-backed chunk-size control in the console would move and change nothing,
and you would find out a corpus later. Everything comes from the launch
contract instead.

Precedence, highest first: **command-line flag → environment variable →
`[[plugin]].url` (endpoint only) → built-in default**.

| Flag | Environment variable | Default | Meaning |
| --- | --- | --- | --- |
| `--data-dir` | `TDCC_VECTOR_STORE_DATA_DIR` | `~/.tdcc/vector-store` | The only directory this plugin writes to. |
| `--embeddings-url` | `TDCC_VECTOR_STORE_EMBEDDINGS_URL` | `http://127.0.0.1:9337/v1` | Base URL or full endpoint; `/embeddings` is appended if absent. Also accepts `[[plugin]].url`. |
| `--embedding-model` | `TDCC_VECTOR_STORE_EMBEDDING_MODEL` | *(unset)* | Sent as `model`, and pinned into every collection. Omitted from the request when unset — most servers reject that, and startup warns. |
| `--chunk-chars` | `TDCC_VECTOR_STORE_CHUNK_CHARS` | `1200` | Target passage size, in characters. Minimum 64. |
| `--chunk-overlap-chars` | `TDCC_VECTOR_STORE_CHUNK_OVERLAP_CHARS` | `200` | Overlap, rounded to whole blocks. Must be below the target. `0` disables it and warns. |
| `--max-chunk-chars` | `TDCC_VECTOR_STORE_MAX_CHUNK_CHARS` | `2400` | Hard ceiling. Must be at least the target. |
| `--max-collections` | `TDCC_VECTOR_STORE_MAX_COLLECTIONS` | `64` | |
| `--max-chunks-per-collection` | `TDCC_VECTOR_STORE_MAX_CHUNKS_PER_COLLECTION` | `50000` | The brute-force ceiling. Raising it warns at startup. |
| `--max-store-bytes` | `TDCC_VECTOR_STORE_MAX_STORE_BYTES` | `536870912` | Estimated payload across every collection, minimum 1 MiB. |
| `--max-document-bytes` | `TDCC_VECTOR_STORE_MAX_DOCUMENT_BYTES` | `1048576` | Largest single document per `upsert`. |
| `--max-documents-per-call` | `TDCC_VECTOR_STORE_MAX_DOCUMENTS_PER_CALL` | `64` | |
| `--default-top-k` | `TDCC_VECTOR_STORE_DEFAULT_TOP_K` | `8` | Results when a caller does not ask. Hard ceiling 100, whatever a caller asks for. |
| `--embed-batch-size` | `TDCC_VECTOR_STORE_EMBED_BATCH_SIZE` | `32` | Texts per embeddings request, 1–512. |
| `--request-timeout-seconds` | `TDCC_VECTOR_STORE_REQUEST_TIMEOUT_SECONDS` | `30` | 1–600. |
| `--allow-remote-embeddings` | `TDCC_VECTOR_STORE_ALLOW_REMOTE_EMBEDDINGS` | off | Permit a non-loopback endpoint. See below. |
| *(no flag)* | `TDCC_VECTOR_STORE_API_KEY` | *(unset)* | `Authorization: Bearer …` for the embeddings endpoint. |

A mistyped flag is a startup error, not a silent fallback:

```text
Error: unknown option: --chunk-char (known options: --data-dir, --embeddings-url, …)
```

So is a combination that cannot work — caught at startup rather than on your
first ingest:

```text
Error: chunk overlap must be smaller than the target chunk size, otherwise a chunk
       would repeat its predecessor and the split would not advance
       (see --chunk-chars, --chunk-overlap-chars, --max-chunk-chars)
```

The effective configuration is echoed to stderr at startup and returned by
`status`. The API key never appears in either — it is not a field on the printed
struct, and its type prints as `ApiKey(<redacted>)`.

---

## Security

**Blast radius.**

| Touches | What, exactly |
| --- | --- |
| Filesystem | **Only** `--data-dir` (default `~/.tdcc/vector-store`). It creates `collections/`, writes one `.jsonl` per collection, and reads them back. |
| Network | One outbound request shape, to the one configured embeddings URL, carrying the text being embedded. No other host, ever. |
| Subprocesses | None. |
| Mesh | None. No channel and no event is declared, and delivery is allowlist-based. |
| Listening sockets | None. The host owns every projection. |

**It does not read your documents off disk.** `upsert` takes text; `source` is a
label that is recorded and returned but never opened. That is the narrowest
useful permission for a plugin whose whole job is to be handed content, it
removes an entire class of traversal bug, and it composes with
[`code-context`](../code-context), which does own a root and does read files.

**Loopback by default.** Every `upsert` sends whole documents to the embeddings
endpoint and every `query` sends the question. A non-loopback endpoint is
refused at startup unless you pass `--allow-remote-embeddings`:

```text
Error: refusing to send document text to the non-loopback embeddings endpoint
       https://api.example.com/v1/embeddings: pass --allow-remote-embeddings to allow it
```

The check parses the URL rather than matching strings, so
`http://127.0.0.1@evil.example/v1` does not sneak through. It is a **syntactic**
guard against pasting a public endpoint by accident, not a defence against a
hostile DNS server.

**No credentials in configuration.** A URL carrying a username or password is
refused with a pointer to `TDCC_VECTOR_STORE_API_KEY`, because `[[plugin]].args`
and `[[plugin]].url` are written into `~/.tdcc/config.toml` in plaintext and show
up in process listings. There is no flag that takes a key — a test asserts that
no flag ever will. The key is never logged, never in an error, and never in a
tool result.

**Everything a caller can grow is bounded**: collections, passages per
collection, total bytes, document size, documents per call, metadata entries and
their key and value lengths, identifier lengths, query length, and `top_k`.

**Failures are loud.** A `query` that cannot embed returns an error, not an
empty result list. An outage and a genuinely empty index look identical from the
outside, and the difference is the whole value of the tool. `status` is the one
exception — reporting an unreachable backend is its job, so it returns that as a
result.

**Delete has no default scope**, and `documents` requires the ids.

**A corrupt log is a startup failure, not a silent partial load.** A collection
whose log is unreadable in the middle refuses to load, names the line, and says
the file was not modified — quietly dropping half a collection would be worse.

---

## Building against the SDK

`tdcc-plugin` is **not published to crates.io under that name** — it was renamed
from `mesh-llm-plugin` and lives in a private repository, so a plain
`tdcc-plugin = "0.72.1"` does not resolve. This crate therefore points at a local
checkout:

```toml
tdcc-plugin = { version = "0.72.1", path = "../../../tdcc-mesh/crates/tdcc-plugin" }
```

which expects the two repositories side by side:

```text
<parent>/tdcc-plugins/plugins/vector-store/   this crate
<parent>/tdcc-mesh/crates/tdcc-plugin/        the SDK
```

**When the SDK is published, delete the `path` key and keep the `version`.**
Nothing else in this crate changes. If your checkout lives elsewhere, either
adjust the path or add a patch to a `.cargo/config.toml` above the crate:

```toml
[patch.crates-io]
tdcc-plugin = { path = "/absolute/path/to/tdcc-mesh/crates/tdcc-plugin" }
```

Then:

```bash
cargo test
cargo build --release
```

`tdcc-plugin` builds its protocol types with `prost-build`, so the first build
downloads a vendored `protoc`. No system protobuf compiler is required.

### Tests

162 tests, none of which need internet access.

The pure logic is unit tested directly beside the code it covers: chunking
(heading stacks, fence handling, line spans, overlap, every degenerate input),
similarity maths, collection-name validation and path containment, configuration
parsing and precedence, embeddings response parsing, and the store's replay,
tombstone, compaction and bounds behaviour against real temporary directories.

`src/end_to_end.rs` stands up a **stub embeddings server on loopback** — a real
HTTP server, not a mocked client — and drives the whole path through it: ingest a
structured document, restart the process, query it in different words, check the
citation against the source lines, prove a metadata filter narrows, prove
collections cannot see each other, prove a swapped embedding model is refused,
prove a dead backend fails the query instead of reporting no matches, and prove
a failed ingest stores nothing.

The stub controls the *geometry* so the tests are about the store's behaviour.
**No test here measures, or claims anything about, the retrieval quality of any
real embedding model.** That is a property of your corpus and your embedder.

Two areas the suite does not cover, because they need a live host: the
initialize handshake and manifest projection, and concurrent tool calls against
one collection from separate host requests.

---

## Package and install locally

macOS or Linux, from this directory:

```bash
cargo build --release
rm -rf target/package
mkdir -p target/package/vector-store
cp target/release/vector-store target/package/vector-store/vector-store
cp plugin.toml target/package/vector-store/plugin.toml
cp README.md   target/package/vector-store/README.md
tar -C target/package -czf target/vector-store-0.1.0-local.tar.gz vector-store

tdcc plugins install --archive ./target/vector-store-0.1.0-local.tar.gz \
  --name vector-store --version 0.1.0
tdcc plugins info vector-store
```

Windows uses `vector-store.exe` and a `.zip` whose single top-level directory is
`vector-store/`:

```powershell
cargo build --release
Remove-Item -Recurse -Force target\package -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force target\package\vector-store | Out-Null
Copy-Item target\release\vector-store.exe target\package\vector-store\vector-store.exe
Copy-Item plugin.toml target\package\vector-store\plugin.toml
Copy-Item README.md   target\package\vector-store\README.md
Compress-Archive -Path target\package\vector-store `
  -DestinationPath target\vector-store-0.1.0-local.zip -Force

tdcc plugins install --archive .\target\vector-store-0.1.0-local.zip `
  --name vector-store --version 0.1.0
```

This plugin declares neither a config schema nor a web UI, so
`--print-package-manifest` emits `{}` and `plugin-manifest.json` may be left out
of the archive entirely.

Set `TDCC_PLUGIN_DIR` to an empty directory first if you do not want it landing
in your real plugin store.

## Run it

```toml
# config.toml
version = 1

[[plugin]]
name = "vector-store"
enabled = true
args = [
  "--embeddings-url", "http://127.0.0.1:11434/v1",
  "--embedding-model", "nomic-embed-text",
]
```

```bash
tdcc client --port 9337 --console 3131 --config ./config.toml
```

Confirm the backend before trusting anything else:

```bash
curl --fail -X POST http://127.0.0.1:3131/api/plugins/vector-store/tools/status \
  -H 'Content-Type: application/json' -d '{}'
```

`"embeddings": {"reachable": true, "dimensions": 768, "latency_ms": 9}` means you
are ready. `"reachable": false` carries the reason verbatim and names the
setting that fixes it.

Running the binary directly, outside a host, fails immediately:

```text
Error: TDCC_PLUGIN_ENDPOINT is not set for plugin process
```

That is correct — the host owns the control endpoint and passes it in.

### Tuning before you ingest

1. `preview_chunks` a representative document. No backend needed.
2. Look at the passages. Are facts being separated from their headings? Are code
   blocks intact? Is anything carrying a `split_reason` of `hard`?
3. Adjust `--chunk-chars` and `--chunk-overlap-chars` and preview again.
4. Only then ingest, and check `stats` for the chunk count you expected.

If retrieval disappoints afterwards, `stats` before blame: an empty collection, a
model pin you forgot about, or a filter that excludes everything all look
identical to "the embeddings are bad".

---

## Limitations, stated plainly

- **It never reads your files.** `source` is a label. Something else — your
  agent, `code-context`, a script — has to read the text and pass it in. That is
  a deliberate limit on blast radius, not an oversight, but it does mean this
  plugin alone cannot index a directory.
- **Brute force has a ceiling.** Past a few tens of thousands of passages per
  collection, queries get linearly slower and you want a real vector database.
  The cap is enforced at 50 000 by default and says so when you hit it.
- **Text only.** No images, no PDFs, no Office documents. Extract text first.
- **Markdown is the structure it understands.** HTML, reStructuredText and
  AsciiDoc fall back to paragraph splitting, which is decent but loses the
  breadcrumb. Source code is chunked as paragraphs, not as functions —
  [`code-context`](../code-context) is the tool that understands symbols.
- **Sentence splitting is naive.** "e.g." and "Dr. Smith" produce a boundary.
  That costs a slightly early cut inside an already-oversized paragraph, which
  is much cheaper than the abbreviation dictionary it would take to avoid.
- **Scores are not calibrated and not portable.** A 0.8 from one embedder means
  nothing about a 0.8 from another, and neither is a probability.
- **Retrieval quality is not this plugin's to claim.** It is a property of your
  corpus, your chunk sizes, and your embedding model. Nothing here has been
  benchmarked against a retrieval dataset, and no number in this document is a
  quality measurement.
- **There is no reranker and no hybrid search.** Pure dense retrieval, one
  vector per passage. Keyword-heavy queries — an exact error code, a function
  name — are the known weak case; `source_prefix` and metadata filters are the
  blunt tools available for it.
- **The store is per-node.** It is not shared or replicated across mesh peers.
- **One process per data directory.** Two `tdcc` instances pointed at the same
  `--data-dir` will corrupt each other's logs; there is no lock file.

## License

Apache-2.0, matching this repository.
