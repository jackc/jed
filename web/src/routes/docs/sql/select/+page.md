<script>
	import LiveSql from '$lib/components/LiveSql.svelte';

	const seed = `CREATE TABLE product (
  id       i32 PRIMARY KEY,
  name     text NOT NULL,
  category text NOT NULL,
  price    numeric(8,2) NOT NULL
);
INSERT INTO product VALUES
  (1, 'Pen',      'office',  1.50),
  (2, 'Notebook', 'office',  4.00),
  (3, 'Coffee',   'kitchen', 9.99),
  (4, 'Mug',      'kitchen', 12.50);`;

	const grouped = `SELECT category, count(*) AS items, sum(price) AS total
FROM product
GROUP BY category
ORDER BY category;`;

	const filtered = `SELECT name, price
FROM product
WHERE price > 4
ORDER BY price DESC;`;

	const cte = `WITH kitchen AS (
  SELECT name, price FROM product WHERE category = 'kitchen'
)
SELECT name, price FROM kitchen ORDER BY price DESC;`;

	const recursive = `WITH RECURSIVE series(n) AS (
  SELECT 1
  UNION ALL
  SELECT n + 1 FROM series WHERE n < 5
)
SELECT n FROM series ORDER BY n;`;

	const nestedWith = `SELECT category, cheapest
FROM (
  WITH cheap AS (
    SELECT category, min(price) AS cheapest FROM product GROUP BY category
  )
  SELECT category, cheapest FROM cheap WHERE cheapest <= 10
) AS s
ORDER BY category;`;

	const dataModifying = `WITH discounted AS (
  UPDATE product SET price = price * 0.9
  WHERE category = 'kitchen'
  RETURNING name, price
)
SELECT name, price FROM discounted ORDER BY price;`;

	const derived = `SELECT category, top
FROM (SELECT category, max(price) AS top FROM product GROUP BY category) AS d
WHERE top > 5
ORDER BY category;`;

	const valuesBody = `SELECT label, qty * 2 AS doubled
FROM (VALUES ('a', 1), ('b', 2), ('c', 3)) AS v (label, qty)
ORDER BY label;`;

	const lateral = `SELECT c.category, top.name, top.price
FROM (SELECT DISTINCT category FROM product) AS c
CROSS JOIN LATERAL (
  SELECT name, price FROM product
  WHERE category = c.category
  ORDER BY price DESC LIMIT 1
) AS top
ORDER BY c.category;`;

	const unnestExample = `SELECT u AS tag
FROM unnest(ARRAY['red', 'green', 'blue']) AS u
ORDER BY u;`;

	const containmentExample = `SELECT ARRAY[1, 2, 3] @> ARRAY[2]   AS contains,
       ARRAY[2]       <@ ARRAY[1, 2, 3] AS contained_by,
       ARRAY[1, 2]    && ARRAY[2, 3]    AS overlaps;`;

	const quantifiedExample = `SELECT 2 = ANY(ARRAY[1, 2, 3])  AS any_match,
       5 > ALL(ARRAY[1, 2, 3])  AS all_greater,
       9 = ANY(ARRAY[1, 2, 3])  AS no_match;`;

	const quantifiedSubquery = `SELECT name, price
FROM product
WHERE price > ANY(SELECT price FROM product WHERE category = 'office')
ORDER BY price;`;

	const variadicExample = `SELECT num_nulls(1, NULL, 3)                AS spread,
       num_nulls(VARIADIC ARRAY[1, NULL, 3]) AS variadic,
       num_nonnulls(1, NULL, 3)              AS non_nulls;`;

	const arrayCompositeSeed = `CREATE TYPE addr AS (street text, zip i32);
CREATE TABLE person (id i32 PRIMARY KEY, places addr[]);
INSERT INTO person VALUES
  (1, ARRAY[ROW('Main', 90210), ROW('Side', 5)]),
  (2, '{"(Oak,)"}');`;

	const arrayCompositeExample = `SELECT id,
       places[1]            AS first,
       (places[1]).street   AS first_street,
       (places[1]).zip      AS first_zip
FROM person
ORDER BY id;`;

	const unnestCompositeExample = `SELECT u          AS place,
       (u).street AS street,
       (u).zip    AS zip
FROM unnest('{"(Main,90210)","(Side,5)"}'::addr[]) AS u
ORDER BY u;`;

	const arrayCompositeFnExample = `SELECT id,
       cardinality(places)            AS n,
       '(Side,5)'::addr = ANY(places) AS has_side
FROM person
ORDER BY id;`;

	const compositeArrayFieldSeed = `CREATE TYPE poly AS (name text, pts i32[]);
CREATE TABLE shapes (id i32 PRIMARY KEY, p poly);
INSERT INTO shapes VALUES
  (1, ROW('a', ARRAY[10, 20, 30])),
  (2, ROW('b', ARRAY[5]));`;

	const compositeArrayFieldExample = `SELECT id,
       p,
       (p).pts      AS points,
       (p).pts[1]   AS first_point
FROM shapes
ORDER BY p;`;

	const dateSeed = `CREATE TABLE event (id i32 PRIMARY KEY, name text, on_day date);
INSERT INTO event VALUES
  (1, 'launch',  '2024-01-15'),
  (2, 'review',  '2023-11-02'),
  (3, 'kickoff', '2024-06-15'),
  (4, 'someday', 'infinity');`;

	const dateExample = `SELECT name, on_day
FROM event
WHERE on_day < DATE '2024-03-01'
ORDER BY on_day;`;
</script>

