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

This slice ships the **"unit + time" input subset** and **full arithmetic**, and **defers**: the
ISO-8601 `P…` and SQL-standard combined input forms, field qualifiers (`YEAR TO MONTH`), the
`interval(p)` precision typmod, CAST to/from interval, and interval as a key. Those are documented
narrowings (§6).

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

## 3. Parsing — the "unit + time" subset

A value is written either as a single-quoted string adapting in an interval context (literal
adaptation, like timestamp/bytea — the executor's resolve-time coercion) or with the
**`INTERVAL '…'` keyword literal** (which names the type, so it works context-free, e.g.
`SELECT INTERVAL '1 day'` and inside arithmetic `ts + INTERVAL '1 day'`). Both parse the same
grammar. Parsing happens at **resolve time**, before any scan, so a bad literal traps
deterministically (like bytea/timestamp).

The accepted input is a whitespace-separated sequence of **segments**, optionally preceded by `@`
and optionally followed by `ago`:

```
input   := '@'? segment (ws segment)* (ws 'ago')?          # `ago` may also abut a bare time
         | '@'? (segment ws)* bare                  # a unitless number only as the LAST field
segment := sign? number ws? unit        # a unit value (with optional fractional part)
         | sign? HH ':' MM (':' SS ('.' frac)?)?   # a bare time (hours unbounded)
bare    := sign? number                 # seconds, exactly as if `seconds` followed it
sign    := '+' | '-'                    # per-segment
```

- **Units** (case-insensitive, with abbreviations): `millennium/millennia/mil`, `century/
  centuries/cent/c`, `decade(s)/dec`, `year(s)/yr/yrs/y`, `month(s)/mon/mons`, `week(s)/w`,
  `day(s)/d`, `hour(s)/hr/hrs/h`, `minute(s)/min/mins`, `second(s)/sec/secs/s`,
  `millisecond(s)/msec/ms`, `microsecond(s)/usec/us`. A bare `m` is **not** accepted (it is
  ambiguous between months and minutes in PG); spell it out. An unknown unit traps `22007`.
- **Per-field sign.** Each segment carries its own optional sign (`'1 day -02:00'` = `1 day`,
  `-2 hours`).
- **`ago`** at the end negates the **whole** interval (all three fields).
- **Bare time `HH:MM[:SS[.ffffff]]`.** A number immediately followed by `:` is the hour of a time
  segment; the hour is **unbounded** (`'100:00:00'`), minutes/seconds are 0–59, and 0–6 fractional
  digits are exact, 7+ rounded to µs **half away from zero** (the engine's one mode — decimal.md
  §3, identical to timestamp).

**Unitless numbers and the deferred SQL-standard forms — fail closed.** PostgreSQL reads interval
input right to left and gives a unitless number a *positional* meaning: the rightmost one is
seconds, but one left of a bare time is days (`'1 1:00'` = `1 day 01:00:00`), one left of an
`hour` field is days (`'5 1 hour'` = `5 days 01:00:00`), and `N-M` is the SQL-standard year-month
form (`'1-2'` = `1 year 2 mons`, `'1-2 3 4:05:06'` = `1 year 2 mons 3 days 04:05:06`). jed defers
these SQL-standard forms, and must never *misread* them (an earlier parser read `'1-2'` as `1 s` +
`-2 s` = `-00:00:01`). Two rules make the deferral fail closed with `22007`:

1. **Fields are whitespace-separated.** A segment may not start immediately after the previous one
   (`'1-2'`, `'1day-2'`, `'1hour2min'`, `'1:00-2'` all trap). Only a unit word may abut its own
   number (`'1day'`), and `ago` may abut the field before it (`'1:00:00ago'`) — both as in PG.
2. **A unitless number is accepted only as the last field**, where it means seconds (`'5'`,
   `'-1.5'`, `'1 day 2'` = `1 day 00:00:02`). That is exactly where PG also reads seconds: in PG
   `'X N'` is equivalent to `'X N seconds'`. Anywhere else — `'1 1:00'`, `'5 1 hour'`, `'1 2'`,
   `'5 ago'` — it traps. PG rejects `'1 2'` / `'5 ago'` too; for `'1 1:00'` / `'5 1 hour'` /
   `'1-2'` the trap is a **deliberate divergence** (PG accepts), pinned by the `parse_error`
   vectors in [../encoding/intervals.toml](../encoding/intervals.toml) rather than the corpus.

PG also accepts `'.5'` and `'5.'` (no digits on one side of the point); jed traps both — fail
closed, not a misread.

**Each field at most once (PG's `DecodeInterval` field mask).** An input that names the same field
twice is malformed — `22007`, `invalid input syntax for type interval: "<input>"` — never summed.
Each segment claims a set of field bits, and a segment whose bits overlap an earlier one's is the
duplicate:

| segment | claims |
|---|---|
| `millennium`, `century`, `decade`, `year`, `month`, `week`, `day`, `hour`, `minute`, `millisecond`, `microsecond` | that unit's own bit (spellings and signs do not matter: `'1 day 1d'`, `'-1 day +1 day'` are duplicates) |
| `second` with no fraction, or a fraction of zero (`'1.0 sec'`) | second |
| `second` with a nonzero fraction | second + millisecond + microsecond |
| a trailing unitless number | as `second` (`'1 sec 5'`, `'1:00 5'` are duplicates; `'1 ms 5'` is not) |
| a bare time `HH:MM[:SS[.ffffff]]` | hour + minute + second + millisecond + microsecond |

So `week` and `day` are **different** fields (`'1 week 1 day'` = `8 days`), as are the year-based
units (`'1 decade 1 year'` = `11 years`) and `second`/`millisecond`/`microsecond`
(`'1 sec 1 ms'` = `00:00:01.001`), while `'1 hour 1:00'`, `'1.5 sec 1 ms'`, and `'1 day 1 day ago'`
are rejected. The rule lives in the one parser, so it binds every text→interval path (the literal,
an adapting string, `::interval`/CAST, the runtime text cast, a bound parameter, container element
text).

*Error precedence.* PG reads the fields right to left, adding each into its running fields (out of
range → its overflow error) **before** testing it for a duplicate. jed parses left to right, then
replays that order over the parsed fields: a duplicate traps `22007` unless the running total from
the right end through the duplicate is already outside the `i32` month/day or `i64` µs range, in
which case it traps `22008` as PG would (`'2000000000 days 2000000000 days'`,
`'1 day 1 day 99999999999 years'` → `22008` — PG's code is `22015`, the ledgered code divergence;
`'99999999999 years 1 day 1 day'` → `22007`). Two residual differences, both on input PG also
rejects: jed's running months fold years into months where PG keeps them apart until the end, and an
error found while *reading* a segment (unknown unit, too many digits, a non-final unitless number)
is reported left to right as before, so input that is both malformed and duplicated or out of range
can report the other code.

**Fractional-unit cascade.** A unit value may carry a fractional part, which cascades down using
1 month = 30 days and 1 day = 24 h, **per token, independently** (PG semantics; jed computes it with
**exact integer math**, no float):

```
1.5 years   -> 1 year 6 mons          (0.5 * 12 = 6 months, exact)
1.5 months  -> 1 mon 15 days          (0.5 * 30 = 15 days, exact)
1.5 days    -> 1 day 12:00:00         (0.5 * 86400 s, exact)
1.15 months -> 1 mon 4 days 12:00:00  (0.15 * 30 = 4.5 days = 4 days + 0.5 day)
```

The exact rule for one token `value unit`, with `value = N/D` (a signed integer numerator over a
power-of-ten denominator) and the unit's base weight (`months_per`, `days_per`, or `micros_per` —
exactly one nonzero):

1. Compute the integer part in the unit's base field (truncate toward zero); the remainder cascades.
2. A remaining fractional **month** becomes days (`× 30`): integer part → `days`, remainder cascades.
3. A remaining fractional **day** becomes µs (`× 86_400_000_000`).
4. A **time-unit** value is µs directly.
5. The accumulated µs numerator (over `D`) is rounded to an integer **half away from zero** once, at
   the bottom.

All arithmetic is exact `i128` integer math over `N/D`; a field beyond `i32`/`i64` traps `22008`.

**Errors.** Malformed / unparseable syntax (`22007` `invalid_datetime_format`); a field beyond the
representable range (`22008` `datetime_field_overflow`). Same codes as timestamp — and PG uses these
same two classes for interval input.

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

1. **The fractional-unit cascade** (parse AND `× / number`) — exact integer/decimal math, **one**
   rounding mode (half away from zero), identical months/days/micros and identical render across
   cores. The #1 divergence risk; pinned by [intervals.toml](../encoding/intervals.toml) and
   oracle-diffed vs PG.
2. **The canonical span is the comparison/dedup key, not the field triple.** `=`, `<`, `ORDER BY`,
   and the dedup bucket all key on `span()`; the surviving dedup representative is first-seen.
3. **128-bit math** — Rust `i128`, TS `BigInt`, Go a hand-rolled signed 128-bit compare
   (`math/bits`, never a silent 64-bit wrap). Identical on negatives and extremes.
4. **TS `micros` is `bigint`** end to end (parse, value, codec, span) — `number` loses i64
   precision (CLAUDE.md §2).
5. **Render byte-identity** — field order, zero-field omission, the `+`/`-` sign placement, the
   unbounded hour, the plural rule (plural when ≠ 1, so `-1` is plural), and the bare `00:00:00` for
   the zero interval. Mirrored in the Ruby reference.
6. **Field overflow** — `i32` months/days, `i64` micros; `interval ± interval` and `ts − ts`
   justify can overflow → `22008`, one bound, identical across cores.
7. **Literal asymmetry** — a string adapts to interval *in an interval context*; with the
   `INTERVAL` keyword it is always interval. A bare string with no context stays text.
