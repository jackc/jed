# Data-type formatting functions — design

> The reasoning behind `to_char`, `to_timestamp`, `to_date`, and `to_number`. The **data is
> authoritative**: [../types/formatting.toml](../types/formatting.toml) (the template vocabulary —
> keyword tables, modifiers, word lists), [../functions/catalog.toml](../functions/catalog.toml)
> (the overloads, volatility, error codes), [../errors/registry.toml](../errors/registry.toml)
> (`22009`). The algorithms are **PostgreSQL 18's `formatting.c`**, transcribed; this doc records
> how the transcription is pinned so three cores agree byte for byte, and every place jed departs
> from PostgreSQL. The corpus is the contract: `suites/expr/to_char_datetime.test`,
> `to_char_number.test`, `to_timestamp.test`, `to_number.test`,
> `suites/resource/scalar_output_formatting.test`, `suites/query/explain_formatting.test`.

## 1. Surface

| Function | Result | Volatility | Notes |
|---|---|---|---|
| `to_char(timestamp, text)` | text | immutable | ±infinity → NULL |
| `to_char(timestamptz, text)` | text | **stable** | decomposed in the session zone |
| `to_char(interval, text)` | text | immutable | calendar patterns are `22007` |
| `to_char(i16 \| i32 \| i64, text)` | text | immutable | `int4_to_char` / `int8_to_char` |
| `to_char(decimal, text)` | text | immutable | `numeric_to_char` |
| `to_char(f32 \| f64, text)` | text | immutable | `float4_to_char` / `float8_to_char` |
| `to_timestamp(text, text)` | timestamptz | **stable** | offset-less input read in the session zone |
| `to_date(text, text)` | date | immutable | |
| `to_number(text, text)` | decimal | immutable | |
| `to_timestamp(f64)` | timestamptz | immutable | Unix-epoch seconds (`float8_timestamptz`) |

All are STRICT (a NULL argument is NULL). An **empty template** makes the datetime `to_char`, `to_number`,
and the empty-template `to_timestamp` path behave as PostgreSQL: `to_char(ts, '')` is NULL,
`to_char(n, '')` is `''`, `to_number(s, '')` is NULL, and `to_timestamp(s, '')` / `to_date(s, '')`
parse nothing (year 0 = `0001-01-01 BC`).

**Volatility is jed's per-operation rule** ([index-dependencies.md](index-dependencies.md)), not
PostgreSQL's blanket STABLE. PostgreSQL marks every formatting function STABLE because `lc_numeric` /
`lc_time` can change the output; jed has no locale (§4), so an overload that reads only its arguments
is immutable and admissible in an index expression. The two that read the session zone are stable
(`42P17` in an index expression), like the other session-zone consumers (timezones.md §9.4). This is a
deliberate, documented divergence on PostgreSQL's `42P17` surface.

**Resolution.** Overloads are selected by argument family (functions.md §9). There is **no `date`
overload**: PostgreSQL implicitly casts a `date` to `timestamptz`, and jed has no implicit
date→timestamp cast in function resolution (the `date_trunc` rule, timezones.md §9.1), so
`to_char(d, …)` is `42883` — write `d::timestamp`. A bare string first argument is ambiguous
(`42725`), as in PostgreSQL. The integer overloads split by width like PostgreSQL's `int4` / `int8`
functions: `i16` and `i32` take `int4_to_char`, `i64` takes `int8_to_char`. They differ only in EEEE
(§6.3) and in where `V` overflows. Because a **bare integer literal is `i64`** in jed (types.md §6),
`to_char(125, '9.9EEEE')` takes the int8 path (`1.3e+02`) where PostgreSQL reads `125` as `int4`
(`1.2e+02`) — a consequence of the documented literal-typing divergence; write `125::i32`. An integer
argument does not promote to `f64` (`to_timestamp(1700000000)` is `42883`, the jed strictness of
numeric_transcendental.test); write `1700000000::f64`.

