<script>
	import CodeTabs from '$lib/components/CodeTabs.svelte';
</script>

<svelte:head>
	<title>Opening a database — jed</title>
	<meta name="description" content="Open or create a single-file jed database from Rust, Go, or TypeScript." />
</svelte:head>

# Opening a database

A jed database is a single file on disk. Open or create one, run SQL against it, and commit when
you're done. Pass a path for a durable file, or open a transient **in-memory** database for tests
and scratch work.

Opening or creating returns a **`Database`** — the handle you run SQL through. Its `execute`, `query`,
`executeScript`, and the `update` / `view` transaction helpers each run on a **fresh session** and
commit it, so a bare statement autocommits. For durable per-connection state — a transaction spanning
several calls, session variables, or a configured/untrusted caller — mint a separate **session** from
the same handle (see [Authorization](../authorization/) and
[Resource limits](../resource-limits/)).

Use the **language selector** in the top bar to switch this example between Rust, Go, and
TypeScript.

<CodeTabs topic="open-database" />

## Durability

A bare `execute` **autocommits durably**: it runs on a fresh session that commits before the call
returns, so the new state is on disk (an in-memory database has nothing to flush). To apply several
statements **atomically**, run them in one `update` closure — or on a single session's explicit
`begin` / `commit` block, where a `rollback` (or dropping the session) discards the uncommitted work.

jed keeps durable data in one file without a WAL. Commits write changed pages once, record their
checksums with the new root, and finish with one durable flush in the steady state. Opening validates
the latest commit before adopting it; file growth and resuming writes after recovery can require
additional flushes. Format v33 requires all processes sharing a file to use a compatible jed version.
After a storage write or flush error, close and reopen the handle before writing again so recovery
can determine the committed outcome. An encoding or size error before storage writes does not
require reopening; discard the failed transaction and continue using the handle.

## Sharing a file between processes

Local file databases use crash-clean **shared multi-process coordination by default**. Several Rust,
Go, or Node processes may open the same file at once; readers keep stable snapshots, while one writer
at a time commits globally. The usual one-process case holds an exclusive fast-path lease, so queries
and commits add no foreground lock calls or extra metadata reads. Contended processes take the slower,
append-only commit path.

Set the open/create `locking` option to `shared` to require this behavior, `exclusive` to reject other
processes, or `none` only when an external coordinator provides the same safety. `auto` (the default)
selects shared on supported local hosts. `file_lock_timeout_ms` / `FileLockTimeoutMs` /
`fileLockTimeoutMs` bounds open/join waiting (default 5 seconds); the separate session
`lock_timeout_ms` / `LockTimeoutMs` / `lockTimeoutMs` bounds writer waiting and reports `55P03`.

Node uses a small first-party native helper solely for OS file locks because Node has no standard
`flock`/`LockFileEx` API. SQL and storage still run in the independent TypeScript engine; browser/OPFS
builds do not load the helper. A missing platform artifact fails closed instead of using PID or mtime
leases.

## Reclaiming space (compaction)

jed reuses the pages that deletes and updates free, but a file never shrinks by itself: after a big
delete it keeps its peak size. To give that space back, compact the database:
`db.compact("main")` (Rust), `db.Compact("main")` (Go), or `db.compact("main")` (TypeScript). Pass
an attachment's name to compact that database instead.

Compaction rewrites the committed data as a fresh, contiguous file next to the original
(`<path>.jedtmp`), syncs it, and atomically renames it into place, so a crash at any point leaves
either the old file or the new one, never a mix. Afterwards the file is exactly as large as its live
data. The database version advances by one; rows, query results, query costs, and prepared
statements are unchanged. An in-memory database compacts too, which lowers its `storage_bytes`.

Compaction is an explicit maintenance step and never waits. It fails with `55006` while anything
else is using the database — an open write transaction, a read session or open cursor on the
handle, or another process with the file open — so close those and retry. A read-only database is
`25006`, and the browser (OPFS) host does not support compaction yet (`0A000`). The rewrite needs
free disk space for a second copy of the live data, and keeps the file's permission bits but not
its owner.

## In-memory databases

Every example on the **SQL** pages of these docs runs against an in-memory database, right in your
browser — the same engine, no file. Create one by calling the unified create constructor with no path:
`Database::create(CreateOptions::default())` (Rust), `jed.CreateDatabase(jed.CreateOptions{})` (Go), or
`createDatabase({})` (TypeScript).

## Running untrusted queries

jed's built-in surface is pure: SQL cannot access the filesystem, network, processes, or
clock beyond the sanctioned host seam. Configure the session's work and input limits before
serving untrusted SQL. The scalar allocation budget below covers specific expanding kernels;
[resource limits](../resource-limits/) explains the remaining memory coverage.

- **Cost ceiling — `set_max_cost(limit)`** / `SetMaxCost` / `setMaxCost`. Bounds the deterministic
  *execution* cost; a query that reaches the ceiling aborts with `54P01`. `0` (the default) is
  unlimited.
- **Scalar allocation — `set_max_scalar_bytes(bytes)`** / `SetMaxScalarBytes` / `setMaxScalarBytes`.
  Limits cumulative repeat/padding output and decimal-transcendental scratch reservations per
  statement; default **64 MiB**, non-positive restores the default, over-budget fails `54P04`.
- **Query memory — `set_max_query_memory_bytes(bytes)`** / `SetMaxQueryMemoryBytes` /
  `setMaxQueryMemoryBytes`. Bounds the logical bytes of rows a statement holds at once (row
  buffers and result collectors); **unlimited** by default, non-positive restores unlimited,
  over-budget fails `54P05`.
- **Input size — `set_max_sql_length(bytes)`** / `SetMaxSQLLength` / `setMaxSqlLength`. Bounds the
  *input SQL length* (in bytes), rejecting an over-long statement with `54000` before it is parsed —
  so a giant query can't exhaust parse memory. The default is **1 MiB**; `0` is unlimited. Because
  jed parses one statement per call, this also bounds the parse tree's size (a million-column
  `SELECT` is just bytes).

Four further limits are fixed engine constants (no configuration): a statement may not nest
expressions/subqueries more than **256** deep (`54001`), a single identifier may not exceed
**63 bytes** (`42622`), a composite type may not nest more than **32** composites deep
(`54001` at `CREATE TYPE` — a chain of small `CREATE TYPE`s that the input-size cap can't see),
and a `json`/`jsonb` document or `jsonpath` may not nest more than **256** levels deep (`54001`
when it is parsed or built — a short string such as `repeat('[', 100000)` can't overflow the
stack). Each limit is deterministic and identical across the Rust, Go, and TypeScript cores.
