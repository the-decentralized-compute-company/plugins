# semantic-cache

Cache completions by **meaning**, so a reworded but equivalent prompt reuses
the answer instead of paying for it again — and report the saving as a number
you can check.

```text
"how do I reset my password?"                 → model call, answer stored
"what is the procedure to reset my password?" → cache hit, 0 tokens spent
"how do I reset my passphrase?"               → miss (0.94 < 0.95 threshold)
"what is the capital of France?"              → miss
```

The last two lines matter as much as the second. A semantic cache that hits too
eagerly returns a *confidently wrong answer*, which is far worse than a miss.
Everything about the defaults here is chosen to fail towards the miss.

---

## Before you start: this plugin needs an embeddings endpoint

To compare prompts by meaning, something has to turn them into vectors. This
plugin does not ship a model; it calls an **OpenAI-compatible
`POST /v1/embeddings`**.

**The TDCC node does not currently expose one.** Its OpenAI frontend on
`127.0.0.1:9337` serves `/v1/models`, `/v1/chat/completions`, `/v1/completions`
and `/v1/responses`; embeddings are listed as out of scope in that component's
own documentation. So out of the box you must point this plugin at an
embeddings server:

```toml
[[plugin]]
name = "semantic-cache"
args = ["--embeddings-url", "http://127.0.0.1:11434/v1",
        "--embedding-model", "nomic-embed-text"]
```

Any local server that exposes an OpenAI-compatible `/v1/embeddings` works —
Ollama, `llama-server --embeddings`, LM Studio, and vLLM serving an embedding
model are the usual choices. Do not take that list as verified for your
version; **run the `status` tool, which sends one real probe request and tells
you the truth**:

```bash
curl --fail -X POST http://127.0.0.1:3131/api/plugins/semantic-cache/tools/status \
  -H 'Content-Type: application/json' -d '{}'
```

The default endpoint is `http://127.0.0.1:9337/v1` — the node itself — so that
the day the node grows an embeddings route this plugin works with no
configuration. Until then that default fails, loudly and legibly, rather than
pretending to be a cache that simply never hits.

---

## How the matching works

Two stages, and the split between them is the whole safety story.

**Stage 1 — the bucket. Exact match, no exceptions.** A SHA-256 digest of every
field that changes what the model would say:

| In the bucket | Why |
| --- | --- |
| completion model id | a different model gives a different answer |
| embedding model id | vectors from two embedders are not comparable |
| `temperature`, `top_p` | different sampling, different output |
| the tool set | a model with tools answers differently from one without |
| **the entire message prefix** | this is where the system prompt lives, and where "and the second one?" gets its meaning |
| `extra_key` | your slot for `response_format`, `seed`, a prompt-template version, a corpus id |

Requests whose buckets differ can never see each other's answers. Not "are
unlikely to" — cannot.

**Stage 2 — the neighbour. Semantic match, within one bucket.** Only the
trailing user message is embedded and compared, by cosine similarity, against
the entries already in that bucket. The highest scorer wins or nothing does.

Two guards sit on stage 2:

- **The threshold**, default `0.95`. Sentence embeddings place negations and
  near-antonyms ("how do I *enable* X" / "how do I *disable* X") very close
  together. A threshold that looks generous on a paraphrase set will also merge
  pairs that mean the opposite.
- **The length guard**, default ratio `2.0`. A paraphrase is roughly as long as
  what it paraphrases. When a two-word question scores 0.96 against a 600-word
  one, the score is measuring shared topic, not shared meaning.

Every miss reports `best_similarity` — the score that lost — so you can tune the
threshold against your own traffic instead of guessing.

### What is never cached

| Refused | Reason |
| --- | --- |
| `is_error: true` | caching an error serves it back for the whole TTL, long after the transient cause is gone |
| an empty completion | nothing to serve |
| `finish_reason` other than `stop` | `length` is truncated, `tool_calls` is a request to go and do something, `content_filter` is a refusal that may not apply to another wording |
| `temperature` above `--max-temperature` | past a point the caller is explicitly asking for variance, and a frozen sample stops being a cache |
| an entry larger than the whole byte budget | it could only be stored by evicting everything else |

