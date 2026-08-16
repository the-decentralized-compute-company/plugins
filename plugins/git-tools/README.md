# git-tools

The questions about a repository that searching its files cannot answer.

[`code-context`](../code-context) indexes what a repository's files *say*.
`git-tools` exposes what its history *did*: when a line changed, who changed it,
what landed between two releases, and what is uncommitted right now. Seven MCP
tools over one or more repositories the machine's operator listed by path.

Everything here is **read-only**, and that is a design decision rather than a
current limitation — see [Read-only, permanently](#read-only-permanently).

## Tools

All seven are projected by the host as `git-tools.<name>` on the MCP endpoint
and at `POST /api/plugins/git-tools/tools/<name>`.

| Tool | What it does |
| --- | --- |
| `status` | Which repositories this plugin can read, whether each opens, where HEAD is, and the limits and disclosure policy the operator set. Reads no history. Call it first. |
| `log` | Commits newest first, filtered by path, author, message text, and date. `rev` plus `exclude_rev` gives exactly what landed between two releases. |
| `show` | One commit in full: message, author, committer, parents, changed files with line counts, and optionally the patch. |
| `diff` | Two revisions compared: every differing file, insertion and deletion counts, rename detection, optional patch text, optional merge-base (`a...b`) semantics. |
| `blame` | Line-by-line attribution for one file, with the commit, author, and date behind each line. |
| `refs` | Branches and tags with the commit each points at, newest first. This is where you find the release tags the other tools take. |
| `repo_status` | `git status`: staged, modified, deleted, renamed, and untracked files, plus the branch and any in-progress rebase or merge. |

### Why there are two things called "status"

The catalog convention is that `status` means *"what is this plugin configured
as, without touching anything expensive"* — seven of the eleven plugins here
have one, and it is the tool an operator calls when everything else is failing.
Git's `status` means something else entirely: the state of a working tree.
Rather than overload one name, both exist under distinct names. `status` is the
plugin; `repo_status` is the repository.

### A worked example

"What went into 2.0, in the docs directory?" — `rev` and `exclude_rev` instead of
a `v1.0.0..v2.0.0` range string, because [a revision argument never contains a
range](#what-a-revision-may-contain):

```json
{
  "rev": "v2.0.0",
  "exclude_rev": "v1.0.0",
  "paths": ["docs/*.md"],
  "limit": 2,
  "include_stats": true
}
```

```json
{
  "commits": [
    {
      "author": {
        "date": "2024-03-15T11:21:07+00:00",
        "email": "grace@example.org",
        "name": "Grace Hopper",
        "offset_minutes": 0,
        "timestamp": 1710501667
      },
      "commit": "d670d2ce756a82eb3f28fa69a59262a64fcd4865",
      "committer": {
        "date": "2024-03-15T11:21:07+00:00",
        "email": "grace@example.org",
        "name": "Grace Hopper",
        "offset_minutes": 0,
        "timestamp": 1710501667
      },
      "merge": false,
      "message": "docs: expand the guide",
      "message_truncated": false,
      "parents": ["242a3e8c3e84a2f5bf867bb19a5bdf7f75dfe834"],
      "short": "d670d2ce756a",
      "stats": { "deletions": 0, "files_changed": 1, "insertions": 1 },
      "summary": "docs: expand the guide"
    }
  ],
  "commits_scanned": 3,
  "exclude_rev": {
    "commit": "74e6e637d75e5b9c9847cb8554d8d562efffe49b",
    "requested": "v1.0.0",
    "short": "74e6e637d75e"
  },
  "limit": 2,
  "more_available": false,
  "repository": "repo",
  "returned": 2,
  "rev": {
    "commit": "d670d2ce756a82eb3f28fa69a59262a64fcd4865",
    "requested": "v2.0.0",
    "short": "d670d2ce756a"
  },
  "skip": 0,
  "truncated": false
}
```

That payload is not illustrative — it is checked in as a constant and asserted
against the real response by
`history::tests::the_readme_example_is_exactly_what_log_returns`, with the
second commit elided here for length. The fixture uses fixed commit times and
fixed content, so the commit ids are stable on every machine. If the shape
changes, that test fails before this document becomes wrong.

Three fields exist because a shortened answer must not look like a complete one:
`commits_scanned` is the work done including commits the filters rejected,
`more_available` says paging further with `skip` will return more, and
`truncated` says a cap stopped the walk early — with `truncated_reason` naming
which cap and what to do about it.

## Read-only, permanently

There is no `commit`, no `checkout`, no `fetch`, no `push`, no `tag`, no
`config` write, and there will not be. Handing a model write access to a
repository is a bad idea, and this is not the plugin to explore it: a mistake
there is somebody else's work destroyed on somebody else's machine, discovered
later.

The claim is enforced in four places rather than promised in one:

1. **No write tool is declared.** `tools::tests::no_tool_name_suggests_a_write`
   fails if a future tool is named `commit`, `push`, `fetch`, `checkout`,
   `merge`, `rebase`, `reset`, `apply`, `revert`, `stash`, `remote`, `config`,
   or a dozen other write-shaped words.
2. **No network transport is linked.** `git2` is built with
   `default-features = false`, which drops its `https` and `ssh` features, so
   there is no TLS stack and no libssh2 in the binary. Clone, fetch, and push
   are unavailable at the library level rather than merely unused. `status`
   reports this straight out of `git2::Version`, and
   `inventory::tests::status_reports_the_backend_the_limits_and_every_repository`
   asserts both flags are false.
3. **The one libgit2 call that could write is disabled.** Computing status can
   refresh the index's stat cache, which writes `.git/index`. `repo_status`
   sets `update_index(false)` explicitly. The consequence is real and stated
   here rather than discovered: on a repository whose index is stale, a file
   whose modification time changed without its content changing may be reported
   as modified until some other tool refreshes the index.
4. **No subprocess.** Nothing shells out, so nothing can invoke a `git` that
   does something else.

## Why libgit2 and not the git binary

Both were on the table. The `git` executable is universal, needs no C
dependency, and behaves exactly like the git a contributor already has.
libgit2 was chosen anyway, for three reasons that all point the same way:

**There is no argument vector to inject into.** A subprocess turns every
model-supplied string into a potential option. A ref named `--upload-pack=…`,
`--output=…`, or `-c core.pager=…` is a real attack against `git log`, and the
defence is a matrix of per-subcommand `--` placement and per-flag knowledge that
has to stay correct forever. In-process there is no argv at all. The revision
guard in `src/guard.rs` still refuses a leading `-` — see
[What a revision may contain](#what-a-revision-may-contain) — but it is a second
line rather than the only one.

**There is no PATH to hijack.** `capability-attest` resolves `nvidia-smi`
through `PATH` and its README says plainly that a `PATH` an attacker controls
means an attacker-chosen binary. A library has no such lookup.

**There is no version drift.** Output formats, porcelain stability, and locale
all vary between git versions and platforms; parsing them is a permanent tax.
libgit2 hands back typed objects.

The cost is honest and worth naming: **libgit2-sys vendors and compiles
libgit2, so building this plugin needs a C compiler.** `sqlite-query` already
sets that precedent here by statically linking the SQLite amalgamation. It also
means a second implementation of git's rules, which occasionally differs from
the git binary in corners — history simplification on merges is the one that
shows up in this plugin, and it is documented where it matters.

## Security

This plugin reads the history of repositories on hardware that may not belong to
the person asking the question. Its blast radius is stated here rather than
implied.

**What it can touch.** The repositories named in `--repo`, read-only. No
network — no transport is linked. No subprocess. No writes of any kind. No
credentials: this plugin reads none and has no configuration that could hold
one.

**History contains more than the working tree does.** This is the most important
sentence in this document. `code-context` can only show a model what is checked
out right now. `git-tools` can reach every version of every file that was ever
committed. A secret that was committed and later removed is still in the
history, and `log`, `show`, `diff`, and `blame` can all surface it. Rotating a
leaked key removes the risk; deleting the file does not, and neither does this
plugin. **Configure repositories whose entire history you would be willing to
show the model, not repositories whose current state you would.**

Author and committer email addresses are personal data and appear in every
commit. `--redact-emails` replaces them with `<redacted>` in every response.
Names are kept, because a blame without names answers nothing.

**How confinement is enforced.** An operator lists repositories as
`--repo <alias>=<path>`. A caller supplies an alias, never a path, so the
reachable set is fixed at launch. Each configured path goes through
`repos::open_confined`, which enforces three rules — at startup *and* again on
every call, because a repository can be edited in between:

1. **The configured path must itself be the repository.** libgit2 is asked to
   open with `NO_SEARCH`, so pointing at `/srv/repo/src` fails rather than
   silently opening `/srv/repo`.
2. **The working tree must be the configured path.** A repository's
   `.git/config` may set `core.worktree` to any directory, which would make
   `repo_status` read files the operator never listed. The canonical working
   tree is compared against the canonical configured root and a mismatch is
   refused.
3. **The git directory must live inside the configured path.** A `.git` *file*
   containing `gitdir: …` — what `git worktree add` and submodules produce —
   points the object store elsewhere entirely. Refused for the same reason.

Containment is compared component-wise on canonical paths, so `/srv/repo-backup`
does not count as being inside `/srv/repo` the way a textual prefix check would
say it does. Canonicalizing is what makes a symlink or a Windows junction unable
to help: the resolved path is compared, not the written one.

`src/repos.rs` has the tests that prove each refusal, and each one first asserts
that the escape genuinely works before asserting that it is refused — a test
that passes because libgit2 could not follow a gitlink proves nothing.

**Two consequences worth knowing.** A linked worktree created by
`git worktree add` is refused, because its gitdir is outside its own directory;
configure the main repository instead. And a submodule's contents are never
read: `diff` sets `ignore_submodules`, `repo_status` sets `exclude_submodules`,
and `blame` on a submodule path returns an error naming what it is.

**What tool responses do not contain.** Any filesystem path. Every path in a
response is repository-relative, and a confinement error names the rule that
refused it rather than the location it refused —
`repos::tests::confinement_errors_never_carry_a_filesystem_path` asserts no
error string contains a `/` or a `\`. The absolute paths are printed once, to
stderr, at startup, for the operator.

**What a hostile repository cannot do.** libgit2 runs no hooks and no external
filter drivers, so a `.git/config` full of `core.hooksPath` and
`filter.*.clean` commands is inert here in a way it would not be under the git
binary. libgit2's own repository-ownership check is left at its default, so a
repository owned by a different user is refused unless `safe.directory` says
otherwise; that error surfaces in the startup log.

**No sandbox.** Confinement is this plugin's own code, in this plugin's own
process, with the operator's privileges. Installing any plugin runs third-party
native code on your machine. Read the source before you trust the claim.

### What a revision may contain

Every `rev`, `from_rev`, `to_rev`, `exclude_rev`, and `oldest_rev` goes through
`guard::parse_revision` before anything reaches libgit2. The function returns a
`Revision` newtype, and nothing else in the crate calls `revparse_single`, so
"no unvalidated revision reaches git" is a property of the type system rather
than of somebody remembering.

Allowed: ASCII letters, digits, and `. _ - / + ~ ^`. That covers every ref name
git itself accepts, plus `HEAD~5` and `HEAD^2`.

| Refused | Because |
| --- | --- |
| a leading `-` | It is an option, not a ref. Nothing here builds a command line, so this cannot inject today — it is kept so the guarantee survives a backend that does. |
| `:` anywhere | `HEAD:path` addresses a blob and `:/text` runs a message search. Neither is something a `log` or `diff` argument needs. Git also forbids `:` in ref names, so nothing legitimate is lost. |
| `{` and `}` | Removes `HEAD@{2}`, `@{upstream}`, and `HEAD^{/regex}` in one rule. The last is a caller-supplied regex run over every commit message. |
| `..` | A range is expressed as two arguments, not one string. |
| anything else | Whitespace, control characters, NUL, and non-ASCII cannot appear in a git ref name. |

Paths get their own guard: absolute and rooted paths, `..` segments, drive
letters, NTFS alternate-data-stream syntax, and git's magic pathspec prefixes
(`:(exclude)`, `:!`, `:/`) are all refused. `blame` additionally refuses globs,
because it addresses exactly one file. These are tree paths rather than
filesystem paths and could not escape the repository anyway — they are sanitized
on filesystem rules regardless, because a path that cannot escape today is one
refactor away from one that can.

`src/guard.rs` tests every row of that table, including the argument-injection
shapes.

### Text filters are substrings, not regexes

`log`'s `author` and `grep`, and `refs`'s `pattern`, match case-insensitive
substrings. A regex would be more expressive and would also hand a model the
ability to spend unbounded CPU on somebody else's machine — a pattern with
nested quantifiers applied to every commit message in a large repository is a
denial of service that arrives looking like a search. Substring matching is
linear.

## Limits

Every one is enforced in code and covered by a test. The operator-settable ones
have a ceiling they cannot be raised past, so a typo in `args` cannot remove the
bound entirely.

| Limit | Default | Ceiling | Set with |
| --- | --- | --- | --- |
| Commits returned by one `log` | 30, max 200 | 5000 | `--max-commits` |
| Commits a walk may examine | 50 000 | 5 000 000 | `--max-scan-commits` |
| Patch text per response | 256 KiB | 8 MiB | `--max-patch-bytes` |
| Files before rename detection is skipped | 400 | 20 000 | `--max-rename-candidates` |
| Lines per `blame` | 2000 | 50 000 | `--max-blame-lines` |
| File size `blame` accepts | 1 MiB | 32 MiB | `--max-blame-file-bytes` |
| Blob size still diffed as text | 8 MiB | — | fixed |
| Files listed per response | 1000 | — | fixed |
| Refs listed / refs examined | 200 / 5000 | — | fixed |
| `repo_status` entries | 1000 | — | fixed |
| Pathspecs per call | 32 | — | fixed |
| Commit message bytes | 8 KiB in `log`, 64 KiB in `show` | — | fixed |
| Context lines in a patch | 3, max 25 | — | fixed |

`truncated: true` means a cap stopped the work early, so more may exist;
`truncated_reason` names which cap and what to change. Totals are counted across
the whole diff even when the file list was capped, so `files_changed` is never
quietly just `files.len()`.

## Performance, measured

Two numbers surprised us enough to change the design, so both are written down
with the measurement that produced them. These are one laptop's timings, not a
benchmark — the shape is the point.

**Rename detection dominates a wide diff.** On a 3065-file release range in the
TDCC repository: computing the diff took **19 ms** and `find_similar` took
**12 seconds**. Inexact rename detection compares every removed file against
every added one. So it is skipped above `--max-rename-candidates`, exactly as
git's own `diff.renameLimit` does, and the response says
`renames: "skipped_too_many_files"` rather than presenting a move as an
unexplained delete-plus-add. That took the same call from 12.8 s to 2.9 s.

**A `blame` line range bounds the answer, not the walk.**

| Repository | Commits | Request | Wall clock |
| --- | --- | --- | --- |
| `tdcc-plugins` | 13 | whole 938-line file | 0.23 s |
| `tdcc-plugins` | 13 | lines 1–5 | 0.01 s |
| `tdcc-mesh` | 1994 | whole 217-line file | 11.4 s |
| `tdcc-mesh` | 1994 | lines 1–5 | 9.7 s |

Blame cost is roughly *commits walked × the cost of one tree comparison*.
Narrowing 217 lines to 5 removed about a seventh of it. The lever that works is
`oldest_rev`, which stops the walk at a named revision: on the 9.3 s call above,
an `oldest_rev` recent enough to cover the lines asked about brought it to
**0.02 s**. Lines older than the boundary are attributed to it and flagged
`boundary: true`, so a bounded answer is visibly bounded rather than wrong.

For reference, on the same 1994-commit repository: `log` with a limit of 30 is
56 ms, `show HEAD` with a patch is 34 ms, and `status` is 2 ms.

## Configuration

At least one repository is required, and it arrives through `[[plugin]].args`:

```toml
# ~/.tdcc/config.toml
version = 1

[[plugin]]
name = "git-tools"
enabled = true
command = "/opt/git-tools/git-tools"
args = ["--repo", "mesh=/srv/repos/tdcc-mesh", "--repo", "plugins=/srv/repos/tdcc-plugins"]
```

| Flag | Environment fallback | Default | Meaning |
| --- | --- | --- | --- |
| `--repo <alias>=<path>` | `TDCC_GIT_TOOLS_REPO` | — | Required, repeatable. The environment form separates entries with `;`. |
| `--max-commits <n>` | `TDCC_GIT_TOOLS_MAX_COMMITS` | `200` | Ceiling on `log`'s `limit`. |
| `--max-scan-commits <n>` | `TDCC_GIT_TOOLS_MAX_SCAN_COMMITS` | `50000` | Commits a walk may examine before reporting truncation. |
| `--max-patch-bytes <n>` | `TDCC_GIT_TOOLS_MAX_PATCH_BYTES` | `262144` | Diff text per response. |
| `--max-rename-candidates <n>` | `TDCC_GIT_TOOLS_MAX_RENAME_CANDIDATES` | `400` | Files before rename detection is skipped. |
| `--max-blame-lines <n>` | `TDCC_GIT_TOOLS_MAX_BLAME_LINES` | `2000` | Lines per `blame`. |
| `--max-blame-file-bytes <n>` | `TDCC_GIT_TOOLS_MAX_BLAME_FILE_BYTES` | `1048576` | Largest file `blame` accepts. |
| `--no-content` | `TDCC_GIT_TOOLS_NO_CONTENT` | off | Never return file content: no diff hunks, no blame line text. |
| `--redact-emails` | `TDCC_GIT_TOOLS_REDACT_EMAILS` | off | Replace every author and committer email with `<redacted>`. |

Flags beat the environment; both `--max-commits 50` and `--max-commits=50` work.
An unknown flag or an out-of-range value is a **startup error, not a warning** —
an operator who mistypes `--no-contents` would otherwise believe content is
withheld when it is not.

`--no-content` is the knob for a node lending capacity to strangers. It leaves
every tool working and every commit, author, path, and line count readable; only
the file content itself is withheld. Asking for a patch under it is an error
naming the flag, never an empty success.

Omitting `repo` in a tool call is allowed only when exactly one repository is
configured. With several it is an error listing the choices, because silently
picking the first is how a model ends up confidently answering about the wrong
codebase.

### Why there is no `[plugin.settings]` schema

`[plugin.settings]` never reaches a plugin process. The host stores those values,
the console renders them, and a web UI bundle reads them back — but there is no
settings field in the launch contract or the initialize handshake. A repository
list rendered there would look authoritative and do nothing at all. So this
plugin declares no config schema and reads its repositories from `args`, which
is one of the two channels that does reach the process.

The practical consequence: **changing the repository list means editing
`config.toml` and restarting `tdcc`**, not clicking something in the console.

Nothing here is key-shaped, so nothing needs the environment for secrecy — but
the environment fallbacks exist because `args` is echoed by
`tdcc plugins info` and some operators prefer their paths out of it.

## Failure behaviour

A tool that cannot do its job says so; it never returns an empty success.

| Situation | Result |
| --- | --- |
| Unknown repository alias | Error listing the configured aliases |
| Repository deleted or moved since startup | Error naming the alias; confinement is re-checked on every call |
| Revision does not exist | Error naming it and pointing at `refs` |
| Revision is malformed | Error naming the rule that refused it |
| Revision names a tree or blob | Error saying so, rather than guessing a nearby commit |
| Repository has no commits | Error saying so, rather than an empty commit list |
| File missing at that revision | Error suggesting `log` with that path to find out why |
| `blame` on a binary or oversized file | Error naming the reason and the flag that governs it |
| `repo_status` on a bare repository | Error naming the tools that still work on it |
| Patch requested under `--no-content` | Error naming the flag |
| One repository unreadable at startup | The others still start; `status` reports the failure with its reason |
| No repository readable at startup | The process exits, because every tool would fail |

`status` is the deliberate exception in one direction: it reports a broken
repository as `state: "unavailable"` with a reason instead of failing the whole
call, because reporting what is wrong is exactly its job.

## Known limitations

- **History simplification on merges is approximate.** A commit counts as
  touching a path when its diff *against its first parent* touches it. Git's own
  simplification is subtler. In practice a merge that resolved a conflict in a
  file is reported as touching it, while one that took a side wholesale usually
  is not. `show` likewise displays a merge against its first parent only, and
  says so in `diff_against`.
- **`repo_status` never refreshes the index**, so a file whose modification time
  changed without its content changing may be reported modified until another
  tool refreshes it. That is the price of never writing.
- **Date filters are absolute, never relative.** `since` and `until` take
  `YYYY-MM-DD`, `YYYY-MM-DDTHH:MM:SSZ`, or an epoch second, always read as UTC.
  There is no `2 weeks ago`: it would need a "now" that differs between the
  asking node and the answering one, and a filter whose meaning depends on which
  machine evaluated it is worse than no filter.
- **Shallow clones end early and look normal.** `log` stops at the graft and
  `blame` attributes lines to the boundary. `status` reports `shallow: true` per
  repository, which is the only warning you get.
- **`refs` never contacts a remote.** Remote-tracking branches are whatever the
  last `fetch` somebody else ran left on disk. There is no transport in this
  binary to make them fresher.
- **Ordering is by commit time**, which is what `git log` does by default and
  which a rewritten or rebased history can make non-monotonic.
- **Two implementations of git's rules exist on your machine** once this is
  installed, and libgit2 is not the one that wrote the repository.

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
    plugins/git-tools/
```

If your layout differs, change the path, or add a `[patch]` section pointing at
wherever your checkout lives. **Once the SDK is published, replace that line
with the version dependency and delete the path.** Pin it to a version
compatible with the `tdcc` release you target: the initialize handshake requires
an exact protocol-version match, so a host and a plugin built against different
protocol versions refuse to connect at startup.

The first build downloads a vendored `protoc` through `tdcc-plugin`'s
`prost-build` step. No system protobuf compiler is required. **A C compiler
is required**, because `libgit2-sys` compiles a vendored libgit2.

### Dependencies

Beyond the SDK: `git2` with `default-features = false`, and the usual `anyhow` /
`serde` / `serde_json` / `schemars` / `tokio`. `git2` in that configuration
brings five crates of its own — `bitflags`, `libc`, `libgit2-sys`, `log`, and
`url` — and no TLS stack. Nothing is pulled in for testing: the temp directory
and repository fixtures in `src/testsupport.rs` are hand-rolled so the release
dependency set stays as small as the job.

## Build and test

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

149 tests, no host required. They cover the revision and pathspec guards
including every argument-injection shape; repository confinement including a
redirected `core.worktree` and a `.git` gitlink pointing outside the root, each
asserted to genuinely resolve before being asserted to be refused; the
calendar arithmetic in both directions, including leap years, pre-epoch times,
and dates that never existed; budget and truncation on multi-byte text; and the
full behaviour of all seven tools against real repositories built with libgit2's
write APIs — real objects, not mocks, because a stub could not tell you that a
blame hunk's `orig_start_line` means what this code assumes it means.

Roughly: `render` 20, `history` 19, `settings` 19, `guard` 17, `blame` 17,
`repos` 14, `changes` 13, `inventory` 13, `resolve` 8, `tools` 6, `testsupport`
3.

What the tests do **not** cover: anything that needs a running host — the
initialize handshake, the HTTP projection, health during a long walk. Those rest
on the checklist in [the plugin guide](../../README.md#test-before-publishing).
The performance numbers above came from ad-hoc runs against two real
repositories, not from the test suite; they are the one set of claims here with
no test behind them, which is why each says what was measured and on what.

## Package and install locally

macOS or Linux, from this directory:

```bash
cargo build --release
rm -rf target/package
mkdir -p target/package/git-tools
cp target/release/git-tools target/package/git-tools/git-tools
cp plugin.toml target/package/git-tools/plugin.toml
cp README.md target/package/git-tools/README.md
tar -C target/package -czf target/git-tools-0.1.0-local.tar.gz git-tools

tdcc plugins install --archive ./target/git-tools-0.1.0-local.tar.gz \
  --name git-tools --version 0.1.0
tdcc plugins info git-tools
```

Windows uses `git-tools.exe` and a `.zip` whose single top-level directory is
`git-tools/`:

```powershell
Compress-Archive -Path target\package\git-tools `
  -DestinationPath target\git-tools-0.1.0-local.zip -Force
tdcc plugins install --archive .\target\git-tools-0.1.0-local.zip `
  --name git-tools --version 0.1.0
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
curl --fail -X POST http://127.0.0.1:3131/api/plugins/git-tools/tools/status \
  -H 'Content-Type: application/json' -d '{}'

curl --fail -X POST http://127.0.0.1:3131/api/plugins/git-tools/tools/refs \
  -H 'Content-Type: application/json' -d '{"kind":"tags","limit":5}'

curl --fail -X POST http://127.0.0.1:3131/api/plugins/git-tools/tools/log \
  -H 'Content-Type: application/json' \
  -d '{"paths":["src/main.rs"],"limit":5}'

curl --fail -X POST http://127.0.0.1:3131/api/plugins/git-tools/tools/blame \
  -H 'Content-Type: application/json' \
  -d '{"path":"src/main.rs","start_line":1,"end_line":20}'
```

And the ones that should fail:

```bash
curl -X POST http://127.0.0.1:3131/api/plugins/git-tools/tools/diff \
  -H 'Content-Type: application/json' \
  -d '{"from_rev":"--upload-pack=touch /tmp/pwned"}'
# → from_rev: a revision must not start with '-'; that is an option, not a ref

curl -X POST http://127.0.0.1:3131/api/plugins/git-tools/tools/blame \
  -H 'Content-Type: application/json' -d '{"path":"../../../etc/passwd"}'
# → path: a path must not contain a '..' segment
```

On the host MCP endpoint the same tools are namespaced `git-tools.log`,
`git-tools.blame`, and so on.

### Running it directly

Running the binary with a repository but no host fails immediately:

```text
git-tools: reading "mesh" at /srv/repos/tdcc-mesh (read-only)
Error: TDCC_PLUGIN_ENDPOINT is not set for plugin process
```

That is correct. The host owns the control endpoint and passes it in through the
launch contract; a plugin must never invent a socket path of its own.

## License

Apache-2.0, matching this repository. See [LICENSE](../../LICENSE).