<svelte:head>
	<title>Querying — jed</title>
	<meta name="description" content="SELECT, WHERE, ORDER BY, GROUP BY and aggregates in jed — run against a live database." />
</svelte:head>

# Querying

`SELECT` supports the usual shape: `WHERE`, `ORDER BY`, `LIMIT` / `OFFSET`, `DISTINCT`, joins,
`GROUP BY` with `HAVING`, set operations, subqueries, and `WITH` (common table expressions).
Aggregates use PostgreSQL-style widening
(for example, `sum` over `numeric` returns `numeric`, exact), and accept a leading `DISTINCT`
(`count(DISTINCT x)`) and a trailing `FILTER (WHERE …)` (`count(*) FILTER (WHERE x > 0)`).
`GROUP BY` also takes `GROUPING SETS`, `ROLLUP`, and `CUBE` to compute several groupings at once,
with the `GROUPING(col)` function to tell subtotal rows apart.

Grouping and aggregation:

<LiveSql {seed} query={grouped} rows={6} />

Filtering and ordering — edit the `WHERE` and `ORDER BY` and re-run:

<LiveSql {seed} query={filtered} rows={6} />

## Common table expressions (`WITH`)

A `WITH` clause names a query and exposes it to the `FROM` clause like a table. Define one or more —
each is visible to later ones and to the main query:

<LiveSql {seed} query={cte} rows={4} />

CTEs follow PostgreSQL's evaluation rule: a CTE referenced once is **inlined**, one referenced
several times (or marked `MATERIALIZED`) runs once and its rows are **buffered** and reused. Add an
optional column-rename list (`WITH t (a, b) AS (…)`).

### Data-modifying CTEs

A CTE's body — and the statement a `WITH` prefixes — may be an `INSERT`, `UPDATE`, or `DELETE`. Its
`RETURNING` rows flow forward like any CTE's, so one statement can write and read its own changes:

<LiveSql {seed} query={dataModifying} rows={2} />

The canonical use is moving rows between tables atomically:

```sql
WITH moved AS (DELETE FROM inbox WHERE ready RETURNING *)
INSERT INTO archive SELECT * FROM moved;
```

