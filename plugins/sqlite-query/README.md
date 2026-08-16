# sqlite-query

Lets a model answer questions about real data in SQLite databases, without
anyone pasting a schema into a prompt and without handing the model a general
purpose file reader.

An operator lists the database files the plugin may open. The model can then see
what tables exist, read their full schema, and run SQL against them — read-only,
bounded, and confined to that list.

## Tools

| Tool | What it does |
| --- | --- |
| `list_databases` | The configured databases, their access mode, and whether each file is currently readable. |
| `list_tables` | Tables and views in one database, each with its column names. |
| `describe_table` | One table or view in full: column types, `NOT NULL`, defaults, primary key, foreign keys with their referenced columns, indexes, and the original `CREATE` statement. |
| `query` | One read-only SQL statement, bounded by a row cap, a response size cap, and a statement timeout. |
| `execute` | One statement that may write. Refused unless the operator registered that database with `--db-rw`. |

On the host MCP endpoint these are namespaced: `sqlite-query.query`,
`sqlite-query.describe_table`, and so on.

`describe_table` is where most of the value is. A model that can see that
`orders.customer_id` is an `INTEGER` referencing `customers(id)` writes a
correct join on the first try; a model that can only see table names guesses.

## What it is allowed to touch

This runs on somebody else's hardware, so the blast radius is worth stating
exactly.

**It can read** the SQLite files named in `[[plugin]].args`, and nothing else on
the filesystem.

**It cannot write** to any of them unless the operator opted a specific database
into `--db-rw`. That is not a check on the SQL text — it is the file handle:
connections are opened `SQLITE_OPEN_READ_ONLY`, so a write fails inside SQLite
no matter how the statement is phrased. Scanning for the word `DROP` would be
defeated by a view, a trigger, a comment, or a different keyword; a read-only
file descriptor is not.

**It cannot reach a different file.** Two things stop that:

- **No tool takes a path.** Every tool selects a database by an alias, and an
  alias only exists because it appeared in the launch arguments. There is no
  code path from caller input to a filesystem path, so there is nothing for
  `../` to escape through.
- **`ATTACH` is refused.** A read-only connection can still run
  `ATTACH DATABASE '/etc/secrets.db' AS leak` and then `SELECT` from it. A
  SQLite authorizer callback denies `ATTACH` and `DETACH` at statement-compile
  time, in every mode including `--db-rw`.

The same authorizer denies, on read-only connections, every mutating action and
temp table/view/index/trigger creation — the temp database is writable even when
the main one is not. In *every* mode it denies `PRAGMA` outside the plugin's own
fixed introspection statements, along with `load_extension`, `readfile`,
`writefile` and similar file-touching SQL functions. Any action code it does not
recognise is denied rather than allowed.

**It opens no sockets** and makes no network requests. It has no HTTP routes, no
mesh channels, no mesh event subscriptions and no web UI — nothing is declared,
and host delivery is allowlist-based, so nothing is delivered.

**It stores nothing.** No caches, no temp files, no logs beyond the startup
lines it writes to stderr.

One thing to be aware of: `list_databases` reports the configured file paths, so
a caller can see where the data lives. That is deliberate — an operator needs to
know which file an answer came from — but if a path is itself sensitive, mount
the database somewhere neutral.

## Bounds on a result

A model will eventually write `SELECT * FROM events`. Every query runs under
four caps, all set by the operator:

| Cap | Default | Argument |
| --- | --- | --- |
| Rows per result | 200 | `--max-rows` |
| Approximate response size | 262144 bytes | `--max-bytes` |
| Bytes kept from one cell | 2048 | `--max-cell-bytes` |
| Wall-clock per statement | 5000 ms | `--timeout-ms` |

The caps stop the scan as it runs, rather than reading everything and trimming
afterwards, so a huge table costs a fast, small answer instead of a slow, large
one. The timeout is enforced with `sqlite3_interrupt` from a watchdog thread, so
it bounds real elapsed time including time spent blocked on a lock — not just a
count of VM steps.

