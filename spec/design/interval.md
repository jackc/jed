# The `interval` type — design

> The reasoning behind the interval type. The **data is authoritative**
> ([../types/scalars.toml](../types/scalars.toml) — the type, family, encoding method;
> [../types/compare.toml](../types/compare.toml) — comparability/ordering;
> [../types/casts.toml](../types/casts.toml) — casts (only `interval → text` so far);
> [../functions/catalog.toml](../functions/catalog.toml) — comparison + arithmetic operators;
> [../fileformat/format.md](../fileformat/format.md) — the on-disk value codec + type code;
> [../encoding/intervals.toml](../encoding/intervals.toml) — the parse/render vectors).
> This doc is the *why* and the precise field/parse/render/arithmetic math the three cores must
> reproduce **byte-identically** (CLAUDE.md §2, §8). When a decision here changes, change the data
> and here in the same edit.

`interval` is a **span of time**. It is the engine's first type whose comparison key differs from
its stored representation, and the slice that lights up the engine's **first timestamp arithmetic**.

The type accepts PostgreSQL's full interval input — its unit syntax, the SQL-standard forms, and
ISO 8601 (§3) — and **full arithmetic**; field qualifiers (`YEAR TO MONTH`) and the `interval(p)`
precision typmod (§6.1).

## 1. Representation — three independent fields

A value is PostgreSQL's `struct Interval`: **`months` (i32)**, **`days` (i32)**, **`micros`
(i64)** — three independent fields, not one scalar.

They are kept **separate on purpose**, because months and days are not fixed-length:

- A month is 28–31 days, so `timestamp + interval '1 month'` is a **calendar** add (Jan 31 + 1
  month → Feb 28/29, day clamped to the target month) — genuinely different from `+ '30 days'`.
- A day is a **calendar day for a `timestamptz`** — `timestamptz + interval '1 day'` keeps the
  wall-clock time in the session zone, so across a DST change the instant moves by 23 or 25 h,
  exactly as PostgreSQL's `timestamptz_pl_interval` (§5; [timezones.md §6](timezones.md)) — and a
  fixed 24 h for a zoneless `timestamp` or `date`. `interval '24 hours'` is always 24 h.

`micros` is microsecond resolution like the rest of the engine; the time-of-day part of an
interval is **not** justified into days (`INTERVAL '100 hours'` keeps `micros = 100 h`, `days = 0`,
and renders `100:00:00`; `justify_hours` moves it, §5.1).

## 2. The canonical span — comparison, ordering, dedup

Comparison, ordering (`ORDER BY`), and dedup (`DISTINCT` / `GROUP BY` / set-op identity) collapse
the three fields into a single **128-bit microsecond span** (PG `interval_cmp_value`):

```
span(iv) = (iv.months * 30 + iv.days) * 86_400_000_000 + iv.micros          # signed 128-bit
```

i.e. **1 month = 30 days** and **1 day = 24 h = 86 400 s**. Two intervals are equal iff their spans
are equal, so

```
INTERVAL '1 mon' = INTERVAL '30 days' = INTERVAL '720:00:00'                 # all TRUE
```

This single span governs `=`, `<`, `>`, `<=`, `>=`, `IS [NOT] DISTINCT FROM`, `ORDER BY`,
`DISTINCT`, `GROUP BY`, and set-operation identity. The order is **total** (no NaN). 128 bits is
required: `(i32*30 + i32) * 86.4e9 + i64` overflows i64 but fits i128 with vast headroom (each core
uses i128 / `BigInt` / a hand-rolled 128-bit compare).

