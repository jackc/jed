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

Use the **language selector** in the top bar to switch this example between Rust, Go, and
TypeScript.

<CodeTabs topic="open-database" />

## Durability

Writes accumulate until you **commit**. Closing a database discards uncommitted changes — commit is
always explicit. An in-memory database's commit is a no-op (there is no file to flush). Commits are
durable: the new state lands on disk before the call returns.

## In-memory databases

Every example on the **SQL** pages of these docs runs against an in-memory database, right in your
browser — the same engine, no file. Create one with `Database::new()` (Rust), `jed.NewDatabase()`
(Go), or `new Database()` (TypeScript).

## Running untrusted queries

jed is built to evaluate **untrusted, user-supplied SQL** safely: a query — even a hostile one —
cannot reach outside the database, corrupt memory, or exhaust resources. The built-in function
surface is pure (no filesystem, network, process, or clock access beyond a host-injected seam), and
three limits bound the work any one statement can do. Two are caller-set **per-handle settings** you
configure once on whatever handle serves untrusted queries:

- **Cost ceiling — `set_max_cost(limit)`** / `SetMaxCost` / `setMaxCost`. Bounds the deterministic
  *execution* cost; a query that reaches the ceiling aborts with `54P01`. `0` (the default) is
  unlimited.
- **Input size — `set_max_sql_length(bytes)`** / `SetMaxSQLLength` / `setMaxSqlLength`. Bounds the
  *input SQL length* (in bytes), rejecting an over-long statement with `54000` before it is parsed —
  so a giant query can't exhaust parse memory. The default is **1 MiB**; `0` is unlimited. Because
  jed parses one statement per call, this also bounds the parse tree's size (a million-column
  `SELECT` is just bytes).

Two further limits are fixed engine constants (no configuration): a statement may not nest
expressions/subqueries more than **256** deep (`54001`), and a single identifier may not exceed
**63 bytes** (`42622`). Each limit is deterministic and identical across the Rust, Go, and
TypeScript cores.
