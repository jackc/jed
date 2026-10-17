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
  (6, 6), (7, 7), (8, 8), (9, 9), (10, 10);
CREATE TABLE region (id i32 PRIMARY KEY, name text NOT NULL);
INSERT INTO region VALUES
  (1, 'Kansai'), (2, 'France'), (3, 'Italy'), (4, 'Germany'), (5, 'Peru');`;

	const fullScan = `EXPLAIN SELECT name FROM city ORDER BY name LIMIT 2;`;
	const pkBound = `EXPLAIN SELECT name FROM city WHERE id = 3;`;
	const indexBound = `EXPLAIN SELECT name FROM city WHERE region = 1;`;
	const indexRange = `EXPLAIN SELECT name FROM city WHERE region > 1;`;
	const pointSet = `EXPLAIN SELECT name FROM city WHERE id IN (1, 3, 5);`;
	const anyPointSet = `EXPLAIN SELECT name FROM city WHERE id = ANY(ARRAY[1, 3, 5]);`;
	const stockSeed = `CREATE TABLE stock (
  store i32,
  sku   i32,
  qty   i32 NOT NULL,
  PRIMARY KEY (store, sku)
);
INSERT INTO stock
  SELECT s, k, (s * k) % 50
  FROM generate_series(1, 10) AS s, generate_series(1, 500) AS k;`;
	const tupleSet = `EXPLAIN SELECT sku, qty FROM stock
WHERE store = 3 AND sku = ANY(ARRAY[7, 42, 99]);`;
	const saleSeed = `CREATE TABLE sale (
  id    i32 PRIMARY KEY,
  store i32 NOT NULL,
  sku   i32 NOT NULL
);
INSERT INTO sale
  SELECT g, (g - 1) % 10 + 1, (g - 1) / 10 + 1
  FROM generate_series(1, 5000) AS g;
CREATE INDEX sale_store_sku ON sale (store, sku);`;
	const indexTupleSet = `EXPLAIN SELECT id FROM sale
WHERE store = 3 AND sku = ANY(ARRAY[7, 42, 99]);`;
	const indexNestedLoop = `EXPLAIN SELECT c.name
FROM city c JOIN trip t ON c.id = t.city_id;`;
	const hashJoin = `EXPLAIN SELECT c.name, t.id
FROM city c JOIN trip t ON c.zone = t.city_id;`;
	const nwayJoin = `EXPLAIN SELECT c.name, t.id, r.name
FROM city c
JOIN trip t ON c.id = t.city_id
JOIN region r ON r.id = c.region;`;
	const pushdown = `EXPLAIN SELECT c.name, t.id
FROM city c JOIN trip t ON c.zone = t.city_id
WHERE c.zone < 8 AND t.id > 2;`;
	const contradiction = `EXPLAIN SELECT name FROM city WHERE zone > 5 AND zone < 3;`;
	const derivedPushdown = `EXPLAIN SELECT v.name
FROM (SELECT id, name, region FROM city) v
WHERE v.id = 3;`;
	const groupedPushdown = `EXPLAIN SELECT g.region, g.cities
FROM (SELECT region, count(*) AS cities FROM city GROUP BY region) g
WHERE g.region = 2;`;
	const windowPushdown = `EXPLAIN SELECT r.name, r.pos
FROM (SELECT name, region,
             row_number() OVER (PARTITION BY region ORDER BY name) AS pos
      FROM city) r
WHERE r.region = 2;`;
	const ctePushdown = `EXPLAIN WITH c AS (SELECT id, name, region FROM city)
SELECT c.name FROM c WHERE c.id = 3;`;
	const setopPushdown = `EXPLAIN SELECT x.id
FROM (SELECT id FROM city UNION SELECT city_id FROM trip) x
WHERE x.id = 3;`;
	const onPushdown = `EXPLAIN SELECT c.name, t.id
