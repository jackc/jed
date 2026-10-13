<script>
	import LiveSql from '$lib/components/LiveSql.svelte';

	const seed = `CREATE TEMP TABLE scratch (
  id  i32 PRIMARY KEY,
  tag text NOT NULL,
  n   i32 UNIQUE
);
INSERT INTO scratch VALUES (1, 'alpha', 10), (2, 'beta', 20), (3, 'gamma', 30);`;

	const query = `SELECT id, tag, n FROM scratch ORDER BY n DESC;`;
</script>

<svelte:head>
	<title>Temporary tables — jed</title>
	<meta name="description" content="CREATE TEMP TABLE — session-local scratch tables that make zero writes to the database file, support full CRUD with constraints, and are bounded by a deterministic storage budget." />
</svelte:head>

# Temporary tables

`CREATE TEMP TABLE` (or `CREATE TEMPORARY TABLE` — they are synonyms) declares a **session-local**
scratch table. It behaves like an ordinary table — full `INSERT` / `SELECT` / `UPDATE` / `DELETE`,
plus `PRIMARY KEY`, `NOT NULL`, `DEFAULT`, `CHECK`, and `UNIQUE` constraints — with three deliberate
differences:

- **It makes zero writes to the database file.** A temp table lives outside the durable on-disk
  image — in memory, or in a private scratch file once it grows large — so creating or filling one
  never touches the database file.
- **It is private to your session** and is dropped automatically when the session ends.
- **Its storage is bounded.** A per-session byte ceiling (`max_temp_storage_bytes`) caps how much a
  session's temp tables can hold, so a temp table is safe to expose even to untrusted queries.

<LiveSql {seed} {query} rows={6} />

Constraints work exactly as they do on a persistent table — try these in the panel above:

- **UNIQUE** — `INSERT INTO scratch VALUES (4, 'delta', 10);` &rarr; error `23505` (`n = 10` exists)
- **PRIMARY KEY** — `INSERT INTO scratch VALUES (1, 'dup', 99);` &rarr; error `23505`
- **NOT NULL** — `INSERT INTO scratch VALUES (5, NULL, 99);` &rarr; error `23502`
- **CRUD** — `UPDATE scratch SET tag = 'BETA' WHERE id = 2; DELETE FROM scratch WHERE id = 3;
  SELECT * FROM scratch ORDER BY id;`

## Names can't collide

Unlike PostgreSQL — which lets a temp table *shadow* a permanent one of the same name — jed
**precludes overlaps**: a temp-table name may not collide with any existing table, index, or
sequence (and vice-versa). The conflict is reported at `CREATE` time:

```sql
CREATE TABLE t (id i32 PRIMARY KEY);
CREATE TEMP TABLE t (id i32 PRIMARY KEY);   -- error 42P07: relation already exists
```

`CREATE TEMP TABLE IF NOT EXISTS` turns that conflict into a no-op, so with the persistent `t` above
it creates nothing (PostgreSQL would create a temp `t` hiding the persistent one):

```sql
CREATE TEMP TABLE IF NOT EXISTS t (id i32 PRIMARY KEY);   -- no-op: t already exists
```

`TEMP` / `TEMPORARY` are recognized only between `CREATE` and `TABLE`, so a table *named* `temp` is
still an ordinary persistent table:

```sql
CREATE TABLE temp (id i32 PRIMARY KEY);     -- a persistent table called "temp"
```

## End-of-transaction behavior (`ON COMMIT`)

An optional `ON COMMIT` clause after the column list chooses what happens to a temp table each time
a transaction commits, exactly as in PostgreSQL:

- `ON COMMIT PRESERVE ROWS` — the default: nothing happens.
- `ON COMMIT DELETE ROWS` — every commit empties the table (its rows and index entries). The table
  itself stays, and an auto-numbered column keeps counting from where it was.
- `ON COMMIT DROP` — the table, its indexes, and its auto-numbered sequences are dropped when the
  transaction that created it commits.

```sql
BEGIN;
CREATE TEMP TABLE batch (id i32 PRIMARY KEY, n i32) ON COMMIT DROP;
INSERT INTO batch VALUES (1, 10), (2, 20);
SELECT sum(n) FROM batch;   -- 30
COMMIT;                     -- batch is gone
```

