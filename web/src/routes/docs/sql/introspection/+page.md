<script>
	import LiveSql from '$lib/components/LiveSql.svelte';

	const seed = `CREATE TYPE addr AS (street text NOT NULL, zip i32);
CREATE SEQUENCE invoice_no START WITH 1000;
CREATE TABLE customer (
  id    serial PRIMARY KEY,
  name  text NOT NULL,
  email varchar(80) CONSTRAINT customer_email_uq UNIQUE,
  home  addr,
  age   i32 CHECK (age >= 0)
);
CREATE INDEX customer_name_idx ON customer (name);
CREATE TABLE booking (
  room     i32,
  during   i32range,
  customer i32 REFERENCES customer (id),
  price    decimal(8,2),
  PRIMARY KEY (room, during),
  EXCLUDE USING gist (during WITH &&)
);`;

	const query = `SELECT table_name, name, ordinal, type, not_null, pk_ordinal
FROM jed_columns
ORDER BY table_name, ordinal;`;
</script>

<svelte:head>
	<title>Introspection — jed</title>
	<meta name="description" content="The jed_ catalog relations — discover tables, columns, indexes, constraints, sequences, user-defined types, and planner statistics from SQL, queryable and joinable per attached database." />
</svelte:head>

# Introspection

The `jed_` catalog relations describe a database *from SQL*. They are ordinary read-only
relations — select from them, filter them, join them — whose rows are computed on the fly from
the database's catalog. Nothing is stored, so they are always current.

Eight relations ship today:

- **`jed_tables`** — one row per user table: `name` (the table name as written in `CREATE TABLE`).
- **`jed_columns`** — one row per column of every user table: `table_name`, `name`, `ordinal`
  (1-based, declaration order), `type` (the canonical type text — `i32`, `varchar(80)`,
  `decimal(8,2)`, `i32[]`, `i32range`, a composite's name, …), `not_null` (declared `NOT NULL`
  or a `PRIMARY KEY` member), and `pk_ordinal` (the column's 1-based position in the primary
  key, in **key** order; `NULL` for a non-member).
- **`jed_indexes`** — one row per secondary index: `name`, `table_name`, `columns` (a `text[]` of
  the indexed column names in key order), `is_unique`, and `method` (`btree` / `gin` / `gist`).
  The primary key owns no index object, so it is not listed here — see `jed_columns.pk_ordinal`.
- **`jed_constraints`** — one row per `CHECK` / `UNIQUE` / `FOREIGN KEY` / `EXCLUDE` constraint:
  `name`, `table_name`, `type`, `columns` (a `text[]` of the member/local columns; `NULL` for a
  `CHECK`), `expression` (the `CHECK` text; `NULL` otherwise), and `ref_table` / `ref_columns`
  (the foreign-key parent; `NULL` otherwise). A `UNIQUE` constraint *is* its backing unique index,
  so it appears in both `jed_indexes` and `jed_constraints` under the same name.
- **`jed_statistics`** — one summary per analyzed column: table/column name, analyzed row and NULL
  counts, stale flag, optional distinct count, sample rows, average width, and MCV/histogram counts.
  Typed distribution values stay internal; run [`ANALYZE`](../statistics/) to create or refresh a
  fact. A successful table write retains its facts and marks them stale.
- **`jed_sequences`** — one row per sequence: `name`, `start_value`, `min_value`, `max_value`,
  `increment_by`, `cycle`, `cache_size` (PostgreSQL's `pg_sequences` spelling), `last_value` (the
  last value handed out; `NULL` until the sequence is first read), and `owned_by_table` /
  `owned_by_column` (the `serial` or `IDENTITY` column that owns it; `NULL` for a standalone
  sequence).
- **`jed_types`** — one row per user-defined type: `name` and `kind` (`composite`, the only kind
  you can create today). Built-in types are not listed.
- **`jed_type_fields`** — one row per field of every composite type, shaped like `jed_columns`:
  `type_name`, `name`, `ordinal`, `type` (the same canonical type text), and `not_null`.

<LiveSql {seed} {query} rows={10} />

Things to try in the panel above:

- List the tables — `SELECT name FROM jed_tables ORDER BY name;`
- Reconstruct a key — `SELECT name FROM jed_columns WHERE table_name = 'booking' AND pk_ordinal IS NOT NULL ORDER BY pk_ordinal;`
- List the indexes — `SELECT name, table_name, columns, is_unique, method FROM jed_indexes ORDER BY table_name, name;`
- List the constraints — `SELECT name, table_name, type, columns, expression, ref_table, ref_columns FROM jed_constraints ORDER BY table_name, type, name;`
- Inspect planner facts — `SELECT table_name, column_name, analyzed_rows, is_stale, distinct_count FROM jed_statistics ORDER BY table_name, column_name;`
- List the sequences — `SELECT name, start_value, last_value, owned_by_table, owned_by_column FROM jed_sequences ORDER BY name;`
- Advance one, then look again — `SELECT nextval('invoice_no');`
- Describe a composite type — `SELECT name, ordinal, type, not_null FROM jed_type_fields WHERE type_name = 'addr' ORDER BY ordinal;`
- Find the columns typed at a user-defined type — `SELECT c.table_name, c.name, t.name FROM jed_types t JOIN jed_columns c ON c.type = t.name;`
- Find every foreign key and its parent — `SELECT table_name, columns, ref_table, ref_columns FROM jed_constraints WHERE type = 'foreign_key';`
- Count columns per table — `SELECT table_name, count(*) FROM jed_columns GROUP BY table_name;`
- Create a table or index, then re-run the query — the new rows appear immediately.

## Scoping — one catalog per database

Every database carries its own catalog relations, reached with the same qualifier as its tables:
`jed_tables` (or `main.jed_tables`) reads the main database, `temp.jed_tables` lists the
session's temporary tables (and `temp.jed_sequences` their owned sequences), and
`reports.jed_tables` lists an [attached
database](../../api/opening-a-database/)'s tables. An unqualified name always means `main`.

## Read-only, and gated like any table

The catalog relations cannot be written or dropped — `INSERT`/`UPDATE`/`DELETE`, `CREATE INDEX
… ON`, and `DROP TABLE` against one raise error `42809`. Creating any relation whose name begins
with `jed_` is rejected (`42939`): the prefix is reserved for the engine.

Under a restricted [session](../../api/authorization/), a catalog relation is authorized exactly
like a user table: a session with explicit grants sees the schema only if the host granted
`SELECT` on that relation. Schema visibility is a policy decision, not a default.

Two practical notes:

- **Select columns by name.** The relations grow by *adding* columns over time, so
  `SELECT *` positionally is not a stable contract.
- The relations list **user objects only** — they do not list themselves.