FROM city c LEFT JOIN trip t ON c.id = t.city_id AND t.id > 5;`;
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
served by scan order, a bounded top-k heap, or a full sort. It renders the plan as an ordinary result set. Bare
`EXPLAIN` has five columns:

- **`depth`** — the plan node's nesting level (0 = the top of the pipeline). The rows are a
  pre-order walk of the plan tree, so they read top-down as execution reads bottom-up.
- **`node`** — the operator (`Scan`, `Filter`, `Sort`, `Aggregate`, `Nested Loop`, `Hash Join`, `Limit`, …).
- **`detail`** — its attributes (the access path, key counts, and so on; `-` when it has none).
- **`est_rows`** — rows the node is estimated to deliver to its parent. A write root reports
  estimated affected rows.
- **`est_cost`** — cumulative estimated work through that node, using the same deterministic cost
  units as execution.

The parenthesized option form controls additional detail and columns:

```sql
EXPLAIN (VERBOSE, COSTS OFF, LANE ON) SELECT name FROM city WHERE id = 3;
EXPLAIN (ANALYZE TRUE, COSTS FALSE) SELECT name FROM city WHERE id = 3;
```

`VERBOSE` replaces compact predicate counts with canonical resolved expressions and appends the
projection list as `output=[...]`. `COSTS` controls the two estimate columns (on by default), `ANALYZE`
adds `actual_cost`, and `LANE` adds `streaming`, `buffered`, or `deferred`. Boolean options accept
`TRUE`/`FALSE` or `ON`/`OFF`; a missing value means true. Exact float bounds are shown as their
PostgreSQL text rather than a placeholder. SQL/JSON expressions include their resolved
`RETURNING`, wrapper/quotes, and `ON EMPTY`/`ON ERROR` behavior. Lanes describe the public dispatcher:
sequence-mutating SELECTs and WITH statements containing DML are buffered writes regardless of shape.

The plan is a **deterministic** function of the query and the database, so every jed core selects
the identical plan and estimates (with only the ledgered native float-text layout exception). For a one-table query, jed compares the complete scheduled work
of full, primary-key, ordered B-tree, GIN, GiST, and interval-set paths, including natural ordering,
the residual/projection, and LIMIT/OFFSET early-out. An `ORDER BY ... LIMIT` may also admit an
order-only B-tree walk. Exact ties use a fixed access-kind order and then lowercased index name.
Estimates are planner heuristics, not a resource limit or a promise to equal execution; runtime cost
ceilings always use the actual meter. For base-table `INNER`/`CROSS` joins, jed jointly compares
physical relation order, access paths, and legal nested-loop, index-nested-loop, and hash alternatives.
EXPLAIN's child order is the chosen execution order, not necessarily SQL source order. Search is
exhaustive and left-deep through eight movable relations, then uses deterministic cheapest-next
construction. Outer joins and dependency-bearing inputs are hard fences: nothing crosses them, but
base-table islands on either side may still be searched. Mutation target scans retain their staged
policy.

## A blocking sort and bounded top-k

With no usable scan order, `ORDER BY` is blocking and the scan reads the whole table. A finite LIMIT
adds `top-k=K` to the Sort: jed retains only `K = OFFSET + LIMIT` rows while preserving the exact
stable full-sort result. `touched=` reports how many stored columns the statement actually references;
UPDATE/DELETE include filter and assignment sources plus the storage-reading side of `RETURNING`.

<LiveSql demo="fullScan" seed={seed} query={fullScan} rows={8} />

## A primary-key lookup

A `WHERE` on the primary key bounds the scan to a point lookup — the `PK bound` detail names the
column and predicate. The original `WHERE` stays as the residual `Filter` above the scan.

For a composite primary key, the detail lists the usable members in key order. jed seeks an exact
tuple when every member has an equality, or scans a leading equality prefix with an optional range
on the next member; a predicate on only a non-leading member remains a full scan. `IN` lists and
`= ANY(array)` on the members are covered under *OR / IN key intervals* below.

<LiveSql demo="pkBound" seed={seed} query={pkBound} rows={8} />

## A secondary-index bound

An applicable predicate on an indexed non-key column contributes an index candidate. When its
estimate wins, EXPLAIN shows `Index bound: using <index>`. An **equality** (`region = 1`) is normally
selective enough to seek the matching entries:

<LiveSql demo="indexBound" seed={seed} query={indexBound} rows={8} />

A **range** on an indexed column (`region > 1`, `<`, `<=`, `>=`, `BETWEEN`) is also a candidate, but
a broad secondary-index range can cost more than reading the table once because every admitted index
entry needs a table point fetch. This tiny-table example therefore chooses the full scan:

<LiveSql demo="indexRange" seed={seed} query={indexRange} rows={8} />

On a **composite** index over `(a, b, …)` the candidate bound extends to a **multi-column prefix**: an equality
on the leading columns, optionally followed by a range on the next (`a = 1 AND b > 3`). The `WHERE`
always stays the residual filter, so the rows are identical to a full scan — only the work drops.
The same `Index bound` detail appears below an `Update` or `Delete` root when a write's target scan
uses that index; an indexed `IN` list appears as an `Index interval set`. An `UPDATE` or `DELETE`
chooses its target scan by the same estimate as a one-table `SELECT`, so the two show the same bound
for the same `WHERE`. The chosen scan also fixes the order in which a write visits rows: when several
rows would raise an error, the first one visited — in that scan's order — is the one reported.

## OR / IN key intervals

An `IN`-list on the primary key — or an OR of equality/range predicates on that key — becomes a
canonical interval set, not a full scan. jed encodes runtime values, clips a co-present AND range,
and merges duplicates/overlaps before scanning each disjoint interval once. The same applies to an
indexed non-key column (`Index interval set: using <index>`).

<LiveSql demo="pointSet" seed={seed} query={pointSet} rows={8} />

`key = ANY(array)` is the array spelling of the same list, and it takes the same path: each element
of the array becomes one point in the interval set. That is what makes a bound list — `WHERE id =
ANY($1)` — a set of key lookups rather than a full scan. The array must be constant for the query (a
parameter, a literal or cast array, an outer query's column, or an `ARRAY[...]` of constants). Its
element type is usually the key's type, but an integer array also works against an integer key of
another width or a `decimal` key, and a `decimal` array against an integer key: each element is
converted to the key's type, and one no key can equal (out of range, or with a fractional part) is
skipped. EXPLAIN counts it as one of the set's intervals and marks it `arrays=1`,
since its point count is known only when the query runs. The estimate reads the array's elements,
so a bound list is planned with the values each execution binds.

<LiveSql demo="anyPointSet" seed={seed} query={anyPointSet} rows={8} />

On a **composite** primary key, each member of the key can take one of these lists. `store = 3 AND
sku = ANY(...)` looks up each `(store, sku)` pair instead of reading all of store 3's rows, and
`store IN (1, 2) AND sku IN (7, 9)` looks up all four pairs. The detail names the key members the
set covers, in key order. Members are used in key order while each has an equality or a list of
values, and the first member with a range (`sku > 40`) is the last one used. With no condition on a
later member, the set scans each listed prefix, so `store IN (1, 2)` reads both stores' rows. To keep
the work bounded, a list is used only while the number of combinations stays at or below 65,536.

<LiveSql demo="tupleSet" seed={stockSeed} query={tupleSet} rows={8} />

A multi-column secondary index takes the same lists over its columns in index order. With an index on
`(store, sku)`, the same `WHERE` looks up each `(store, sku)` pair in the index and fetches only those
rows. The detail lists the index columns the set covers after the index name. To find each entry's
row, jed steps over every index column from the first non-leading column with a list or range onward,
which works for fixed-width types and for `text`, `bytea`, `decimal`, and `interval`; a `text` column
with a non-`C` collation there (even one the query does not mention) leaves only the plain index
bound.

<LiveSql demo="indexTupleSet" seed={saleSeed} query={indexTupleSet} rows={8} />

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

<LiveSql demo="indexNestedLoop" seed={seed} query={indexNestedLoop} rows={8} />

## A hash join

At an eligible `INNER` equality step, jed compares an in-memory `Hash Join` in each legal physical
orientation with its nested-loop and INL alternatives. The selected physical inner is the hash
build and the accumulated left prefix is the probe. Bare same-type `ON` column equalities become hash keys;
multiple keys follow source order. NULL keys never match, and the complete `ON` predicate is still
checked on candidates. `keys=N` makes the choice visible. `LEFT` joins retain their authored order,
and a `CROSS JOIN ... WHERE a = b` is not rewritten into hash form. The build table is currently
in-memory; grace-hash spill is a later storage slice.

<LiveSql demo="hashJoin" seed={seed} query={hashJoin} rows={8} />

## Bounded N-way join ordering

For three or more base relations, each left-deep step carries its selected access method and join
algorithm. An index-nested-loop dependency becomes legal only after its referenced sibling is in the
physical left prefix. Each authored `ON` expression stays intact and runs at the earliest step where
its original right-side owner and every referenced relation are present. With an ordered LIMIT, jed
materializes the selected left subtree and streams only the final join step, so its estimates never
discount work the executor still performs.

<LiveSql demo="nwayJoin" seed={seed} query={nwayJoin} rows={11} />

## Pushed filters and contradictions

Before choosing access paths, jed rewrites the `WHERE` in two ways. In a join, a condition that
mentions only one base table and **cannot raise an error** runs **inside that table's scan**, so
rejected rows never reach the join. That covers comparisons, `IS [NOT] NULL`, `IS [NOT] DISTINCT
FROM`, and `AND`/`OR`/`NOT` over columns, literals, and parameters, plus the casts and functions that
never fail (`length`, `lower`/`upper`, the trims, `starts_with`, `strpos`, integer → `text`/`decimal`/
float casts, …) and a `LIKE`/`ILIKE` whose pattern is known not to end in a lone `\` (a bound
parameter counts, see below). Arithmetic and anything else that can fail stays where you wrote it. EXPLAIN shows it on the `Scan` as `filter:conjuncts=N`
(`VERBOSE` spells the expression), and the `Filter` node above the join keeps only what still needs
both sides (it disappears when nothing is left). Conditions on the NULL-extended side of an outer join
stay above the join, where they see the NULL-extended rows.

<LiveSql demo="pushdown" seed={seed} query={pushdown} rows={8} />

If the `WHERE`'s literals rule out every row — `zone > 5 AND zone < 3`, `x = 1 AND x = 2`,
`WHERE false` — the statement reads no table at all. The plan keeps its shape, the `Filter` detail
ends in `contradiction`, and every scan estimates (and costs) zero. An ungrouped aggregate still returns
its one row. `UPDATE` and `DELETE` get the same check: a contradictory mutation reads no row, evaluates
none of its `SET` or `RETURNING` expressions, and changes nothing.

<LiveSql demo="contradiction" seed={seed} query={contradiction} rows={4} />

A **bound parameter** counts as a literal here and in every estimate: each execution is planned with
its values (its *custom plan* — see [prepared statements](/docs/api/queries)), so `zone > $1 AND zone
< $2` bound to 5 and 3 reads no table either. Run `EXPLAIN` from your host with the values to see the
plan they get; a `$N` still prints as `$N`. Without values, `EXPLAIN` plans the parameters as unknown.

A condition on a subquery in `FROM` moves **into** the subquery when it only compares the subquery's
plain output columns and the subquery has no `LIMIT` or `OFFSET`. The subquery is then planned as if
you had written the condition inside it — here it becomes a primary-key lookup on `city` rather than a
filter over every row the subquery returns. (A subquery that doesn't qualify, a materialized `WITH`
query, or a set-returning function in a join still gets the condition applied to its rows before the
join, shown as `filter:conjuncts=N` on its node.)

<LiveSql demo="derivedPushdown" seed={seed} query={derivedPushdown} rows={4} />

A grouped subquery takes the condition too when it only compares **grouping columns**: it then decides
which whole groups exist, so it runs before the grouping — here it bounds the `city_region` index under
the `Aggregate`, and only one region's cities are counted. A condition on an aggregate result, or on a
`ROLLUP`/`CUBE`/`GROUPING SETS` subquery, stays outside.

<LiveSql demo="groupedPushdown" seed={seed} query={groupedPushdown} rows={4} />

A subquery with a window function takes the condition when every window is `PARTITION BY` the columns
it compares: it then keeps or drops whole partitions, so each row's window value is unchanged. Here
`r.region = 2` runs below the `Window` and bounds `city_region`, and only region 2's cities are
numbered. A condition on any other column — or on a window without `PARTITION BY` — stays outside,
since filtering first would change the numbering.

<LiveSql demo="windowPushdown" seed={seed} query={windowPushdown} rows={4} />

A `WITH` query that is used **exactly once** (and not marked `MATERIALIZED`) runs in place, like a
subquery — and is planned there like one, so it takes the condition the same way. Its body appears
under its `CTE Scan` node; here it becomes a primary-key lookup. A `WITH` query used twice is computed
once into a buffer, so a condition is only applied to the rows each use reads from it.

<LiveSql demo="ctePushdown" seed={seed} query={ctePushdown} rows={5} />

A `UNION`, `INTERSECT`, or `EXCEPT` subquery takes the condition into **every branch**, and each branch
plans it on its own table: here it is a primary-key lookup on `city` and a filter on `trip`'s scan.
A branch whose column jed widens to the combined type gets the condition with the widening applied: an
`i32` branch of an `i64` column takes it as written, and an `i32` branch of a `decimal` column (or an
`f32` branch of an `f64` one) reads its column through `cast(… as decimal)`, exactly the value the
combined column holds.

<LiveSql demo="setopPushdown" seed={seed} query={setopPushdown} rows={6} />

The same applies to a join's `ON`: a condition on one table moves to that table when the join allows
it — either side of an inner join, or the side an outer join fills with NULLs. Here `t.id > 5` only
decides which `trip` rows can match, so it runs in `trip`'s scan (and bounds its key); the join keeps
`c.id = t.city_id`, and cities without a matching trip are still returned with NULLs. A condition on
the side an outer join preserves stays in the `ON`.

<LiveSql demo="onPushdown" seed={seed} query={onPushdown} rows={4} />

## Aggregation

`GROUP BY` adds an `Aggregate` node over the scan, carrying the grouping-key and aggregate counts.

<LiveSql demo="aggregate" seed={seed} query={aggregate} rows={8} />

## EXPLAIN ANALYZE — the real cost

Plain `EXPLAIN` only **plans** the statement — it never runs it, so `EXPLAIN DELETE …` deletes
nothing. `EXPLAIN ANALYZE` also **executes** the statement and reports its **actual** accrued
[cost](../select/) and row count on an `Analyze` node. Because jed's cost is deterministic, this
figure is exact and reproducible — not a wall-clock estimate. The `actual_cost` column gives inclusive
measured attribution for every plan node; `est_rows` and `est_cost` remain the independent planner
estimate columns. Nested query frames keep same-labeled scans distinct, and work from a folded
uncorrelated expression subquery is included at its containing visible operator and parents.

<LiveSql demo="analyze" seed={seed} query={analyze} rows={8} />

> `EXPLAIN ANALYZE` of an `INSERT` / `UPDATE` / `DELETE` **does** run the mutation (and commits it),
> exactly like PostgreSQL. Use plain `EXPLAIN` to inspect a write's plan without changing any data.

Plain EXPLAIN also supports a data-modifying `WITH`: every `CTE <name>` definition and its complete
write subtree are displayed in authored order, followed by the primary query or DML subtree, without
running any of them.

`EXPLAIN CREATE TABLE … AS` (and `EXPLAIN SELECT … INTO`) shows the plan of the statement's query, as
in PostgreSQL. Plain `EXPLAIN` creates nothing. It still checks the query and the table name, and
with `IF NOT EXISTS` an existing table gives an empty plan. `EXPLAIN ANALYZE` creates and fills the
table, and its `Analyze` node reports the statement's cost and the number of rows inserted.
