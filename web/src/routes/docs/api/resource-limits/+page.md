<script>
	import CodeTabs from '$lib/components/CodeTabs.svelte';
</script>

<svelte:head>
	<title>Resource limits — jed</title>
	<meta name="description" content="Bound what an untrusted jed query can consume: a per-statement cost ceiling (max_cost, 54P01) and a per-session cumulative cost budget (lifetime_max_cost, 54P02)." />
</svelte:head>

# Resource limits

jed meters the **execution cost** of every query deterministically — the same query against the same
database always costs the same, on every core. Two ceilings turn that meter into the resource half of
the "untrusted SQL is safe to run" guarantee (the [Authorization](../authorization/) page is
the privilege half). Pair them and you can hand an adversary a query surface.

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
