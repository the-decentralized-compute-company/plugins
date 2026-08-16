# pdf-extract

Get the text out of the documents people actually have.

`pdf-extract` reads PDFs from directories the operator configures and exposes
them as five MCP tools: extract text, describe a document, pull out tables, list
what is available, and report the limits in force. Every page comes back with
its page number, so a model can cite `page 4` instead of paraphrasing what it
thinks it saw.

Two things make this harder than it looks, and both are the point of the plugin:

- **A PDF is a page-description format, not a document format.** There is no
  paragraph, no column, no reading order — only instructions that put glyphs at
  coordinates. An extractor that reads the content stream in operator order
  interleaves the columns of a two-column page and produces text no model can
  follow. This one keeps every string's position and recovers the order from
  geometry. See [Reading order](#reading-order).
- **A scanned page contains no text at all.** Returning an empty string for it
  looks exactly like a successful extraction of a blank page, which is the most
  confusing failure in this problem. This plugin labels those pages
  `image_only`, and errors rather than returning nothing. **There is no OCR
  here.** See [Scans and OCR](#scans-and-ocr).

The plugin is confined to configured roots and refuses to read outside them.
That is the part to read carefully — see [Security](#security).

## Tools

All five are projected by the host as `pdf-extract.<name>` on the MCP endpoint
and at `POST /api/plugins/pdf-extract/tools/<name>`.

| Tool | What it does |
| --- | --- |
| `extract_text` | Text of a PDF, whole or by page range, one entry per page with its number, kind, and column count. `layout` picks `auto`, `single`, or `preserve`. |
| `document_info` | Page count, PDF version, size, title/author/dates, and every page classified `text`, `ocr_layer`, `image_only`, or `empty`. `has_extractable_text` says whether `extract_text` will produce anything. |
| `extract_tables` | Tables as rows of cells, with the page number and an occupancy score. Alignment-based; drawn borders are ignored. |
| `list_documents` | The PDFs inside the configured roots. The `path` on each result is exactly the string the other tools take. |
| `status` | Root labels and every limit in force. Touches no file, so it answers when everything else is failing. |

### An example

A two-page invoice: a typed page and a scanned one. The values below are what
`the_readme_example_is_what_the_tools_actually_return` in `src/tools.rs`
asserts, so they cannot drift away from the code that produces them.

```json
{
  "path": "docs/invoice.pdf",
  "layout": "auto",
  "pages_in_document": 2,
  "pages_returned": 2,
  "image_only_pages": [2],
  "truncated": false,
  "notes": [
    "1 of the 2 pages read are images with no text layer and produced nothing. pdf-extract does not do OCR; run the file through an OCR tool first if you need those pages."
  ],
  "pages": [
    {
      "page": 1,
      "kind": "text",
      "columns": 1,
      "characters": 82,
      "text": "ACME Supply Co.\n\nItem Quantity Total\nWidget 2 24.00\nGasket 10 8.50\nFlange 1 119.00"
    },
    {
      "page": 2,
      "kind": "image_only",
      "columns": 1,
      "characters": 0,
      "text": ""
    }
  ]
}
```

Page two is the part that matters. It is not an empty page; it is a page whose
content is a picture, it says so, and the note says what would be needed to read
it.

`extract_tables` on page one:

```json
{
  "path": "docs/invoice.pdf",
  "pages_examined": 1,
  "tables_found": 1,
  "truncated": false,
  "tables": [
    {
      "page": 1,
      "columns": 3,
      "rows": 4,
      "occupancy": 1.0,
      "cells": [
        ["Item", "Quantity", "Total"],
        ["Widget", "2", "24.00"],
        ["Gasket", "10", "8.50"],
        ["Flange", "1", "119.00"]
      ]
    }
  ]
}
```

## Reading order

Reading order is recovered by a recursive XY cut, in `src/layout.rs`. At each
step the region is examined for a vertical corridor of whitespace running its
whole height — a column gutter — and split there if one is found; failing that,
for a horizontal band running its whole width — a block separation — and split
there. When neither exists the region is a leaf: its runs are grouped into lines
by baseline and read top to bottom, left to right.

Trying the vertical cut first is deliberate. A page with a full-width heading
above two columns has no clear gutter at the top level, because the heading
crosses it; the horizontal cut separates heading from body, and the body then
splits into columns. Reversing the order would find a paragraph gap inside one
column and cut the page across both.

### The hard case, and how it is decided

Two columns of prose and a two-column list of terms and definitions look
identical from above — two dense stacks of lines with a corridor between them —
and have to be read in opposite ways. Prose is read one column at a time; a
definition list is read one *row* at a time, or every term is separated from its
definition.

Three guards decide it, and a cut happens only if all three agree:

1. Each side has at least three lines. A two-row label/value pair is never a
   column split.
2. The two sides overlap vertically by at least half their height. A block at
   the top left and one at the bottom right share a corridor and are not
   columns.
3. The region does not read as a table. `crate::tables` answers this, and the
   discriminator is that **a column of prose fills the width of its band and a
   table cell does not** — a wrapped paragraph runs to the edge of its column on
   nearly every line, and `Widget` does not.

Every response reports `columns` per page, so the decision is visible. When
`auto` gets it wrong there are two escapes:

| `layout` | What it does | When |
| --- | --- | --- |
| `auto` (default) | Detect columns and blocks and read them in order. | Articles, papers, reports. |
| `single` | No column detection: group runs into lines by position, read down the page. | When `columns` came back higher than the page really has. |
| `preserve` | Draw the page into a fixed-pitch character grid, keeping horizontal positions as padding. | Receipts, statements, forms — anything where the alignment carries the meaning. |

### What it does not handle

- **A table whose cells wrap onto several lines** is not reassembled. Rows and
  cells are recovered from single baselines, so a definition spanning four lines
  leaves its term's column empty on three of them. `preserve` renders such a
  page faithfully; `auto` will usually read it as two columns.
- **Text drawn at an angle** is placed by its origin and sorted with everything
  else. Rotated *pages* are handled properly — `/Rotate` is folded into the page
  transform, so a landscape page authored in portrait user space comes back
  horizontal and with its displayed dimensions — but a diagonal watermark inside
  a page will land somewhere arbitrary in the reading order.
- **Right-to-left and vertical scripts.** Runs are ordered left to right and
  lines top to bottom. Arabic and Hebrew text is decoded correctly and ordered
  by position, which is not the same as ordered by reading.
- **Ligatures and glyphs with no `/ToUnicode` entry.** Text is decoded through
  the font's own encoding and `ToUnicode` CMap. A subset font that ships neither
  produces the wrong characters, and nothing here can tell that it has.

## Scans and OCR

**There is no OCR in this plugin and none is planned.** It reads text that is
present in the file; it does not recognise text in an image. Every page is
classified so the difference is never silent:

| Kind | Meaning |
| --- | --- |
| `text` | Text a reader can see. Extracted normally. |
| `ocr_layer` | Text, but all of it invisible, over at least one image — the signature of a scan that has already been through OCR. The text is returned, and it is only as good as that OCR. |
| `image_only` | Images and no text at all. Nothing can be extracted. |
| `empty` | Neither text nor images. Genuinely a blank page. |

If **every** page a caller asked for produced no characters, `extract_text`
returns an error rather than an empty success, and the error says which case it
was:

```text
`docs/scan.pdf`: all 12 page(s) read are images with no text layer — this is a
scan. No text was extracted, and returning an empty result would look like an
empty document. pdf-extract does not do OCR; run the file through an OCR tool
and extract from its output.
```

A blank document gets a different message naming blankness, because "this is a
scan" and "this page is empty" are different answers. When only *some* pages are
scans, the text that exists is returned, the scanned pages are listed in
`image_only_pages`, and a note explains.

## Tables

Detection is alignment-based, in `src/tables.rs`: several consecutive rows whose
cells start at the same handful of horizontal positions. A row is split into
cells at gaps wider than 0.75 of the line height, and also on two-or-more
consecutive spaces inside a single string, which is how a producer that emitted
a whole row as one string wrote its padding.

**Drawn borders are ignored.** Vector graphics are not read at all, so a ruled
table and an unruled one are detected identically — and a bordered table whose
text does not line up is not recovered.

Three guards keep prose from being reported as a table:

- **Occupancy.** A column must carry a cell in at least half the rows.
  Justified prose has stretched word spaces that split lines into pieces, but
  the pieces start somewhere different on every line, so no column survives.
- **Collisions.** A table is rejected if more than a fifth of its cells have to
  share a column with another cell from the same row.
- **Fill.** A cell that runs most of the way to the next column is a line of
  text, not a cell. This is what stops the body of a two-column article being
  returned as a two-column table.

Each table reports `occupancy`: the mean fraction of rows in which each detected
column carried a value. `1.0` is a grid with no holes; a lower number means the
alignment was ragged and the grid should be read with suspicion. A page with no
aligned rows returns an empty list **with a note saying why and what to try
instead** — that is a real answer, not a failure.

## Security

This plugin reads documents on hardware that may not belong to the person asking
the question. Its blast radius is deliberately narrow, and stated here rather
than implied.

**What it can touch.** Regular files with a `.pdf` extension under the roots the
operator configured. Nothing else. **No network, no subprocess, no writes** — it
opens files read-only and never creates, modifies, or deletes anything.

**How confinement is enforced.** Callers address files as
`<root label>/<path inside that root>`; a label is mandatory even with one root
configured, and absolute paths are refused by shape. Two independent layers then
apply, because either alone is bypassable:

1. *Lexical*, in `sanitize_relative`. Absolute paths, rooted paths, Windows
   drive prefixes, NTFS alternate-data-stream syntax, and any `..` segment are
   refused before a syscall happens. `..` is rejected outright rather than
   normalized away, even when it would land back inside the root.
2. *Physical*, in `Roots::resolve`. The joined path is canonicalized — which
   resolves symlinks, junctions, and `.` — and containment is re-checked
   component-wise against the canonical root. A symlink inside a root that
   points outside it fails here.

`src/paths.rs` has the test that proves it: it creates a real symlink (or, on
Windows, a directory junction) inside a root pointing at a file outside it,
asserts the link genuinely resolves, and then asserts the resolver still refuses
it. `list_documents` adds a third layer — it skips symbolic links entirely
rather than following them, so a link out of a root lists nothing rather than
everything.

**Bounds, because a PDF is a parser-hostile format.** Every one is enforced in
code and covered by a test:

| Guard | Default | Why |
| --- | --- | --- |
| File size | 32 MiB | Reached by a `stat`, before a byte is read. |
| Header check | `%PDF-` in the first 1 KiB | "Not a PDF" beats a parser error nobody can act on. |
| Decompressed stream size | 128 MiB per stream | The decompression-bomb guard. Object streams inflate while the document loads, before any of this crate's code runs. |
| Pages per call | 200 | A 5000-page document cannot be asked for in one go. |
| Characters per call | 200 000 | One answer cannot swallow a context window. |
| Runs per page | 200 000 | A page with a million separately positioned strings sheds rather than growing. |
| `preserve` line width | 400 characters | A run positioned a million points off the page cannot become a line of a million spaces. |
| Form XObject recursion | depth 12, plus a visited set | A form that draws itself terminates. |
| `/W` width ranges | 65 536 entries | A crafted width array cannot ask for a map of four billion entries. |
| Wall clock | 30s per call | See below. |

**The timeout works two ways, and needs both.** A cooperative deadline is
checked between pages and every 512 content-stream operators, so work stops on
its own rather than being abandoned mid-parse. The handler additionally races
the whole blocking task against the same budget plus two seconds, for time spent
inside a single `lopdf` call that the cooperative check never reaches. **If the
race is what fires, the work is abandoned, not cancelled** — the parse finishes
on its worker thread and its result is discarded. The error says so.

**A panicking parser does not take the node down.** Every tool runs its work on
`spawn_blocking`, so a panic inside the PDF parser is caught at the join and
returned as an error naming the file as unreadable. **A stack overflow is not
survivable this way**: a deeply nested object graph that exhausts the thread
stack aborts the process, and the host restarts the plugin. That is a real
limitation, and it is here rather than hidden.

**Encrypted PDFs.** `lopdf` tries the empty user password, which opens the
common "owner password only" case. Anything needing a real password is refused
with a message saying so; **this plugin accepts no passwords**, because a
password in `[[plugin]].args` is written to `config.toml` and echoed back by
`tdcc plugins info`.

**What tool responses do not contain.** The absolute path of any root. Paths in
every response are `<label>/<relative>`, and path errors quote the caller's own
input rather than the resolved location. The absolute roots are printed once, to
stderr, at startup — for the operator, not for the model.

**No secrets, and no sandbox.** This plugin reads no credentials and has no
configuration that could hold one. Confinement is enforced by this plugin's own
code, in this plugin's own process, with the operator's privileges. Installing
any plugin runs third-party native code on your machine. Read the source before
you trust the claim.

**A document is untrusted input to the model, too.** Text extracted from a PDF
is data, not instructions. A PDF someone else wrote can contain text that reads
like a command — including in an invisible OCR layer a human reviewer would not
see. That is a property of every document-reading tool and worth knowing when
deciding which directories to point this at.

## Configuration

At least one root is required, and it arrives through `[[plugin]].args`:

```toml
# ~/.tdcc/config.toml
version = 1

[[plugin]]
name = "pdf-extract"
enabled = true
command = "/opt/pdf-extract/pdf-extract"
args = ["--root", "/srv/documents", "--root", "invoices=/srv/accounts/2024"]
```

| Flag | Environment fallback | Default | Meaning |
| --- | --- | --- | --- |
| `--root <dir>` | `TDCC_PDF_EXTRACT_ROOTS` | — | Required, repeatable. A directory the plugin may read. |
| `--root <label>=<dir>` | same | — | The same, with the label callers use in a path. |
| `--max-file-bytes <n>` | `TDCC_PDF_EXTRACT_MAX_FILE_BYTES` | `33554432` | Refuse larger files. Range `1024`–`536870912`. |
| `--max-pages <n>` | `TDCC_PDF_EXTRACT_MAX_PAGES` | `200` | Pages parsed per call. Range `1`–`10000`. |
| `--max-chars <n>` | `TDCC_PDF_EXTRACT_MAX_CHARS` | `200000` | Characters returned per call. Range `1000`–`20000000`. |
| `--timeout-secs <n>` | `TDCC_PDF_EXTRACT_TIMEOUT_SECS` | `30` | Wall-clock budget per call. Range `1`–`600`. |
| `--max-decompressed-bytes <n>` | `TDCC_PDF_EXTRACT_MAX_DECOMPRESSED_BYTES` | `134217728` | Per-stream inflate ceiling. Range `1048576`–`2147483648`. |

Precedence is **flag beats environment beats built-in default**, and there is a
test for it. Both `--root /x` and `--root=/x` work. `TDCC_PDF_EXTRACT_ROOTS`
takes a list separated by the platform's path separator (`:` on Unix, `;` on
Windows), and its entries may be `label=dir` too.

**An unknown flag or an out-of-range value is a startup error, not a warning.**
A typo in `--max-file-bytes` that was quietly ignored would leave an operator
believing a ceiling was in force when it was not.

### Root labels

A bare `--root /srv/documents` takes its label from the final path component:
`documents`. Non-alphanumeric characters become `-`, so `--root "/My Documents"`
gives `My-Documents`. Two roots whose *derived* labels collide get numbered
(`reports`, `reports-2`) because the operator chose neither name; two roots
given the *same explicit* label are a startup error, because they did.

Callers write `documents/reports/q4.pdf`. `list_documents` returns exactly those
strings, so nothing has to be assembled by hand, and `status` lists the labels.

### Why there is no `[plugin.settings]` schema

`[plugin.settings]` never reaches a plugin process. The host stores those values,
the console renders them, and a web UI bundle reads them back — but there is no
settings field in the launch contract or the initialize handshake. A `roots`
setting would look authoritative in the console and do nothing at all. So this
plugin declares no config schema and reads its roots from `args`, which is one
of the two channels that does reach the process.

The practical consequence: **changing a root means editing `config.toml` and
restarting `tdcc`**, not clicking something in the console.

## What is read out of the file

From PDF 32000-1 chapter 9, in `src/glyphs.rs`:

- the graphics state stack (`q`, `Q`) and the current transformation matrix
  (`cm`), so text inside a scaled or translated form lands where it is drawn
- the text and line matrices (`BT`, `Tm`, `Td`, `TD`, `T*`, `'`, `"`)
- font size, character spacing, word spacing, horizontal scaling, leading, rise,
  and render mode (`Tf`, `Tc`, `Tw`, `Tz`, `TL`, `Ts`, `Tr`)
- glyph widths, from `/Widths` for simple fonts and `/W` and `/DW` for composite
  ones, so the advance after a string is measured rather than guessed. A font
  that declares no width for a code falls back to half an em, which is close for
  a proportional face and wrong for a monospace one.
- `TJ` positioning adjustments, which is how producers write the gap between two
  words — a space is inserted where the geometric gap exceeds 0.22 of the line
  height, not wherever a string ends
- form XObjects (`Do`), recursively, because plenty of producers put the whole
  page inside one
- images, both `Do` on an image XObject and inline `BI`, which is what
  distinguishes a blank page from a scan
- `/MediaBox` and `/Rotate`, inherited from `/Parent` when the page does not
  carry them

Composite (Type0) fonts are assumed to use a two-byte CMap, which `Identity-H`
and `Identity-V` do. A one-byte or mixed-width CMap gives the wrong *widths*;
the decoded *text* is unaffected, because that goes through the font's real CMap.

Text drawn in render mode 3 or 7 is invisible but is still collected — that is
what an OCR layer is — and counted separately so a page of it can be reported as
`ocr_layer` rather than as ordinary text.

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
    plugins/pdf-extract/
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

Beyond the SDK: `lopdf` for the PDF object layer, and the usual `anyhow` /
`serde` / `serde_json` / `schemars` / `tokio`. `lopdf` is used as a parser and a
decoder — the document, its objects, its content streams, font encodings, and
`ToUnicode` CMaps. Everything above that, including the text state machine, the
positioning, the reading order, and the tables, is in this crate; `lopdf`'s own
`extract_text` discards positions and is exactly the naive approach this plugin
exists to avoid.

Nothing is pulled in for testing. The temp-directory helper, the symlink helper,
and the PDF *writer* in `src/testsupport.rs` are hand-rolled so the release
dependency set stays as small as the job.

## Build and test

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

137 tests, all beside the code they cover. They include real PDF files — the
writer in `src/testsupport.rs` emits valid documents with real content streams
that the same reader the plugin uses parses back — so the layout assertions are
made against bytes rather than against strings. Covered: the path confinement
rules including the symlink escape, option parsing and precedence, the text
matrix and every spacing operator, glyph widths, page rotation through all four
quarter turns, a `/MediaBox` that does not start at the origin, image-only and
OCR-layer classification, two- and three-column reading order, the definition
list that must *not* be split into columns, table detection and the three guards
that keep prose out of it, page-selection parsing, PDF date normalization, the
listing walk and its symlink refusal, and the character, page, and time budgets.

## Package and install locally

macOS or Linux, from this directory:

```bash
rm -rf target/package
mkdir -p target/package/pdf-extract
cp target/release/pdf-extract target/package/pdf-extract/pdf-extract
cp plugin.toml target/package/pdf-extract/plugin.toml
cp README.md target/package/pdf-extract/README.md
tar -C target/package -czf target/pdf-extract-0.1.0-local.tar.gz pdf-extract

tdcc plugins install --archive ./target/pdf-extract-0.1.0-local.tar.gz \
  --name pdf-extract --version 0.1.0
tdcc plugins info pdf-extract
```

Windows uses `pdf-extract.exe` and a `.zip` whose single top-level directory is
`pdf-extract/`:

```powershell
Compress-Archive -Path target\package\pdf-extract `
  -DestinationPath target\pdf-extract-0.1.0-local.zip -Force
tdcc plugins install --archive .\target\pdf-extract-0.1.0-local.zip `
  --name pdf-extract --version 0.1.0
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
curl --fail -X POST http://127.0.0.1:3131/api/plugins/pdf-extract/tools/status \
  -H 'Content-Type: application/json' -d '{}'

curl --fail -X POST http://127.0.0.1:3131/api/plugins/pdf-extract/tools/list_documents \
  -H 'Content-Type: application/json' -d '{"name_contains":"invoice"}'

curl --fail -X POST http://127.0.0.1:3131/api/plugins/pdf-extract/tools/document_info \
  -H 'Content-Type: application/json' -d '{"path":"documents/invoice.pdf"}'

curl --fail -X POST http://127.0.0.1:3131/api/plugins/pdf-extract/tools/extract_text \
  -H 'Content-Type: application/json' \
  -d '{"path":"documents/invoice.pdf","pages":"1-3","layout":"preserve"}'

curl --fail -X POST http://127.0.0.1:3131/api/plugins/pdf-extract/tools/extract_tables \
  -H 'Content-Type: application/json' -d '{"path":"documents/invoice.pdf"}'
```

And the one that should fail:

```bash
curl -X POST http://127.0.0.1:3131/api/plugins/pdf-extract/tools/extract_text \
  -H 'Content-Type: application/json' -d '{"path":"documents/../../etc/passwd"}'
# → `documents/../../etc/passwd` was refused: path must not contain a '..' segment

curl -X POST http://127.0.0.1:3131/api/plugins/pdf-extract/tools/extract_text \
  -H 'Content-Type: application/json' -d '{"path":"/etc/passwd"}'
# → `/etc/passwd` was refused: path must be `<root label>/<path inside that
#   root>`, not an absolute path
```

On the host MCP endpoint the same tools are namespaced `pdf-extract.extract_text`,
`pdf-extract.document_info`, and so on.

### Running it directly

Running the binary with a root but no host fails immediately:

```text
pdf-extract: `documents/` is /srv/documents
Error: TDCC_PLUGIN_ENDPOINT is not set for plugin process
```

That is correct. The host owns the control endpoint and passes it in through the
launch contract; a plugin must never invent a socket path of its own.

## Failure behaviour

A tool that cannot do its job says so; it never returns an empty success.

| Situation | Result |
| --- | --- |
| Every requested page is a scan | Error naming the scan and naming OCR as the missing step |
| Every requested page is blank | A different error, naming blankness, pointing at `document_info` |
| Some pages are scans | The text that exists, plus `image_only_pages` and a note |
| Path escapes a root, or has no label | Error naming the rule that refused it and listing the available labels |
| File is not a PDF | Error saying the `%PDF-` header is missing |
| File is larger than the ceiling | Error with both numbers and the flag that raises it |
| PDF is encrypted with a real password | Error saying so, and that this plugin takes no passwords |
| PDF is damaged | Error saying it could not be parsed — never an empty document |
| Budget runs out | Error naming `--timeout-secs`, and saying whether the work was abandoned |
| Parser panics on a malformed file | Error saying the file is not readable; the plugin keeps running |
| No table on the page | `tables: []` with a note explaining alignment-based detection and suggesting `layout: "preserve"` |
| No root configured | The process exits at startup with the flag and environment variable to set |

## License

Apache-2.0, matching this repository. See [LICENSE](../../LICENSE).
