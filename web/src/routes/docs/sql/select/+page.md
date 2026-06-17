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

	const unnestExample = `SELECT u AS tag
FROM unnest(ARRAY['red', 'green', 'blue']) AS u
ORDER BY u;`;

	const containmentExample = `SELECT ARRAY[1, 2, 3] @> ARRAY[2]   AS contains,
       ARRAY[2]       <@ ARRAY[1, 2, 3] AS contained_by,
       ARRAY[1, 2]    && ARRAY[2, 3]    AS overlaps;`;
</script>

<svelte:head>
	<title>Querying — jed</title>
	<meta name="description" content="SELECT, WHERE, ORDER BY, GROUP BY and aggregates in jed — run against a live database." />
</svelte:head>

# Querying

`SELECT` supports the usual shape: `WHERE`, `ORDER BY`, `LIMIT` / `OFFSET`, `DISTINCT`, joins,
`GROUP BY` with `HAVING`, set operations, and subqueries. Aggregates use PostgreSQL-style widening
(for example, `sum` over `numeric` returns `numeric`, exact).

Grouping and aggregation:

<LiveSql {seed} query={grouped} rows={6} />

Filtering and ordering — edit the `WHERE` and `ORDER BY` and re-run:

<LiveSql {seed} query={filtered} rows={6} />

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

## Cost

Cost is shown with every result. Each query accrues a deterministic cost, and a caller can set a
ceiling so an expensive query aborts with `54P01` rather than running away — which is what makes it
safe to run untrusted SQL.
