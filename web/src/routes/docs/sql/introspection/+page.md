<script>
	import LiveSql from '$lib/components/LiveSql.svelte';

	const seed = `CREATE TYPE addr AS (street text, zip i32);
CREATE TABLE customer (
  id    i32 PRIMARY KEY,
  name  text NOT NULL,
  email varchar(80),
  home  addr
);
CREATE TABLE booking (
  room   i32,
  during i32range,
  price  decimal(8,2),
  PRIMARY KEY (room, during)
);`;

	const query = `SELECT table_name, name, ordinal, type, not_null, pk_ordinal
FROM jed_columns
ORDER BY table_name, ordinal;`;
</script>

<svelte:head>
	<title>Introspection — jed</title>
	<meta name="description" content="The jed_tables and jed_columns catalog relations: discover tables and columns from SQL — queryable, joinable, per attached database." />
</svelte:head>

# Introspection

The `jed_` catalog relations describe a database *from SQL*. They are ordinary read-only
relations — select from them, filter them, join them — whose rows are computed on the fly from
the database's catalog. Nothing is stored, so they are always current.

Two relations ship today:

- **`jed_tables`** — one row per user table: `name` (the table name as written in `CREATE TABLE`).
- **`jed_columns`** — one row per column of every user table: `table_name`, `name`, `ordinal`
  (1-based, declaration order), `type` (the canonical type text — `i32`, `varchar(80)`,
  `decimal(8,2)`, `i32[]`, `i32range`, a composite's name, …), `not_null` (declared `NOT NULL`
  or a `PRIMARY KEY` member), and `pk_ordinal` (the column's 1-based position in the primary
  key, in **key** order; `NULL` for a non-member).

<LiveSql {seed} {query} rows={10} />

Things to try in the panel above:

- List the tables — `SELECT name FROM jed_tables ORDER BY name;`
- Reconstruct a key — `SELECT name FROM jed_columns WHERE table_name = 'booking' AND pk_ordinal IS NOT NULL ORDER BY pk_ordinal;`
- Count columns per table — `SELECT table_name, count(*) FROM jed_columns GROUP BY table_name;`
- Create a table, then re-run the query — the new rows appear immediately.

## Scoping — one catalog per database

Every database carries its own catalog relations, reached with the same qualifier as its tables:
`jed_tables` (or `main.jed_tables`) reads the main database, `temp.jed_tables` lists the
session's temporary tables, and `reports.jed_tables` lists an [attached
database](../../api/opening-a-database/)'s tables. An unqualified name always means `main`.

## Read-only, and gated like any table

The catalog relations cannot be written or dropped — `INSERT`/`UPDATE`/`DELETE`, `CREATE INDEX
… ON`, and `DROP TABLE` against one raise error `42809`. Creating any relation whose name begins
with `jed_` is rejected (`42939`): the prefix is reserved for the engine.

Under a restricted [session](../../api/authorization/), a catalog relation is authorized exactly
like a user table: a session with explicit grants sees the schema only if the host granted
`SELECT` on `jed_tables` / `jed_columns`. Schema visibility is a policy decision, not a default.

Two practical notes:

- **Select columns by name.** The relations grow by *adding* columns over time, so
  `SELECT *` positionally is not a stable contract.
- The relations list **user objects only** — they do not list themselves.

Coming later: `jed_indexes`, `jed_constraints`, `jed_sequences`, and `jed_types`.
