<script>
	import LiveSql from '$lib/components/LiveSql.svelte';

	const seed = `CREATE TEMP TABLE scratch (
  id  i32 PRIMARY KEY,
  tag text NOT NULL,
  n   i32 UNIQUE
);
INSERT INTO scratch VALUES (1, 'alpha', 10), (2, 'beta', 20), (3, 'gamma', 30);`;

	const query = `SELECT id, tag, n FROM scratch ORDER BY n DESC;`;

	const sharedSeed = `CREATE SHARED TEMP TABLE cache (
  key text PRIMARY KEY,
  hits i32 NOT NULL
);
INSERT INTO cache VALUES ('a', 1), ('b', 2), ('c', 3);`;

	const sharedQuery = `SELECT key, hits FROM cache ORDER BY hits DESC;`;
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

- **It makes zero writes to the database file.** A temp table lives entirely in memory, outside the
  durable on-disk image, so creating or filling one never touches the file.
- **It is private to your session** and is dropped automatically when the session ends.
- **Its storage is bounded.** A per-session byte budget (`temp_buffers`) caps how much a session's
  temp tables can hold, so a temp table is safe to expose even to untrusted queries.

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

`TEMP` / `TEMPORARY` are recognized only between `CREATE` and `TABLE`, so a table *named* `temp` is
still an ordinary persistent table:

```sql
CREATE TABLE temp (id i32 PRIMARY KEY);     -- a persistent table called "temp"
```

## Bounded storage

Temp tables retain rows across statements, which the per-statement cost ceiling does not bound — so
a session carries a `temp_buffers` budget (in bytes; the host sets it, default 32 MiB, `0` means
unlimited). A write that would push the session's total temp storage past the budget is rejected
with error `54P03` and rolled back, leaving the already-committed rows intact. This makes a temp
table a safe, bounded scratch space for untrusted SQL — paired with the per-statement
[cost limit](../../api/resource-limits/), a query can be given scratch space without risking
unbounded memory.

## Shared temporary tables

A plain temp table is **private** to the session that created it. A `CREATE SHARED TEMP TABLE` (or
`CREATE SHARED TEMPORARY TABLE`) instead declares a **database-wide** temp table: one set of rows
**visible to and writable by every session** of the open database — yet, like a session-local temp
table, it still makes **zero writes to the file**.

<LiveSql seed={sharedSeed} query={sharedQuery} rows={6} />

It behaves like an ordinary table for the session using it — full CRUD, the same constraints, the
same deferred features — and a write becomes visible to other sessions only when its transaction
commits (writes ride the single-writer gate; a reader sees a consistent snapshot of both the
persistent and shared-temp tables). It is dropped when the **database** is closed, and is never
recovered on reopen.

This is jed-specific: PostgreSQL's `GLOBAL TEMPORARY` shares only a table *definition* and gives
each session its own *data*, so jed coins the new `SHARED` keyword to mean it shares the **data**
too. `SHARED` must be immediately followed by `TEMP`/`TEMPORARY` (a `SHARED` table is always
temporary; a bare `CREATE SHARED TABLE …` is a syntax error `42601`). Its DDL is gated by a separate
`allow_shared_temp_ddl` capability, and its global storage by a `shared_temp_mem` budget — the same
`54P03` on overflow.

`TEMP` / `TEMPORARY` / `SHARED` are recognized only between `CREATE` and `TABLE`, so a table *named*
`shared` is still an ordinary persistent table:

```sql
CREATE TABLE shared (id i32 PRIMARY KEY);    -- a persistent table called "shared"
```

A standalone `CREATE INDEX` works on a temp table too (both session-local and shared) — the index
lives in the same in-memory temp snapshot, so it makes no writes to the file, is built and used to
speed up queries, and is dropped with its table. Its DDL is gated by the same `allow_temp_ddl` /
`allow_shared_temp_ddl` capability as the table.

## Not yet supported on a temp table

This first release keeps a few things off temp tables (both session-local and shared — each reported
as `0A000`, *feature not supported*), to be lifted in later releases: `FOREIGN KEY` constraints,
`serial` / `GENERATED AS IDENTITY` columns, and composite-typed and `COLLATE` columns.