## 2. Templates

A template is parsed by `parse_format`: at each position, an optional **prefix modifier** (`FM`, `TM`;
datetime templates only), then a **keyword** found by sequential search from the first keyword sharing
the current byte — the first whose name is a prefix of the remaining template wins, so table order is
the longest-match rule (`HH24` before `HH`, `DDD` before `DD`, `Month` before `Mon`) — then an optional
**postfix modifier** (`TH`, `th`, `SP`). Anything else is a literal node: a double-quoted string (a
backslash inside quotes takes the next character literally), a backslash before a double quote, or one
UTF-8 character. In a datetime template, printable ASCII that is neither a letter nor a digit is a
*separator* and whitespace a *space* (the reader treats both specially, §5). A modifier with no keyword
after it is dropped. `SP` (Oracle's spelling mode) and `FX` render nothing.

The keyword tables, the modifiers, and the fixed word lists (month/day names, Roman numerals, AM/PM,
AD/BC, ordinal suffixes) are mechanical data, so they live in `spec/types/formatting.toml` and are
codegen'd into each core (`formatting_gen.{rs,go,ts}`, `scripts/gen_formatting.rb`; `mise run verify`
checks the files are current). The generator checks that keywords sharing a first byte are contiguous,
which the search relies on. Everything else is hand-written per core.

Templates and inputs are processed as **UTF-8 bytes**, exactly as `formatting.c` does — the TS core
encodes its UTF-16 strings first — so a multibyte character is one literal node, a byte-level prefix
search never splits a pattern, and the three cores agree byte for byte. PostgreSQL caches parsed
templates; jed parses per call (the template may be a column).

## 3. `to_char` over datetimes and intervals

The value is broken into PostgreSQL's `fmt_tm` (year, month, day, hour, minute, second, microsecond,
weekday, day of year, UTC offset, zone abbreviation) and `DCH_to_char` renders each node.

- **timestamp:** the wall clock (`timestamp2tm`); no zone, so `TZ` is empty and `OF`/`TZH`/`TZM` are
  `+00`/`+00`/`00`.
- **timestamptz:** the instant converted to the **session zone** (one `timezone` cost unit), with the
  zone's UTC offset at that instant (`OF` prints hours and minutes; seconds of a local-mean-time offset
  are dropped, as in PostgreSQL) and its abbreviation from the zone's TZif data (`EST`, `IST`, `LMT`).
  UTC prints `UTC`. A **fixed-offset** session zone has no abbreviation — PostgreSQL's `±HH:MM` zone
  spelling (the only one jed's session zone accepts, POSIX sign) renders `TZ` empty.
- **interval:** PostgreSQL's `interval2itm` decomposition (`months / 12` years, `months % 12` months,
  days, and the time split into an unbounded hour count, minutes, seconds, microseconds — each keeping
  its sign), with day-of-year approximated as `(years·12 + months)·30 + days`. Patterns tied to a
  calendar date (`Month`, `Mon`, `Day`, `Dy`, `D`, `ID`, `AD`/`BC`, `TZ`, `OF`, `TZH`, `TZM`) are
  `22007` "invalid format specification for an interval value". `HH`/`HH12` print the 12-hour clock
  even for an interval.

