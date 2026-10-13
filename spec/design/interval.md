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
precision typmod are deferred (§6).

## 1. Representation — three independent fields

A value is PostgreSQL's `struct Interval`: **`months` (i32)**, **`days` (i32)**, **`micros`
(i64)** — three independent fields, not one scalar.

They are kept **separate on purpose**, because months and days are not fixed-length:

- A month is 28–31 days, so `timestamp + interval '1 month'` is a **calendar** add (Jan 31 + 1
  month → Feb 28/29, day clamped to the target month) — genuinely different from `+ '30 days'`.
- A day is 24 h here (jed has no time zones; under DST PG's day can be 23/25 h, but that is a
  zone concern jed does not have).

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
and with no field qualifier (§6). It accepts every form PostgreSQL does — the PostgreSQL unit
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
4. **Field qualifiers** (`INTERVAL '1' YEAR TO MONTH`, `interval(p)`) are not implemented (§6), so
   the rightmost unitless number is always seconds and a two-part time without a fraction is always
   `H:M`.

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
| timestamp[tz] | `+` / `-` | interval | timestamp[tz] (months with day-clamping, then days as 24 h, then micros; overflow `22008`) |
| interval | `+` | timestamp[tz] | timestamp[tz] (commutes) |
| timestamp | `-` | timestamp | interval (`micros = a − b`, `months = 0`, then `justify_hours`) |
| timestamptz | `-` | timestamptz | interval (same) |

`timestamp − timestamp` produces a **justified** interval: the difference in µs is computed, then
whole 24 h chunks move into `days` (PG `timestamp_mi` → `interval_justify_hours`), `months = 0`.

**`interval × / number` is a documented PG divergence.** PostgreSQL multiplies/divides the fields
through `double` and cascades the fraction months→days→micros; jed does the **same cascade with
exact integer/decimal math** (no float in the value path — CLAUDE.md §8), rounding the µs result
half away from zero. The two agree on all "nice" factors and differ only on sub-unit ties (the same
class as the half-away-vs-half-even timestamp-fraction divergence). Recorded in the override ledger.

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
- **Field qualifiers — deferred.** `INTERVAL '1' YEAR TO MONTH`, `interval(p)`, and a typmod
  on an interval column are not implemented (grammar + typmod work); without one, input reads as
  PG's unqualified `interval` does (§3).
- **On-disk value codec** (type code **11**, [format.md](../fileformat/format.md)): a fixed 16-byte
  body — `i32 months`, `i32 days`, `i64 micros`, big-endian two's-complement, no sign-flip, no
  length prefix.
- **Cost** ([cost.md](cost.md)): every interval/temporal arithmetic node — compare, `±`, unary
  minus, `timestamp ± interval`, `timestamp − timestamp`, and the `×÷ number` cascade — charges
  **one** uniform `operator_eval`, like integer/timestamp. The cascade is **bounded fixed-width
  work** (the factor is digit-capped, the math is `i128` / a fixed bignum, not unbounded decimal
  limbs), so no size-scaled `decimal_work` is needed; an untrusted query's cascade is already
  bounded. No new cost unit.

## 7. Determinism traps (the cross-core checklist)

1. **The fractional-unit cascade** (parse AND `× / number`) — exact integer/decimal math, µs
   rounded half away from zero (a year fraction's months half to even, as PG's `rint`), identical
   months/days/micros and identical render across cores. The #1 divergence risk; pinned by
   [intervals.toml](../encoding/intervals.toml) and oracle-diffed vs PG.
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
