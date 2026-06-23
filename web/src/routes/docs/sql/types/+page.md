<script>
	import LiveSql from '$lib/components/LiveSql.svelte';

	const decimalDemo = `SELECT
  0.1 + 0.2     AS sum,
  (1.50 = 1.5)  AS equal_by_value,
  1.50          AS preserves_scale;`;

	const overflowDemo = `SELECT CAST(32767 AS i16) + CAST(1 AS i16) AS overflows;`;

	const boolCastDemo = `SELECT
  CAST(true AS int)    AS true_to_int,
  false::int           AS false_to_int,
  0::boolean           AS zero_to_bool,
  CAST(-5 AS boolean)  AS nonzero_to_bool;`;

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

	const tzDemo = `SELECT
  timestamptz '2024-01-15 12:00:00+00' AT TIME ZONE 'UTC'      AS at_utc,
  timestamptz '2024-01-15 12:00:00+00' AT TIME ZONE '+05:30'   AS at_plus_530,
  timestamptz '2024-01-15 12:00:00+00' AT TIME ZONE '-08:00'   AS at_minus_8,
  timestamp   '2024-01-15 06:30:00'    AT TIME ZONE '+05:30'   AS back_to_instant;`;
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

## Boolean ⇄ integer casts

Following PostgreSQL, `boolean` casts to and from `int` (`i32`) only, and always with an explicit
`CAST` / `::` — never implicitly. `true` becomes `1` and `false` becomes `0`; an integer becomes
`false` when it is `0` and `true` for any nonzero value (including negatives):

<LiveSql query={boolCastDemo} rows={3} />

Only `i32` is involved: a `boolean ⇄ i16` or `boolean ⇄ i64` cast is rejected (`42804`), matching
PostgreSQL's choice of `int4` as the sole boolean-integer cast.

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

## Time zones

`timestamptz` stores a UTC instant; a time zone is an I/O-time interpretation, never part of the
stored value or its order (PostgreSQL's model). The `AT TIME ZONE` operator converts both ways —
`timestamptz AT TIME ZONE zone` renders the instant as the local wall clock in `zone`, and
`timestamp AT TIME ZONE zone` interprets a wall clock as being in `zone` and gives back the UTC
instant. `UTC` and fixed offsets like `+05:30` are built in (note the **POSIX sign**: `'+05:30'`
means UTC−5:30, matching PostgreSQL):

<LiveSql query={tzDemo} rows={1} />

Named IANA zones (`America/New_York`, `Europe/Paris`, …) come from a time-zone database the host
**loads** as bytes — the engine ships none by itself, so a query can only ever *use* an
already-loaded zone (an unknown zone raises `22023`). This is the same host-loaded-data model jed
uses for Unicode collation; see the design notes for the `JTZ` bundle and the `db.loadTimeZoneData`
host call.

See the full [type reference](../../reference/types/) for every scalar type and its range.