The rendering is `formatting.c`'s, including its C details, which the cores reproduce exactly:
`"%0*d"` zero padding counts the sign toward the width (`-05`), `FM` sets the width to zero, names are
blank-padded to 9 (Roman months to 4) unless `FM` or `TM`, `TH` / `th` append `st`/`nd`/`rd`/`th` (teens
take `th`), `CC` / `Y,YYY` / the ISO-year and partial-year patterns use C's truncating `/` and `%`.
PostgreSQL's calendar helpers (`date2j`, `j2date`, `date2isoweek`, `date2isoyear`, `date2isoyearday`,
`isoweek2j`) are ported with C `int` arithmetic — **int32 wrapping**, since PostgreSQL builds with
`-fwrapv`, and `unsigned` arithmetic in `j2date` — because an interval's year count can leave the range
where they are exact and the result must still be one defined value. The weekday is
`(julian + 1) mod 7`; beyond PostgreSQL's range a Julian day can be negative, where jed uses a floor
modulo (PostgreSQL's array index would be out of bounds there).

## 4. Locale

jed has no locale; the oracle pins `lc_time = lc_numeric = C` (oracle_profile.toml), and jed is defined
as PostgreSQL under that setting:

- `TM` (translation mode) renders the same English names, but **unpadded** (PostgreSQL pads only the
  non-translated names) and through the full case conversion — identical for ASCII names. In
  `to_timestamp`, a `TM` name is matched with ASCII case folding; PostgreSQL's collation-aware folding
  differs only for non-ASCII look-alikes (`ſ`, the Kelvin sign), a documented narrowing.
- `D` (locale decimal point) is `.`, `G` (group separator) `,`, `L` (currency symbol) a space, `S`'s
  signs `-` / `+` — exactly `NUM_prepare_locale` under the C locale.

## 5. `to_timestamp` and `to_date`

`DCH_from_char` reads the input node by node, then `do_to_timestamp` assembles the fields; the cores
port both step for step.

1. **Fields.** A numeric field reads a fixed width (the pattern's length; `HH`/`HH24`/`TZH`/`TZM` read
   2, `MS` 3, `US` 6, `FFn` n) unless `FM` is given or the next node is not a digit pattern, in which
   case it reads every digit there (C `strtol`, so a sign and leading spaces are accepted). Too few
   characters, a partial field, a non-number, or a value outside `int` are `22007` / `22008`. A field
   set twice to different values is `22007` "conflicting values". `MS`/`US`/`FFn` scale a short
   reading (`.5` is 500 ms). `Q` is read and ignored. `TH` skips two characters.
2. **Separators and spaces.** Outside `FX`, leading spaces and spaces before each field are skipped, a
   template separator or space matches one input separator or space (or nothing), and a literal
   character skips one input character. `FX` makes every template character consume exactly one input
   character.
3. **Words.** `AM`/`PM`, `AD`/`BC`, month and day names (full or abbreviated), and Roman months are
   matched ASCII case-insensitively against the English lists.
4. **Offsets.** `TZH`, `TZM`, and `OF` (`±HH[:MM]`) give the zone; a minus swallowed as a separator
   before `TZH` still counts. Hours past 15 or minutes past 59 are **`22009`** "time zone displacement
   out of range". `TZ` reads an offset like `OF`; a **zone abbreviation** (a letter where `TZ` reads)
   is **`0A000`**: PostgreSQL resolves it against its `timezone_abbreviations` table and the session
   zone's abbreviations, and jed has no abbreviation table yet (timezones.md §15). A documented
   divergence, recorded in the override ledger.
5. **Assembly** (`do_to_timestamp`): `SSSS` seconds of the day, the 12-hour clock (`22007` for an hour
   outside 1–12), `YY`-style years adjusted toward 2020 (`adjust_partial_year_to_2020`), `CC` with a
   two-digit year (`21 05` is 2005), BC years, `J`, ISO week dates (`IYYY`/`IW`/`ID`, or `IDDD`), `WW`
   / `W` week numbers, day of year (`22007` without a year), then PostgreSQL's `ValidateDate` and time
   range checks (`22008` "date/time field value out of range"). Mixing Gregorian and ISO-week patterns
   is `22007`. With no date fields the date is year 0 (`0001-01-01 BC`), as in PostgreSQL.
6. **The instant.** With an explicit offset the wall clock minus the offset is the instant; otherwise
   the wall clock is read **in the session zone** (one `timezone` cost unit), with PostgreSQL's DST gap
   and overlap choice (`localToInstantMicros`, timezones.md §6). `FFn` with `n < 6` then rounds the
   instant to `n` fractional digits half away from zero **relative to PostgreSQL's 2000-01-01 epoch**
   (`AdjustTimestampForTypmod`), so a tie between 1970 and 2000 rounds the way PostgreSQL does.
7. **Range.** The result must be a finite jed `timestamp` (int64 microseconds, timestamp.md §1) or
   `date` (int32 days, date.md §1); otherwise `22008`. jed's ranges differ from PostgreSQL's at the
   extremes (wider below 4713 BC — `to_timestamp('-5', 'J')` is a value; narrower above year 294247),
   ledgered.

PostgreSQL's *standard* parsing mode (`std`) serves only the SQL/JSON `.datetime()` method, which jed
does not support by decision ([jsonpath.md](jsonpath.md) §3.1), so it is not implemented.

`to_timestamp(f64)` is `float8_timestamptz`: NaN is `22008`, ±Infinity are ±infinity, and a finite
value is `rint((seconds − 946684800) · 10⁶)` microseconds after PostgreSQL's 2000 epoch — the same
float operations, so the rounding matches to the microsecond — checked against jed's range.

## 6. `to_char` over numbers

### 6.1 The template

`NUMDesc_prepare` folds the number patterns into a description — digits before and after the decimal
point, the sign style (`S` before or after, `MI`, `PL`, `SG`, `PR`), fill mode, zero padding from the
first `0`, `B`, `RN`, `V` (multiplier digits), `EEEE` — and rejects impossible combinations with
**`42601`** (`9.9.9`, `S` twice, `S` with `MI`/`PL`/`SG`/`PR`, `PR` before a digit, `V` with a decimal
point, `RN` with anything but `FM`, `EEEE` not last or with other modifiers).

### 6.2 The processor

`NUM_processor` lays the number's digit string out against the nodes: leading blanks or zeros, the sign
when the first significant position is reached, digits, the decimal point, group separators only inside
the number, `FM`'s trailing-zero suppression (`get_last_relevant_decnum`, keeping digits a `0` pattern
asks for), the post-sign or `PR`'s `>`, ordinal suffixes, Roman numerals right-aligned to 15. A number
with more integer digits than the template is **`#` fill** (`###.##`). The output is a C string:
PostgreSQL stops at the first NUL byte, which a digit position past the end of the number writes, and
the cores truncate there the same way.

