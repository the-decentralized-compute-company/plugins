# model-mirror

Everyone on a mesh pulling the same 20 GB model from the same origin is the
obvious waste to remove. `model-mirror` lets one node hold a model cache and
serve it to its peers, so an artifact crosses the origin's link once instead of
once per node.

The hard part is not moving bytes. It is being able to prove that the bytes a
peer received are the bytes it asked for — because a mirror that can serve a
substituted model is a supply-chain attack with extra steps. Everything below
is arranged around that.

- **Integrity:** every path in and out is gated on a SHA-256 digest, on write
  and on read, and a mismatch is loud and refused.
- **Identity:** artifacts are named with TDCC's own canonical ref,
  `org/repo@revision/file`, not a parallel scheme.
- **Consent:** the operator sets a disk cap, a bandwidth cap, and an import
  root. Without a disk cap the plugin holds and serves nothing.

---

## The integrity contract

### Verify on write

| Entry point | What happens before anything is published |
| --- | --- |
| `import` | The source file is streamed through SHA-256 while it is copied. If an `expected_sha256` was supplied and does not match, the copy is deleted and the call fails. If the file changed size mid-copy, same. |
| `finalize_receive` | The complete staged file is digested and compared to the digest the caller pinned at `begin_receive`. On mismatch the staged bytes are **deleted** — not quarantined — because nothing ever trusted them, and a plausible-looking partial file must not survive for a later resume to adopt. |

Nothing reaches the blob store without a full-file digest match first.

### Verify on read

Three layers, cheapest first:

1. **Tripwire, every serve.** Recorded size and mtime are compared against the
   file on disk. A mismatch quarantines the artifact and fails the read. This
   costs one `stat`, and catches the ordinary case of "something replaced the
   file".
2. **Full re-digest, rate limited.** At the start of a transfer (`offset = 0`),
   if the artifact's last full verification is older than
   `--reverify-after-secs` (default 24 h), it is re-hashed end to end. This is
   what catches a same-length substitution, which the tripwire cannot see.
   Re-digesting on *every* chunk of a 20 GB file would be theatre, so the
   frequency is the operator's dial.
3. **Per-chunk digest, every chunk.** Every `read_chunk` response carries the
   SHA-256 of exactly the bytes it returned, so a receiver verifies
   incrementally instead of trusting a multi-hour stream and finding out at the
   end.

A quarantined artifact stops being served *and* stops being advertised. The
bytes stay on disk, and the recorded size and mtime are left untouched as
evidence, so an operator can look at what happened before running `evict`.

### End to end

The check that actually matters is the receiver's: `finalize_receive` compares
the assembled file against the digest **the receiver** pinned at
`begin_receive`. That check does not trust the serving node at all. A hostile
or compromised mirror can waste your bandwidth; it cannot put a different model
in your cache.

### What a digest here does not prove

It proves the bytes did not change between two points. It does **not** prove
they are the bytes the model's author published.

Hugging Face's file listing does not expose per-file SHA-256 — `model-hf`'s
`list_files` leaves `ModelArtifactFile::sha256` as `None` — so the first digest
for any artifact is computed locally, from a file somebody already downloaded.
Treat a digest learned from a peer as *that peer's claim*. If your supply chain
has to be provable, pin digests out of band and pass them to `import` and
`begin_receive`.

The plugin does not paper over this. `peers` returns a `digest_conflicts` list
naming every disagreement — between two peers, or between a peer and this node —
and `find` sets `peers_disagree` when the holders of an artifact do not agree.
Neither picks a winner. A disagreement means somebody is serving the wrong
bytes, and resolving it is a human decision.

---

## Artifact identity

Artifacts are keyed by the **canonical ref** that TDCC already uses:

```text
org/repo@0c7f4a9…/UD-IQ2_M/GLM-5.1-UD-IQ2_M-00001-of-00006.gguf
└─repo──┘ └revision┘ └────────────── repo-relative file ──────────────┘
```

This is the string produced by `model_ref::format_canonical_ref` and carried on
`model_artifact::ResolvedModelArtifact::canonical_ref` and
`model_hf::HfModelIdentity::canonical_ref`. The plugin depends on `model-ref`
directly, so the selector (`UD-IQ2_M`), the shard-collapsed distribution id
(`GLM-5.1-UD-IQ2_M`), and the display model id (`org/repo:UD-IQ2_M`) are all
derived by the same code the host uses, and a mirror listing reads the same as
a `tdcc` model listing.

The `revision` is always a resolved commit sha, never `main` — that is what the
host puts in a canonical ref, and it is what makes the ref immutable enough to
be a cache key.

