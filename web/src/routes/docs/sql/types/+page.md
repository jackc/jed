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

	const textNumCastDemo = `SELECT
  ('42'::text)::int                AS text_to_int,
  ('  -7 '::text)::bigint          AS trims_and_signs,
  ('3.14159'::text)::numeric(4,2)  AS text_to_numeric,
  ('yes'::text)::boolean           AS text_to_bool;`;

	const uuidCastDemo = `SELECT
  ('550E8400-E29B-41D4-A716-446655440000'::text)::uuid  AS text_to_uuid,
  '550e8400-e29b-41d4-a716-446655440000'::uuid::text    AS uuid_to_text,
  '550e8400-e29b-41d4-a716-446655440000'::uuid::bytea   AS uuid_to_bytea,
  '\\x550e8400e29b41d4a716446655440000'::bytea::uuid    AS bytea_to_uuid;`;

	const arrayCastDemo = `SELECT
  (ARRAY[1, 2, 3])::text                     AS array_to_text,
  ('{10,20,30}'::text)::i32[]                AS text_to_array,
  (ARRAY[1, 2, 3]::i32[])::i64[]             AS widen_elements,
  (ARRAY[1.7, 2.2, -2.5]::numeric[])::i32[]  AS round_elements;`;

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

	const convDemo = `SELECT
  date_trunc('hour', timestamp '2024-03-15 13:47:23.5')   AS truncated,
  EXTRACT(dow FROM date '2024-03-15')                     AS weekday,
  EXTRACT(epoch FROM timestamptz '2024-03-15 12:00:00+00') AS epoch,
  EXTRACT(day FROM interval '40 days 5 hours')            AS interval_days,
  (timestamp '2024-03-15 13:47:23')::date                 AS as_date;`;

	const dateArithDemo = `SELECT
  date '2024-01-15' + 30                    AS in_30_days,
  date '2024-03-01' - date '2024-01-15'     AS days_between,
  date '2024-01-31' + interval '1 month'    AS month_clamped,
  date '2024-01-15' - interval '12 hours'   AS midnight_minus;`;
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

## Parsing text into numbers and booleans

An explicit `CAST` / `::` parses a `text` value into a number or boolean **at runtime** — `i16` /
`i32` / `i64`, `decimal` (with a `numeric(p,s)` re-scale), `f32` / `f64`, and `boolean`. The string
is trimmed, a leading sign is accepted, and a `boolean` follows PostgreSQL's spellings
(`t`/`true`/`yes`/`on`/`1` …). A malformed string raises `22P02` and an out-of-range integer raises
`22003`:

<LiveSql query={textNumCastDemo} rows={1} />

The type must be **named** — a bare string never silently becomes a number, so `int_col = '42'` stays
a type error (`42804`). jed uses its own literal grammar, so hex, digit underscores, and `NaN` are
rejected (`22P02`) where PostgreSQL accepts them. (Parsing a string into a `date` / `timestamp` /
`interval` / `bytea` is a separate feature — use that type's literal form, e.g. `date '2024-01-15'`.)

## UUID ⇄ text and bytea casts

A `uuid` casts to and from `text` and `bytea`, always with an explicit `CAST` / `::`. `text → uuid`
parses PostgreSQL-flexibly (braces, hyphens after any byte pair, a bare 32-hex run, any case), and
`uuid → text` always renders the canonical lowercase `8-4-4-4-12` form. Because a UUID *is* exactly
16 bytes, it also casts to and from `bytea` (those 16 raw bytes); `bytea → uuid` requires exactly 16
bytes and raises `22P02` otherwise:

<LiveSql query={uuidCastDemo} rows={1} />

`text → uuid` matches PostgreSQL. The other three are deliberately stricter or additional: jed makes
`uuid → text` explicit-only (PostgreSQL assignment-casts any type to `text`), and the `bytea` ↔ `uuid`
casts are a jed convenience PostgreSQL does not offer at all.

## Array casts

An array casts three ways, all with an explicit `CAST` / `::`. `array → text` renders the `{…}` form
(`array_out`); `text → T[]` parses the `{…}` form per row (`array_in`); and an `array → array` of a
different element type casts **each element** through the scalar cast, preserving the array's shape
(its dimensions, lengths, and lower bounds):

<LiveSql query={arrayCastDemo} rows={1} />

The element cast follows the scalar matrix exactly — so widening (`i32[] → i64[]`), `numeric[] → i32[]`
(rounding half away from zero), and `text[] → i32[]` all work, while an element pair with no scalar
cast is a type error (`42804`). Like `uuid`/`json → text`, `array → text` is explicit-only: an array
never silently lands in a `text` column.

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

### Truncating, extracting, and converting

`date_trunc(unit, value)` rounds a `timestamp` / `timestamptz` / `interval` **down** to a unit
(`hour`, `day`, `week`, `month`, `quarter`, `year`, …); `EXTRACT(field FROM value)` pulls a single
field out as exact `numeric` (`year`, `dow`, `epoch`, `doy`, ISO `week`, …); and the
`timestamp` / `timestamptz` / `date` types **cast across** each other. For a `timestamptz`, both
`date_trunc` and `EXTRACT` (and the casts) decompose the instant **in the session time zone** — the
panel below runs in the default `UTC` session, and `date_trunc(unit, timestamptz, zone)` takes an
explicit zone:

<LiveSql query={convDemo} rows={1} />

(`date_part` is deferred — it returns `double precision`, and jed has no binary float type — as are
the `text`↔datetime casts; cast a string with the `timestamp '…'` / `date '…'` literal form instead.)

### Date arithmetic

A `date` does calendar arithmetic, matching PostgreSQL. Adding or subtracting an **integer** shifts
the day count and stays a `date`; subtracting one date from another gives the **number of days
between** as an `i32`; and adding or subtracting an **interval** widens the date to midnight and
returns a `timestamp` (month steps clamp the day-of-month, so Jan 31 + 1 month is Feb 29 in a leap
year). The `±infinity` dates absorb any shift, and an out-of-range result raises `22008`:

<LiveSql query={dateArithDemo} rows={1} />


See the full [type reference](../../reference/types/) for every scalar type and its range.