Outside a `BEGIN … COMMIT` block every statement commits on its own, so an `ON COMMIT DELETE ROWS`
table is emptied right after each statement (only that statement's own `RETURNING` rows see what it
wrote), and an `ON COMMIT DROP` table disappears as soon as its `CREATE` finishes. `ROLLBACK`
discards a transaction's changes as usual, so no action runs. `ON COMMIT` is only for temporary
tables; on any other table it is error `42P16`.

## From a query (`CREATE TEMP TABLE … AS`)

A temp table can be created from a query's result, columns and rows, just like a
[persistent one](../tables/). `ON COMMIT` goes before `AS`, as in PostgreSQL:

```sql
CREATE TEMP TABLE top_tags AS SELECT tag, n FROM scratch WHERE n >= 20;
BEGIN;
CREATE TEMP TABLE batch ON COMMIT DROP AS SELECT id FROM scratch WITH NO DATA;
-- … fill and use batch …
COMMIT;                     -- batch is gone
```

## Bounded storage

Temp tables retain rows across statements, which the per-statement cost ceiling does not bound — so
a session carries a `max_temp_storage_bytes` ceiling (in bytes; the host sets it, default 1 GiB, `0`
means unlimited). A write that would push the session's total temp storage past the ceiling is
rejected with error `54P03` and rolled back, leaving the already-committed rows intact. This bounds
retained temp storage.

A second setting, `temp_buffers` (default 32 MiB, `0` means never spill), bounds how much of it stays
in memory. Once a session's temp tables outgrow it, a database opened from a file moves them into a
private scratch file in the operating system's temp directory (removed when the session ends) and
keeps only a `temp_buffers`-sized page cache in memory. Spilling never changes results, costs, or
where `54P03` fires. In-memory and browser (OPFS) databases have no scratch directory, so their temp
tables stay in memory, bounded by `max_temp_storage_bytes` alone. Configure the
[execution and scalar limits](../../api/resource-limits/) separately; transient query memory
and pending writes still need broader memory admission.

## Indexes

A standalone `CREATE INDEX` works on a temp table — the index lives in the same in-memory temp
snapshot, so it makes no writes to the file, is built and used to speed up queries, and is dropped
with its table. Its DDL is gated by the same `allow_temp_ddl` capability as the table.

## Auto-numbered columns

`serial` / `bigserial` / `smallserial` and `GENERATED { ALWAYS | BY DEFAULT } AS IDENTITY` columns
work on a temp table too, exactly as on a persistent table:

```sql
CREATE TEMP TABLE log (id bigserial PRIMARY KEY, msg text NOT NULL);
INSERT INTO log (msg) VALUES ('first'), ('second');   -- id auto-numbers 1, 2
```

The auto-created backing sequence lives in the **same in-memory temp snapshot**, so — like the temp
table's rows and indexes — it makes **zero writes to the file**, and it is dropped automatically when
the table is (`DROP TABLE` removes both). It is reachable by its derived name (`nextval('log_id_seq')`,
`currval`, `setval`), shares the relation namespace (a name collision is `42P07`). Its DDL is gated by
the same `allow_temp_ddl` capability.

## Composite-typed columns

A column of a user-defined composite (row) type works on a temp table too, exactly as on a persistent
table:

```sql
CREATE TYPE addr AS (street text, zip i32);
CREATE TEMP TABLE person (id i32 PRIMARY KEY, home addr);
INSERT INTO person VALUES (1, ROW('Main', 90210));
SELECT (home).zip FROM person WHERE id = 1;   -- 90210
```

The composite **type** itself is always permanent — `CREATE TYPE` is ordinary persistent DDL — and a
temp table only **references** it. The column's storage codec is resolved against the type catalog when
the table is created and is self-contained thereafter, so a temp table with a composite column still
makes **zero writes to the file**. `ROW(...)` construction, rendering, field access, and
comparison/ordering all behave exactly as on a persistent column. A composite column **can** be a
`PRIMARY KEY` (or a `UNIQUE` / indexed column) on a temp table too, keyed by the composite value.
Dropping a type that a temp table still references is blocked (`2BP01`), just as for a permanent table.

## Not yet supported on a temp table

This release keeps a few things off temp tables (each reported as `0A000`, *feature not supported*),
to be lifted in later releases: `FOREIGN KEY` constraints and `COLLATE` columns.