Every part of the statement reads **one pre-statement snapshot** — the parts cannot see each other's
effects on the target tables, so data crosses only through a CTE's `RETURNING` buffer (`SELECT
count(*) FROM inbox` next to the `DELETE` above still sees the rows). The data-modifying parts run
once each, in order, and the whole statement is one all-or-nothing transaction.

### Recursive CTEs (`WITH RECURSIVE`)

`WITH RECURSIVE` lets a CTE reference **itself**, computing a result by iterating to a fixpoint —
hierarchy walks, graph reachability, generated series. The body is a `UNION`: the left side (the
**non-recursive term**) seeds the result, and the right side (the **recursive term**) is re-evaluated
against the rows the previous step produced until it yields none:

<LiveSql {seed} query={recursive} rows={5} />

`UNION ALL` keeps every row; `UNION` drops rows that duplicate one already produced (which is what
makes a cyclic graph walk terminate). The column types are fixed by the non-recursive term. A
recursion with no stopping condition runs until it hits the statement's
[cost ceiling](/docs/api/resource-limits) — set one when running untrusted queries. `SEARCH` /
`CYCLE` clauses and mutual recursion are not yet supported.

### Nested `WITH`

A `WITH` clause may also prefix any **parenthesized** query — a subquery, a derived table, or another
CTE's body — so you can scope helper CTEs to just the part of the statement that needs them. The
nested CTEs (including `WITH RECURSIVE`) get the full machinery above:

<LiveSql {seed} query={nestedWith} rows={2} />

A nested `WITH` establishes its **own** scope: its CTEs are visible only inside that parenthesized
query. One difference from PostgreSQL: a nested `WITH` does **not** inherit the *enclosing*
statement's CTEs — an outer CTE name referenced inside an inner `WITH` resolves to a base table (or is
an error), not the outer CTE. Data-modifying CTEs (`INSERT`/`UPDATE`/`DELETE`) stay top-level only, as
in PostgreSQL.

## Derived tables (`FROM (SELECT …) AS t`)

A `FROM` item can be a parenthesized subquery used as a relation — a **derived table**. It is an
anonymous, always-inlined single-reference CTE: the body runs in place, and you reference its output
columns through the alias. The alias is optional (matching PostgreSQL 18); when present it may carry a
column-rename list (`AS t (a, b)`):

<LiveSql {seed} query={derived} rows={4} />

By default the body is an independent query — it cannot see the enclosing query's other `FROM`
relations. Mark it `LATERAL` (below) to correlate it. A parenthesized join (`FROM (a JOIN b …)`) and a
`WITH` body are not yet supported.

## `VALUES` lists in `FROM` (`FROM (VALUES …) AS v(x)`)

A derived table's body may also be a `VALUES` list — a literal table of rows you write inline,
without a base table. The columns default to `column1`, `column2`, … (overridable by the alias's
column-rename list), and each column's type unifies across the rows the way `UNION` does. Each value
is a general constant expression (`qty * 2` below):

<LiveSql {seed} query={valuesBody} rows={3} />

Order or limit the **outer** query (`FROM (VALUES …) v ORDER BY x`); a trailing `ORDER BY` / `LIMIT`
inside the parentheses is not yet supported.

## `LATERAL` joins

A `LATERAL` `FROM` item may reference columns of the `FROM` relations that appear **before** it — a
dependent join re-evaluated once per left-hand row. It is the idiomatic way to express
**top-N-per-group**: for each category, the single most expensive product.

<LiveSql {seed} query={lateral} rows={2} />

Reach it through explicit join syntax — `CROSS JOIN LATERAL`, `JOIN LATERAL … ON …`, or `LEFT JOIN
LATERAL … ON …` (a `LEFT JOIN` keeps a left row even when the lateral side produces no rows). A
sub-`SELECT` or `VALUES` body is lateral only with the keyword; a **table function** (`generate_series`,
`unnest`) is *implicitly* lateral, so `… CROSS JOIN unnest(t.tags)` correlates with or without it.

## Set-returning functions in `FROM`

A `FROM` item can be a set-returning function — a computed row source instead of a stored table.
`generate_series(start, stop[, step])` yields an integer series; `unnest(anyarray)` expands an array
into one row per element (a multidimensional array flattens, a `NULL` element becomes a `NULL` row,
and a `NULL` or empty array yields no rows). The produced relation has one column, named after the
function or its alias, and composes with `WHERE` / `ORDER BY` / `LIMIT` / joins like any other:

<LiveSql query={unnestExample} rows={6} />

## Array containment and overlap

The `@>` (contains), `<@` (contained by), and `&&` (overlaps) operators compare two arrays as sets:
`a @> b` is true when every element of `b` appears in `a`, `a && b` when they share at least one
element. Matching is strict — a `NULL` element matches nothing, including another `NULL` — and a
`NULL` whole array yields `NULL`:

<LiveSql query={containmentExample} rows={2} />

## Quantified comparisons (`ANY` / `ALL`)

A comparison operator followed by `ANY` (or its synonym `SOME`) or `ALL` over an array tests it
against every element. `x = ANY(arr)` is the array spelling of `IN` — true when `x` equals some
element; `x op ALL(arr)` is true when the comparison holds for every element. Both are three-valued,
exactly like `IN`: a `NULL` element (or a `NULL` `x`) makes the result `NULL` when no element settles
it, an empty array makes `ANY` false and `ALL` true, and a `NULL` whole array yields `NULL`:

<LiveSql query={quantifiedExample} rows={2} />

The operand may also be a **subquery** — `x op ANY/ALL(SELECT …)`, the subquery spelling of `IN`.
`x = ANY(SELECT …)` is exactly `x IN (SELECT …)`; the other operators generalize it over the
subquery's single column, with the same three-valued fold (and no row-count limit):

<LiveSql {seed} query={quantifiedSubquery} rows={4} />

## Variadic functions (`VARIADIC`)

A variadic function takes a variable number of trailing arguments. `num_nulls` and `num_nonnulls`
count the `NULL` / non-`NULL` arguments — either as a spread of values, or, with the `VARIADIC`
keyword, by passing one array whose elements are the arguments. The two forms agree, and the spread
form never returns `NULL` (it counts), while `VARIADIC` over a `NULL` array yields `NULL`:

<LiveSql query={variadicExample} rows={2} />

## Arrays of composite types

An array's element type can be a composite type, so a column holds a list of rows: `addr[]` is an
array of `addr`. Build one with the `ARRAY[ROW(…)]` constructor or the `'{…}'::addr[]` text literal,
subscript it to read an element (`places[1]`), and reach into a field with `(places[1]).street`.
Comparison, `ORDER BY`, `DISTINCT`, and `GROUP BY` all work — a `NULL` field inside a composite
element is comparable (so two arrays with matching `NULL` fields are equal and sort together), unlike
a bare row comparison:

<LiveSql seed={arrayCompositeSeed} query={arrayCompositeExample} rows={2} />

`unnest` expands a composite array into one **composite row** per element — read the whole row
(`u`) or reach into a field (`(u).zip`):

<LiveSql seed={arrayCompositeSeed} query={unnestCompositeExample} rows={2} />

The array functions and operators work over composite elements too — `array_append`, `||`,
`cardinality`, `@>`/`<@`/`&&`, `array_remove`, and `= ANY` / `= ALL` (which compares whole rows,
so a matching `NULL` field still counts as equal):

<LiveSql seed={arrayCompositeSeed} query={arrayCompositeFnExample} rows={2} />

## Composite types with array fields

The nesting works the other way too: a composite type can have an **array-typed field**, so one
row holds a list — `CREATE TYPE poly AS (name text, pts i32[])`. Build a value with `ROW(name,
ARRAY[…])` (or write the field as a text literal, `ROW(name, '{10,20,30}')`), read the whole array
with `(p).pts`, and subscript it with `(p).pts[1]`. Comparison and `ORDER BY` follow the row order
field-by-field, using the array's element-wise order for the array field:

<LiveSql seed={compositeArrayFieldSeed} query={compositeArrayFieldExample} rows={2} />

## Dates

A `date` is a calendar date — year/month/day, no time or zone. Write one as a quoted ISO string
(`'2024-01-15'`) that adapts to the column, or with the `DATE '…'` keyword literal. Dates compare
and sort chronologically, and `infinity` / `-infinity` are first-class values. (A literal carrying
a time keeps only the date part, so `'2024-01-15 09:30:00'` stores `2024-01-15`.)

<LiveSql seed={dateSeed} query={dateExample} rows={3} />

## Cost

Cost is shown with every result. Each query accrues a deterministic cost, and a caller can set a
ceiling so an expensive query aborts with `54P01` rather than running away — which is what makes it
safe to run untrusted SQL.

When an `ORDER BY` already matches the order the data is stored in — an ascending prefix of the
table's `PRIMARY KEY` — jed skips the sort and reads rows straight from the table in order. Paired
with a `LIMIT`, this becomes a **top-N**: the scan stops as soon as the window is full, so
`SELECT … ORDER BY <pk> LIMIT 100` over a million-row table reads ~100 rows, not a million. A
collated primary key is stored in its collation's order, so a collated `ORDER BY` is satisfied the
same way, with no in-memory re-sort. (Ordering by a non-key column, or `DESC`, still does a full
sort for now.)