When a cap is hit, the response says so twice: a machine-readable
`"truncated": "row_limit"` or `"byte_limit"`, and a `note` that starts with
`INCOMPLETE:`. A shortened cell also carries the marker inside its own value:

```text
"a very long document…[+94211 bytes truncated]"
```

Nothing is ever quietly cut. A caller that receives 200 of 4,000,000 rows is
told, because the alternative is a confident wrong answer.

Each response also echoes the limits it ran under, so `12 rows` can be told
apart from `12 rows, and that was the cap`.

## Result shape

Rows are positional arrays under a separate `columns` list, which costs far
fewer tokens than repeating every column name on every row:

```json
{
  "database": "shop",
  "columns": [
    { "name": "id", "declared_type": "INTEGER" },
    { "name": "email", "declared_type": "TEXT" }
  ],
  "rows": [[1, "ada@example.com"], [2, "grace@example.com"]],
  "row_count": 2,
  "truncated": null,
  "truncated_cells": 0,
  "estimated_bytes": 84,
  "elapsed_ms": 1,
  "limits": { "max_rows": 200, "max_bytes": 262144, "max_cell_bytes": 2048, "timeout_ms": 5000 }
}
```

Three conversions are worth knowing about:

- SQLite `TEXT` is arbitrary bytes, not guaranteed UTF-8. Invalid sequences are
  replaced rather than failing the query.
- `REAL` values that JSON cannot hold come back as the strings `"NaN"`,
  `"Inf"`, `"-Inf"` — not as `null`, which a model would read as an empty
  column.
- A `BLOB` becomes `{"type": "blob", "bytes": 4, "hex": "deadbeef",
  "hex_complete": true}`, so a hex dump can never be mistaken for the column's
  text value.

## Configuration

Databases are configured through `[[plugin]].args`, not through
`[plugin.settings]`. That is not a style choice: host-owned settings are stored
and rendered by the console but are never delivered to the plugin process, so a
settings key could not actually restrict which files this plugin opens. The
plugin therefore declares no config schema.

```toml
# ~/.tdcc/config.toml
version = 1

[[plugin]]
name = "sqlite-query"
args = [
  "--db", "shop=/srv/data/shop.db",
  "--db", "analytics=/srv/data/analytics.db",
  "--max-rows", "500",
  "--timeout-ms", "3000",
]
```

| Argument | Meaning |
| --- | --- |
| `--db <alias>=<path>` | Register a database the model may read. Repeatable. |
| `--db-rw <alias>=<path>` | Register a database `execute` may also write to. Repeatable. See the warning below. |
| `--max-rows <n>` | Rows per result. 1–10000, default 200. |
| `--max-bytes <n>` | Approximate response size. 1024–8388608, default 262144. |
| `--max-cell-bytes <n>` | Bytes kept from one cell. 16–1048576, default 2048. |
| `--timeout-ms <n>` | Wall-clock per statement. 1–120000, default 5000. |

`--flag value` and `--flag=value` both work. Aliases are ASCII letters, digits,
`_` and `-`, starting with a letter or digit, up to 64 characters.

The same settings can come from the environment, which is handy in a container.
Arguments win for the scalar limits; database lists from both sources are
merged, and a duplicate alias is an error rather than a silent override.

| Variable | Equivalent |
| --- | --- |
| `TDCC_SQLITE_QUERY_DB` | `--db`, as `alias=path` entries separated by `;` |
| `TDCC_SQLITE_QUERY_DB_RW` | `--db-rw`, same format |
| `TDCC_SQLITE_QUERY_MAX_ROWS` | `--max-rows` |
| `TDCC_SQLITE_QUERY_MAX_BYTES` | `--max-bytes` |
| `TDCC_SQLITE_QUERY_MAX_CELL_BYTES` | `--max-cell-bytes` |
| `TDCC_SQLITE_QUERY_TIMEOUT_MS` | `--timeout-ms` |