**Equal but not identical.** Two span-equal intervals can carry different fields (`'1 mon'` vs
`'30 days'`). The **stored / rendered representation is preserved** — each renders its own fields —
but they share one DISTINCT/GROUP BY bucket and one set-op identity. The surviving representative of
a dedup is the **first-seen** one in the deterministic scan order, exactly as for `decimal` (`1.5`
vs `1.50`): the bucket key is the canonical span (decimal's canonical value), the surviving `Value`
keeps its own fields. Reusing that machinery is what keeps the multiset byte-identical cross-core.

NULL is the largest value (sorts last ascending), three-valued logic throughout — the existing
machinery, unchanged. interval × any other family is a `42804` type error (no cross-family rule).

## 3. Parsing — PostgreSQL's `interval_in`

A value is written either as a single-quoted string adapting in an interval context (literal
adaptation, like timestamp/bytea — the executor's resolve-time coercion) or with the
**`INTERVAL '…'` keyword literal** (which names the type, so it works context-free, e.g.
`SELECT INTERVAL '1 day'` and inside arithmetic `ts + INTERVAL '1 day'`). Both parse the same
text. Parsing happens at **resolve time**, before any scan, so a bad literal traps
deterministically (like bytea/timestamp). The one parser binds every text→interval path: the
literal, an adapting string, `::interval`/CAST, the runtime text cast, a bound parameter, and
container element text.

The parser is a **port of PostgreSQL 18's `interval_in`** (`datetime.c`), step for step, under
`IntervalStyle = postgres` (jed has no `IntervalStyle` setting; the oracle profile pins `postgres`)
and with no field qualifier (a qualifier changes two rules — §6.1). It accepts every form PostgreSQL does — the PostgreSQL unit
syntax, the SQL-standard forms, and ISO 8601 — with the few differences listed at the end.

```
INTERVAL '1 year 2 mons 3 days 04:05:06'   -- PostgreSQL units and a time
INTERVAL '1-2'                             -- SQL standard: 1 year 2 mons
INTERVAL '1 1:00'                          -- SQL standard: 1 day 01:00:00
INTERVAL '1-2 3 4:05:06'                   -- 1 year 2 mons 3 days 04:05:06
INTERVAL 'P1Y2M3DT4H5M6S'                  -- ISO 8601, format with designators
INTERVAL 'P0001-02-03T04:05:06'            -- ISO 8601, alternative format
```

### 3.1 Fields (`ParseDateTime`)

The text is first split into **fields**. Whitespace separates fields (C-locale `isspace`); any other
ASCII punctuation that *starts* a field is only a separator (so `@`, `,` and `;` are ignored —
`'@ 1 day'`, `'1,day'`); a non-ASCII or control character is malformed. Letters are lowercased. A
field is one of:

| kind | shape | examples |
|---|---|---|
| number | digits, with at most one `.` fraction | `5`, `1.5`, `.5` |
| date | digits with a `-`, `/` or `.` separator; or a word run into digits or punctuation | `1-2`, `5.`, `1/2`, `p1d` |
| time | digits then `:` (then digits, `:`, `.`) | `1:00`, `100:00:00.5` |
| signed | `+`/`-` (whitespace may follow it) then digits, `:`, `.`, `-` | `-1`, `+02:03`, `- 5`, `-1-2` |
| word | letters | `day`, `ago` |
| signed word | `+`/`-` then letters | `-infinity` |

A number abuts its unit (`'1day'`, `'1hour 2min'`). A word followed by `-`, `/`, `.`, or by `+` or a
digit when it is not a date keyword, runs on into one malformed field (`'1day-2'`, `'1hour2min'`).
More than 25 fields, or more than PostgreSQL's 256-byte field buffer of field text, is malformed.

### 3.2 Reading the fields (`DecodeInterval`)

The fields are read **right to left**, so a unit word is met before the number it applies to:

- **A unit word** (case-insensitive) sets the unit of the number to its left: `millennium/
  millennia/mil/mils`, `century/centuries/cent/c`, `decade(s)/dec/decs`, `year(s)/yr/yrs/y`,
  `month(s)/mon/mons`, `week(s)/w`, `day(s)/d`, `hour(s)/hr/hrs/h`, `minute(s)/min/mins/mm`,
  `second(s)/sec/secs/s`, `millisecond(s)/msec/msecs/msecond/mseconds/ms`, `microsecond(s)/usec/
  usecs/usecond/useconds/us`. As in PG only the first 10 letters are compared, so any word
  starting `millennium`, `millisecon` or `microsecon` is that unit. Two unit words in a row, or a
  unit with no number, are malformed — except that, as in PG, a unit word with a time to its left
  is dropped (`'2:00 hour'` = `02:00:00`). `at` and `on` are skipped. Any other word is malformed
  (`22007`), and so are PG's other unit and date keywords (`quarter`, `timezone`, `dow`, a month
  name, `epoch`, …) wherever they would apply to a number.
- **A number with no unit is positional.** It takes the unit last read to its right — **seconds**
  at the right end (`'5'`, `'1 day 2'` = `1 day 00:00:02`) — except that the number left of an
  **hour** or of a **time** is **days** (`'1 1:00'` = `1 day 01:00:00`, `'5 1 hour'` =
  `5 days 01:00:00`). So two unitless numbers in a row repeat a field (`'1 2'` is a duplicate
  seconds field — malformed, as in PG); a skipped `at`/`on` resets the unit to seconds.
- **`N-M`** is the SQL-standard **years-months** field: `N` years and `M` months, `M` 0–11
  (else `22008`), the sign of `N` applying to both (`'-1-2'` = `-1 years -2 mons`).
- **A time** is `H:M`, `H:M:S`, or `H:M:S.f` with the hour unbounded, minutes 0–59 and seconds
  0–60 (else `22008`); two parts with a fraction are minutes and seconds (`'1:30.5'` =
  `00:01:30.5`). Fractional seconds round to µs (below). A signed time (`'-1:30'`) negates the
  whole time. A time **replaces** the µs accumulated so far rather than adding to it, as PG's does,
  so a fractional unit read before it (to its right) keeps only its month/day part:
  `'1.5 days 1:00'` = `1 day 13:00:00` but `'1:00 1.5 days'` = `1 day 01:00:00`.
- **Signs are per field.** Under `IntervalStyle = postgres` a leading sign does **not** carry to
  the fields after it: `'-1 2:03:04'` = `-1 days +02:03:04` (SQL-standard style would read
  `-1 days -02:03:04`; jed has no such style).
- **`ago`**, only as the last field, negates the whole interval.

**Each field at most once (PG's field mask).** An input that names the same field twice is
malformed — `22007`, `invalid input syntax for type interval: "<input>"` — never summed. Each field
claims a set of field bits, and a field whose bits overlap one already read is the duplicate:

| field | claims |
|---|---|
| `millennium`, `century`, `decade`, `year`, `month`, `week`, `day`, `hour`, `minute`, `millisecond`, `microsecond` (a unitless number taking one of these) | that unit's own bit (spellings and signs do not matter: `'1 day 1d'`, `'-1 day +1 day'` are duplicates) |
| `N-M` years-months | month |
| `second` with no fraction, or a fraction of zero (`'1.0 sec'`) | second |
| `second` with a nonzero fraction | second + millisecond + microsecond |
| a time | hour + minute + second + millisecond + microsecond |

So `week` and `day` are **different** fields (`'1 week 1 day'` = `8 days`), as are the year-based
units (`'1 decade 1 year'` = `11 years`) and `second`/`millisecond`/`microsecond`
(`'1 sec 1 ms'` = `00:00:01.001`), while `'1 hour 1:00'`, `'1.5 sec 1 ms'`, `'1 sec 5'`, and
`'1 day 1 day ago'` are rejected.

*Error order.* Each field is added into its running total (out of range → `22008`) **before** it is
checked for a duplicate, right to left, so the error is the first one met from the right:
`'2000000000 days 2000000000 days'` and `'1 day 1 day 99999999999 years'` → `22008`,
`'99999999999 years 1 day 1 day'` → `22007`.

### 3.3 ISO 8601 (`DecodeISO8601Interval`)

Only when the field reading finds the text malformed is it read as **ISO 8601** — the whole,
untrimmed text, so a leading or trailing space or a lowercase `p` is malformed:

- **Format with designators:** `P` then `nY`, `nM`, `nW`, `nD` in any order, then optionally `T`
  and `nH`, `nM`, `nS` (`'P1Y2M3DT4H5M6S'`, `'P2W'`, `'PT1.5S'`). Any field may be negative or
  fractional (`'P-1Y-2M'`, `'P1.5Y'`), weeks may combine with the other units, and `'PT'` is zero.
  A repeated designator is not an error — the values add (`'P1D2D'` = `3 days`), as in PG.
- **Alternative format:** `PYYYY-MM-DD`, `PYYYY-MM`, or `PYYYY`, optionally followed by
  `THH:MM:SS` (or `THH:MM`, `THH`); or the basic form `PYYYYMMDD[THHMMSS]` (exactly 8 and 6 digits).
  `'P0001-02-03T04:05:06'` = `'P00010203T040506'` = `1 year 2 mons 3 days 04:05:06`.
- **Numbers** are read as C `strtod` reads them (PG's `ParseISO8601Number`): an optional `-`, digits
  with an optional fraction, an optional exponent (`'P1e2D'` = `100 days`), and C's hexadecimal
  form (`'PT0x10H'` = `16:00:00`; `D` is a hexadecimal digit, so `'P0x10D'` is `269 years`). A
  magnitude beyond `1e15` is `22008`; one `strtod` itself reports out of range (beyond ~1.8e308, or
  nonzero below ~2.2e-308 and not an exact double) is malformed, `22007`; `-inf`/`-nan` is
  `22008`.
- No `ago`, `@`, or whitespace: `'P1D ago'` is malformed.

### 3.4 Fractions

A field's fractional part cascades down using 1 month = 30 days and 1 day = 24 h, **per field,
independently**, exactly as PG's `Adjust*` helpers do — jed computes it with **exact integer math**
over the decimal (no float):

```
1.5 years     -> 1 year 6 mons      a year fraction rounds to whole months (half to even, PG's rint);
1.01 years    -> 1 year             the rest of the month is dropped
1.375 years   -> 1 year 4 mons      (4.5 months, to even)
1.5 months    -> 1 mon 15 days      a month fraction: whole days (× 30, truncated), then µs
1.15 months   -> 1 mon 4 days 12:00:00
1.5 weeks     -> 10 days 12:00:00   a week fraction: whole days (× 7, truncated), then µs
1.5 days      -> 1 day 12:00:00     a day fraction: µs (× 86 400 s)
1.5 hours     -> 01:30:00           hours/minutes/seconds/ms/µs fractions: µs
```

The remaining µs of a field round **half away from zero**, the engine's one mode
([decimal.md](decimal.md) §3), and so do fractional seconds in a time beyond 6 digits
(`'0:00:00.0000005'` → 1 µs). A field beyond `i32` (years, months, days) or `i64` (µs) traps
`22008`; so do years×12 + months beyond `i32` at the end (`interval out of range`).

**Errors.** Malformed text is `22007` `invalid input syntax for type interval: "<input>"`; a field out
of range is `22008` `interval field value out of range: "<input>"` (PG raises its dedicated
`22015 interval_field_overflow` — the ledgered code divergence; the final years×12 + months check is
`22008` in PG too).

### 3.5 Where jed differs from PostgreSQL

Each is a consequence of an existing jed rule, and each is pinned in
[../encoding/intervals.toml](../encoding/intervals.toml) or the override ledger:

1. **Exact numbers, not `double`.** PG carries a field's fraction (and every ISO number) in a
   `double`; jed carries it exactly — the fraction to 25 decimal digits (a hexadecimal one to 64
   bits), far below a microsecond for every unit; a fraction whose nonzero digits all lie past the
   25th still counts as nonzero for the seconds field mask. The answers agree except where PG's
   rounding decides: a µs **tie** rounds half away from zero in jed (`'0.0000005 sec'`, `'4.5 us'`,
   `'PT040506.5'` round up; PG's `double` arithmetic gives the lower value), a fraction too
   close to a month boundary for a `double` to resolve (`'P0.0416666666666666667Y'`), and the
   `strtod` range limits above, which jed compares to 40 significant digits.
2. **A bare `m`** (PG: minutes) is ambiguous with months and is rejected (`22007`); spell it out
   (`mm`, `min`, `mon`).
3. **`infinity` / `-infinity`** are PG's infinite intervals; jed has no infinite interval (§5.1), so
   they are malformed (`22007`).
4. **Field qualifiers** (`INTERVAL '1' YEAR TO MONTH`) change two rules when the text is read under
   one (§6.1): the rightmost unitless number takes the qualifier's least field, and a two-part time
   is `M:S` under `MINUTE TO SECOND`. Unqualified, the number is seconds and the time `H:M`, as here.

## 4. Rendering — PostgreSQL `IntervalStyle = postgres`

`Value::render()` emits PG's default `postgres` style with pure integer→string formatting (no
locale). The zero interval is `00:00:00`. Otherwise:

1. From `months`: `year = months / 12` and `mon = months % 12` (truncating, same sign).
2. Emit the nonzero **year**, **mon**, **day** parts in order, each as `N unit`/`N units` (plural
   when the value is **not** exactly `1`, so `-1` is `-1 years`). Parts are space-separated; a `+`
   precedes a positive part **only** when a previous part was negative (PG's `AddPostgresIntPart`).
3. From `micros` (its own sign): emit the time `HH:MM:SS[.ffffff]` when `micros != 0` **or** when no
   field has been emitted yet (the all-zero case prints `00:00:00`). The hour is **unbounded** and
   ≥ 2 digits; minute/second are 2-digit, 0–59; the fraction is appended only when nonzero with
   trailing zeros trimmed (identical to timestamp). The sign is `-` when `micros < 0`, else `+` when
   a previous field was negative, else nothing.

```
INTERVAL '1 year 2 months 3 days 4:5:6'  -> 1 year 2 mons 3 days 04:05:06
INTERVAL '-1 sec'                          -> -00:00:01
INTERVAL '1.5 days'                        -> 1 day 12:00:00
INTERVAL '2 mons -1 day'                   -> 2 mons -1 days
INTERVAL '-1 year 2 mons'                  -> -1 years +2 mons
INTERVAL '0 sec'                           -> 00:00:00
```

Because the sign placement and plural rules are easy to get subtly wrong, the corpus rows are
**bootstrapped from the live PG `db` oracle** (CLAUDE.md §7/§12) and pinned.

## 5. Arithmetic

Interval is the engine's first family-driven cross-type arithmetic and its first timestamp
arithmetic. Result-type rules (hand-written in each core's resolver; the catalog rows document the
surface):

| left | op | right | result |
|---|---|---|---|
| interval | `+` / `-` | interval | interval (field-wise; **no** justification — PG keeps fields independent; overflow `22008`) |
| `-` (unary) | | interval | interval (negate all three fields) |
| interval | `*` / `/` | integer/decimal | interval (and `number * interval` commutes) |
| timestamp | `+` / `-` | interval | timestamp (months with day-clamping, then days as 24 h, then micros; overflow `22008`) |
| timestamptz | `+` / `-` | interval | timestamptz (**in the session zone**, PG `timestamptz_pl_interval`: months with day-clamping and days are added on the zone's wall clock, each step re-resolving the wall clock through the zone as input does at a DST gap/overlap; then micros on the instant; overflow `22008`. One `timezone` cost unit per calendar step performed — §6) |
| interval | `+` | timestamp[tz] | timestamp[tz] (commutes) |
| timestamp | `-` | timestamp | interval (`micros = a − b`, `months = 0`, then `justify_hours`) |
| timestamptz | `-` | timestamptz | interval (same) |

`timestamp − timestamp` produces a **justified** interval: the difference in µs is computed, then
whole 24 h chunks move into `days` (PG `timestamp_mi` → `interval_justify_hours`), `months = 0`.

**`timestamptz ± interval` reads the session zone.** The months step converts the instant to its
wall clock in the session zone, adds the months with day-of-month clamping, and re-resolves the wall
clock through the zone (the input rules at a DST gap/overlap, [timezones.md §6](timezones.md)); the
days step does the same with calendar days; the micros are then added on the instant. So
`+ interval '1 day'` across a DST change keeps the wall-clock time where `+ interval '24 hours'`
moves the instant by exactly 24 h — PostgreSQL's behavior, verbatim. A zoneless `timestamp` (or a
`date`, which widens to a midnight timestamp) never consults a zone: a day is 24 h. Because the
result depends on the session zone, an index expression over `timestamptz ± interval` is
nonimmutable (`42P17`) unless the interval is a constant with zero months and days — a pure
microsecond shift ([index-dependencies.md](index-dependencies.md)). `uuidv7(shift)` takes the same
zoned shift ([entropy.md §4](entropy.md)).

**`interval × / number` is PostgreSQL's cascade in exact arithmetic.** PostgreSQL's `interval_mul` /
`interval_div` truncate the whole months and days (`(int32)` casts), then cascade the fractional
months into days and the fractional days into the time — with two **TSROUND** quantizations: the
fractional-month days are rounded to **1e-6 day** and the seconds remainder to **1 µs**, both ties to
even (C `rint`); a seconds remainder of **24 h or more** moves its whole days up into the days field;
the final time is `rint(time × factor + remainder)`. jed reproduces every step **in exact rational
arithmetic** (no float in the value path — CLAUDE.md §8; `i128` / big integers) with the same
quantization points and the same half-to-even rounding, so `interval '1 year' / 7` is PG's
`1 mon 21 days 10:17:08.5344` (the quantized `0.428571 day`, not the exact `10:17:08.571429`) and
`interval '1 month 1 day' * 0.75` is `23 days 06:00:00`. The one residual divergence is PG's
`double`: where its rounding error lands the value on the other side of a quantum boundary (an exact
tie such as `1 month * 1.00000005` = 1.5 micro-days, which jed rounds to even and PG's double may
not), the two differ by one quantum. Such ties are kept out of the oracle corpus; nothing else
diverges.

**Construction — `make_interval(years, months, weeks, days, hours, mins, secs)`.** The interval
*constructor* is jed's first **named + defaulted** function (every parameter named, DEFAULT 0;
[functions.md](functions.md) §11). `years/months` fold into the `months` field (×12), `weeks/days`
into `days` (×7), `hours/mins/secs` into `micros`. The integer-field math stays exact (this module's
`make_interval` helper, checked `i32`/`i64` → `22008`); the **one** float step — PG's `secs`
(`double precision`) `× 10⁶`, rounded half away from zero to a µs integer — lives in the *executor*
so the interval module stays float-free, and because it is a single correctly-rounded multiply the
resulting interval is **in-contract** (byte-identical cross-core, not an `R`-exempt float). It shares
the `× / number` half-away-vs-half-even sub-µs tie divergence above (avoided in the corpus by using
exactly-representable `secs`).

### 5.1 Justification — `justify_hours` / `justify_days` / `justify_interval`

PostgreSQL's three interval normalizers, each `interval → interval`, immutable, NULL-propagating, and
ported **step for step** from PG's `interval_justify_*` (exact integer math; `/` and `%` truncate
toward zero, as in C):

- **`justify_hours(iv)`** moves whole 24 h chunks of `micros` into `days`
  (`days += micros / 86_400_000_000`, `micros %= 86_400_000_000`), then gives `days` and `micros`
  the same sign by borrowing one day (`days > 0 && micros < 0` → `micros += 24 h`, `days -= 1`, and
  the mirror). `months` is untouched. `justify_hours('1 day -1 hour') = 23:00:00`.
- **`justify_days(iv)`** moves whole 30-day chunks of `days` into `months`, then gives `months` and
  `days` the same sign by borrowing one month. `micros` is untouched.
  `justify_days('1 mon -1 day') = 29 days`.
- **`justify_interval(iv)`** does both, leaving `|micros| < 24 h`, `|days| < 30`, and **all three
  fields the same sign**: (1) when `days` and `micros` share a sign, pre-justify `days` into
  `months` (it keeps step 3's sum within `i32`); (2) move whole days out of `micros`; (3) add them
  to `days` and move whole months out of `days`; (4) if `months` and the remaining day+time disagree
  in sign (a zero `days` defers to `micros`'s sign), borrow one month; (5) borrow one day as in
  `justify_hours`. `justify_interval('1 mon -1 hour') = 29 days 23:00:00`.

The only error is **`22008` "interval out of range"**, PostgreSQL's code and message: a `days` sum
beyond `i32` in `justify_hours`, or a `months` sum beyond `i32` in `justify_days` /
`justify_interval`. Each call charges one `operator_eval`, like every scalar function. PostgreSQL
returns its infinite intervals unchanged; jed has no infinite interval, so every input is finite.

### 5.2 `age` — the symbolic difference

`age(a, b)` over two `timestamp`s or two `timestamptz`s returns an interval that keeps **calendar
years and months**, where `a - b` counts days: `age(timestamp '2001-04-10', timestamp '1957-06-13')`
is `43 years 9 mons 27 days`. It is ported step for step from PostgreSQL's `timestamp_age` /
`timestamptz_age`:

1. Decompose both values into wall-clock fields (year, month, day, hour, minute, second, µs). A
   `timestamp` is its own wall clock; a `timestamptz` is decomposed **in the session zone**
   ([timezones.md](timezones.md) §9), whatever offset its literal was written with.
2. Subtract field by field (`a`'s minus `b`'s). If `a < b` (comparing the values themselves — for
   `timestamptz` the instants, not the wall clocks), negate every field so the later value comes
   first.
3. Borrow upward while a field is negative: µs from seconds, seconds from minutes, minutes from
   hours, hours from days, then **days from months, adding the length of the earlier value's month**
   (`b`'s when `a ≥ b`, else `a`'s — leap Februaries included, on the astronomical year so 1 BC is
   a leap year), then months from years.
4. Negate every field back if step 2 did, and assemble `months = year·12 + month`, `days`,
   `micros = time` (PG `itm2interval`).

So `age('2024-03-01', '2024-01-31') = 1 mon 1 day` (January's 31 days absorb the day deficit) and
the reverse is `-1 mons -1 days`. Because the sign comes from the instants but the fields from the
session-zone wall clocks, a DST fall-back pair whose later instant has the earlier wall clock borrows
through the year and comes out as PG prints it (`-1 mons +29 days 23:40:00`) — oracle-pinned.

**One argument.** `age(x)` is PG's SQL wrapper `age(CURRENT_DATE::timestamp[tz], x)`: the statement
clock's day in the session zone ([date.md](date.md) §6), at midnight — for `timestamptz`, that
midnight read in the session zone (PostgreSQL's DST gap rule if midnight does not exist). It reads
the clock seam once per statement ([entropy.md](entropy.md) §5, the `date-clock-literal` ledger
entry), so it is **STABLE**, never folded, and un-indexable (`42P17`), as in PostgreSQL.

**Errors and divergences.**

- **Infinity.** jed has no infinite interval (§1). PostgreSQL returns `±infinity` when exactly one
  operand is infinite and raises `22008` "interval out of range" for the same infinity twice; jed
  raises that `22008` for **every** infinite operand — the `timestamp − timestamp` rule (§5),
  ledgered in the override file. The check precedes any zone consultation.
- **Overflow.** A month total beyond `i32` or a time total beyond `i64` is `22008` "interval out of
  range" (PG `itm2interval`); jed's timestamp range keeps the year difference far inside it.
- **Volatility.** `age(timestamp, timestamp)` is immutable. `age(timestamptz, timestamptz)` reads
  the session zone, so it is **stable** and an index expression using it is `42P17`
  ([index-dependencies.md](index-dependencies.md)); PostgreSQL labels it immutable and accepts the
  index although its value changes with `TimeZone` — a ledgered divergence.
- **No implicit casts.** There is no `date` or mixed `timestamp`/`timestamptz` overload (`42883`;
  PostgreSQL casts implicitly) — jed's standing rule ([timezones.md](timezones.md) §9.1). Cast
  explicitly.

**Cost.** One `operator_eval` per call, plus one `timezone` unit per zone consultation: one per
decomposed `timestamptz` operand (none for `±infinity`, which errors first), and for the
one-argument form one for the current day and, for `timestamptz`, one for its midnight instant.

## 6. Literals, casts, keys, cost

- **Literals.** A single-quoted string adapts in an interval context (§3); the `INTERVAL '…'`
  keyword literal names the type for context-free positions. With no interval context and no
  keyword, a string stays text.
- **Casts** ([casts.toml](../types/casts.toml)): `interval → text` renders the `postgres`
  IntervalStyle form above (the any-scalar → text cast, [types.md](types.md) §5). The runtime
  `text → interval` cast (`CAST(s AS interval)` / `s::interval` on a non-literal text expression)
  parses the per-row string with the same §3 parser, raising `22007`/`22008` per row; it is STABLE,
  like PostgreSQL's `interval_in`, so an index expression containing it is `42P17`.
  `CAST(interval AS interval)` is the identity and `$1::interval` types the parameter. Every other
  cast into or out of interval is a forbidden pair (`42804`; PG `42846`).
- **Keys.** interval **is** a key (method `interval-span-i128`, [encoding.md §2.10](encoding.md)):
  the 16-byte order-preserving encoding of the canonical 128-bit **span** (§2) — `int-be-signflip`
  at i128 width (bias `2^127`, big-endian). It is a valid `PRIMARY KEY` / ordered secondary index /
  `UNIQUE` key and a FK target, and (being fixed-width) a **GIN element** too. Because the key is the
  span, two span-equal but field-distinct values (`1 mon` / `30 days` / `720:00:00`) share a key —
  the **"equal but not identical"** wrinkle: a `UNIQUE` interval index treats them as one (matching
  `1 mon = 30 days`, §2), exactly like decimal's scale-independence (`1.5` / `1.50`,
  [decimal.md](decimal.md)). The stored *value* still keeps each interval's own three fields (the
  value codec below) and renders them distinctly; only the *key* canonicalizes. Pinned by
  [../encoding/interval.toml](../encoding/interval.toml) and the `interval_pk_table.jed` golden.
- **Field qualifiers and precision** (`interval YEAR TO MONTH`, `INTERVAL '1' DAY`, `interval(3)`,
  `interval DAY TO SECOND(3)`) are a type modifier — §6.1. Without one, input reads as PG's
  unqualified `interval` does (§3).
- **On-disk value codec** (type code **11**, [format.md](../fileformat/format.md)): a fixed 16-byte
  body — `i32 months`, `i32 days`, `i64 micros`, big-endian two's-complement, no sign-flip, no
  length prefix.
- **Cost** ([cost.md](cost.md)): every interval/temporal arithmetic node — compare, `±`, unary
  minus, `timestamp ± interval`, `timestamp − timestamp`, and the `×÷ number` cascade — charges
  **one** uniform `operator_eval`, like integer/timestamp. `timestamptz ± interval` additionally
  charges one **`timezone`** unit (the AT TIME ZONE unit) per calendar step a finite operand
  performs — one for a nonzero months field, one for a nonzero days field — before the shift runs; a
  pure time-of-day interval, an ±infinity operand, or a zoneless `timestamp`/`date` charges none. The
  cascade is **bounded fixed-width work** (the factor is digit-capped, the math is `i128` / a fixed
  bignum, not unbounded decimal limbs), so no size-scaled `decimal_work` is needed; an untrusted
  query's cascade is already bounded. No new cost unit.
- **The factor digit cap.** An `interval × / number` factor is read as an exact fraction from the
  decimal's canonical text: at most **18 integer digits** and **18 significant fractional digits**
  (trailing fractional zeros stripped first, so a `numeric(p, 10)` column value such as
  `1.0000000000` is the factor `1`); beyond either bound is `22008` "interval factor has too many
  digits". The bound keeps the Rust `i128` cascade exact — every intermediate overflow it could still
  reach implies an `i32`/`i64` field overflow, so all three cores trap the same `22008` — and lies far
  past the ~16 significant digits PostgreSQL's `double` factor carries.

### 6.1 Field qualifiers and precision — the interval typmod

An interval column, composite field, or cast target may carry a **field qualifier**, a
**precision**, or both — PostgreSQL's interval type modifier. Like `numeric(p,s)` and `varchar(n)`
([types.md](types.md) §12/§15) it is a modifier, not a type: comparison, the span key, and the
value codec are unchanged.

**Grammar** (PostgreSQL's `opt_interval` / `INTERVAL '(' Iconst ')'`, [grammar.md](grammar.md) §6):
after the type name `interval` — in a column or field definition, `ALTER … TYPE`, `CAST(… AS …)`,
`::` — and after an `INTERVAL '…'` typed literal (which then means `CAST('…' AS interval
<qualifier>)`), one of

- a qualifier: `YEAR`, `MONTH`, `DAY`, `HOUR`, `MINUTE`, `SECOND`, `YEAR TO MONTH`, `DAY TO HOUR`,
  `DAY TO MINUTE`, `DAY TO SECOND`, `HOUR TO MINUTE`, `HOUR TO SECOND`, `MINUTE TO SECOND` — the
  thirteen of [datetime_units.toml](../types/datetime_units.toml) `interval_qualifier`, bare
  (unquoted) words; a qualifier ending in `SECOND` may take `(p)`: `SECOND(3)`, `DAY TO SECOND(3)`;
- or a precision alone: `interval(p)`.

Any other `TO` pairing (`DAY TO MONTH`, `SECOND TO MINUTE`, `YEAR TO SECOND`), a precision on a
qualifier not ending in `SECOND` (`YEAR(2)`), `interval(p)` followed by a qualifier, and a second
modifier (`interval(1, 2)`) are `42601`. `0 ≤ p ≤ 6`; a larger `p` is reduced to 6 (PostgreSQL also
warns; jed has no warning channel). Because the field words follow the type, `SELECT interval '1'
day` is the qualified literal, not a column alias `day` — PostgreSQL's reading. The words stay
non-reserved everywhere else.

**Reading text under a qualifier** — PostgreSQL's `DecodeInterval` with a `range`. Only text that
PostgreSQL reads with the target's typmod is read this way: a string literal cast or adapted to the
type, a string assigned to a qualified column or field, and a field of `'(…)'` record text. Two
differences from the unqualified parse (§3):

- the **rightmost unitless number** takes the qualifier's least field instead of seconds:
  `interval '5' DAY` is `5 days`, `interval '5' HOUR TO MINUTE` is `00:05:00`, `interval '1.5'
  YEAR TO MONTH` is 1.5 months (then cut, below). After an `hour` or a time field the next unitless
  number is still days, as §3.
- under **`MINUTE TO SECOND`**, a two-field time `a:b` (no seconds) reads as **minutes:seconds**:
  `interval '2:03' MINUTE TO SECOND` is `00:02:03`, where every other qualifier reads `02:03:00`. A
  minutes field past 59 is then `22008`, PG's `22015` (the §3 code divergence).

A **runtime** text value cast to a qualified interval (`col::interval MINUTE TO SECOND`) is parsed
**without** the qualifier and then cut, because PostgreSQL's run-time text→interval cast
(`CoerceViaIO`) calls `interval_in` with no typmod and applies `interval_scale` after — so
`'2:03'::text::interval MINUTE TO SECOND` is `02:03:00`.

**Cutting** — PostgreSQL's `AdjustIntervalForTypmod`, applied after every read and to every value
stored in or cast to a qualified type: the fields **below the least field** go, the fields above are
**kept** (PG's "fields to the right of the last one specified are zeroed out, but those to the left
of it remain valid"):

| least field | qualifiers | months | days | time |
|---|---|---|---|---|
| year | `YEAR` | truncated toward zero to whole years | 0 | 0 |
| month | `MONTH`, `YEAR TO MONTH` | kept | 0 | 0 |
| day | `DAY` | kept | kept | 0 |
| hour | `HOUR`, `DAY TO HOUR` | kept | kept | truncated toward zero to whole hours |
| minute | `MINUTE`, `DAY TO MINUTE`, `HOUR TO MINUTE` | kept | kept | truncated toward zero to whole minutes |
| second | `SECOND`, `… TO SECOND` | kept | kept | kept |

So `interval '1 year 2 months 3 days 04:05:06' HOUR` is `1 year 2 mons 3 days 04:00:00` and
`interval '-23 months' YEAR` is `-1 years`. Then, if a precision is declared, the microseconds field
**rounds half away from zero** to `10^(6−p)` µs (`interval '1.2345' SECOND(2)` → `00:00:01.23`,
`interval '-0.5' SECOND(0)` → `-00:00:01`); a rounding that overflows the `i64` is `22008`
("interval out of range").

**Where it applies** — the same places as a timestamp precision (timestamp.md §8): literal casts
(folded at resolve), runtime casts of an interval or text value (a scaling `Cast`, one extra
`operator_eval`), `INSERT`/`UPDATE` assignment, a constant `DEFAULT`, composite fields, and
`ALTER COLUMN … TYPE`. A query output keeps the typmod as [create-table-as.md](create-table-as.md) §2
describes. `jed_columns.type` shows PostgreSQL's spelling: `interval day to second(3)`,
`interval(2)`, `interval year` ([introspection.md](introspection.md) §5). A typmod on `interval[]`
stays `0A000`.

**On disk** (`format_version` 43, [format.md](../fileformat/format.md)): an interval column entry
or composite field appends `u8 interval_fields` (the qualifier code, 0 for none) then `u8 precision`
(`0xFF` for none). The value codec is unchanged.

Pinned by suites/types/interval_qualifier.test (oracle-checked) and
suites/introspection/temporal_typmod.test.

## 7. Determinism traps (the cross-core checklist)

1. **The fractional-unit cascade** (parse AND `× / number`) — exact integer/decimal math: the
   parser rounds µs half away from zero (a year fraction's months half to even, as PG's `rint`);
   the `× / number` cascade is PG's step for step with its two TSROUND quantizations and half-to-even
   rounding (§5). Identical months/days/micros and identical render across cores. The #1 divergence
   risk; pinned by [intervals.toml](../encoding/intervals.toml) and oracle-diffed vs PG.
2. **The parser is a port, not a grammar.** PG's field splitting, right-to-left positional reading,
   field masks, and ISO fallback (§3) are mirrored step for step in every core — including PG's
   quirks (a time replaces earlier µs; `at`/`on` reset the positional unit; 10-letter unit
   prefixes; the 25-field / 256-byte limits).
3. **The canonical span is the comparison/dedup key, not the field triple.** `=`, `<`, `ORDER BY`,
   and the dedup bucket all key on `span()`; the surviving dedup representative is first-seen.
4. **128-bit math** — Rust `i128`, TS `BigInt`, Go a hand-rolled signed 128-bit compare
   (`math/bits`, never a silent 64-bit wrap). Identical on negatives and extremes.
5. **TS `micros` is `bigint`** end to end (parse, value, codec, span) — `number` loses i64
   precision (CLAUDE.md §2).
6. **Render byte-identity** — field order, zero-field omission, the `+`/`-` sign placement, the
   unbounded hour, the plural rule (plural when ≠ 1, so `-1` is plural), and the bare `00:00:00` for
   the zero interval. Mirrored in the Ruby reference.
7. **Field overflow** — `i32` months/days, `i64` micros; `interval ± interval` and `ts − ts`
   justify can overflow → `22008`, one bound, identical across cores.
8. **Literal asymmetry** — a string adapts to interval *in an interval context*; with the
   `INTERVAL` keyword it is always interval. A bare string with no context stays text.
