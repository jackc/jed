<script>
	import LiveSql from '$lib/components/LiveSql.svelte';

	const seed = `CREATE TABLE product (
  id       int32 PRIMARY KEY,
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

	const derived = `SELECT category, top
FROM (SELECT category, max(price) AS top FROM product GROUP BY category) AS d
WHERE top > 5
ORDER BY category;`;

	const unnestExample = `SELECT u AS tag
FROM unnest(ARRAY['red', 'green', 'blue']) AS u
ORDER BY u;`;

	const containmentExample = `SELECT ARRAY[1, 2, 3] @> ARRAY[2]   AS contains,
       ARRAY[2]       <@ ARRAY[1, 2, 3] AS contained_by,
       ARRAY[1, 2]    && ARRAY[2, 3]    AS overlaps;`;

	const quantifiedExample = `SELECT 2 = ANY(ARRAY[1, 2, 3])  AS any_match,
       5 > ALL(ARRAY[1, 2, 3])  AS all_greater,
       9 = ANY(ARRAY[1, 2, 3])  AS no_match;`;

	const variadicExample = `SELECT num_nulls(1, NULL, 3)                AS spread,
       num_nulls(VARIADIC ARRAY[1, NULL, 3]) AS variadic,
       num_nonnulls(1, NULL, 3)              AS non_nulls;`;

	const arrayCompositeSeed = `CREATE TYPE addr AS (street text, zip int32);
CREATE TABLE person (id int32 PRIMARY KEY, places addr[]);
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

	const compositeArrayFieldSeed = `CREATE TYPE poly AS (name text, pts int32[]);
CREATE TABLE shapes (id int32 PRIMARY KEY, p poly);
INSERT INTO shapes VALUES
  (1, ROW('a', ARRAY[10, 20, 30])),
  (2, ROW('b', ARRAY[5]));`;

	const compositeArrayFieldExample = `SELECT id,
       p,
       (p).pts      AS points,
       (p).pts[1]   AS first_point
FROM shapes
ORDER BY p;`;
</script>

<svelte:head>
	<title>Querying — jed</title>
	<meta name="description" content="SELECT, WHERE, ORDER BY, GROUP BY and aggregates in jed — run against a live database." />
</svelte:head>

# Querying

`SELECT` supports the usual shape: `WHERE`, `ORDER BY`, `LIMIT` / `OFFSET`, `DISTINCT`, joins,
`GROUP BY` with `HAVING`, set operations, subqueries, and `WITH` (common table expressions).
Aggregates use PostgreSQL-style widening
(for example, `sum` over `numeric` returns `numeric`, exact).

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
optional column-rename list (`WITH t (a, b) AS (…)`). `WITH RECURSIVE` and data-modifying CTEs are
not yet supported.

## Derived tables (`FROM (SELECT …) AS t`)

A `FROM` item can be a parenthesized subquery used as a relation — a **derived table**. It is an
anonymous, always-inlined single-reference CTE: the body runs in place, and you reference its output
columns through the alias. The alias is optional (matching PostgreSQL 18); when present it may carry a
column-rename list (`AS t (a, b)`):

<LiveSql {seed} query={derived} rows={4} />

The body is an independent query — it cannot see the enclosing query's other `FROM` relations
(`LATERAL` is not yet supported). A parenthesized join (`FROM (a JOIN b …)`) and a `WITH`/`VALUES`
body are likewise not yet supported.

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
row holds a list — `CREATE TYPE poly AS (name text, pts int32[])`. Build a value with `ROW(name,
ARRAY[…])` (or write the field as a text literal, `ROW(name, '{10,20,30}')`), read the whole array
with `(p).pts`, and subscript it with `(p).pts[1]`. Comparison and `ORDER BY` follow the row order
field-by-field, using the array's element-wise order for the array field:

<LiveSql seed={compositeArrayFieldSeed} query={compositeArrayFieldExample} rows={2} />

## Cost

Cost is shown with every result. Each query accrues a deterministic cost, and a caller can set a
ceiling so an expensive query aborts with `54P01` rather than running away — which is what makes it
safe to run untrusted SQL.
