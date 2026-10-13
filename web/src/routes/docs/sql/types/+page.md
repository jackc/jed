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

	const textTemporalCastDemo = `SELECT
  ('2024-01-15 10:30'::text)::timestamp        AS text_to_timestamp,
  ('2024-01-15 10:30-05'::text)::timestamptz   AS with_offset,
  ('1 day 2 hours'::text)::interval            AS text_to_interval,
  ('\\x48656c6c6f'::text)::bytea               AS text_to_bytea;`;

	const intervalInputDemo = `SELECT
  interval '1-2'                    AS sql_year_month,
  interval '1 1:00'                 AS sql_day_time,
  interval 'P1Y2M3DT4H5M6S'         AS iso_designators,
  interval 'P0001-02-03T04:05:06'   AS iso_alternative;`;

	const toTextDemo = `SELECT
  42::text                                      AS int_to_text,
  CAST(1.50 AS text)                            AS keeps_scale,
  true::text                                    AS bool_to_text,
  '\\x00ff'::bytea::text                        AS bytea_to_text,
  (timestamptz '2024-03-15 02:30:00+00')::text  AS in_session_zone,
  (interval '90 minutes')::text                 AS interval_to_text,
  12345::varchar(3)                             AS truncated;`;

	const concatDemo = `SELECT
  'jed' || '-' || 'db'  AS joined,
  'row ' || 42          AS with_number,
  1.50 || ' USD'        AS keeps_scale,
  'x' || NULL           AS null_result;`;

	const varcharDemo = `SELECT
  'hello world'::varchar(5)     AS truncated,
  'café'::varchar(4)            AS fits_by_codepoint,
  'ok'::varchar(8)              AS within_limit,
  'jedi'::character varying(3)  AS two_word;`;

	const uuidCastDemo = `SELECT
  ('550E8400-E29B-41D4-A716-446655440000'::text)::uuid  AS text_to_uuid,
  '550e8400-e29b-41d4-a716-446655440000'::uuid::text    AS uuid_to_text,
  '550e8400-e29b-41d4-a716-446655440000'::uuid::bytea   AS uuid_to_bytea,
  '\\x550e8400e29b41d4a716446655440000'::bytea::uuid    AS bytea_to_uuid;`;

	const byteaInputDemo = `SELECT
  '\\xdead beef'::bytea  AS hex,
  'abc'::bytea          AS plain,
  'a\\\\b\\101'::bytea    AS escaped;`;

	const byteaDemo = `SELECT
  length('\\x0102ff'::bytea)                      AS len,
  '\\x0102'::bytea || '\\xff'                     AS concatenated,
  substring('\\x0102030405'::bytea FROM 2 FOR 3)  AS middle,
  get_byte('\\x0102ff'::bytea, 2)                 AS last_byte;`;

	const byteaEditDemo = `SELECT
  position('\\x03'::bytea IN '\\x010203'::bytea)                  AS pos,
  overlay('\\x0102030405'::bytea PLACING '\\xaabb'::bytea FROM 2)  AS spliced,
  set_byte('\\x010203'::bytea, 0, 255)                           AS patched,
  get_bit('\\x0102'::bytea, 9)                                    AS bit9,
  btrim('\\x0001020100'::bytea, '\\x0001'::bytea)                 AS trimmed;`;

	const arrayCastDemo = `SELECT
  (ARRAY[1, 2, 3])::text                     AS array_to_text,
  ('{10,20,30}'::text)::i32[]                AS text_to_array,
  (ARRAY[1, 2, 3]::i32[])::i64[]             AS widen_elements,
  (ARRAY[1.7, 2.2, -2.5]::numeric[])::i32[]  AS round_elements;`;

	const jsonUpdateDemo = `CREATE TABLE docs (id i32 PRIMARY KEY, body jsonb, raw json);
INSERT INTO docs VALUES (1, '{"a":1}', '{ "a" : 1 }');
UPDATE docs
SET body = body || '{"active":true}',
    raw = body
WHERE id = 1
RETURNING body, raw;`;

	const jsonSetLaxDemo = `SELECT
  jsonb_set('{"a":1,"b":2}', '{a}', '99')      AS set_a,
  jsonb_set_lax('{"a":1,"b":2}', '{a}', NULL)  AS json_null,
  jsonb_set_lax('{"a":1,"b":2}', '{a}', NULL,
                null_value_treatment => 'delete_key') AS deleted;`;

	const jsonReadDemo = `SELECT
  '{"a" : [1,  2], "a": {"b" :  1.50}}'::json  -> 'a'      AS json_member,
  '{"a" : [1,  2], "a": {"b" :  1.50}}'::jsonb -> 'a'      AS jsonb_member,
  '{"a" : [1,  2], "a": {"b" :  1.50}}'::json #>> '{a,b}'  AS b_text;`;

	const rowToJsonDemo = `CREATE TYPE addr AS (street text, zip i32);
SELECT
  row_to_json('(Main,90210)'::addr)                     AS named,
  row_to_json(ROW(1, NULL, ARRAY[1, 2], '{"b":2}'::jsonb)) AS anonymous,
  row_to_json(ROW(timestamp '2024-01-15 10:30:00'))     AS stamp;`;

	const toJsonDemo = `SELECT
  to_jsonb(ROW('b', 1))                                   AS jsonb_row,
  to_json('{{1,2},{3,4}}'::i32[])                         AS nested,
  json_build_object(date '2024-01-15', interval '1 day') AS date_key,
  jsonb_agg(d)                                            AS dates
FROM (VALUES (date '2024-01-15'), (date '2024-02-15')) AS v(d);`;

	const jsonpathMethodDemo = `SELECT
  jsonb_path_query_array('[-1.5, "2.5", 7]', '$[*].type()')          AS item_types,
  jsonb_path_query_array('["12", 3.7, "-4"]', '$.integer()')         AS integers,
  jsonb_path_query_array('{"x": 1, "y": [2, 3]}', '$.keyvalue().key') AS keys;`;

	const jsonpathArithDemo = `SELECT
  jsonb_path_query_first('{"price": 2.50, "qty": 3}', '$.price * $.qty')   AS total,
  jsonb_path_query_array('[1, 2, 3, 4]', '$[*] ? (@ % 2 == 0)')           AS evens,
  jsonb_path_query_first('[10, 20, 30]', '$[last - 1]')                    AS second_last,
  jsonb_path_query_first('{"a": 1, "b": 2}', '($.a + $.b).type()')         AS sum_type;`;

	const jsonpathVarsDemo = `SELECT
  jsonb_path_query_array('[3, 8, 12, 20]', '$[*] ? (@ >= $min && @ < $max)',
                         vars => '{"min": 5, "max": 15}')                     AS in_range,
  jsonb_path_query_array('[1, "x", 2]', '$[*].double()', silent => true)   AS before_error,
  JSON_VALUE('{"price": 2.50}', '$.price * $qty' PASSING 4 AS qty)         AS total;`;

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

	const justifyDemo = `SELECT
  justify_hours(interval '50 hours')            AS hours_to_days,
  justify_days(interval '45 days')              AS days_to_months,
  justify_interval(interval '1 mon -1 hour')    AS same_sign;`;

	const ageDemo = `SELECT
  age(timestamp '2001-04-10', timestamp '1957-06-13')  AS years_months_days,
  age(timestamp '2024-03-01', timestamp '2024-01-31')  AS month_end,
  timestamp '2024-03-01' - timestamp '2024-01-31'      AS plain_difference;`;

	const dateArithDemo = `SELECT
  date '2024-01-15' + 30                    AS in_30_days,
  date '2024-03-01' - date '2024-01-15'     AS days_between,
  date '2024-01-31' + interval '1 month'    AS month_clamped,
  date '2024-01-15' - interval '12 hours'   AS midnight_minus;`;

	const dateClockDemo = `SELECT
  current_date                  AS today,
  date 'tomorrow'               AS tomorrow,
  'yesterday'::date             AS yesterday,
  'epoch'::date                 AS epoch,
  make_date(2024, 2, 29)        AS built,
  'tomorrow'::date - 'today'    AS one_day;`;
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