A POSIX path may legally contain `;`, so use `--db` rather than the environment
variable if one of yours does.

There are no secrets in any of this. Nothing here is key-shaped, and the plugin
reads no credentials of any kind.

### Startup behaviour

A malformed argument is fatal — the process refuses to start and prints what was
wrong. Starting with a misunderstood configuration is how a plugin ends up
exposing a database nobody meant to expose.

A well-formed but empty configuration is not fatal. The plugin starts, logs a
line to stderr explaining what to add, and every tool reports the same thing.
That is more useful to an operator than a restart loop.

On startup it prints one stderr line per database, so `tdcc` logs show exactly
what was exposed and in which mode.

### Write mode

`--db-rw` is off for every database by default and has to be turned on one
database at a time. Turning it on means the `execute` tool can run any statement
that database's schema allows — `UPDATE`, `DELETE`, `DROP TABLE`, all of it —
driven by a model. There is no undo. Point it at a scratch database, not at
anything you would miss.

`execute` still refuses `ATTACH`, `DETACH`, `PRAGMA` and file-access functions,
and still runs under the row, byte and time caps. `SQLITE_OPEN_CREATE` is not
set, so a typo in the path fails instead of quietly creating an empty database.

## Prerequisites and known limits

- **The database files must already exist.** This plugin never creates one.
- **WAL-mode databases need a writable directory.** SQLite requires write
  access to the `-shm` wal-index file, or to the directory containing the
  database if that file does not exist, even for a read-only connection. If
  that is not available the open fails and the plugin reports SQLite's error
  verbatim. Copy the database, or run the plugin as a user that can write the
  sidecar files.
- **One statement per call.** Enforced by SQLite's parser, which refuses a
  trailing second statement — not by counting semicolons.
- **Positional parameters only.** Use `?1`, `?2` … and pass values in `params`.
  Only null, booleans, numbers and strings can be bound; serialize structured
  values to a JSON string first.
- **No `PRAGMA` from `query`.** Use `list_tables` and `describe_table`, which
  run the plugin's own fixed introspection statements.
- **Only the `main` schema.** With `ATTACH` denied there is nothing else to
  reach.
- **No cross-database joins**, for the same reason. Query each database
  separately.

## Building against the SDK

`tdcc-plugin` is not published to crates.io under that name yet, so a version
requirement like `tdcc-plugin = "0.72.1"` will not resolve. `Cargo.toml` here
points at a local checkout of the main TDCC repository:

```toml
tdcc-plugin = { path = "../../../tdcc-mesh/crates/tdcc-plugin" }
```

That path assumes `tdcc-plugins` and `tdcc-mesh` are siblings. Adjust it to
wherever your checkout lives, or replace it with a git dependency if you have
access:

```toml
tdcc-plugin = { git = "https://github.com/the-decentralized-compute-company/tdcc-mesh", tag = "v0.72.1" }
```

**Once the SDK is published**, a public consumer replaces that line with a
plain version requirement and deletes nothing else:

```toml
tdcc-plugin = "0.72.1"
```

Pin an exact version compatible with the `tdcc` release you target. The
initialize handshake requires an exact protocol-version match, so a host and a
plugin built against mismatched protocol versions refuse to connect loudly at
startup rather than misbehaving later.

The other notable dependency is `rusqlite` with `bundled`, `hooks` and
`column_decltype`. `bundled` statically links SQLite, so the plugin does not
depend on whatever `libsqlite3` a contributor happens to have; it needs a C
compiler at build time. `hooks` provides the authorizer callback that refuses
`ATTACH`. `column_decltype` is how a result set reports declared column types.

## Build and test

```bash
cargo test
cargo build --release
```

The first build compiles the SQLite amalgamation from source, so a C compiler
has to be on the machine. The SDK builds its protocol types with a vendored
`protoc`. No system protobuf compiler and no SQLite development package are
required.

## Package and install locally

From this directory, on macOS or Linux:

