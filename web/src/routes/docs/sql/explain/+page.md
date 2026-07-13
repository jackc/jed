<script>
	import LiveSql from '$lib/components/LiveSql.svelte';

	const seed = `CREATE TABLE city (
  id     i32 PRIMARY KEY,
  name   text NOT NULL,
  region i32 NOT NULL,
  zone   i32 NOT NULL
);
CREATE INDEX city_region ON city (region);
INSERT INTO city VALUES
  (1, 'Tokyo',  1, 1),
  (2, 'Osaka',  1, 1),
  (3, 'Paris',  2, 3),
  (4, 'Lyon',   2, 3),
  (5, 'Kyoto',  1, 1),
  (6, 'Rome',   3, 6),
  (7, 'Milan',  3, 7),
  (8, 'Berlin', 4, 8),
  (9, 'Bonn',   4, 9),
  (10, 'Lima',  5, 10);
CREATE TABLE trip (id i32 PRIMARY KEY, city_id i32 NOT NULL);
INSERT INTO trip VALUES
  (1, 3), (2, 1), (3, 3), (4, 4), (5, 5),
  (6, 6), (7, 7), (8, 8), (9, 9), (10, 10);`;

	const fullScan = `EXPLAIN SELECT name FROM city ORDER BY name LIMIT 2;`;
	const pkBound = `EXPLAIN SELECT name FROM city WHERE id = 3;`;
	const indexBound = `EXPLAIN SELECT name FROM city WHERE region = 1;`;
	const indexRange = `EXPLAIN SELECT name FROM city WHERE region > 1;`;
	const pointSet = `EXPLAIN SELECT name FROM city WHERE id IN (1, 3, 5);`;
	const indexNestedLoop = `EXPLAIN SELECT c.name
FROM city c JOIN trip t ON c.id = t.city_id;`;
	const hashJoin = `EXPLAIN SELECT c.name, t.id
FROM city c JOIN trip t ON c.zone = t.city_id;`;
	const aggregate = `EXPLAIN SELECT region, count(*)
FROM city GROUP BY region;`;
	const analyze = `EXPLAIN ANALYZE SELECT name FROM city WHERE id = 3;`;
</script>

<svelte:head>
	<title>EXPLAIN — jed</title>
	<meta name="description" content="EXPLAIN in jed renders the planner's chosen plan with deterministic row and cost estimates — plus the access path, join shape, and sort elision — while EXPLAIN ANALYZE reports real deterministic execution cost. Run live." />
</svelte:head>

# EXPLAIN

`EXPLAIN` shows **how** jed will run a statement — which access path the planner chose (a full scan,
a primary-key lookup, a B-tree/GIN/GiST/interval bound), how joins are shaped, and whether an `ORDER BY` is
served by scan order, a bounded top-k heap, or a full sort. It renders the plan as an ordinary result set with five
columns:

- **`depth`** — the plan node's nesting level (0 = the top of the pipeline). The rows are a
  pre-order walk of the plan tree, so they read top-down as execution reads bottom-up.
- **`node`** — the operator (`Scan`, `Filter`, `Sort`, `Aggregate`, `Nested Loop`, `Hash Join`, `Limit`, …).
- **`detail`** — its attributes (the access path, key counts, and so on; `-` when it has none).
- **`est_rows`** — rows the node is estimated to deliver to its parent. A write root reports
  estimated affected rows.
- **`est_cost`** — cumulative estimated work through that node, using the same deterministic cost
  units as execution.

The plan is a **deterministic** function of the query and the database, so every jed core renders
the identical plan and estimates. For a one-table query, jed compares the complete scheduled work
of full, primary-key, ordered B-tree, GIN, GiST, and interval-set paths, including natural ordering,
the residual/projection, and LIMIT/OFFSET early-out. An `ORDER BY ... LIMIT` may also admit an
order-only B-tree walk. Exact ties use a fixed access-kind order and then lowercased index name.
Estimates are planner heuristics, not a resource limit or a promise to equal execution; runtime cost
ceilings always use the actual meter. For eligible two-table `INNER`/`CROSS` joins, jed also compares
both physical relation orders and legal nested-loop, index-nested-loop, and hash alternatives;
EXPLAIN's child order is the chosen execution order, not necessarily SQL source order. Outer joins,
dependency-bearing inputs, wider joins, and mutation target scans retain their staged policies.

## A blocking sort and bounded top-k

With no usable scan order, `ORDER BY` is blocking and the scan reads the whole table. A finite LIMIT
adds `top-k=K` to the Sort: jed retains only `K = OFFSET + LIMIT` rows while preserving the exact
stable full-sort result. `touched=` reports how many columns the query actually references.