### 6.3 The entry points

- **int4 (`i16`, `i32`) / int8 (`i64`):** the decimal digits, zero-padded to the fractional digits.
  `RN` uses the value (int8: `PG_INT32_MAX` when out of range, which prints 15 `#`). `V` multiplies by
  `10ⁿ` through `dtoi4`/`int4mul` (int8: `dtoi8`/`int8mul`), `22003` on overflow. EEEE: int4 formats
  the float8 value with `"%+.*e"` (**round half to even**); int8 goes through `numeric_out_sci`
  (**round half away from zero**). *PostgreSQL 18.6, the live oracle, multiplied int4 by
  `(int32) pow(10, n)` in C `int` arithmetic, so an overflowing product wrapped; REL_18_STABLE commit
  `ec434db1f49` (in 18.7) made it `22003`. jed follows the fixed code; the oracle disagreement is
  ledgered.*
- **decimal:** `V` multiplies by `10ⁿ`; the value is rounded to the fractional digits (half away from
  zero) and printed; `RN` rounds to an integer; EEEE is `numeric_out_sci` — the significand rounded
  half away to the fractional digits, then `e±XX` — which keeps a significand that rounds up to `10`
  (`9.9996` with `9.999EEEE` is `10.000e+00`). jed's `decimal` is finite, so PostgreSQL's `NaN` /
  `Infinity` branches never occur.