Each refusal is a *result*, not an error: `store` returns
`{"stored": false, "rejected": {"reason": "…"}}` and counts it, so `stats`
shows you why a cache is not filling up.

### Freshness

Entries carry a TTL, default **one hour**. A cache that answers "what is
deployed right now" from last week is a correctness bug, not a saving. Raise
`--ttl-seconds` for stable reference-style workloads; pass a short
`ttl_seconds` per `store` call for anything that reads current state.

---

## Storage: local, bounded, in memory only

**Nothing is written to disk. Ever.**

That is deliberate for software that runs on hardware other people contributed
to a mesh. Every entry holds a user's prompt, the model's answer, and an
embedding of the prompt — the most sensitive text passing through the node.
Persisting it would leave a searchable transcript on a contributor's disk that
outlives the process they can see in their process list, and it would quietly
break the "restart clears it" assumption every operator already has. Losing the
cache on restart costs one cold period; the alternative costs somebody else's
privacy.

Two bounds, both enforced on every insert:

- `--max-entries` (default `1000`) — predictable row count. With a
  1536-dimension embedder that is roughly 6 MiB of vectors plus the stored
  text.
- `--max-bytes` (default 64 MiB) — an estimate of payload bytes held. One
  30 MiB completion must not occupy the cache just because it is a single row.

**Eviction is expired-first, then least-recently-used.** LRU rather than LFU
because a cache like this earns its keep on a hot working set of
repeatedly-asked questions, and LFU keeps a once-popular entry resident long
after the topic has moved on. Choosing the victim is an O(n) scan over at most
`max_entries` rows — microseconds at the default — which is cheaper than
maintaining an intrusive list for no measurable gain.

Storing the same wording twice **replaces** the entry rather than adding a row,
so one hot prompt cannot evict the rest of the cache.

---

## Tools

All five are on the host MCP endpoint namespaced as `semantic-cache.<tool>`,
and callable over HTTP at
`POST /api/plugins/semantic-cache/tools/<tool>`.

| Tool | Does | Network |
| --- | --- | --- |
| `lookup` | find a cached answer for a prompt | one embedding call, unless the wording is an exact match |
| `store` | record a completion for later reuse | one embedding call, only after the cheap rules pass |
| `stats` | **the number**: hit rate, tokens saved, entries, evictions, config | none |
| `status` | is the embeddings backend actually reachable? | one probe embedding call |
| `purge` | drop `expired`, one `model`, or `all` | none |

`lookup`, `store` and `stats` are also mounted as HTTP routes at
`/api/plugins/semantic-cache/http/{lookup,store,stats}` — a proxy sitting in
front of `:9337` is the natural place to use this cache, and a proxy speaks
HTTP, not MCP.

### `lookup`

```bash
curl --fail -X POST http://127.0.0.1:3131/api/plugins/semantic-cache/http/lookup \
  -H 'Content-Type: application/json' -d '{
    "model": "qwen3-8b",
    "temperature": 0,
    "messages": [
      {"role": "system", "content": "Answer in one sentence."},
      {"role": "user",   "content": "what is the procedure to reset my password?"}
    ]
  }'
```

A hit:

```json
{
  "hit": true,
  "match_kind": "semantic",
  "similarity": 0.981,
  "threshold": 0.95,
  "completion": "Use the reset link on the sign-in page.",
  "entry": {
    "entry_id": 1,
    "age_seconds": 44,
    "expires_in_seconds": 3556,
    "previous_hits": 0,
    "cached_query": "how do I reset my password?"
  },
  "tokens_saved": {"prompt_tokens": 120, "completion_tokens": 45, "total_tokens": 165},
  "bucket": "6f4c…"
}
```

A miss carries the diagnosis instead:

```json
{"hit": false, "miss_reason": "below_threshold", "best_similarity": 0.94, "threshold": 0.95}
```

`miss_reason` is one of `bucket_empty` (nothing comparable was ever stored),
`below_threshold`, `length_guard`, or `temperature_above_limit`.

