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

	const grouped = `SELECT category, count(*) items, sum(price) AS total
FROM product
GROUP BY category
ORDER BY category;`;

	const aggOrder = `SELECT category, jsonb_agg(name ORDER BY price DESC) AS by_price
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

	const graphSeed = `CREATE TABLE edge (src i32, dst i32, PRIMARY KEY (src, dst));
INSERT INTO edge VALUES (1, 2), (2, 3), (3, 1), (3, 4);`;

	const searchCycle = `WITH RECURSIVE walk(node) AS (
  SELECT 1
  UNION ALL
  SELECT edge.dst FROM edge JOIN walk ON edge.src = walk.node
) SEARCH DEPTH FIRST BY node SET ord
  CYCLE node SET is_cycle USING path
SELECT node, is_cycle, path FROM walk ORDER BY ord;`;

	const nestedWith = `WITH prices AS (
  SELECT category, price FROM product
)
SELECT category, cheapest
FROM (
  WITH cheap AS (
    SELECT category, min(price) AS cheapest FROM prices GROUP BY category
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

	const valuesLimited = `SELECT label
FROM (VALUES ('a', 3), ('b', 1), ('c', 2) ORDER BY column2 LIMIT 2) AS v (label, qty);`;

	const valuesQuery = `SELECT name, price FROM product WHERE category = 'office'
UNION ALL
VALUES ('Stapler', 7.25), ('Tape', 2.00)
ORDER BY price;`;

	const setOps = `(SELECT name, price FROM product WHERE category = 'office' ORDER BY price DESC LIMIT 1)
UNION ALL
(SELECT name, price FROM product WHERE category = 'kitchen' ORDER BY price DESC LIMIT 1)
ORDER BY price;`;

	const parenSeed = `CREATE TABLE customer (id i32 PRIMARY KEY, name text NOT NULL);
CREATE TABLE product (id i32 PRIMARY KEY, name text NOT NULL);
CREATE TABLE orders (
  id          i32 PRIMARY KEY,
  customer_id i32 NOT NULL,
  product_id  i32 NOT NULL
);
INSERT INTO customer VALUES (1, 'Ada'), (2, 'Grace'), (3, 'Linus');
INSERT INTO product VALUES (1, 'Pen'), (2, 'Mug');
INSERT INTO orders VALUES (1, 1, 1), (2, 1, 2), (3, 2, 2);`;

	const parenthesized = `SELECT c.name, p.name AS product
FROM customer AS c
LEFT JOIN (orders AS o JOIN product AS p ON p.id = o.product_id)
  ON o.customer_id = c.id
ORDER BY c.name, product;`;

	const lateral = `SELECT c.category, top.name, top.price
FROM (SELECT DISTINCT category FROM product) AS c
CROSS JOIN LATERAL (
  SELECT name, price FROM product
  WHERE category = c.category
  ORDER BY price DESC LIMIT 1
) AS top
ORDER BY c.category;`;

	const conditional = `SELECT name,
       CASE WHEN price > 5 THEN 'premium' ELSE 'basic' END AS tier,
       coalesce(nickname, name) AS display,
       least(price, 10.00) AS capped
FROM (SELECT name, price, CASE WHEN id = 3 THEN 'Joe' END AS nickname FROM product) AS p
ORDER BY name;`;

	const unnestExample = `SELECT colors.tag
FROM unnest(ARRAY['red', 'green', 'blue']) AS colors(tag)
ORDER BY colors.tag;`;

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

	const rowSubquery = `SELECT name, category, price
FROM product
WHERE (category, price) IN (
  SELECT category, max(price) FROM product GROUP BY category
)
ORDER BY name;`;

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
An `ORDER BY` key is a **general expression** (`ORDER BY a + b`, `ORDER BY abs(x)`, `ORDER BY sum(x)`
in a grouped query — a column name is its simplest form), an **output-column alias / name**
(`SELECT a + b AS s … ORDER BY s` sorts by the aliased value — a bare name binds an output column
before an input column, matching PostgreSQL), or an **output-column position** (`ORDER BY 1` sorts by
the first select-list item), each with optional `ASC` / `DESC` and `NULLS FIRST` / `NULLS LAST`.
Aggregates use PostgreSQL-style widening
(for example, `sum` over `numeric` returns `numeric`, exact), and accept a leading `DISTINCT`
(`count(DISTINCT x)`) and a trailing `FILTER (WHERE …)` (`count(*) FILTER (WHERE x > 0)`).
Beyond `count`, `sum`, `avg`, `min`, and `max`, the aggregates include `bool_and`, `bool_or`,
`every`, `bit_and`, `bit_or`, `bit_xor`, `array_agg` (`array_agg(name ORDER BY price)`), `string_agg`
(`string_agg(name, ', ' ORDER BY name)`), and the statistical `var_pop`, `var_samp`, `variance`,
`stddev_pop`, `stddev_samp`, `stddev`, `covar_pop`, `covar_samp`, `corr`, and `regr_count`,
`regr_avgx`, `regr_avgy`, `regr_sxx`, `regr_syy`, `regr_sxy`, `regr_slope`, `regr_intercept`, and
`regr_r2`. As in PostgreSQL, the variance and standard-deviation aggregates are exact over integers
and `numeric` (a `numeric` result) and use `double precision` over floats, and the two-argument ones
return `double precision` for any numeric arguments.
An aggregate's argument list may end in its own `ORDER BY` — the order it folds its input in — which
is what fixes the element order of `jsonb_agg` / `json_agg` and the last-wins key of
`jsonb_object_agg` (`jsonb_agg(name ORDER BY price DESC)`, `jsonb_object_agg(k, v ORDER BY ts)`).
`GROUP BY` also takes `GROUPING SETS`, `ROLLUP`, and `CUBE` to compute several groupings at once,
with the `GROUPING(col)` function to tell subtotal rows apart, and groups by an output column,
a select-list position (`GROUP BY 1`), or any expression (`GROUP BY a + b`), not just a column.
The ordered-set aggregates `mode`, `percentile_cont`, and `percentile_disc` are written with a
`WITHIN GROUP (ORDER BY …)` clause (for example, `percentile_cont(0.5) WITHIN GROUP (ORDER BY x)`
for the median), and the hypothetical-set aggregates `rank`, `dense_rank`, `percent_rank`, and
`cume_dist` take a hypothetical row the same way (`rank(50) WITHIN GROUP (ORDER BY score)`).
`FILTER (WHERE …)` and these aggregate forms also compose with window functions (`OVER`).

A select-list (or `RETURNING`) item names its output column with `AS` (`count(*) AS items`) or, as
in PostgreSQL, without it (`count(*) items`, `id "Id"`). Without `AS`, the label may be any word
except the 39 that can continue a query or an expression in PostgreSQL — such as `from`, `where`,
`order`, `limit`, `union`, `over`, `filter`, `year`, `day`, and `with`. Those need `AS` or
quotes (`1 AS year`, `1 "year"`); the full list is in
[grammar.md §8](https://github.com/jackc/jed/blob/master/spec/design/grammar.md).

Grouping and aggregation:

<LiveSql demo="grouped" {seed} query={grouped} rows={6} />

An aggregate that collects its group in a chosen order — each category's products, priciest first:

<LiveSql demo="aggOrder" {seed} query={aggOrder} rows={3} />

`AND` and `OR` evaluate their operands **left to right and stop early**: a `FALSE` left operand
decides `AND` and a `TRUE` one decides `OR`, so the right operand is never evaluated. A guard
written first therefore protects what follows it — `WHERE qty <> 0 AND total / qty > 5` never
divides by zero. In a `WHERE`, `ON`, or `HAVING`, a `NULL` condition also stops the `AND` chain,
since the row can no longer qualify; elsewhere (the select list, `CASE`, `CHECK`) a `NULL` left
operand decides neither, so the right operand still runs. PostgreSQL leaves the order unspecified;
jed guarantees it.

Filtering and ordering — edit the `WHERE` and `ORDER BY` and re-run:

<LiveSql demo="filtered" {seed} query={filtered} rows={6} />

## Window functions and named windows

A window function (`row_number`, `rank`, `dense_rank`, `percent_rank`, `cume_dist`, `ntile`, `lag`,
`lead`, `first_value`, `last_value`, `nth_value`, or any aggregate) with an `OVER` clause computes a
value for each row from a set of related rows, without collapsing them. A `WINDOW` clause (after
`HAVING`, before `ORDER BY`) names window definitions so several functions can share one, and a
definition may extend an earlier one:

```sql
SELECT id, category,
       rank() OVER w                                   AS price_rank,
       sum(price) OVER (w ROWS UNBOUNDED PRECEDING)    AS running_total,
       lag(price) OVER w                               AS previous_price
FROM product
WINDOW w AS (PARTITION BY category ORDER BY price)
ORDER BY id;
```

The rules are PostgreSQL's. `OVER w` copies `w` whole, frame included. `OVER (w …)` and
`WINDOW w2 AS (w …)` extend `w`: they keep its `PARTITION BY`, keep its `ORDER BY` or add one if it
has none, and supply their own frame. Adding a `PARTITION BY`, replacing an existing `ORDER BY`, or
extending a window that has a frame is an error (`42P20`), as is defining a name twice. A name that
is not defined at the same query level is `42704`. Every `WINDOW` entry is checked even when no
`OVER` uses it. Functions whose windows have the same `PARTITION BY` and `ORDER BY` share a single
partition-and-sort pass.

## Set operations (`UNION` / `INTERSECT` / `EXCEPT`)

`UNION`, `INTERSECT`, and `EXCEPT` combine two queries' rows; the bare form removes duplicates and
`ALL` keeps them. `INTERSECT` binds tighter than `UNION` and `EXCEPT`. The output columns take the left
query's names, and each column's type unifies across both sides. A trailing `ORDER BY` / `LIMIT`
applies to the whole result.

Parenthesize an operand to give it its own `ORDER BY` / `LIMIT` / `OFFSET` (or its own `WITH`) or to
override precedence — here, the most expensive product of each category:

<LiveSql demo="setOps" {seed} query={setOps} rows={2} />

As in PostgreSQL, a parenthesized query is the same query as its contents: `(SELECT a FROM t) ORDER BY
b` sorts the inner query, and writing the same clause inside and after the parentheses
(`(SELECT … LIMIT 2) LIMIT 1`) is an error (`42601`).

A set operation can also be the source of an `INSERT`, parenthesized or `WITH`-prefixed forms
included. Each unified column type must be assignable to its target column, and `ON CONFLICT` and
`RETURNING` work as usual — try it in the panel above:

```sql
INSERT INTO product
  SELECT id + 10, name || ' refill', category, price FROM product WHERE category = 'office'
  UNION ALL
  SELECT 20, 'Kettle', 'kitchen', 30.00
RETURNING id, name;
```

## Common table expressions (`WITH`)

A `WITH` clause names a query and exposes it to the `FROM` clause like a table. Define one or more —
each is visible to later ones and to the main query:

<LiveSql demo="cte" {seed} query={cte} rows={4} />

CTEs follow PostgreSQL's evaluation rule: a CTE referenced once is **inlined**, one referenced
several times (or marked `MATERIALIZED`) runs once and its rows are **buffered** and reused. An
inlined CTE is planned where it is used, so the main query's `WHERE` conditions on it are pushed into
its body — `WITH c AS (SELECT * FROM t) SELECT * FROM c WHERE id = 5` looks up `id = 5` by key (see
[EXPLAIN](../explain/)). Add an optional column-rename list (`WITH t (a, b) AS (…)`).

### Data-modifying CTEs

A CTE's body — and the statement a `WITH` prefixes — may be an `INSERT`, `UPDATE`, or `DELETE`. Its
`RETURNING` rows flow forward like any CTE's, so one statement can write and read its own changes:

<LiveSql demo="dataModifying" {seed} query={dataModifying} rows={2} />

The canonical use is moving rows between tables atomically:

```sql
WITH moved AS (DELETE FROM inbox WHERE ready RETURNING *)
INSERT INTO archive SELECT * FROM moved;
```

Every part of the statement reads **one pre-statement snapshot** — of every table it touches,
temporary and attached-database tables included — so the parts cannot see each other's effects on
the target tables, and data crosses only through a CTE's `RETURNING` buffer (`SELECT count(*) FROM
inbox` next to the `DELETE` above still sees the rows). The data-modifying parts run once each, in
order, and the whole statement is one all-or-nothing transaction. Foreign keys are checked against
the earlier parts' writes: a parent row a CTE inserts satisfies a child row the statement inserts
after it.

### Recursive CTEs (`WITH RECURSIVE`)

`WITH RECURSIVE` lets a CTE reference **itself**, computing a result by iterating to a fixpoint —
hierarchy walks, graph reachability, generated series. The body is a `UNION`: the left side (the
**non-recursive term**) seeds the result, and the right side (the **recursive term**) is re-evaluated
against the rows the previous step produced until it yields none:

<LiveSql demo="recursive" {seed} query={recursive} rows={5} />

`UNION ALL` keeps every row; `UNION` drops rows that duplicate one already produced (which is what
makes a cyclic graph walk terminate). The column types are fixed by the non-recursive term. A
recursion with no stopping condition runs until it hits the statement's
[cost ceiling](../../api/resource-limits/) — set one when running untrusted queries. Mutual
recursion is not yet supported.

A recursive CTE may end with `SEARCH` and `CYCLE` clauses, as in PostgreSQL. `SEARCH DEPTH FIRST BY
cols SET ord` adds a column whose order is depth-first (`BREADTH FIRST` for breadth-first), and
`CYCLE cols SET is_cycle USING path` adds the path of rows visited and a flag that marks a row
repeating one on its path — a marked row is not expanded, so a walk over a cyclic graph ends. Use
`TO value DEFAULT value` to choose the flag's values instead of `true` / `false`:

<LiveSql demo="searchCycle" seed={graphSeed} query={searchCycle} rows={5} />

The added columns are anonymous `record` and `record[]` values. They live only in the query: a
`CREATE TABLE … AS` or view column of that type is an error (`42P16`), as in PostgreSQL.

### Nested `WITH`

A `WITH` clause may also prefix any **parenthesized** query — a subquery, a derived table, or another
CTE's body — so you can scope helper CTEs to just the part of the statement that needs them. The
nested CTEs (including `WITH RECURSIVE`) get the full machinery above:

<LiveSql demo="nestedWith" {seed} query={nestedWith} rows={2} />

A nested `WITH` establishes a **child** scope: its CTEs are visible only inside that parenthesized
query, and it inherits CTEs visible in the enclosing statement. An inner CTE with the same name
shadows the outer one after it is declared; its own non-recursive body still sees the outer binding
because a CTE cannot see itself. Data-modifying CTEs (`INSERT`/`UPDATE`/`DELETE`) stay top-level only,
as in PostgreSQL.

Inside a subquery, a nested CTE's body may reference columns of an *enclosing* query level, like any
correlated subquery: `SELECT o.id, (WITH c AS (SELECT o.x AS v) SELECT v FROM c) FROM o` returns each
row's `x`. It still cannot see the `FROM` of the query its `WITH` belongs to. The subquery then runs
once per outer row, and a materialized CTE is recomputed for each of those runs.

## Derived tables (`FROM (SELECT …) AS t`)

A `FROM` item can be a parenthesized subquery used as a relation — a **derived table**. It is an
anonymous, always-inlined single-reference CTE: the body runs in place, and you reference its output
columns through the alias. The alias is optional (matching PostgreSQL 18); when present it may carry a
column-rename list (`AS t (a, b)`):

<LiveSql demo="derived" {seed} query={derived} rows={4} />

A table or [view](../views/) in `FROM` takes the same column-rename list after its alias:
`FROM product AS p (pid, pname)` exposes `product`'s first two columns as `p.pid` and `p.pname`
(the rest keep their names). As in PostgreSQL, the old names are no longer visible through that
alias, `*` expands under the new names, and a list longer than the table's columns is `42P10`. The
list needs an alias: `FROM product (pid)` would be a call of a function named `product`.

By default the body cannot see the other `FROM` relations beside it — mark it `LATERAL` (below) to
correlate it with them. Inside a subquery, though, the body may reference columns of an *enclosing*
query level, like any correlated subquery: `SELECT (SELECT z FROM (SELECT a.x AS z) d) FROM a`.

Each output column keeps its body column's collation, as in PostgreSQL: a `text COLLATE "unicode"`
column selected through a derived table or a CTE still orders and compares by `unicode` outside it.

A collation also flows through text functions and set operations. `ORDER BY btrim(name)` sorts by
`name`'s collation, and a `UNION`, `INTERSECT`, or `EXCEPT` output column takes the collation its
inputs share. Combining two columns with different collations is an error (`42P21`), except under
`UNION ALL`: there the column has no collation, and sorting it needs an explicit
`ORDER BY x COLLATE "…"` (otherwise `42P22`).

Every other text-valued expression carries a collation the same way: a cast to `text`, `CASE`,
`COALESCE`, `GREATEST`/`LEAST`, `min`/`max`, `lag`/`lead`/`first_value`, `mode`, and a scalar
subquery all keep their inputs' collation. `min(name)` and `max(name)` also compare by it, and
`name < ANY (SELECT …)` compares by `name`'s collation. Sorting or grouping by an expression that
mixes two columns' different collations (`ORDER BY coalesce(a, b)`) is `42P21`; add a `COLLATE` to
pick one.

A `text[]` carries its elements' collation in the same way. A `text[]` column takes a `COLLATE`
clause (`tags text[] COLLATE "unicode"`) or the database default, and `ORDER BY tags`, `tags < …`,
`min(tags)`, and a primary key or index over it order the elements by that collation. `ARRAY[name]`,
`tags[1]`, `array_append(tags, …)`, and `unnest(tags)` keep it, and `name < ANY (tags)` combines
`name`'s collation with the array's.

## `VALUES` lists in `FROM` (`FROM (VALUES …) AS v(x)`)

A derived table's body may also be a `VALUES` list — a literal table of rows you write inline,
without a base table. The columns default to `column1`, `column2`, … (overridable by the alias's
column-rename list), and each column's type unifies across the rows the way `UNION` does. Each value
is a general constant expression (`qty * 2` below):

<LiveSql demo="valuesBody" {seed} query={valuesBody} rows={3} />

A trailing `ORDER BY` / `LIMIT` / `OFFSET` inside the parentheses sorts and windows the rows before
the outer query sees them. Its keys name the `VALUES` columns `column1`, `column2`, … (or a position),
not the alias's renamed columns:

<LiveSql demo="valuesLimited" {seed} query={valuesLimited} rows={2} />

A `VALUES` list is also a query on its own, wherever a `SELECT` can go: a whole statement (`VALUES
(1, 'one'), (2, 'two') ORDER BY column2`), an operand of `UNION` / `INTERSECT` / `EXCEPT`, a `WITH`
or view body, or a subquery (`x IN (VALUES (1), (3))`). Its columns are again `column1`, `column2`,
…, and a column of only strings or only `NULL`s is `text`, so `SELECT 1 UNION VALUES (NULL)` is a
type error (`42804`) where `SELECT 1 UNION SELECT NULL` is not — as in PostgreSQL:

<LiveSql demo="valuesQuery" {seed} query={valuesQuery} rows={4} />

As an `INSERT` source, a `VALUES` list on its own (even parenthesized) is the `INSERT`'s own row list,
whose slots may be `DEFAULT`. Followed by a set operator or a trailing `ORDER BY` / `LIMIT`, it becomes
a query instead — `INSERT INTO t VALUES (1, 'a') UNION ALL SELECT id, name FROM u` — where `DEFAULT`
is an error (`42601`).

## `LATERAL` joins

A `LATERAL` `FROM` item may reference columns of the `FROM` relations that appear **before** it — a
dependent join re-evaluated once per left-hand row. It is the idiomatic way to express
**top-N-per-group**: for each category, the single most expensive product.

<LiveSql demo="lateral" {seed} query={lateral} rows={2} />

Reach it through explicit join syntax — `CROSS JOIN LATERAL`, `JOIN LATERAL … ON …`, or `LEFT JOIN
LATERAL … ON …` (a `LEFT JOIN` keeps a left row even when the lateral side produces no rows) — or
through a **comma**: `FROM t, LATERAL (…)` is the same as `t CROSS JOIN LATERAL (…)`. A
sub-`SELECT` or `VALUES` body is lateral only with the keyword; a **table function** (`generate_series`,
`unnest`) is *implicitly* lateral, so `… CROSS JOIN unnest(t.tags)` correlates with or without it.

On the right side of a `RIGHT` or `FULL` join, a lateral item may not reference that join's left side
(`42P10`, as in PostgreSQL) — those rows must be kept whole, not re-evaluated per left row. It may
still reference tables **outside** the join: an earlier comma item (`FROM a, b RIGHT JOIN LATERAL
(SELECT a.x) s ON …`), or an outer query from inside a subquery.

A comma in `FROM` is an implicit `CROSS JOIN` generally — `FROM a, b` is `a CROSS JOIN b`. The comma
binds looser than `JOIN`, so each comma-separated item is its own join group: a join's `ON` may
reference only relations inside its own group (`FROM a, b JOIN c ON a.x = c.x` is an error), matching
PostgreSQL. A `RIGHT` or `FULL` join therefore applies within its group: in `FROM a, b RIGHT JOIN c ON
…` every unmatched `c` row is kept once per row of `a`.

To select all columns of **one** relation in a join, use a qualified star `t.*` — the per-table
counterpart of `*`. Unlike bare `*`, it can be mixed with other items: `SELECT a.*, b.label FROM a
JOIN b ON …` expands all of `a`'s columns followed by `b.label`.

`JOIN ... USING (col, …)` is an equi-join on the named columns that **merges** each into a single
output column — `a JOIN b USING (k)` joins on `a.k = b.k` and the result carries one `k` (listed
first, before the other columns; in a chain of joins each join's merged columns lead its own output,
so the last join's come first, as in PostgreSQL). A bare `k` then refers to the merged column; `a.k` and `b.k` still
reach each side. It works with `LEFT`/`RIGHT` joins and multi-column lists; `FULL JOIN ... USING` is
not yet supported.

`NATURAL JOIN` is the shorthand: `a NATURAL JOIN b` joins on **every column the two tables share by
name** (merging each, like `USING`). With no shared column it is a `CROSS JOIN`. It composes with
`LEFT`/`RIGHT` (`a NATURAL LEFT JOIN b`); `NATURAL FULL JOIN` is not yet supported.

## Parenthesized joins (`FROM (a JOIN b ON …)`)

Parentheses group joins into a single `FROM` item and so set the join tree. Below, every customer is
kept, and a customer's orders appear only where the inner join of `orders` and `product` matched —
Linus, with no orders, still gets a row:

<LiveSql demo="parenthesized" seed={parenSeed} query={parenthesized} rows={4} />

Without an alias the inner tables' names stay visible (`o.customer_id`, `p.name`, `p.*`) and must not
repeat another `FROM` name. With an alias, `(a JOIN b ON …) AS g` behaves like the derived table
`(SELECT * FROM a JOIN b ON …) AS g`: you reference its columns through `g` (an optional column list
`AS g (x, y)` renames them), and the inner names are hidden. The join's `ON` clauses see only its own
tables, and it must contain at least one join — `FROM (a)` is an error. A `LATERAL` item or table
function inside the parentheses also sees the tables to the group's **left**, aliased or not, as in
PostgreSQL: in `FROM a JOIN (b JOIN LATERAL (SELECT a.x + b.y AS v) s ON true) ON true` the group is
re-evaluated for each row of `a`. As with any `LATERAL` reference, the group may not be the right side
of a `RIGHT` or `FULL` join to the table it references (`42P10`); a `RIGHT JOIN LATERAL` *inside* the
group may reference a table to the group's left.

## Conditional expressions (`CASE`, `COALESCE`, `GREATEST`/`LEAST`, `NULLIF`)

`CASE` is the SQL conditional: the searched form `CASE WHEN cond THEN r … [ELSE e] END` returns
the first `WHEN` whose condition is true (an unmatched `CASE` with no `ELSE` is `NULL`), and the
simple form `CASE x WHEN v THEN r … END` compares an operand for equality per branch.
`COALESCE(a, b, …)` returns its first non-`NULL` argument, or `NULL` when all are — the usual
way to substitute a default (`coalesce(nickname, name)`) or make an empty aggregate count as zero
(`coalesce(sum(x), 0)`). `CASE` and `COALESCE` both evaluate lazily, left to right, exactly as far
as needed — a `1/0` in an unreached branch or argument never runs — and the branch/argument types
must unify to one common type (numerics promote, `f32` with `f64` is `f64`, and identical types of
any kind — `jsonb`, ranges, arrays, composites — always unify; mixing, say, an integer and a text
branch is an error). `UNION`/`INTERSECT`/`EXCEPT` and `VALUES` columns unify by the same rule; the
deduplicating set operations (all but `UNION ALL`) also need an equality operator, so they reject a
`json` or `jsonpath` column — use `jsonb`, or `UNION ALL`.

`GREATEST(a, b, …)` / `LEAST(a, b, …)` are the variadic maximum / minimum — handy to clamp a value
(`least(price, 10.00)`) or take a row-wise max across columns. `NULL` arguments are **ignored**
(the result is `NULL` only when every argument is `NULL`), and they share `COALESCE`'s type
unification. Unlike `CASE`/`COALESCE` they are **eager** — every argument is evaluated, so
`greatest(1, 1/0)` does divide by zero:

<LiveSql demo="conditional" {seed} query={conditional} rows={4} />

`NULLIF(a, b)` is the inverse of `COALESCE`: it returns `NULL` when `a = b`, and `a` otherwise — the
usual guard against dividing by zero (`total / nullif(n, 0)` is `NULL`, not an error, when `n` is
0) or for treating a placeholder as missing (`nullif(trim(name), '')`). A `NULL` on either side never
matches, so `nullif(a, NULL)` is `a`. The two operands are compared like `a = b`, so they must be
comparable, and the result has `a`'s type — except that an integer compared with a `decimal` gives a
`decimal`, as in PostgreSQL. Like `GREATEST`/`LEAST` it is eager: both operands are always
evaluated.

## Set-returning functions in `FROM`

A `FROM` item can be a set-returning function — a computed row source instead of a stored table.
`generate_series(start, stop[, step])` yields an integer series; `unnest(anyarray)` expands an array
into one row per element (a multidimensional array flattens, a `NULL` element becomes a `NULL` row,
and a `NULL` or empty array yields no rows). The produced relation has one column, named after the
function or its alias by default — except that a function with a named output parameter keeps that
name, so `jsonb_array_elements(doc) AS e` exposes `e.value`, as in PostgreSQL. The alias alone also
reads that one column, so `SELECT e FROM jsonb_array_elements(doc) AS e` works too. Add a
column-alias list when the output deserves a clearer name:
`unnest(tags) AS expanded(tag)` exposes `expanded.tag`. Multi-column table functions accept the same
left-to-right renaming, and a shorter list leaves the remaining output names unchanged. The relation
composes with `WHERE` / `ORDER BY` / `LIMIT` / joins like any other:

<LiveSql demo="unnestExample" query={unnestExample} rows={6} />

## Array containment and overlap

The `@>` (contains), `<@` (contained by), and `&&` (overlaps) operators compare two arrays as sets:
`a @> b` is true when every element of `b` appears in `a`, `a && b` when they share at least one
element. Matching is strict — a `NULL` element matches nothing, including another `NULL` — and a
`NULL` whole array yields `NULL`:

<LiveSql demo="containmentExample" query={containmentExample} rows={2} />

## Quantified comparisons (`ANY` / `ALL`)

A comparison operator followed by `ANY` (or its synonym `SOME`) or `ALL` over an array tests it
against every element. `x = ANY(arr)` is the array spelling of `IN` — true when `x` equals some
element; `x op ALL(arr)` is true when the comparison holds for every element. Both are three-valued,
exactly like `IN`: a `NULL` element (or a `NULL` `x`) makes the result `NULL` when no element settles
it, an empty array makes `ANY` false and `ALL` true, and a `NULL` whole array yields `NULL`:

<LiveSql demo="quantifiedExample" query={quantifiedExample} rows={2} />

The operand may also be a **subquery** — `x op ANY/ALL(SELECT …)`, the subquery spelling of `IN`.
`x = ANY(SELECT …)` is exactly `x IN (SELECT …)`; the other operators generalize it over the
subquery's single column, with the same three-valued fold (and no row-count limit):

<LiveSql demo="quantifiedSubquery" {seed} query={quantifiedSubquery} rows={4} />

## Row comparisons and row-valued subqueries

A parenthesized list of two or more expressions — `(a, b)`, the same value as `ROW(a, b)` — is a
**row**. Rows compare field by field: `=` needs every field equal, and `<` / `>` are decided by the
first field that differs, so `WHERE (category, id) > ('kitchen', 3)` is a keyset-pagination predicate.
A `NULL` field makes the comparison `NULL` when it is the field that decides.

A row on the left of a comparison, `IN`, or `ANY` / `ALL` makes the subquery on the right
**row-valued**: it must return exactly as many columns as the row has fields (otherwise `42601`), and
each of its rows is compared as a row. The single-row comparison `(a, b) = (SELECT x, y …)` still
requires at most one row (`21000`); `IN`, `ANY`, and `ALL` take any number:

<LiveSql demo="rowSubquery" {seed} query={rowSubquery} rows={4} />

## Variadic functions (`VARIADIC`)

A variadic function takes a variable number of trailing arguments. `num_nulls` and `num_nonnulls`
count the `NULL` / non-`NULL` arguments — either as a spread of values, or, with the `VARIADIC`
keyword, by passing one array whose elements are the arguments. The two forms agree, and the spread
form never returns `NULL` (it counts), while `VARIADIC` over a `NULL` array yields `NULL`:

<LiveSql demo="variadicExample" query={variadicExample} rows={2} />

## Arrays of composite types

An array's element type can be a composite type, so a column holds a list of rows: `addr[]` is an
array of `addr`. Build one with the `ARRAY[ROW(…)]` constructor or the `'{…}'::addr[]` text literal,
subscript it to read an element (`places[1]`), and reach into a field with `(places[1]).street`.
Comparison, `ORDER BY`, `DISTINCT`, and `GROUP BY` all work — a `NULL` field inside a composite
element is comparable (so two arrays with matching `NULL` fields are equal and sort together), unlike
a bare row comparison:

<LiveSql demo="arrayCompositeExample" seed={arrayCompositeSeed} query={arrayCompositeExample} rows={2} />

`unnest` expands a composite array into one **composite row** per element — read the whole row
(`u`) or reach into a field (`(u).zip`):

<LiveSql demo="unnestCompositeExample" seed={arrayCompositeSeed} query={unnestCompositeExample} rows={2} />

The array functions and operators work over composite elements too — `array_append`, `||`,
`cardinality`, `@>`/`<@`/`&&`, `array_remove`, and `= ANY` / `= ALL` (which compares whole rows,
so a matching `NULL` field still counts as equal):

<LiveSql demo="arrayCompositeFnExample" seed={arrayCompositeSeed} query={arrayCompositeFnExample} rows={2} />

## Composite types with array fields

The nesting works the other way too: a composite type can have an **array-typed field**, so one
row holds a list — `CREATE TYPE poly AS (name text, pts i32[])`. Build a value with `ROW(name,
ARRAY[…])` (or write the field as a text literal, `ROW(name, '{10,20,30}')`), read the whole array
with `(p).pts`, and subscript it with `(p).pts[1]`. Comparison and `ORDER BY` follow the row order
field-by-field, using the array's element-wise order for the array field:

<LiveSql demo="compositeArrayFieldExample" seed={compositeArrayFieldSeed} query={compositeArrayFieldExample} rows={2} />

## Dates

A `date` is a calendar date — year/month/day, no time or zone. Write one as a quoted ISO string
(`'2024-01-15'`) that adapts to the column, or with the `DATE '…'` keyword literal. Dates compare
and sort chronologically, and `infinity` / `-infinity` are first-class values. (A literal carrying
a time keeps only the date part, so `'2024-01-15 09:30:00'` stores `2024-01-15`.)

<LiveSql demo="dateExample" seed={dateSeed} query={dateExample} rows={3} />

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

Joins also reuse indexes without statistics: when the right side's primary key, B-tree key, GIN
array predicate, or GiST range/scalar predicate takes its query value from a bare column of an
earlier `FROM` item, jed opens that bound once per left row and residual-checks the complete join
predicate. NULL and empty values keep the same SQL semantics, including LEFT-join NULL extension.
With `ORDER BY` on the outer primary key plus `LIMIT`, these storage-key-ordered inner bounds compose
with the join top-N and later outer rows are never opened after the window fills.