- **f32 / f64:** PostgreSQL's float paths use C `printf`. jed reproduces it from the float's **exact
  binary value** (the exact decimal expansion every core already has for `float → decimal`, float.md
  §6), rounded half to even (glibc's rule), with PostgreSQL's own `snprintf` spellings: `NaN`,
  `Infinity`, `-Infinity`, a sign on negative zero, precision capped at 350 with the rest zero-filled.
  At most `FLT_DIG` (6) / `DBL_DIG` (15) significant digits are kept: digits past them are dropped from
  the fraction (and the processor's NUL truncation, §6.2, then ends the output at the last digit
  printed). `V` multiplies in the float's own precision (f32 × f32) by glibc's `pow(10, n)`: the nearest
  double to `10ⁿ` (the decimal spelling `1e<n>` read correctly rounded) except at the two exact ties
  below `10³⁰⁹`, `n = 23` and `n = 210`, where glibc returns the upper neighbor — checked against
  PostgreSQL for every `n` from 0 to 308; no language's `pow` is used. EEEE of NaN / ±Infinity is `#` fill.
  This is in contract — no float formatting library is involved, so there is no determinism-ledger
  entry; the `R` tolerance never applies to a text result.

## 7. `to_number`

The processor runs in reading mode: each `9` / `0` / `.` / `D` node reads one input digit or the decimal
point (after an optional leading space and sign), `,` / `G` skip a group separator, `S` / `MI` / `PL` /
`SG` / `PR` read signs before or after the digits (`<…>` for `PR`), `L` and `TH` skip non-data
characters, literal nodes skip one character each, and `RN` reads a Roman numeral (strictly validated —
`22P02` "invalid Roman numeral"). `EEEE` is `0A000` "not supported for input". The digits read are then
`numeric_in` with typmod `(pre + V digits + fractional digits read, fractional digits read)`: no digits
is `22P02`, more integer digits than the template allows is `22003` "numeric field overflow". `V`
multiplies by `power(10, −n)` with PostgreSQL's `numeric_power` result scale, so `to_number('12345',
'99V999')` is `12.3450000000000000000`.

## 8. Divergences from PostgreSQL (all recorded in the oracle-override ledger)

1. No `date` overload — `42883` (§1).
2. A bare integer literal is `i64`, so it takes the int8 path (§1, §6.3).
3. A zone abbreviation in `to_timestamp` input is `0A000` (§5.4).
4. jed's timestamp / date ranges at the extremes (§5.7).
5. int4 `V` overflow follows the fixed PostgreSQL code, not the 18.6 oracle (§6.3).
6. Volatility: the zone-free overloads are immutable (§1).
7. `TM` input matching folds ASCII only (§4).
8. A fixed-offset session zone renders `TZ` empty, also for a zero offset spelled `+00:00`, which jed's
   session zone cannot tell from `UTC` (renders `UTC`; PostgreSQL renders the `+00:00` spelling empty).

## 9. Cost and untrusted-query safety

All of these are pure functions of their arguments and, for the two stable overloads, the session zone
(read through the tz seam, CLAUDE.md §13) — no I/O, no host reach. `to_char` is a **growth kernel**
([cost.md](cost.md) §8.1): its output is at most about 13 bytes per template byte (`J`, `SSSS`, and an
interval's `HH` expand the most), so it charges `scalar_byte` × the UTF-8 bytes of its result after
building it and is free over constant operands; a recursive chain feeding its output back as its
template is stopped by the cost ceiling (`suites/resource/scalar_output_formatting.test`). The other
functions build a fixed-size value from a scan of their input and charge only `operator_eval` (plus the
`timezone` unit where the session zone is read, and `decimal_work` for `to_number`'s `V` power). A
template is parsed in one pass, the processors are linear in template plus input, and there is no
backtracking, so no template is pathological.

## 10. Status

Landed in all three cores, oracle-checked against PostgreSQL 18. Follow-on (TODO.md): zone
abbreviations in `to_timestamp` input, with the abbreviation-table decision (timezones.md §15).