Pass `min_similarity` to be **stricter for one call**. Values below the
operator's configured floor are clamped up to it: a caller — which on an MCP
surface is frequently a language model — may tighten the node's risk budget,
never widen it.

### `store`

Same key fields, plus the answer:

```bash
curl --fail -X POST http://127.0.0.1:3131/api/plugins/semantic-cache/http/store \
  -H 'Content-Type: application/json' -d '{
    "model": "qwen3-8b",
    "temperature": 0,
    "messages": [
      {"role": "system", "content": "Answer in one sentence."},
      {"role": "user",   "content": "how do I reset my password?"}
    ],
    "completion": "Use the reset link on the sign-in page.",
    "finish_reason": "stop",
    "prompt_tokens": 120,
    "completion_tokens": 45
  }'
```

The token counts are what `stats` adds up. Omit them if you do not have them —
the saving is then undercounted rather than guessed.

### `stats` — the measurement

```bash
curl --fail http://127.0.0.1:3131/api/plugins/semantic-cache/http/stats
```

The numbers below are the ones this crate's own end-to-end test produces
against a **stub** embedder (`src/end_to_end.rs`): one entry stored, then the
same question reworded, then asked verbatim, then an unrelated question. Each
value is pinned by an assertion in that test, so this example cannot drift away
from the code. They show the shape and the arithmetic. **They are not a
benchmark, and they say nothing about how well any real embedding model
paraphrases.** Your hit rate is a property of your workload.

```jsonc
// abridged; the real response also carries eviction and purge counters
{
  "counters": {
    "lookups": 3,
    "hits_exact": 1,
    "hits_semantic": 1,
    "misses_by_reason": {"below_threshold": 1},
    "stores_accepted": 1,
    "embedding_calls": 3,
    "embedding_failures": 0,
    "prompt_tokens_saved": 240,
    "completion_tokens_saved": 90
  },
  "hits": 2,
  "misses": 1,
  "hit_rate": 0.6666666666666666,
  "tokens_saved_total": 330,
  "entries": 1,
  "buckets": 1,
  "approx_bytes": 338,
  "config": { "...": "the effective configuration, minus the API key" },
  "notes": ["tokens_saved_total sums the token counts the caller reported when each entry was stored, …"]
}
```

`approx_bytes` is small here only because the stub returns two-dimensional
vectors; a 768-dimension embedder adds roughly 3 KiB per entry.

Two caveats travel *inside* the response, in `notes`, so the figure is never
quoted without them:

- `tokens_saved_total` sums the token counts the caller reported **at store
  time**. For an exact hit that is precise. For a reworded hit the incoming
  prompt tokenizes slightly differently, so treat the prompt half as an
  estimate.
- `approx_bytes` estimates the payload held — prompt, completion, vector,
  per-entry overhead. It is not process memory.

---

## Configuration

`[plugin.settings]` is **not** used, and this plugin deliberately declares no
`config_schema`. The host contract is explicit that settings are stored and
validated by the host and never delivered to the plugin process — so a
schema-backed similarity slider in the console would be a control that moves
and changes nothing. Rather than ship that, everything comes from the launch
contract.

Precedence, highest first: **command-line flag → environment variable →
`[[plugin]].url` (endpoint only) → built-in default**.

