<script>
	import LiveSql from '$lib/components/LiveSql.svelte';

	const seed = `CREATE TABLE city (
  id     i32 PRIMARY KEY,
  name   text NOT NULL,
  region i32 NOT NULL
);
CREATE INDEX city_region ON city (region);
INSERT INTO city VALUES
  (1, 'Tokyo',  1),
  (2, 'Osaka',  1),
  (3, 'Paris',  2),
  (4, 'Lyon',   2),
  (5, 'Kyoto',  1);
CREATE TABLE trip (id i32 PRIMARY KEY, city_id i32 NOT NULL);
INSERT INTO trip VALUES (1, 3), (2, 1), (3, 3);`;

	const fullScan = `EXPLAIN SELECT name FROM city ORDER BY name;`;
	const pkBound = `EXPLAIN SELECT name FROM city WHERE id = 3;`;
	const indexBound = `EXPLAIN SELECT name FROM city WHERE region = 1;`;
	const indexRange = `EXPLAIN SELECT name FROM city WHERE region > 1;`;
	const pointSet = `EXPLAIN SELECT name FROM city WHERE id IN (1, 3, 5);`;
	const indexNestedLoop = `EXPLAIN SELECT c.name
FROM trip t JOIN city c ON c.id = t.city_id;`;
	const aggregate = `EXPLAIN SELECT region, count(*)
FROM city GROUP BY region;`;
	const analyze = `EXPLAIN ANALYZE SELECT name FROM city WHERE id = 3;`;
</script>

<svelte:head>
	<title>EXPLAIN — jed</title>
	<meta name="description" content="EXPLAIN in jed renders the planner's chosen plan as a deterministic depth/node/detail result set — the access path, join shape, and sort elision — and EXPLAIN ANALYZE reports the real, deterministic execution cost. Run live." />
</svelte:head>

# EXPLAIN

`EXPLAIN` shows **how** jed will run a statement — which access path the planner chose (a full scan,
a primary-key lookup, a secondary-index bound), how joins are shaped, and whether an `ORDER BY` is
served by scan order or needs a sort. It renders the plan as an ordinary result set with three
columns:

- **`depth`** — the plan node's nesting level (0 = the top of the pipeline). The rows are a
  pre-order walk of the plan tree, so they read top-down as execution reads bottom-up.
- **`node`** — the operator (`Scan`, `Filter`, `Sort`, `Aggregate`, `Nested Loop`, `Limit`, …).
- **`detail`** — its attributes (the access path, key counts, and so on; `-` when it has none).

The plan is a **deterministic** function of the query and the database, so every jed core renders
the identical plan.

## A full scan

With no usable bound, a scan reads the whole table. `touched=` reports how many columns the query
actually references.

<LiveSql seed={seed} query={fullScan} rows={8} />

## A primary-key lookup

A `WHERE` on the primary key bounds the scan to a point lookup — the `PK bound` detail names the
column and predicate. The original `WHERE` stays as the residual `Filter` above the scan.

<LiveSql seed={seed} query={pkBound} rows={8} />

## A secondary-index bound

A predicate on an indexed non-key column uses the index — `Index bound: using <index>`. An
**equality** (`region = 1`) seeks the matching entries:

<LiveSql seed={seed} query={indexBound} rows={8} />

A **range** on an indexed column (`region > 1`, `<`, `<=`, `>=`, `BETWEEN`) is bounded the same way —
jed range-scans the index leaves instead of the whole table:

<LiveSql seed={seed} query={indexRange} rows={8} />

On a **composite** index over `(a, b, …)` the bound extends to a **multi-column prefix**: an equality
on the leading columns, optionally followed by a range on the next (`a = 1 AND b > 3`). The `WHERE`
always stays the residual filter, so the rows are identical to a full scan — only the work drops.
The same `Index bound` detail appears below an `Update` or `Delete` root when a write's target scan
uses that index; an indexed `IN` list appears as `Index point set`.

## An OR / IN-list of keys

An `IN`-list on the primary key — or the equivalent `id = 1 OR id = 3 OR id = 5` — is a **union of
point lookups**, not a full scan: the `PK point set` detail lists the keys, and jed seeks each one
(de-duplicated) instead of walking the whole table. The same applies to an indexed non-key column
(`Index point set: using <index>`).

<LiveSql seed={seed} query={pointSet} rows={8} />

## An index-nested-loop join

When a join's inner relation is matched on **its own primary key** against a column of the outer
relation (`c.id = t.city_id`), jed seeks that inner row **per outer row** instead of re-scanning the
whole inner table — an `Index-nested-loop PK bound` on the inner `Scan`. The join turns O(N·M) into
O(N·log M), and the plan stays a `Nested Loop` whose inner child names the per-outer-row bound.

<LiveSql seed={seed} query={indexNestedLoop} rows={8} />

## Aggregation

`GROUP BY` adds an `Aggregate` node over the scan, carrying the grouping-key and aggregate counts.

<LiveSql seed={seed} query={aggregate} rows={8} />

## EXPLAIN ANALYZE — the real cost

Plain `EXPLAIN` only **plans** the statement — it never runs it, so `EXPLAIN DELETE …` deletes
nothing. `EXPLAIN ANALYZE` also **executes** the statement and reports its **actual** accrued
[cost](../select/) and row count on an `Analyze` node. Because jed's cost is deterministic, this
figure is exact and reproducible — not a wall-clock estimate.

<LiveSql seed={seed} query={analyze} rows={8} />

> `EXPLAIN ANALYZE` of an `INSERT` / `UPDATE` / `DELETE` **does** run the mutation (and commits it),
> exactly like PostgreSQL. Use plain `EXPLAIN` to inspect a write's plan without changing any data.