> **Path safety.** No caller-supplied string ever becomes a path component.
> On-disk names are `sha256(canonical_ref)` in hex. A peer asking for
> `org/repo@abc/../../../../etc/passwd` gets a 64-character hex key like every
> other request, and either this node holds that artifact or it does not. The
> ref grammar is validated anyway (`..`, empty segments, backslashes, drive
> letters, and control characters are all rejected) so a hostile ref cannot
> propagate through mesh announcements either.

---

## Install and configure

Build it, package it, install it:

```bash
cargo build --release

rm -rf target/package && mkdir -p target/package/model-mirror
cp target/release/model-mirror target/package/model-mirror/model-mirror
cp plugin.toml README.md target/package/model-mirror/
tar -C target/package -czf target/model-mirror-0.1.0-local.tar.gz model-mirror

tdcc plugins install --archive ./target/model-mirror-0.1.0-local.tar.gz \
  --name model-mirror --version 0.1.0
```

On Windows, copy `model-mirror.exe` and build a `.zip` whose single top-level
directory is `model-mirror/`:

```powershell
Compress-Archive -Path target\package\model-mirror `
  -DestinationPath target\model-mirror-0.1.0-local.zip -Force
tdcc plugins install --archive .\target\model-mirror-0.1.0-local.zip `
  --name model-mirror --version 0.1.0
```

Then enable it in `~/.tdcc/config.toml`:

```toml
version = 1

[[plugin]]
name = "model-mirror"
args = [
  "--max-cache-bytes", "250GiB",
  "--serve-bytes-per-minute", "128MiB",
  "--import-root", "/home/dev/.cache/huggingface/hub",
]
```

The plugin declares no `config_schema` or `web_ui`, so its
`plugin-manifest.json` is `{}` and the file may be left out of the archive.
`model-mirror --print-package-manifest` prints it from the same declaration the
runtime serves, if you want it in there anyway.

### Why `args` and not `[plugin.settings]`

Host-owned `[plugin.settings]` values **never reach the plugin process**. There
is no settings field in the launch contract or the initialize handshake; a
plugin only *declares* the schema, and the host stores the values for the
console to render.

Every limit here has to be enforced *inside* this process — a disk cap the
process cannot read is not a disk cap. Declaring these as console settings would
put a control in the UI that looks like it caps disk usage and does not, so this
plugin declares none and reads its limits from `[[plugin]].args`, with
`TDCC_MODEL_MIRROR_*` environment variables as a fallback.

### Limits

| Flag | Environment | Default | Meaning |
| --- | --- | --- | --- |
| `--cache-dir` | `TDCC_MODEL_MIRROR_CACHE_DIR` | `<platform cache dir>/tdcc/model-mirror` | Where this mirror stores its own copy of each artifact. Must be absolute. |
| `--import-root` (repeatable) | `TDCC_MODEL_MIRROR_IMPORT_ROOTS` (path-list) | the Hugging Face hub cache | The only directories `import` may read from. |
| `--max-cache-bytes` | `TDCC_MODEL_MIRROR_MAX_CACHE_BYTES` | **`0`** | Disk this node contributes. `0` means it holds and serves nothing. |
| `--max-chunk-bytes` | `TDCC_MODEL_MIRROR_MAX_CHUNK_BYTES` | `8MiB` (hard ceiling) | Largest single transfer chunk. A caller that names no length gets 1 MiB. |
| `--serve-bytes-per-minute` | `TDCC_MODEL_MIRROR_SERVE_BYTES_PER_MINUTE` | `64MiB` (~8.9 Mbit/s) | Outbound artifact bytes. `0` means unlimited. |
| `--reverify-after-secs` | `TDCC_MODEL_MIRROR_REVERIFY_AFTER_SECS` | `86400` | Re-digest before serving when the last full verification is older than this. |
| `--no-advertise` | — | advertising on | Hold and serve, but announce nothing on the mesh. |

Sizes accept plain bytes, binary suffixes (`KiB`, `MiB`, `GiB`, `TiB`), and
decimal suffixes (`KB`, `MB`, `GB`, `TB`). They mean what they say; disk vendors
and operating systems disagree about this, so the parser refuses to guess and
rejects anything it does not recognise.

**The default of `--max-cache-bytes 0` is deliberate.** An unconfigured mirror
is inert: it starts, reports why it is idle in `status` and in its health
string, and refuses every admit and every serve with a message naming the flag
to set. Contributing someone's disk should be something they typed, not
something they got.

---

## Tools

Projected on the host MCP endpoint as `model-mirror.<name>`, and callable over
HTTP at `POST /api/plugins/model-mirror/tools/<name>`.

### Inspecting

| Tool | Arguments | Returns |
| --- | --- | --- |
| `status` | — | Cache dir, import roots, caps, bytes used, bytes pinned, bytes staged, remaining bandwidth budget, and whether this node is serving at all. |
| `list` | `include_quarantined`, `limit` | Held artifacts with their canonical refs, digests, sizes, states, and pin flags. |
| `peers` | — | What each mesh peer advertises, plus `digest_conflicts`. |
| `find` | `canonical_ref` | Whether this node holds it, which peers advertise it, and whether those peers agree on the digest. |