| Flag | Environment variable | Default | Meaning |
| --- | --- | --- | --- |
| `--embeddings-url` | `TDCC_SEMANTIC_CACHE_EMBEDDINGS_URL` | `http://127.0.0.1:9337/v1` | Base URL or full endpoint. `/embeddings` is appended if absent. Also accepts `[[plugin]].url`. |
| `--embedding-model` | `TDCC_SEMANTIC_CACHE_EMBEDDING_MODEL` | *(unset)* | Sent as `model`. Omitted from the request when unset — most servers reject that, and startup warns about it. |
| `--min-similarity` | `TDCC_SEMANTIC_CACHE_MIN_SIMILARITY` | `0.95` | Cosine threshold, `0.0`–`1.0`. |
| `--ttl-seconds` | `TDCC_SEMANTIC_CACHE_TTL_SECONDS` | `3600` | `0` disables expiry entirely (warned about at startup). |
| `--max-entries` | `TDCC_SEMANTIC_CACHE_MAX_ENTRIES` | `1000` | Row bound. |
| `--max-bytes` | `TDCC_SEMANTIC_CACHE_MAX_BYTES` | `67108864` | Payload-byte bound, minimum 64 KiB. |
| `--max-temperature` | `TDCC_SEMANTIC_CACHE_MAX_TEMPERATURE` | `1.0` | Requests hotter than this are neither stored nor served. `0` restricts caching to greedy decoding. |
| `--max-length-ratio` | `TDCC_SEMANTIC_CACHE_MAX_LENGTH_RATIO` | `2.0` | Reject a neighbour this many times longer or shorter than the query. |
| `--request-timeout-seconds` | `TDCC_SEMANTIC_CACHE_REQUEST_TIMEOUT_SECONDS` | `10` | Embedding call timeout, `1`–`300`. |
| `--allow-remote-embeddings` | `TDCC_SEMANTIC_CACHE_ALLOW_REMOTE_EMBEDDINGS` | off | Permit a non-loopback endpoint. See below. |
| *(no flag)* | `TDCC_SEMANTIC_CACHE_API_KEY` | *(unset)* | `Authorization: Bearer …` for the embeddings endpoint. |

A mistyped flag is a startup error, not a silent fallback:

```text
Error: unknown option: --min-similarty (known options: --embeddings-url, --embedding-model, …)
```

The effective configuration is echoed to stderr at startup and returned by
`stats` and `status`. The API key never appears in either — it is not a field
on the printed struct, and its type prints as `ApiKey(<redacted>)`.

---

## Security

**Blast radius.** One outbound HTTP request shape, to one operator-configured
URL, carrying one thing: the text of the trailing user message. No filesystem
access, no subprocess, no listening socket, no second host.

**Loopback by default.** Every lookup and every store sends prompt text to the
embeddings endpoint. A non-loopback endpoint is refused at startup unless you
pass `--allow-remote-embeddings`:

```text
Error: refusing to send prompts to the non-loopback embeddings endpoint
       https://api.example.com/v1/embeddings: pass --allow-remote-embeddings to allow it
```

The check parses the URL rather than matching strings, so
`http://127.0.0.1@evil.example/v1` does not sneak through (it is rejected
outright — see below). It is a **syntactic** guard against pasting a public
endpoint by accident, not a defence against a hostile DNS server.

**No credentials in configuration.** A URL carrying a username or password is
refused with a pointer to `TDCC_SEMANTIC_CACHE_API_KEY`, because
`[[plugin]].args` and `[[plugin]].url` are written into `~/.tdcc/config.toml`
in plaintext and show up in process listings. The key is environment-only, is
never logged, and is never included in any tool result.

**Failures are loud.** A `lookup` that cannot reach the embedder returns an
error, not a miss. A silent miss is the wrong kindness: an outage and a cold
cache look identical from the outside, and the operator would be left believing
the cache simply was not helping. `status` is the one exception — reporting an
unreachable backend is its job, so it returns that as a result.

**Purge has no default scope.** Clearing a cache is cheap to do and impossible
to undo, so the caller has to name `expired`, `model`, or `all`.

---

## Building

`tdcc-plugin` is **not published to crates.io under that name** — it was
renamed from `mesh-llm-plugin` and lives in a private repository, so a plain
`tdcc-plugin = "0.72.1"` does not resolve. This crate therefore points at a
local checkout:

```toml
tdcc-plugin = { version = "0.72.1", path = "../../../tdcc-mesh/crates/tdcc-plugin" }
```

which expects the two repositories side by side:

