<script>
	import LiveSql from '$lib/components/LiveSql.svelte';

	const seed = `CREATE TABLE event (
  id       i32 PRIMARY KEY,
  category i32,
  label    text
);
CREATE INDEX event_category_idx ON event (category);
INSERT INTO event VALUES
  (1, 0, 'one'),   (2, 0, 'two'),   (3, 0, 'three'),
  (4, 0, 'four'),  (5, 0, 'five'),  (6, 0, 'six'),
  (7, 0, 'seven'), (8, 0, 'eight'), (9, 1, 'rare'),
  (10, NULL, NULL);`;

	const query = `ANALYZE event (category, label);
SELECT table_name, column_name, analyzed_rows, is_stale,
       null_count, distinct_count, average_width, mcv_count, histogram_count
FROM jed_statistics
ORDER BY column_name;`;
</script>

<svelte:head>
	<title>Statistics & ANALYZE — jed</title>
	<meta name="description" content="Collect deterministic transactional column statistics with ANALYZE, inspect them through jed_statistics, and improve jed's cost-based plans." />
</svelte:head>

# Statistics and `ANALYZE`

jed plans from exact table row counts even before statistics exist. `ANALYZE` adds value-distribution
facts so the planner can distinguish a rare indexed value from one that matches most of a table:

```sql
ANALYZE table_name;
ANALYZE table_name (column_name, ...);
```

The command accepts one ordinary table in `main`, `temp`, or an attached database. The first form
collects every column; the second replaces only the listed column facts. Database-wide ANALYZE,
per-column targets, options such as `VERBOSE`, and automatic/background collection are not supported.

Run the live example, then change the final query to `EXPLAIN SELECT id FROM event WHERE category =
0`. The common value is estimated from its most-common-value bucket, so a full scan can beat an index
path that would fetch nearly every row. Try `category = 1` to see the rare-value path.

<LiveSql {seed} {query} rows={8} />

## Deterministic by construction

Each requested column is scanned independently in ascending storage-key order. jed records exact
NULL and average-width facts, a bounded 30,000-row deterministic sample, a 4,096-hash distinct-value
estimate, at most 100 most-common values, and at most 101 equi-depth histogram bounds. There is no
random seed, clock age, host iteration order, or planning-time table read, so the same database bytes
produce the same facts, estimates, plan, and execution cost in Rust, Go, and TypeScript.

Distribution statistics cover canonically ordered scalar and range columns. Composite, array,
`json`, and `jsonb` columns still receive NULL and width facts, but no distinct/MCV/histogram facts.

## GIN term statistics

An array column whose elements a GIN index accepts, and a `jsonb` column, get a different kind of
fact: the 100 most common GIN terms (array elements, or `jsonb` keys and values) with the number of
rows holding each, counted over the whole table. `jed_statistics.term_count` shows the list length.
With them the planner prices `@>`, `&&`, `= ANY`, `?`, `?|`, and `?&` against a literal: a rare tag
picks the GIN index, a tag in nearly every row picks a full scan, and an `@>` of several tags combines
their frequencies rarest first.

```sql
CREATE TABLE post (id i32 PRIMARY KEY, tags text[]);
CREATE INDEX post_tags ON post USING gin (tags);
-- ... load rows ...
ANALYZE post;
EXPLAIN SELECT id FROM post WHERE tags @> '{rare-tag}'::text[];
```

A term missing from a fresh list that holds every term in the table is estimated at zero rows; after
later writes, or when the list was truncated, a missing term gets a small positive estimate.

## Transactional and deliberately stale

`ANALYZE` is a write: it commits atomically in autocommit mode, joins an explicit transaction, rolls
back with that transaction, and is rejected in a read-only transaction. It requires `SELECT` on the
target plus the host's `allow_ddl` capability.

`INSERT`, `UPDATE`, and `DELETE` do not discard useful facts. They retain them and set `is_stale` in
`jed_statistics`; frequencies scale from the analyzed population to the current exact row count.
Low-cardinality distinct counts stay fixed while key-like counts scale proportionally. Refresh is
always explicit:

```sql
ANALYZE event (category); -- label's existing fact is untouched
```

Statistics persist in the single database file and participate in prepared-plan cache validity.
Detailed typed MCV/histogram values remain internal; their effects are visible through
[`EXPLAIN`](../explain/).