### Holding

| Tool | Arguments | Notes |
| --- | --- | --- |
| `import` | `path`, `canonical_ref?`, `expected_sha256?`, `pin` | Takes a file already on this disk into the mirror. `path` must resolve inside an import root. `canonical_ref` may be omitted only when `path` sits in a Hugging Face snapshot layout the identity can be read from. |
| `verify` | `canonical_ref` | Full re-digest. A mismatch quarantines. |
| `pin` | `canonical_ref`, `pinned` | Pinned artifacts are never evicted automatically. |
| `evict` | `canonical_ref?`, `reclaim_bytes?`, `force` | Drop one artifact, or drop least-recently-served artifacts until N bytes are free. Refuses pinned artifacts without `force`. |

### Transferring

| Tool | Arguments | Notes |
| --- | --- | --- |
| `read_chunk` | `canonical_ref`, `offset`, `length?` | Serves one range, base64 encoded, with `chunk_sha256` and `artifact_sha256`. |
| `begin_receive` | `canonical_ref`, `expected_sha256`, `total_bytes` | Reserves disk, returns `received_bytes` to resume from, and sets `already_held` when this node has a verified copy already. |
| `receive_chunk` | `canonical_ref`, `offset`, `data_base64`, `chunk_sha256?` | Append-only: `offset` must equal the bytes already staged. |
| `finalize_receive` | `canonical_ref` | Digests and publishes, or discards and fails. |
| `abort_receive` | `canonical_ref` | Throws away a partial transfer. |

### HTTP routes

Read-only, all of them, so a stray `GET` from a console page or a curl loop can
never change what this node holds. Everything that mutates state is an MCP tool.

| Route | Purpose |
| --- | --- |
| `GET /api/plugins/model-mirror/http/status` | Same as `status` |
| `GET /api/plugins/model-mirror/http/inventory` | What this node advertises to peers |
| `GET /api/plugins/model-mirror/http/chunk?canonical_ref=…&offset=…&length=…` | Same as `read_chunk` |

---

## A transfer, end to end

The node that already has the model publishes it:

```bash
curl --fail -X POST http://127.0.0.1:3131/api/plugins/model-mirror/tools/import \
  -H 'Content-Type: application/json' \
  -d '{"path":"/home/dev/.cache/huggingface/hub/models--org--repo/snapshots/0c7f4a9/Qwen3-8B-Q4_K_M.gguf","pin":true}'
```

The identity comes out of the snapshot path; the digest comes out of the copy.
The response carries both, and that digest is what the other node pins.

The node that wants it opens a transfer, loops, and finalizes:

```jsonc
// begin_receive
{"canonical_ref":"org/repo@0c7f4a9/Qwen3-8B-Q4_K_M.gguf",
 "expected_sha256":"9f86d081…","total_bytes":4920315616}
// → {"received_bytes":0,"total_bytes":4920315616,"percent":0.0,"already_held":false}
```

Then, until `eof`: `read_chunk` on the holder at the current offset,
`receive_chunk` on the receiver at the same offset, passing the holder's
`chunk_sha256` straight through so corruption is caught at the chunk. Both sides
report `percent`. If anything drops, call `begin_receive` again — it returns the
resume offset, and a resume survives a process restart because the staging
record is on disk.

```jsonc
// finalize_receive
{"canonical_ref":"org/repo@0c7f4a9/Qwen3-8B-Q4_K_M.gguf"}
// → the artifact is digested against the pinned expected_sha256 and published,
//   or the staged copy is deleted and the call fails with INTEGRITY FAILURE.
```

Throttling is not an error you have to handle specially: when the bandwidth
budget trims a chunk, `read_chunk` returns a *shorter* chunk with
`"throttled": true`, and you ask for the next offset. Only a completely
exhausted budget returns an error, and it carries `retry_after_ms`. An exhausted
budget is never reported as an empty success.

---

## Mesh advertising

One declared channel, `model-mirror.v1`, and two message kinds:

| `message_kind` | Direction | Body |
| --- | --- | --- |
| `inventory_request` | ask | `{}` |
| `inventory` | answer | `{ artifacts: [...], serving, max_chunk_bytes }` |

This node announces its inventory when it joins the mesh and when a peer comes
up, and answers `inventory_request` on demand. It subscribes to exactly two
events, `peer_up` and `peer_down`; delivery is allowlist-based, so declaring
nothing else means receiving nothing else.

Inbound announcements are treated as untrusted input: every advertised ref is
re-parsed with the same validator used for local requests, every advertised
digest must be 64 hex characters, the list is truncated at 512 artifacts, and a
malformed body is ignored rather than raised. Quarantined artifacts are never
advertised.