```text
<parent>/tdcc-plugins/plugins/semantic-cache/   this crate
<parent>/tdcc-mesh/crates/tdcc-plugin/          the SDK
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

98 tests, none of which need internet access. The pure logic — key
canonicalization, the store
policies, similarity maths, eviction, configuration parsing — is unit tested
directly. `src/end_to_end.rs` stands up a **stub embeddings server on
loopback** (a real HTTP server, not a mocked client) that places prompts on the
unit circle by keyword, and drives the full store → lookup → hit path through
it. That is what proves the claims in this README: the reworded hit, the exact
hit costing no embedding call, the near-neighbour refused at 0.94, the model
and system-prompt isolation, and the arithmetic behind `tokens_saved_total`.

The stub controls the *geometry* so the tests are about the cache's behaviour.
No test here measures, or claims anything about, the quality of a real
embedding model.

---

## Package and install locally

macOS or Linux, from this directory:

```bash
rm -rf target/package
mkdir -p target/package/semantic-cache
cp target/release/semantic-cache target/package/semantic-cache/semantic-cache
cp plugin.toml target/package/semantic-cache/plugin.toml
cp README.md   target/package/semantic-cache/README.md
tar -C target/package -czf target/semantic-cache-0.1.0-local.tar.gz semantic-cache

tdcc plugins install --archive ./target/semantic-cache-0.1.0-local.tar.gz \
  --name semantic-cache --version 0.1.0
tdcc plugins info semantic-cache
```

Windows uses `semantic-cache.exe` and a `.zip` whose single top-level directory
is `semantic-cache/`:

```powershell
Compress-Archive -Path target\package\semantic-cache `
  -DestinationPath target\semantic-cache-0.1.0-local.zip -Force
tdcc plugins install --archive .\target\semantic-cache-0.1.0-local.zip `
  --name semantic-cache --version 0.1.0
```

This plugin declares neither a config schema nor a web UI, so
`--print-package-manifest` emits `{}` and `plugin-manifest.json` may be left
out of the archive entirely.

Set `TDCC_PLUGIN_DIR` to an empty directory first if you do not want it landing
in your real plugin store.

## Run it

```toml
# config.toml
version = 1

[[plugin]]
name = "semantic-cache"
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
curl --fail -X POST http://127.0.0.1:3131/api/plugins/semantic-cache/tools/status \
  -H 'Content-Type: application/json' -d '{}'
```

`"embeddings": {"reachable": true, "dimensions": 768, "latency_ms": 9}` means
you are ready. `"reachable": false` carries the reason verbatim.

Running the binary directly, outside a host, fails immediately:

```text
Error: TDCC_PLUGIN_ENDPOINT is not set for plugin process
```

That is correct — the host owns the control endpoint and passes it in.

## Measure it on your own workload

The point of this plugin is a number you did not have to take on faith:

1. `purge` with `{"scope": "all"}` to start from empty.
2. Run your workload through your gateway or agent, calling `lookup` before
   each model call and `store` after each successful one, passing the real
   token counts.
3. Call `stats`.

`hit_rate` and `tokens_saved_total` are then measurements of *your* traffic. If
the hit rate is disappointing, look at `best_similarity` on the misses before
lowering `--min-similarity`: a cluster just under the threshold means the
threshold is worth revisiting, while a spread from 0.3 to 0.8 means the
workload has little genuine repetition and a lower threshold would only buy
wrong answers.

---

## Limitations, stated plainly

- **It does not intercept anything.** The host owns `/v1/chat/completions`;
  this plugin cannot transparently wrap it. Something — your agent, or a proxy
  in front of `:9337` — has to call `lookup` and `store`. That is why the HTTP
  routes exist.
- **Text content only.** Multimodal message parts are rejected rather than
  silently compared on their captions. A text embedding does not cover an
  image.
- **Streaming is not modelled.** Cache the assembled completion after the
  stream ends, and only if it finished with `stop`.
- **Negation is the known failure mode of every embedding model**, and a
  threshold is a blunt instrument against it. 0.95 makes it unlikely, not
  impossible. If your workload is full of "do X" / "do not X" pairs, raise the
  threshold and watch `best_similarity`.
- **The cache is per-process.** It is not shared across mesh peers, and it
  starts empty after every restart.
- **`tokens_saved_total` trusts the caller.** It sums what was reported at
  store time; it cannot verify those counts.

## License

Apache-2.0, matching this repository.