<LiveSql seed={seed} query={fullScan} rows={8} />

## A primary-key lookup

A `WHERE` on the primary key bounds the scan to a point lookup — the `PK bound` detail names the
column and predicate. The original `WHERE` stays as the residual `Filter` above the scan.

For a composite primary key, the detail lists the usable members in key order. jed seeks an exact
tuple when every member has an equality, or scans a leading equality prefix with an optional range
on the next member; a predicate on only a non-leading member remains a full scan.

<LiveSql seed={seed} query={pkBound} rows={8} />

## A secondary-index bound

An applicable predicate on an indexed non-key column contributes an index candidate. When its
estimate wins, EXPLAIN shows `Index bound: using <index>`. An **equality** (`region = 1`) is normally
selective enough to seek the matching entries:

<LiveSql seed={seed} query={indexBound} rows={8} />

A **range** on an indexed column (`region > 1`, `<`, `<=`, `>=`, `BETWEEN`) is also a candidate, but
a broad secondary-index range can cost more than reading the table once because every admitted index
entry needs a table point fetch. This tiny-table example therefore chooses the full scan:

<LiveSql seed={seed} query={indexRange} rows={8} />

On a **composite** index over `(a, b, …)` the candidate bound extends to a **multi-column prefix**: an equality
on the leading columns, optionally followed by a range on the next (`a = 1 AND b > 3`). The `WHERE`
always stays the residual filter, so the rows are identical to a full scan — only the work drops.
The same `Index bound` detail appears below an `Update` or `Delete` root when a write's target scan
uses that index; an indexed `IN` list appears as an `Index interval set`.

## OR / IN key intervals

An `IN`-list on the primary key — or an OR of equality/range predicates on that key — becomes a
canonical interval set, not a full scan. jed encodes runtime values, clips a co-present AND range,
and merges duplicates/overlaps before scanning each disjoint interval once. The same applies to an
indexed non-key column (`Index interval set: using <index>`).

<LiveSql seed={seed} query={pointSet} rows={8} />

## An index-nested-loop join

When a join's inner relation is matched on **its own primary key or index** against a column of the
outer relation (`c.id = t.city_id`), jed opens that bound **per outer row** instead of re-scanning the
whole inner table — an `Index-nested-loop … bound` on the inner `Scan`. Alongside primary-key and
ordered B-tree comparisons, GIN array predicates (`@>`, `&&`, array `=`, scalar `= ANY`) and GiST
range/scalar predicates (`&&`, `@>`, `=`) can use a bare earlier-sibling column. EXPLAIN names these
`Index-nested-loop GIN bound` and `Index-nested-loop GiST bound`. The plan stays a `Nested Loop`; the
inner child makes the per-outer access method visible.

The indexed relation need not appear second in SQL. This example deliberately writes `city` first;
the planner chooses `trip` as the physical outer and `city` as the bounded inner, while projection
still uses the resolved source columns normally.

<LiveSql seed={seed} query={indexNestedLoop} rows={8} />

## A hash join

For an eligible two-table `INNER` equality join, jed compares an in-memory `Hash Join` in both
physical orientations with its nested-loop and INL alternatives. The selected physical inner is
the hash build and the outer is the probe. Bare same-type `ON` column equalities become hash keys;
multiple keys follow source order. NULL keys never match, and the complete `ON` predicate is still
checked on candidates. `keys=N` makes the choice visible. `LEFT` joins retain their authored order,
and a `CROSS JOIN ... WHERE a = b` is not rewritten into hash form. The build table is currently
in-memory; grace-hash spill is a later storage slice.

<LiveSql seed={seed} query={hashJoin} rows={8} />

## Aggregation

`GROUP BY` adds an `Aggregate` node over the scan, carrying the grouping-key and aggregate counts.

<LiveSql seed={seed} query={aggregate} rows={8} />

## EXPLAIN ANALYZE — the real cost

Plain `EXPLAIN` only **plans** the statement — it never runs it, so `EXPLAIN DELETE …` deletes
nothing. `EXPLAIN ANALYZE` also **executes** the statement and reports its **actual** accrued
[cost](../select/) and row count on an `Analyze` node. Because jed's cost is deterministic, this
figure is exact and reproducible — not a wall-clock estimate. The `Analyze` row repeats its child's
planned `est_rows` and `est_cost`; actual figures remain in `detail`, so the two are never conflated.

<LiveSql seed={seed} query={analyze} rows={8} />

> `EXPLAIN ANALYZE` of an `INSERT` / `UPDATE` / `DELETE` **does** run the mutation (and commits it),
> exactly like PostgreSQL. Use plain `EXPLAIN` to inspect a write's plan without changing any data.