`--no-advertise` turns the announcements off without turning the mirror off.

---

## Known limits

Stated plainly, because the alternative is you finding out later.

- **Chunks ride the control plane.** Transfers are base64 inside JSON tool
  results, so the wire cost is about 4/3 of the payload and throughput is bounded
  by request round-trips, not by your link. At the 8 MiB ceiling that is fine for
  a background mirror and wrong for a foreground download of a 200 GB model. The
  SDK's `.stream_request()` / `.stream_response()` modifiers declare a
  side-stream body mode, but the declarative handler contract in `tdcc-plugin`
  0.72.1 still returns a serializable value rather than a byte stream, so there
  is no honest way for this plugin to use a side stream today. When the SDK
  exposes one, the transfer tools are the only thing that has to change — the
  digest contract and the on-disk layout do not.
- **Import copies, it does not link.** The mirror owns immutable bytes it can
  re-verify; a hard link into somebody else's cache is not that. So a mirrored
  model that also lives in your Hugging Face cache costs the disk twice. Budget
  `--max-cache-bytes` accordingly, or point `--cache-dir` at a different volume.
- **One operation per artifact at a time.** A second call touching the same
  canonical ref gets a clear `busy` error rather than interleaving with the
  first. Different artifacts proceed in parallel.
- **mtime is a tripwire, not proof.** On filesystems with coarse or absent
  mtime the tripwire degrades to a size check, and a same-length substitution is
  then caught by the re-digest rather than immediately. Lower
  `--reverify-after-secs` if that matters on your storage.
- **No automatic pulling.** This plugin advertises, serves, and accepts. It does
  not decide on its own to go and fetch a model, because deciding what a node
  should hold is a scheduling question that belongs to whatever is orchestrating
  the mesh, not to the cache underneath it.

---

## Blast radius

This runs on other people's hardware. What it can touch:

| Resource | Scope |
| --- | --- |
| **Disk (write)** | Only under `--cache-dir`, only in `blobs/`, `entries/`, and `staging/`, only with names that are hex digests. Bounded by `--max-cache-bytes`. |
| **Disk (read)** | Its own cache, plus files under `--import-root` — and only when an operator calls `import`. Paths are canonicalized before the containment check, so a symlink pointing out of the root resolves out of the root and is refused. |
| **Network** | None of its own. It opens no socket and speaks no protocol; the host owns the control connection, the HTTP surface, and mesh transport. Outbound artifact bytes are capped by `--serve-bytes-per-minute`. |
| **Subprocesses** | None. |
| **Secrets** | None. It reads no tokens and needs no credentials. Everything configurable is a path or a number, passed as an argument or an environment variable. |

The plugin never walks your disk looking for models. It holds exactly what was
imported or received, and advertises exactly what verified.

---

## Building against the SDK

`tdcc-plugin` is **not on crates.io** under that name — it was renamed from
`mesh-llm-plugin` and the `tdcc-mesh` repository is private — so a line like
`tdcc-plugin = "0.72.1"` will not resolve. The same is true of `model-ref`.
Both are path dependencies into a sibling checkout of `tdcc-mesh`:

```toml
tdcc-plugin = { path = "../../../tdcc-mesh/crates/tdcc-plugin" }
model-ref   = { path = "../../../tdcc-mesh/crates/model-ref" }
```

That assumes `tdcc-plugins/` and `tdcc-mesh/` sit next to each other. If your
checkouts live elsewhere, either fix the two paths or add a `[patch]` section
pointing at wherever they are.

**Once the SDK is published**, a public consumer replaces those two lines with
version requirements and deletes nothing else:

```toml
tdcc-plugin = "0.72.1"
model-ref   = "0.72.1"
```

Pin `tdcc-plugin` to a version compatible with the `tdcc` release you target.
The initialize handshake requires an exact protocol-version match, so a host and
a plugin built against mismatched protocol versions refuse to connect loudly at
startup rather than misbehaving later.

`tdcc-plugin` builds its protocol types with `prost-build`, so the first build
downloads a vendored `protoc`. No system protobuf compiler is required.

---

## Testing

```bash
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Everything testable without a running host is covered: ref parsing and its
rejections, digest parsing, the eviction plan, the bandwidth token bucket,
option and byte-size parsing, import-root containment, mesh message handling and
digest-conflict detection, and the full store behaviour against a temporary
directory — including tamper detection, same-length substitution caught by
re-verification, a substituted transfer being discarded rather than published,
resume after interruption, and quarantine surviving a restart.

Running the binary with no host, as expected, fails immediately:

```text
Error: TDCC_PLUGIN_ENDPOINT is not set for plugin process
```

That is correct. The host owns the control endpoint and passes it in through the
launch contract; a plugin must never invent one.

---

## License

Apache-2.0.
