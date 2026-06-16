<script>
	import LiveSql from '$lib/components/LiveSql.svelte';

	const decimalDemo = `SELECT
  0.1 + 0.2     AS sum,
  (1.50 = 1.5)  AS equal_by_value,
  1.50          AS preserves_scale;`;

	const overflowDemo = `SELECT CAST(32767 AS int16) + CAST(1 AS int16) AS overflows;`;

	const nullDemo = `SELECT
  (NULL = NULL)   AS eq,
  (NULL IS NULL)  AS is_null,
  (1 = NULL)      AS one_eq_null;`;
</script>

<svelte:head>
	<title>Types — jed</title>
	<meta name="description" content="jed's strict, static type system: exact decimals, defined integer overflow, three-valued NULL logic — live in your browser." />
</svelte:head>

# Types

The type system is the product. Columns are strictly, statically typed — a value is never silently
reinterpreted at runtime. Where the SQL raises a question of semantics, jed follows PostgreSQL
closely.

Every example below is a **live database in your browser**. Edit the SQL and run it.

## Exact decimals

`numeric` / `decimal` is exact base-10, not binary floating point. So `0.1 + 0.2` is exactly `0.3`,
`1.50` equals `1.5` by value, and a value keeps its display scale.

<LiveSql query={decimalDemo} rows={5} />

## Integers with defined overflow

Integers are fixed-width (`int16`, `int32`, `int64`) and **trap on overflow** — there is no silent
wraparound. Adding `1` to the largest `int16` raises error `22003`:

<LiveSql query={overflowDemo} rows={3} />

## Three-valued NULL logic

Comparisons with `NULL` yield `NULL` (unknown), not `false` — three-valued logic, as in PostgreSQL.
Note that `NULL = NULL` is `NULL`, while `NULL IS NULL` is `true`:

<LiveSql query={nullDemo} rows={5} />

See the full [type reference](../../reference/types/) for every scalar type and its range.