<LiveSql demo="decimalDemo" query={decimalDemo} rows={5} />

## Integers with defined overflow

Integers are fixed-width (`i16`, `i32`, `i64`) and **trap on overflow** — there is no silent
wraparound. Adding `1` to the largest `i16` raises error `22003`:

<LiveSql demo="overflowDemo" query={overflowDemo} rows={3} />

## Boolean ⇄ integer casts

Following PostgreSQL, `boolean` casts to and from `int` (`i32`) only, and always with an explicit
`CAST` / `::` — never implicitly. `true` becomes `1` and `false` becomes `0`; an integer becomes
`false` when it is `0` and `true` for any nonzero value (including negatives):

<LiveSql demo="boolCastDemo" query={boolCastDemo} rows={3} />

Only `i32` is involved: a `boolean ⇄ i16` or `boolean ⇄ i64` cast is rejected (`42804`), matching
PostgreSQL's choice of `int4` as the sole boolean-integer cast.

## Parsing text into numbers and booleans

An explicit `CAST` / `::` parses a `text` value into a number or boolean **at runtime** — `i16` /
`i32` / `i64`, `decimal` (with a `numeric(p,s)` re-scale), `f32` / `f64`, and `boolean`. The string
is trimmed, a leading sign is accepted, and a `boolean` follows PostgreSQL's spellings
(`t`/`true`/`yes`/`on`/`1` …). A malformed string raises `22P02` and an out-of-range integer raises
`22003`:

