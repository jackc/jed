<script>
	import CodeTabs from '$lib/components/CodeTabs.svelte';
</script>

<svelte:head>
	<title>Resource limits — jed</title>
	<meta name="description" content="Bound what an untrusted jed query can consume: a per-statement cost ceiling (max_cost, 54P01), a per-session cumulative cost budget (lifetime_max_cost, 54P02), scalar and query-memory budgets (54P04, 54P05), and an in-memory storage limit (54P06)." />
</svelte:head>

# Resource limits

jed meters the **execution cost** of every query deterministically — the same query against the same
database always costs the same, on every core. Cost ceilings bound metered work; a separate
scalar allocation budget rejects large repeat/padding results and decimal scratch allocations
before construction; an opt-in query-memory budget bounds the rows a statement holds at
once; and an opt-in storage limit bounds how much an in-memory database keeps. These are
independent limits, with the coverage described below.

## Two ceilings

- **`max_cost` — per statement (`54P01`).** A ceiling on a **single** statement: the instant a
  query's accrued cost reaches it, execution aborts with `54P01`. This stops one runaway query — a
  cross join, a giant `generate_series`, an expensive expression over a huge input.
- **`lifetime_max_cost` — per session (`54P02`).** A budget on the **whole session's cumulative**
  cost. The session holds a running total into which every statement accrues; the instant that total
  reaches the budget, the in-flight statement aborts with `54P02`. This stops a *flood* of cheap
  statements that each slip under `max_cost` but together burn unbounded CPU.

Both default to `0` (unlimited). A statement aborts at whichever ceiling it reaches first.

<CodeTabs topic="resource-limits" />

## How the budget behaves

- **The partial cost of an aborted statement still counts.** The work happened, so it is charged —
  reaching the budget genuinely spends it.
- **Once spent, the session is done.** Every further statement is rejected `54P02` at *admission*,
  before it can run (so a missing-table query under an exhausted budget is `54P02`, not `42P01`).
- **The cumulative is session state, not data.** It does **not** roll back when a transaction rolls
  back — the compute was spent regardless. Read it any time with the cumulative-cost gauge.

This is the clean "this session has a total compute allowance" model for a multi-tenant or
untrusted-query host: a session granted only the privileges it needs, capped per statement, and
budgeted over its lifetime.

## Scalar allocation budget

`max_scalar_bytes` defaults to **64 MiB per statement**. Set it through session options or
`set_max_scalar_bytes(bytes)` / `SetMaxScalarBytes(bytes)` / `setMaxScalarBytes(bytes)`.
Non-positive values restore the finite default. A reservation above it fails with **`54P04`**;
exact equality is allowed. The budget resets for each statement and accumulates across nested
calls, rows, subqueries and CTEs. A cursor keeps its original budget until it closes.

This budget covers the exact UTF-8 output bytes of **`repeat`, `lpad`, `rpad`**, and logical
scratch reservations in **decimal `sqrt`, `exp`, `ln`, `log`, `log10`, `power`, `pow`**.
The string functions also charge input/output byte work before construction; decimals charge
inside their algorithms. A cost ceiling can reject the work before its allocation budget is used.

## Query memory budget

`max_query_memory_bytes` bounds the **rows and operator state a statement holds at once**:
materialized relations, join results, grouped and `DISTINCT` rows, set-operation and CTE buffers,
`RETURNING` results, and the rows the engine's own collect-everything helpers (Rust `query_rows`,
TypeScript `Statement.all()`) gather — plus the working state of the operators that produce them:
hash-join tables, group and dedup tables, aggregate inputs such as `json_agg`'s, window
partitions, and sort buffers. It also holds the writes an open transaction has staged but not yet
committed (see below). It is **unlimited by default** — like `max_cost`, a host serving untrusted
SQL opts in. Set it through session options or `set_max_query_memory_bytes(bytes)` /
`SetMaxQueryMemoryBytes(bytes)` / `setMaxQueryMemoryBytes(bytes)`; non-positive values restore
unlimited. A statement that would exceed it fails with **`54P05`**; exact equality is allowed.

The budget counts **logical bytes** from a fixed schedule — 32 bytes per row, per value, and per
hash-table entry, plus the UTF-8 length of text, the length of bytea, and similar payloads — not
the runtime's actual allocations. That makes the limit deterministic: the same query aborts at the same row on every
core. Rows released along the way (filtered out, consumed by the next stage, handed to your cursor)
give their bytes back, and a streaming query that hands each row to you as it is produced holds
almost nothing. A cursor keeps its statement's budget until it closes.

