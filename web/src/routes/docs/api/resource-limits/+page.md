<script>
	import CodeTabs from '$lib/components/CodeTabs.svelte';
</script>

<svelte:head>
	<title>Resource limits — jed</title>
	<meta name="description" content="Bound what an untrusted jed query can consume: a per-statement cost ceiling (max_cost, 54P01), a per-session cumulative cost budget (lifetime_max_cost, 54P02), and scalar and query-memory budgets (54P04, 54P05)." />
</svelte:head>

# Resource limits

jed meters the **execution cost** of every query deterministically — the same query against the same
database always costs the same, on every core. Cost ceilings bound metered work; a separate
scalar allocation budget rejects large repeat/padding results and decimal scratch allocations
before construction; and an opt-in query-memory budget bounds the rows a statement holds at
once. These are independent limits, with the coverage described below.

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

`max_query_memory_bytes` bounds the **rows a statement holds at once**: materialized relations,
join results, grouped and `DISTINCT` rows, set-operation and CTE buffers, `RETURNING` results, and
the rows the engine's own collect-everything helpers (Rust `query_rows`, TypeScript
`Statement.all()`) gather. It is **unlimited by default** — like `max_cost`, a host serving untrusted
SQL opts in. Set it through session options or `set_max_query_memory_bytes(bytes)` /
`SetMaxQueryMemoryBytes(bytes)` / `setMaxQueryMemoryBytes(bytes)`; non-positive values restore
unlimited. A statement that would exceed it fails with **`54P05`**; exact equality is allowed.

The budget counts **logical bytes** from a fixed schedule — 32 bytes per row and per value, plus
the UTF-8 length of text, the length of bytea, and similar payloads — not the runtime's actual
allocations. That makes the limit deterministic: the same query aborts at the same row on every
core. Rows released along the way (filtered out, consumed by the next stage, handed to your cursor)
give their bytes back, and a streaming query that hands each row to you as it is produced holds
almost nothing. A cursor keeps its statement's budget until it closes.

## Memory coverage

These budgets are **guardrails, not a process-memory cap**. Still uncovered: operator state
(hash-join tables, sort buffers, `DISTINCT`/group sets, `json_agg`-style
accumulators), writes pending in an open transaction, page caches, and scalar kernels beyond
the ones listed above. On native file hosts, `work_mem` controls sort, hash JOIN, aggregation,
and DISTINCT spilling. Their scratch storage preserves results and deterministic costs;
in-memory and OPFS databases remain resident. `temp_buffers` limits retained temporary storage.
Hosts exposing arbitrary untrusted SQL should combine these settings with process-level memory
limits and account for host-retained results.