<LiveSql demo="textNumCastDemo" query={textNumCastDemo} rows={1} />

The type must be **named** — a bare string never silently becomes a number, so `int_col = '42'` stays
a type error (`42804`). jed uses its own literal grammar, so hex, digit underscores, and `NaN` are
rejected (`22P02`) where PostgreSQL accepts them. 

## Parsing text into dates, times, intervals, and bytea

A runtime `text` value also casts to **`date`**, **`timestamp`**, **`timestamptz`**, **`interval`**,
and **`bytea`**, using the same input rules as that type's literal. A malformed date or time raises
`22007` and an out-of-range field `22008`; a malformed `bytea` (hex or escape format, see
[Binary strings](#binary-strings-bytea)) raises `22P02`:

<LiveSql demo="textTemporalCastDemo" query={textTemporalCastDemo} rows={1} />

A `timestamp` ignores a trailing UTC offset. A `timestamptz` with an offset is that instant; one
**without** an offset is read in the **session time zone**, as in PostgreSQL (the panel runs in
`UTC`). Because the result can depend on the session or the current day (`'today'::date`), an index
expression containing a text cast to `date`, `timestamp`, `timestamptz`, or `interval` is rejected
(`42P17`), exactly as in PostgreSQL; `text → bytea` is fine in an index.

A `timestamptz` *literal* such as `'2024-01-15 10:30'::timestamptz` reads an offset-less string the
same way, in the session time zone (see [Time zones](#time-zones)), so the literal and the `text`
column cast agree.

jed's timestamp input is stricter than PostgreSQL's, the same as for literals: the `'now'`,
`'today'`, and `'epoch'` words are rejected. Interval input reads everything PostgreSQL's does — units
(`'1 day 2 hours'`), the SQL-standard forms (`'1-2'` is `1 year 2 mons`, `'1 1:00'` is
`1 day 01:00:00`), and ISO 8601 (`'P1Y2M3DT4H5M6S'`, `'P2W'`, `'P0001-02-03T04:05:06'`). A
unit-less number takes its unit from its place: seconds at the end (`'1 day 2'`), days before an
hour or a time. As in PostgreSQL, an interval may name each field only once: `'1 day 1 day'` and
`'1 hour 1:00'` raise `22007`, while `'1 week 1 day'` (week and day are different fields) is
`8 days`. jed rejects the ambiguous unit `m` (spell out `min` or `mon`) and `'infinity'`, since it
has no infinite interval.

<LiveSql demo="intervalInputDemo" query={intervalInputDemo} rows={1} />

## Casting to text

Every built-in scalar casts **to** `text` with an explicit `CAST` / `::` (or `::varchar(n)`, which
truncates the result). The text is the type's own output form — exactly what a result cell of that
type shows: numbers in base 10 (a `numeric` keeps its scale), `true` / `false`, `bytea` as `\x` hex,
`uuid` in lowercase, dates, timestamps, and intervals in PostgreSQL's ISO / `postgres` styles, `json`
verbatim and `jsonb` canonical:

<LiveSql demo="toTextDemo" query={toTextDemo} rows={1} />

A `timestamptz` renders **in the session time zone**, with PostgreSQL's offset spelling (`-05`,
`+05:30`); the panel runs in the default `UTC` session. Because the result depends on the session,
an index expression containing `timestamptz::text` is rejected (`42P17`), as in PostgreSQL; every
other cast to text is fine in an index. An `array → text[]` cast renders each element the same way,
and so does `array → text` for a `timestamptz` anywhere inside the array — an element, a composite
field, or a range bound (see [Time zones](#time-zones)).
A `float` uses each engine's shortest round-trip form, so very large or small values may differ in
exponent spelling between the Rust, Go, and TypeScript engines.

Unlike PostgreSQL, the cast is **explicit only**: `INSERT INTO t (name) VALUES (42)` and
`name = 42` stay type errors (`42804`). A composite or range value does not cast to `text` yet.

## Concatenating text

`||` joins two strings. As in PostgreSQL, one side may be any other non-array type, which is first
converted with its text cast above — so `'row ' || 42` is `'row 42'` and a `numeric` keeps its scale.
A `NULL` on either side makes the result `NULL`:

<LiveSql demo="concatDemo" query={concatDemo} rows={1} />

At least one side must be text, a quoted literal, or `NULL`: `1 || 2` is an error (`42883`). When
one side is an array, `||` appends to the array instead; beside a `jsonb` or `bytea` value, a quoted
literal is read as that type (`jsonb` merge, `bytea` concatenation), while a `text`-typed value makes
it text concatenation. Like the cast, `|| timestamptz` renders in the session time zone, so it cannot
appear in an index expression.

## varchar(n) length limits

`varchar(n)` (also spelled `character varying(n)`, and the jed alias `string(n)`) is `text` with a
**maximum length**, counted in Unicode **code points** — so `'café'` (4 code points) fits `varchar(4)`. An explicit `CAST` / `::` silently
**truncates** to `n`, exactly like PostgreSQL, so `'hello world'::varchar(5)` is `'hello'`:

<LiveSql demo="varcharDemo" query={varcharDemo} rows={1} />

Storing into a `varchar(n)` **column** is stricter: an over-length value raises `22001`
(`string_data_right_truncation`), *unless* the excess characters are all spaces — then it is
truncated to `n` (the SQL-standard trailing-space rule). `UPDATE` re-checks the length the same way.
The limit lives on the value, not the type: a `varchar(4)` and an unbounded `text` are the same type
underneath, with the same collation, comparison, and key encoding.

## UUID ⇄ text and bytea casts

A `uuid` casts to and from `text` and `bytea`, always with an explicit `CAST` / `::`. `text → uuid`
parses PostgreSQL-flexibly (braces, hyphens after any byte pair, a bare 32-hex run, any case), and
`uuid → text` always renders the canonical lowercase `8-4-4-4-12` form. Because a UUID *is* exactly
16 bytes, it also casts to and from `bytea` (those 16 raw bytes); `bytea → uuid` requires exactly 16
bytes and raises `22P02` otherwise:

<LiveSql demo="uuidCastDemo" query={uuidCastDemo} rows={1} />

`text → uuid` matches PostgreSQL. The other three are deliberately stricter or additional: jed makes
`uuid → text` explicit-only (PostgreSQL assignment-casts any type to `text`), and the `bytea` ↔ `uuid`
casts are a jed convenience PostgreSQL does not offer at all.

<a id="binary-strings-bytea"></a>

## Binary strings (bytea)

A `bytea` value is written as a string, read the way PostgreSQL reads it. A string that starts with
`\x` is **hex**: two hex digits per byte, either case, with spaces, tabs, or newlines allowed
between (not inside) the pairs. Any other string is the traditional **escape format**: each
character is its own UTF-8 bytes, `\\` is one backslash, and `\nnn` (three octal digits, `\000` to
`\377`) is one byte. Any other backslash raises `22P02`. Output is always hex, as
in PostgreSQL by default:

<LiveSql demo="byteaInputDemo" query={byteaInputDemo} rows={1} />

`bytea` has PostgreSQL's binary-string functions, all counting and indexing **bytes** (the `text`
versions count characters). `length` and `octet_length` return the byte count and `bit_length` eight
times that; `||` concatenates two `bytea` values; `substr` / `substring` take a 1-based window, also
spelled `SUBSTRING(b FROM start FOR count)`; and `get_byte` reads one 0-based byte as an integer
0–255, raising `2202E` for an index outside the value:

<LiveSql demo="byteaDemo" query={byteaDemo} rows={1} />

`position(sub IN b)` finds a byte string (1-based, `0` when absent), `overlay(b PLACING new FROM
start [FOR count])` splices one in, `set_byte` / `set_bit` return a copy with one byte or bit
changed (bits count from the least-significant bit of each byte, so bit 9 is bit 1 of byte 1), and
`btrim` / `ltrim` / `rtrim(b, bytes)` strip any of a set of bytes from the ends. `position` and
`overlay` work the same way on `text`, counting characters:

<LiveSql demo="byteaEditDemo" query={byteaEditDemo} rows={1} />

## Array casts

An array casts three ways, all with an explicit `CAST` / `::`. `array → text` renders the `{…}` form
(`array_out`); `text → T[]` parses the `{…}` form per row (`array_in`); and an `array → array` of a
different element type casts **each element** through the scalar cast, preserving the array's shape
(its dimensions, lengths, and lower bounds):

<LiveSql demo="arrayCastDemo" query={arrayCastDemo} rows={1} />

The element cast follows the scalar matrix exactly — so widening (`i32[] → i64[]`), `numeric[] → i32[]`
(rounding half away from zero), and `text[] → i32[]` all work, while an element pair with no scalar
cast is a type error (`42804`). Like every cast to `text`, `array → text` is explicit-only: an array
never silently lands in a `text` column.

## JSON assignment

`json` preserves its input spelling; `jsonb` stores a canonical document. Both can be updated from
literals, columns, or expressions. Assigning `jsonb` to `json` stores the canonical `jsonb` text,
while the reverse direction needs an explicit `::jsonb` because it can discard whitespace, key
order, and duplicate keys. In a multi-column `UPDATE`, every right-hand side reads the old row, so
`raw = body` below captures the pre-update canonical document:

<LiveSql demo="jsonUpdateDemo" query={jsonUpdateDemo} rows={1} />

`jsonb_set(target, path, value)` replaces the value at a `text[]` path, creating a missing final key
by default. Like PostgreSQL's, it returns SQL `NULL` when `value` is `NULL`. `jsonb_set_lax` instead
applies its `null_value_treatment`: `'use_json_null'` (the default) stores a JSON `null`,
`'delete_key'` removes the path, `'return_target'` returns the document unchanged, and
`'raise_exception'` fails with `22004`. A `NULL` element inside the path is an error (`22004`, "path
element at position N is null") for `jsonb_set`, `jsonb_set_lax`, `jsonb_insert`, and `#-` when
the walk reaches it, while `#>` and `#>>` just return `NULL`. `jsonb_set`, `jsonb_set_lax`, and
`jsonb_insert` accept PostgreSQL's named arguments:

<LiveSql demo="jsonSetLaxDemo" query={jsonSetLaxDemo} rows={1} />

Reading a `json` value keeps its spelling too. `->` and `#>` on `json` return the matched value's
exact original text — whitespace, key order, duplicate keys, and number spelling intact — where
`jsonb` returns the canonical value. With duplicate keys, `json` takes the last match in document
order. `->>` and `#>>` de-escape a string and turn a JSON `null` into SQL `NULL`. The set-returning
functions `json_array_elements[_text]` and `json_each[_text]` work the same way, and `json_each`
lists every member in input order, duplicates included:

<LiveSql demo="jsonReadDemo" query={jsonReadDemo} rows={1} />

Text cannot hold the escape `\u0000` or a UTF-16 surrogate escape that is not half of a pair, so, as
in PostgreSQL, a function that decodes JSON strings rejects them: `jsonb` input, a cast to `jsonb`,
the `json` accessors and set-returning functions above, and the SQL/JSON query functions. A decoded
`\u0000` fails with `22P05`, an unpaired surrogate with `22P02`. These functions check the whole
document, not just the part they return. `json` input only checks syntax, so it stores either escape
unchanged, and `json_typeof`, `json_array_length`, `json_array_elements`, and `IS JSON` read such a
value without error. jsonpath string literals reject the same two escapes.

A `jsonpath` (`jsonb_path_query`, `@?`, `JSON_VALUE`, and the rest) can end a step in an item method:
`.type()`, `.size()`, `.abs()`, `.floor()`, `.ceiling()`, `.double()`, `.bigint()`, `.integer()`,
`.number()`, `.decimal(p, s)`, `.boolean()`, `.string()`, and `.keyvalue()`, as in PostgreSQL. In
the default `lax` mode a method other than `.type()` and `.size()` applies to each element of an
array. A value the method cannot convert fails with `22036` in either mode. `.keyvalue()` turns an
object into `{"key", "value", "id"}` objects. The datetime methods (`.datetime()`, `.date()`, …) are
not supported yet (`0A000`):

<LiveSql demo="jsonpathMethodDemo" query={jsonpathMethodDemo} rows={1} />

A `jsonpath` also does arithmetic: `+`, `-`, `*`, `/`, and `%`, and unary `+` and `-`, with the usual
precedence and parentheses, in a path, a filter, a comparison, or a subscript (`$[last - 1]`). The
math is exact `numeric` arithmetic, as in PostgreSQL. Each operand of a binary operator must be a
single number (in `lax` mode a one-element array counts), or the path fails with `22038`; unary `-`
negates every number of its operand and fails with `2203B` on anything else. Dividing by zero fails
with `22012`, but inside a filter it only makes that comparison unknown:

<LiveSql demo="jsonpathArithDemo" query={jsonpathArithDemo} rows={1} />

A `jsonpath` may name variables, `$name` or `$"any name"`. The `jsonb_path_*` functions bind them
from their third argument, `vars`, a `jsonb` object, and their fourth, `silent`, stops at the first
JSON error instead of raising it: the functions then return what they found before it, and
`jsonb_path_exists` returns `NULL`. Both may be passed by name (`vars => …`, `silent => …`).
`JSON_EXISTS`, `JSON_VALUE`, `JSON_QUERY`, and `JSON_TABLE` bind variables with
`PASSING value AS name`. A variable with no value fails with `42704`, and `vars` must be an object
(`22023`). Passing a date, timestamp, or float value is not supported yet (`0A000`). The `_tz`
forms of the functions (`jsonb_path_exists_tz`, …) behave the same; as in PostgreSQL they are
`STABLE`, so an index expression cannot use them:

<LiveSql demo="jsonpathVarsDemo" query={jsonpathVarsDemo} rows={1} />

An unquoted member name in a `jsonpath` follows PostgreSQL's lexing. It runs until whitespace, `"`,
or one of the path's operator and punctuation characters (`. [ ] ( ) $ @ ? , * + - / % ! = < > & |
: #` and braces), so it may hold any other character, non-ASCII included (`$.é`, `$.a~b`). It takes
the same backslash escapes as a string: `$.a\u0041` reads the key `aA`. A keyword is a member key
too: `$.type` reads the key `type`, and `$.type()` calls the method.

A JSON document may nest at most **256** arrays/objects deep. Parsing a deeper document, or
building one with `jsonb_build_array`, `jsonb_agg`, `||`, `jsonb_set`, and the like, fails with
`54001`. PostgreSQL's limit depends on its stack size and is much higher (about 10 000 levels by
default). jed's fixed limit is the same in every core.

## Values as JSON

`row_to_json(record [, pretty])` renders a composite value as a `json` object, as in PostgreSQL. A
named composite type keys the members by its field names, and an anonymous `ROW(…)` names them
`f1`, `f2`, …. Each field takes its JSON form: numbers stay exact, a `jsonb` field embeds its
canonical text, arrays and nested composites nest, and dates and timestamps become ISO 8601 strings
(a `timestamptz` in the session time zone). With `pretty` set to `true`, each top-level member goes
on its own line. jed has no implicit row type per table, so PostgreSQL's `row_to_json(t)` over a
table alias is not available: build the row with `ROW(t.a, t.b)` or use a composite column:

<LiveSql demo="rowToJsonDemo" query={rowToJsonDemo} rows={1} />

Every other conversion to JSON uses the same rule: `to_json`, `to_jsonb`, `array_to_json(array [,
pretty])`, `JSON_SCALAR(value)` (the same `json` result as `to_json`), `json_build_array`,
`json_build_object`, and the `json_agg` and `json_object_agg` aggregates, each with its `jsonb`
variant. A `jsonb` result keeps an object's keys in canonical order.
An object key is always a string: a date or timestamp key takes the same ISO 8601 form, and an array,
composite, or JSON key is an error (`22023`). Converting a float (`f32`/`f64`) value to JSON is not
supported yet (`0A000`):

<LiveSql demo="toJsonDemo" query={toJsonDemo} rows={1} />

## Three-valued NULL logic

Comparisons with `NULL` yield `NULL` (unknown), not `false` — three-valued logic, as in PostgreSQL.
Note that `NULL = NULL` is `NULL`, while `NULL IS NULL` is `true`:

<LiveSql demo="nullDemo" query={nullDemo} rows={5} />

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

<LiveSql demo="rangeDemo" query={rangeDemo} rows={3} />

<a id="time-zones"></a>

## Time zones

`timestamptz` stores a UTC instant; a time zone is an I/O-time interpretation, never part of the
stored value or its order (PostgreSQL's model). The `AT TIME ZONE` operator converts both ways —
`timestamptz AT TIME ZONE zone` renders the instant as the local wall clock in `zone`, and
`timestamp AT TIME ZONE zone` interprets a wall clock as being in `zone` and gives back the UTC
instant. `UTC` and fixed offsets like `+05:30` are built in (note the **POSIX sign**: `'+05:30'`
means UTC−5:30, matching PostgreSQL):

<LiveSql demo="tzDemo" query={tzDemo} rows={1} />

Named IANA zones (`America/New_York`, `Europe/Paris`, …) come from a time-zone database the host
**loads** as bytes — the engine ships none by itself, so a query can only ever *use* an
already-loaded zone (an unknown zone raises `22023`). This is the same host-loaded-data model jed
uses for Unicode collation; see the design notes for the `JTZ` bundle and the `db.loadTimeZoneData`
host call.

### Rendering in the session time zone

A `timestamptz` is shown in the **session time zone** wherever it becomes text, as in PostgreSQL:
a result column, a `timestamptz` nested inside an array, composite, or range result, and the
`::text` and `array → text` casts. In a session set to `America/New_York`,
`timestamptz '2024-01-15 12:00:00+00'` shows as `2024-01-15 07:00:00-05`; in `Asia/Kolkata` it shows
as `2024-01-15 17:30:00+05:30`. The offset is always written out, so the text names the same instant
whatever zone reads it back. The panels on this page run in the default `UTC` session.

The value itself carries no zone. The host chooses how to render a result: the session's `render`
method (Rust `session.render(&value)`, Go `session.Render(value)`, TypeScript `session.render(value)`)
renders it in the session's current zone, while a `Value`'s own canonical render is always UTC
(`+00`). `EXPLAIN (VERBOSE)` also prints a `timestamptz` constant in UTC, where PostgreSQL uses the
session zone.

### Strings without an offset

Any `timestamptz` input written **without** an offset — a literal (`'2024-01-15 10:30'::timestamptz`,
`timestamptz '…'`, or a string compared against a `timestamptz` column), an `INSERT`/`UPDATE` value, a
column `DEFAULT`, a text value bound to a `timestamptz` parameter, or a `timestamptz` inside array,
range, or composite text — is a wall-clock time **in the session time zone**, as in PostgreSQL. An
explicit offset (`'2024-01-15 10:30-05'`) or `infinity` names the instant directly. A constant
`DEFAULT` is fixed when the table is created, in the creating session's zone.

Two places differ from PostgreSQL. A prepared statement reads the session zone again on every
execution, where PostgreSQL keeps the zone in effect when it was prepared. And an index expression or
partial-index predicate cannot contain an offset-less `timestamptz` literal (`42P17`), because its
meaning would change with the session zone; write the offset, for example
`WHERE created_at > '2024-01-01 00:00+00'`.

### Truncating, extracting, and converting

`date_trunc(unit, value)` rounds a `timestamp` / `timestamptz` / `interval` **down** to a unit
(`hour`, `day`, `week`, `month`, `quarter`, `year`, …); `EXTRACT(field FROM value)` pulls a single
field out as exact `numeric` (`year`, `dow`, `epoch`, `doy`, ISO `week`, …); and the
`timestamp` / `timestamptz` / `date` types **cast across** each other. For a `timestamptz`, both
`date_trunc` and `EXTRACT` (and the casts) decompose the instant **in the session time zone** — the
panel below runs in the default `UTC` session, and `date_trunc(unit, timestamptz, zone)` takes an
explicit zone:

<LiveSql demo="convDemo" query={convDemo} rows={1} />

Over an `interval`, `EXTRACT` reads its separate fields, each keeping its sign: `year`/`month`/`quarter`
from the months, `day`/`week` from the days, `hour`/`minute`/`second` from the time, and `epoch` counts
a year as 365.25 days and a month as 30.

`EXTRACT(julian FROM value)` is the Julian Day: over a `date` the day number (`2000-01-01` is
`2451545`), and over a `timestamp` / `timestamptz` the day number plus the exact fraction of the
(session-local) day, at PostgreSQL's numeric scale — `EXTRACT(julian FROM timestamp '2000-01-01
12:00')` is `2451545.50000000000000000000`.

`date_part(field, source)` is the `double precision` twin of `EXTRACT` — same fields, but the field
name is a runtime string, and over a `date` the timestamp fields exist (`date_part('hour', d)` is
`0`), exactly as in PostgreSQL. (A runtime `text` value casts to every datetime type, and every
datetime casts to `text` — a `timestamptz` in the session time zone.)

Over `infinity` or `-infinity`, a field that only grows with time — `year`, `decade`, `century`,
`millennium`, `julian`, `isoyear`, `epoch` — is `Infinity` or `-Infinity`, and every other field
(`month`, `day`, `hour`, `dow`, …) is NULL: `date_part('year', date 'infinity')` is `Infinity` and
`date_part('month', date 'infinity')` is NULL, as in PostgreSQL. `EXTRACT` returns the same NULLs,
but jed's `numeric` has no infinity, so where PostgreSQL returns `Infinity` it raises `22003`.
EXTRACT's field is also checked against the type before the value is seen, so
`EXTRACT(timezone FROM timestamp 'infinity')` is `0A000`, as for any `timestamp`, where PostgreSQL
returns NULL.

Unit names are case-insensitive and accept PostgreSQL's aliases: `date_trunc('hrs', ts)`,
`EXTRACT(mons FROM ts)`, and `date_part('msec', ts)` work, as do `y`/`yr`/`years`, `d`/`days`,
`w`/`weeks`, `qtr`, `us`/`usec`, `c`/`cent`, and the rest. `EXTRACT` and `date_part` also take `jd`/`j`
for `julian` and `mm` for `minute`. `EXTRACT`'s field may be a bare word, a quoted identifier, or a
string: `EXTRACT(hours FROM ts)`, `EXTRACT("HOURS" FROM ts)`, and `EXTRACT('Hours' FROM ts)` are the
same.

An `interval` keeps its months, days, and time as separate fields, so `interval '50 hours'` stays
`50:00:00`. PostgreSQL's `justify_hours` moves whole 24-hour chunks into days, `justify_days` moves
whole 30-day chunks into months, and `justify_interval` does both and gives every field the same
sign:

<LiveSql demo="justifyDemo" query={justifyDemo} rows={1} />

`age(a, b)` subtracts two `timestamp`s (or two `timestamptz`s) **field by field**, so the answer
keeps calendar years and months where `a - b` counts days; a short day field borrows the length of
the earlier value's month. A `timestamptz` pair is compared in the session time zone, and `age(x)`
measures from midnight today. As in PostgreSQL; the one difference is an `infinity` argument, which
raises `22008` because jed has no infinite interval:

<LiveSql demo="ageDemo" query={ageDemo} rows={1} />

### Date arithmetic

A `date` does calendar arithmetic, matching PostgreSQL. Adding or subtracting an **integer** shifts
the day count and stays a `date`; subtracting one date from another gives the **number of days
between** as an `i32`; and adding or subtracting an **interval** widens the date to midnight and
returns a `timestamp` (month steps clamp the day-of-month, so Jan 31 + 1 month is Feb 29 in a leap
year). The `±infinity` dates absorb any shift, and an out-of-range result raises `22008`:

<LiveSql demo="dateArithDemo" query={dateArithDemo} rows={1} />

A date literal also accepts PostgreSQL's special words: `'epoch'` (1970-01-01) and the
**clock-relative** `'today'` / `'now'` / `'tomorrow'` / `'yesterday'` — the current day in the
session time zone, also reachable as the SQL-standard `current_date` keyword. Unlike PostgreSQL,
jed **never freezes** a clock-relative word into a constant:
`DEFAULT 'today'` re-evaluates on every INSERT (PostgreSQL locks in the table-creation day), a
prepared statement tracks the clock, and an index expression containing one is rejected rather
than silently frozen. `make_date(year, month, day)` builds a date from parts:

<LiveSql demo="dateClockDemo" query={dateClockDemo} rows={1} />


See the full [type reference](../../reference/types/) for every scalar type and its range.
