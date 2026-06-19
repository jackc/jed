<script>
	import LiveSql from '$lib/components/LiveSql.svelte';

	const orderedSeed = `CREATE TABLE city (
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
  (5, 'Kyoto',  1);`;

	const orderedQuery = `SELECT name FROM city WHERE region = 1 ORDER BY name;`;

	const ginSeed = `CREATE TABLE post (
  id    i32 PRIMARY KEY,
  title text NOT NULL,
  tags  i32[]
);
CREATE INDEX post_tags_gin ON post USING gin (tags);
INSERT INTO post VALUES
  (1, 'intro',   ARRAY[10, 20, 30]),
  (2, 'arrays',  ARRAY[20, 40]),
  (3, 'storage', ARRAY[40, 50]),
  (4, 'gin',     ARRAY[10, 20]),
  (5, 'empty',   '{}');`;

	const containsQuery = `SELECT title FROM post
WHERE tags @> ARRAY[10, 20]
ORDER BY id;`;

	const overlapsQuery = `SELECT title FROM post
WHERE tags && ARRAY[30, 40]
ORDER BY id;`;
</script>

<svelte:head>
	<title>Indexes — jed</title>
	<meta name="description" content="CREATE INDEX in jed — ordered B-tree indexes, and GIN inverted indexes that accelerate array containment and overlap, run live." />
</svelte:head>

# Indexes

An index speeds up a lookup **without changing the answer**. A query returns the same rows whether
or not an index exists — the index only changes *which rows are scanned* (and the deterministic
[cost](../select/) shown with each result). jed uses an applicable index automatically, and
keeps every index up to date on each `INSERT`, `UPDATE`, and `DELETE`.

## Ordered indexes (the default)

`CREATE INDEX [name] ON table (column)` builds an ordered B-tree over the column. It accelerates
equality lookups — `WHERE column = …` — by seeking instead of scanning the whole table. The
`PRIMARY KEY` is itself an index, and a `UNIQUE` constraint is backed by a unique index. The
indexed column must be a key-encodable type (the integer widths, `boolean`, `uuid`, `timestamp`);
indexing a `text` or `numeric` column is `0A000` until its key encoding lands.

The `city` table below indexes its `region` code (`1` = Asia, `2` = Europe). Run the lookup, then
edit the `WHERE` to `region = 2` — the index narrows the scan to the matching rows, and the result
is the same set you'd get without it:

<LiveSql seed={orderedSeed} query={orderedQuery} rows={6} />

## GIN indexes for arrays (`USING gin`)

A **GIN** (generalized inverted) index maps the **elements** of an array column to the rows that
contain them, so a query over a multi-valued column narrows to candidate rows instead of reading
the whole table. Add one with `USING gin`:

```sql
CREATE INDEX post_tags_gin ON post USING gin (tags)
```

It accelerates the two array set operators:

- **`tags @> ARRAY[10, 20]`** (contains) — rows whose `tags` contain **all** the query terms. jed
  gathers the rows for each term and **intersects** their lists.
- **`tags && ARRAY[30, 40]`** (overlaps) — rows whose `tags` share **any** query term. jed gathers
  the lists and takes their **union**.

The original `WHERE` stays as the residual filter, so the answer is identical to the full-scan
answer — the index is transparent. Containment (`intro` and `gin` both hold `{10, 20}`):

<LiveSql seed={ginSeed} query={containsQuery} rows={6} />

Overlap (`intro` holds `30`; `arrays` and `storage` hold `40`):

<LiveSql seed={ginSeed} query={overlapsQuery} rows={6} />

### Current scope

GIN this release covers a focused surface (it grows from here):

- **One column, integer-element arrays** — `i16[]`, `i32[]`, or `i64[]`. A multi-column GIN, or an
  array of another element type (`text[]`, `numeric[]`, …), is rejected with `0A000` until its key
  encoding lands.
- **`@>` and `&&` only** — `<@` (contained-by), `= ANY` / `IN` membership, and array `=` still run,
  by full scan; they are not GIN-accelerated yet.
- **No `UNIQUE`** — an inverted index has many entries per row, so `CREATE UNIQUE INDEX … USING gin`
  is rejected (`0A000`), matching PostgreSQL.

`DROP INDEX`, auto-naming, and the `DROP TABLE` cascade work the same as for an ordered index.
