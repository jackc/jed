<script>
	import CodeTabs from '$lib/components/CodeTabs.svelte';
</script>

<svelte:head>
	<title>Resource limits — jed</title>
	<meta name="description" content="Bound what an untrusted jed query can consume: a per-statement cost ceiling (max_cost, 54P01) and a per-session cumulative cost budget (lifetime_max_cost, 54P02)." />
</svelte:head>

# Resource limits

jed meters the **execution cost** of every query deterministically — the same query against the same
database always costs the same, on every core. Cost ceilings bound metered work; a separate
scalar allocation budget rejects large repeat/padding results and decimal scratch allocations
before construction. These are independent limits, with the coverage described below.

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

## Memory coverage

The scalar allowance is **not a whole-query or process-memory cap**. Other scalar kernels,
input values, decoded rows, join/aggregate/distinct state, result collectors and pending writes
still need general memory admission. `work_mem` controls sort spilling and `temp_buffers`
limits retained temporary storage; neither is a total heap limit. Hosts exposing arbitrary
untrusted SQL must account for these remaining allocations and host-retained results.
