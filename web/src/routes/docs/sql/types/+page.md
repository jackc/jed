<script>
	import LiveSql from '$lib/components/LiveSql.svelte';

	const decimalDemo = `SELECT
  0.1 + 0.2     AS sum,
  (1.50 = 1.5)  AS equal_by_value,
  1.50          AS preserves_scale;`;

	const overflowDemo = `SELECT CAST(32767 AS i16) + CAST(1 AS i16) AS overflows;`;

	const nullDemo = `SELECT
  (NULL = NULL)   AS eq,
  (NULL IS NULL)  AS is_null,
  (1 = NULL)      AS one_eq_null;`;

	const rangeDemo = `SELECT
  int4range(1, 10)                               AS r,
  int4range(1, 10) @> 5                           AS contains_5,
  int4range(1, 10) && int4range(8, 20)            AS overlaps,
  int4range(1, 10) + int4range(8, 20)             AS unioned,
  int4range(1, 10) * int4range(8, 20)             AS intersected,
  range_merge(int4range(1,5), int4range(20,30))   AS merged;`;
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

Integers are fixed-width (`i16`, `i32`, `i64`) and **trap on overflow** — there is no silent
wraparound. Adding `1` to the largest `i16` raises error `22003`:

<LiveSql query={overflowDemo} rows={3} />

## Three-valued NULL logic

Comparisons with `NULL` yield `NULL` (unknown), not `false` — three-valued logic, as in PostgreSQL.
Note that `NULL = NULL` is `NULL`, while `NULL IS NULL` is `true`:

<LiveSql query={nullDemo} rows={5} />

## Range types

A range is a structural type over a scalar element — PostgreSQL's six built-in ranges
(`int4range`/`int8range`/`numrange`/`tsrange`/`tstzrange`/`daterange`; jed also spells the integer
ones `i32range`/`i64range`). A range carries inclusive/exclusive (`[1,5)`) and unbounded (`(,5)`)
endpoints and a distinguished `empty`; discrete ranges are stored in the canonical `[)` form.

Construct one with a literal cast (`'[1,5)'::int4range`) or a constructor (`int4range(1, 10)`), test
it with the boolean operators (`@>` contains, `&&` overlaps, `<<`/`>>` strictly left/right, `-|-`
adjacent), and combine ranges with the set operators — `+` (union), `*` (intersection), `-`
(difference), and `range_merge`. A union of non-adjacent ranges raises `22000`; `range_merge` spans
the gap instead:

<LiveSql query={rangeDemo} rows={3} />

See the full [type reference](../../reference/types/) for every scalar type and its range.