```bash
rm -rf target/package
mkdir -p target/package/sqlite-query
cp target/release/sqlite-query target/package/sqlite-query/sqlite-query
cp plugin.toml README.md target/package/sqlite-query/
tar -C target/package -czf target/sqlite-query-0.1.0-local.tar.gz sqlite-query

tdcc plugins install --archive ./target/sqlite-query-0.1.0-local.tar.gz \
  --name sqlite-query --version 0.1.0
tdcc plugins info sqlite-query
```

On Windows, copy `sqlite-query.exe` instead and build a `.zip` whose single
top-level directory is `sqlite-query/`:

```powershell
Compress-Archive -Path target\package\sqlite-query `
  -DestinationPath target\sqlite-query-0.1.0-local.zip -Force
tdcc plugins install --archive .\target\sqlite-query-0.1.0-local.zip `
  --name sqlite-query --version 0.1.0
```

Set `TDCC_PLUGIN_DIR` to an empty directory first if you do not want this
landing in your real plugin store.

The plugin declares neither a config schema nor a web UI, so
`--print-package-manifest` emits `{}` and `plugin-manifest.json` can be left out
of the archive.

## Try it

Make a database:

```bash
sqlite3 /tmp/shop.db <<'SQL'
CREATE TABLE customers (
  id     INTEGER PRIMARY KEY,
  email  TEXT NOT NULL UNIQUE,
  region TEXT DEFAULT 'unknown'
);
CREATE TABLE orders (
  id          INTEGER PRIMARY KEY,
  customer_id INTEGER NOT NULL REFERENCES customers(id) ON DELETE CASCADE,
  total       REAL NOT NULL
);
CREATE INDEX orders_by_customer ON orders(customer_id);
INSERT INTO customers (email, region) VALUES ('ada@example.com', 'eu');
INSERT INTO orders (customer_id, total) VALUES (1, 42.5), (1, 17.0);
SQL
```

Point the plugin at it:

```toml
# config.toml
version = 1

[[plugin]]
name = "sqlite-query"
enabled = true
args = ["--db", "shop=/tmp/shop.db"]
```

```bash
tdcc client --port 9337 --console 3131 --config ./config.toml
```

Then, from another terminal:

```bash
curl --fail -X POST http://127.0.0.1:3131/api/plugins/sqlite-query/tools/list_tables \
  -H 'Content-Type: application/json' -d '{}'

curl --fail -X POST http://127.0.0.1:3131/api/plugins/sqlite-query/tools/describe_table \
  -H 'Content-Type: application/json' -d '{"table":"orders"}'

curl --fail -X POST http://127.0.0.1:3131/api/plugins/sqlite-query/tools/query \
  -H 'Content-Type: application/json' \
  -d '{"sql":"SELECT c.email, count(*) AS orders, sum(o.total) AS spend
              FROM customers c JOIN orders o ON o.customer_id = c.id
              WHERE c.region = ?1 GROUP BY c.id","params":["eu"]}'
```

The `database` field can be left out here because only one is configured. With
several, it becomes required and the error lists the names.

Confirm the read-only default while you are there:

```bash
curl -X POST http://127.0.0.1:3131/api/plugins/sqlite-query/tools/query \
  -H 'Content-Type: application/json' -d '{"sql":"DELETE FROM orders"}'
# -> this statement writes to the database, but this connection is read-only.

curl -X POST http://127.0.0.1:3131/api/plugins/sqlite-query/tools/query \
  -H 'Content-Type: application/json' \
  -d '{"sql":"ATTACH DATABASE '"'"'/etc/passwd'"'"' AS leak"}'
# -> SQLite refused to compile this statement. This connection is read-only …
```

## Running it directly

Running the binary outside a host fails immediately, after printing the
databases it would have exposed:

```text
sqlite-query: shop => /tmp/shop.db (read-only)
Error: TDCC_PLUGIN_ENDPOINT is not set for plugin process
```

That is correct. The host owns the control endpoint and passes it in through
the launch contract; a plugin must never invent a socket path of its own.

## License

Apache-2.0.