Uncommitted writes count against the same budget. Every row version and index entry a transaction
writes reserves its stored size — its on-disk record plus any value stored out of line — until the
transaction commits or rolls back. Each statement starts with those bytes already counted, and after
every statement they must still fit, so a long transaction of many small `INSERT`s fails with
`54P05` instead of growing without bound. Rewriting the same row counts again: the budget bounds how
much a transaction writes, not how much it ends up holding. Temporary tables are bounded by
`temp_buffers` instead.

## Storage limit

`max_storage_bytes` bounds how much an **in-memory database keeps** once its transactions commit —
the one thing the per-statement and per-transaction budgets cannot see, since many small committed
transactions each fit them. It belongs to the database rather than to a session, so every session
shares it. Set it when you create an in-memory database (`max_storage_bytes` / `MaxStorageBytes` /
`maxStorageBytes` in the create options), on an in-memory attachment's source, or later with
`set_max_storage_bytes(name, bytes)` / `SetMaxStorageBytes(name, bytes)` /
`setMaxStorageBytes(name, bytes)`, where `name` is `main` or an attachment. It is **unlimited by
default**; non-positive values restore unlimited. A file-backed database rejects a limit with `0A000`.

The limit counts the database's pages: its page high-water times its page size, which the
`storage_bytes(name)` / `StorageBytes(name)` / `storageBytes(name)` gauge reports. A commit that would
raise it past the limit fails with **`54P06`** and commits nothing, after first reclaiming any dead
pages. jed keeps freed pages for reuse and reclaims them in batches, so a database under a limit of
B bytes reliably holds about B/2 bytes of live pages and may hold up to B. A commit that fits in
freed pages always succeeds, even when a lowered limit is already below the current size, and so
does a commit that only deletes rows or drops objects, so a full database can always be cleaned
up. A read session that predates the last commit can keep dead pages from being reclaimed; closing
it frees them for the next commit.

## Page cache

A file-backed database holds its table data in a **page cache** of leaf pages read from the file,
bounded by the `cache_bytes` / `CacheBytes` / `cacheBytes` open option (default **256 MiB**). The
budget becomes a page count, `max(1, cache_bytes / page_size)`, so a budget smaller than one page
still keeps one page cached. When the cache is full, jed evicts a page; it can always read that page
back from the file. Because of that, **the cache never fails a query**, and its size never changes a
query's results or cost. It is a memory and speed setting, not a limit an untrusted query can hit.

The budget bounds the cached pages, with two qualifications:

- **Pages in use stay alive.** An evicted page is not freed while a query or open cursor is still
  reading it: at most one root-to-leaf path per running query, plus each open cursor's current page.
  So the cache can briefly hold a few pages over its budget with many concurrent readers.
- **Some structures are not in the cache.** The interior pages of every table and index tree stay in
  memory. They are a small fraction of a tree's pages, since each one routes to many leaves. The
  index tree of a GiST index also stays in memory, and grows with the number of indexed rows. `cache_bytes` does not bound these. On a
  file-backed database they grow with the file, so bound them by bounding what is stored. On an
  in-memory database they are part of the pages `max_storage_bytes` counts.

An in-memory database ignores `cache_bytes`: all of its pages stay in memory, and `max_storage_bytes`
is the limit for them. The `storage_bytes(name)` gauge also works on a file-backed database, where it
reports the file's page high-water times its page size. There is no limit on that number yet.

## Memory coverage

These budgets are **guardrails, not a process-memory cap**. The query-memory budget covers row
buffers and operator state: hash-join tables, group, `DISTINCT`, and set-operation tables,
`json_agg`-style and ordered-set accumulators, window partitions, sort buffers and top-k heaps,
the resident part of spilling operators, and the writes pending in an open transaction. The
storage limit covers an in-memory database's committed pages. A file-backed database's page cache
is bounded by its `cache_bytes` open option, which evicts pages and never fails a query (see the
page cache section above). Still uncovered: a size cap for file-backed databases and scalar kernels beyond the ones listed above. On native file hosts,
sort, hash JOIN, aggregation, and DISTINCT spill when they exceed `work_mem` — and also when the
query-memory budget would otherwise reject them, so a large sort or aggregate spills rather than
fails and releases its charge as it spills. Row buffers that cannot spill, such as a derived
table's result, still fail at the budget; `work_mem = 0` disables spilling entirely. Spill scratch storage preserves results and deterministic costs;
in-memory and OPFS databases remain resident. `temp_buffers` limits retained temporary storage.
Hosts exposing arbitrary untrusted SQL should combine these settings with process-level memory
limits and account for host-retained results.
