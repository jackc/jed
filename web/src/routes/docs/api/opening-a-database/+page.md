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

## In-memory databases

Every example on the **SQL** pages of these docs runs against an in-memory database, right in your
browser — the same engine, no file. Create one by calling the unified create constructor with no path:
`Database::create(CreateOptions::default())` (Rust), `jed.CreateDatabase(jed.CreateOptions{})` (Go), or
`createDatabase({})` (TypeScript).

## Running untrusted queries

jed is built to evaluate **untrusted, user-supplied SQL** safely: a query — even a hostile one —
cannot reach outside the database, corrupt memory, or exhaust resources. The built-in function
surface is pure (no filesystem, network, process, or clock access beyond a host-injected seam), and
three limits bound the work any one statement can do. Two are caller-set **per-session settings** you
configure on the session that serves untrusted queries — pass them when you mint it, or set them on
the session:

- **Cost ceiling — `set_max_cost(limit)`** / `SetMaxCost` / `setMaxCost`. Bounds the deterministic
  *execution* cost; a query that reaches the ceiling aborts with `54P01`. `0` (the default) is
  unlimited.
- **Input size — `set_max_sql_length(bytes)`** / `SetMaxSQLLength` / `setMaxSqlLength`. Bounds the
  *input SQL length* (in bytes), rejecting an over-long statement with `54000` before it is parsed —
  so a giant query can't exhaust parse memory. The default is **1 MiB**; `0` is unlimited. Because
  jed parses one statement per call, this also bounds the parse tree's size (a million-column
  `SELECT` is just bytes).

Three further limits are fixed engine constants (no configuration): a statement may not nest
expressions/subqueries more than **256** deep (`54001`), a single identifier may not exceed
**63 bytes** (`42622`), and a composite type may not nest more than **32** composites deep
(`54001` at `CREATE TYPE` — a chain of small `CREATE TYPE`s that the input-size cap can't see).
Each limit is deterministic and identical across the Rust, Go, and TypeScript cores.
