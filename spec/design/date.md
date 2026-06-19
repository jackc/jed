# The `date` type — design

> The reasoning behind the `date` calendar type. The **data is authoritative**
> ([../types/scalars.toml](../types/scalars.toml) — the type, family, encoding method;
> [../types/compare.toml](../types/compare.toml) — comparability/ordering;
> [../types/casts.toml](../types/casts.toml) — casts (deferred);
> [../fileformat/format.md](../fileformat/format.md) — the on-disk value codec + type code;
> [../encoding/dates.toml](../encoding/dates.toml) — the parse/render byte vectors).
> This doc is the *why* and the precise calendar/parse/render arithmetic the three cores must
> reproduce **byte-identically** (CLAUDE.md §2, §8). When a decision here changes, change the
> data and here in the same edit, and update [CLAUDE.md](../../CLAUDE.md) §4 if it revises a
> commitment. It is the **sibling of [timestamp.md](timestamp.md)** and deliberately reuses
> timestamp's calendar core verbatim.

`date` is a **calendar date** — a year/month/day, with **no time, no zone**. It is PostgreSQL's
`date`: the day-granular member of the datetime family, the natural companion to
`timestamp`/`timestamptz`. This slice implements the **core type** — storage, ISO literals,
comparison/ordering, rendering, the `±infinity` sentinels, and a `date` PRIMARY KEY — mirroring
the original timestamp slice. **Date arithmetic** (`date + int`, `date - date`, `date ± interval`)
and **casts** (text↔date, date↔timestamp) are **deferred follow-ons**, exactly as the timestamp
slice deferred interval arithmetic and casts (§6). The non-goal is wire/`pg_catalog` fidelity
(CLAUDE.md §1); the goal is PG's *observable* date behavior on the surface we implement.

## 1. Representation — int32 days since the Unix epoch

A value is an **`int32` count of days** since `1970-01-01`, proleptic Gregorian, **no leap
seconds** (every day is one count). This is the day-granular analogue of `timestamp`'s int64
microseconds, and it deliberately **reuses timestamp's exact calendar core** —
`days_from_civil` / `civil_from_days` (Howard Hinnant), which already measure days from
1970-01-01 ([timestamp.md](timestamp.md) §2). So the two types share one tested civil↔days
algorithm and **cannot drift** from each other.

**Epoch — a documented internal divergence.** PostgreSQL stores `date` as days since
**2000-01-01** (its `POSTGRES_EPOCH_JDATE`); jed uses **1970-01-01**, the same epoch as its
`timestamp`. This is **invisible** to any query — rendering, comparison, ordering, and (the
deferred) `date - date` all observe the same calendar — because the integer is never exposed
(no `date`↔`int` cast this slice). We **own this representation** (CLAUDE.md §1), and choosing
the Unix epoch lets `date` reuse the timestamp calendar verbatim instead of carrying a second
epoch constant.

**Range.** Finite values occupy `i32::MIN + 1 ..= i32::MAX − 1` — roughly **5 877 550 BC …
5 879 610 AD** around 1970, *wider* than PostgreSQL's `4713 BC … 5874897 AD`. A date PG rejects
as out of range but jed accepts (e.g. `5874898-01-01`) is a **documented divergence** (we own
our range — the timestamp.md §1 precedent), recorded in the oracle-override ledger. A parse
whose day count would fall outside the finite int32 range traps `22008` (§2).

**Infinity sentinels.** The two extreme `int32` values are reserved, matching PostgreSQL's
`DATEVAL_NOBEGIN` / `DATEVAL_NOEND`:

- `i32::MIN` (`-2147483648`) = **`-infinity`**
- `i32::MAX` (`2147483647`) = **`+infinity`**

As with timestamp, infinity costs almost nothing: signed-`int32` comparison already gives
`-infinity < every finite < infinity`; the `int-be-signflip` key encoding sends `i32::MIN` →
all-zero (sorts first) and `i32::MAX` → all-ones (sorts last); the 4-byte on-disk codec stores
them verbatim. So **ordering, key encoding, and storage handle infinity for free** — only parse
and render special-case it. (Like timestamp, and unlike `decimal`, which excludes ±Infinity as
a float artifact — [decimal.md](decimal.md) §2; a date infinity is a genuine, totally-ordered
sentinel.)

`date` is its **own family**: it does **not** compare or cast to `timestamp` /
`timestamptz` / `int` this slice (§5) — `date × timestamp` is `42804`.

## 2. Parsing — a text literal adapting in a date context

A `'…'` literal stays a generic text literal and is parsed in the executor's coercion layer
(resolve time), exactly like `timestamp` and `bytea` — **no lexer/parser change**. The accepted
grammar reuses the *full* timestamp literal grammar ([timestamp.md](timestamp.md) §3), but a
date keeps **only the date portion**: any time and offset are **validated then discarded**
(PostgreSQL behavior — confirmed against the live oracle).

```
input  := special | [era_pre] date [ (' '|'T') time ] [offset] [' ' era]
special := ('+'|'-')? 'infinity'           # case-insensitive
era      := 'BC' | 'AD'                     # case-insensitive
date     := year '-' month '-' day         # year 1..7 digits (magnitude; the int32-day range spans ≈ ±5.88M years)
time     := hour ':' minute [ ':' second [ '.' frac ] ]
offset   := 'Z' | ('+'|'-') HH [ ':' MM [ ':' SS ] ]
```

Rules (all identical to timestamp §3 unless noted):

- **Special values (checked first).** `infinity` / `-infinity` (case-insensitive, optional
  leading `+` on `infinity`) parse directly to the `i32::MAX` / `i32::MIN` sentinels. The
  clock-relative specials (`today`, `tomorrow`, `yesterday`, `now`) and the `epoch` alias are
  **not** accepted this slice (deferred); they trap `22007`.
- **Date is required; time/offset are optional and discarded.** `'2024-01-01'`,
  `'2024-01-01 12:34:56'`, `'2024-01-01T12:34:56.789+05'` all parse to the **same** date
  `2024-01-01`. The time and offset are still **parsed and validated** (a malformed or
  out-of-range time/offset still errors — `'2024-01-01 25:00:00'` traps `22008`), but neither
  affects the day.
- **`24:00:00` does NOT roll into the day.** Exactly `24:00:00` is accepted as a valid
  end-of-day time (any other `24:xx` traps `22008`), but the result is the **date as written** —
  `'2024-01-01 24:00:00'::date` is `2024-01-01`, **not** `2024-01-02`. This is the one place
  date and timestamp diverge in field handling: timestamp normalizes `24:00:00` to next-day
  midnight (the instant carries it), whereas a date takes its day from the `year-month-day`
  fields directly and the discarded time never advances it. (Oracle-confirmed.)
- **Offset is ignored.** Like `timestamp` (zoneless), an offset is parsed/validated but **not
  applied**: `'2024-12-31 23:59:59+14'::date` is `2024-12-31`, never shifted to a neighboring
  day. `Z` = `+00`.
- **Era.** A trailing ` BC` / ` AD` maps the displayed year to the astronomical year via
  `astro = 1 − displayed` for `BC` (so `1 BC` = astronomical `0`). No astronomical year 0 on
  input. Same as timestamp.
- **Field validation.** year magnitude `≥ 1` (capped only as an overflow guard — the real bound
  is the int32 day-range check); month `1–12`; day valid for the month
  including Feb-29 on the astronomical year; hour `0–23` (plus exactly `24:00:00`); minute
  `0–59`; **second `0–59` — `:60` is rejected** (`22008`).
  - **Documented PG divergences (oracle-checked), inherited from timestamp §3:** PostgreSQL
    accepts `:60` and rolls it forward, and accepts DateStyle-dependent / non-ISO spellings
    (`Jan 15, 2024`, `01/15/2024`, `20240115`, scientific forms). jed accepts **only** the
    strict ISO `year-month-day` grammar above and **rejects** `:60` — the same strict, locale-
    free, deterministic posture as timestamp.
- **Day computation.** `day_count = days_from_civil(astro, month, day)` (int64 intermediate),
  range-checked to the finite int32 window; a value beyond it (or onto a sentinel) traps
  `22008`. **No instant is computed**, so a far-future date that would overflow timestamp's
  int64-µs range (e.g. `5000000-06-15`) is still a valid date.

**Errors.** Malformed / unparseable syntax traps **`22007`** (`invalid_datetime_format`); a
syntactically valid but out-of-range field (`month 13`, `day 32`, `:60`, bad `24:xx`,
out-of-range offset), or a day count beyond the representable int32 range, traps **`22008`**
(`datetime_field_overflow`). Parsing happens at **resolve time**, before any scan, so a bad
literal in a `WHERE` predicate traps deterministically *before* row iteration — exactly like
timestamp.

## 3. Rendering — byte-identical canonical text

`render_date()` emits the canonical text with pure integer→string formatting and explicit
zero-padding — **no locale, no platform date formatting** (the ICU cautionary tale, CLAUDE.md
§8). Checked in order:

1. **Infinity** (before any field formatting): `i32::MIN` → `-infinity`, `i32::MAX` →
   `infinity`.
2. Decompose via `civil_from_days`, then emit **`YYYY-MM-DD`** — year zero-padded to **at least
   4 digits** and printed in full when wider (`50000-03-04`), month and day zero-padded to 2.
   There is **no** time, fraction, or offset.
3. **Era / year width.** An astronomical year `≤ 0` renders **BC** with displayed year
   `1 − astro` and a trailing ` BC` (astro `0` → `0001-01-01 BC`).

PostgreSQL's default `DateStyle ISO` output is exactly this `YYYY-MM-DD` form. The BC/AD and
wide-year rows of the corpus are **bootstrapped from the live PG oracle** (CLAUDE.md §7/§12).

## 4. Comparison and ordering

`date × date` compares by the **`int32` day count** ([compare.toml](../types/compare.toml),
`via = "none"`): plain signed numeric order, so `-infinity < every finite < infinity`,
`infinity = infinity` is true, and the order is total (no NaN). NULL is the largest value
(sorts last ascending), three-valued logic throughout — the existing machinery, unchanged.
`infinity IS NULL` is false. `IS [NOT] DISTINCT FROM` is the same value comparison with NULL
treated as a comparable value (always definite).

There is **no** cross-family rule: `date × timestamp`, `date × timestamptz`, `date × int`, and
`date × {text,…}` are all `42804`. **Documented divergence:** PostgreSQL implicitly casts
`date → timestamp` so `date < timestamp` is well-typed; jed keeps `date` a **strict island**
this slice (the float-island and timestamp×timestamptz precedents), deferring the date↔timestamp
coercion to the cast follow-on (§6).

## 5. Literals, casts, keys, cost

- **Literals.** A single-quoted string adapts in a date context (§2) — not a distinct token,
  and not a CAST. With no date context (e.g. `'a' = 'b'` with no date column), a string literal
  stays text and compares as text, exactly like timestamp/bytea today.
- **Keyword-introduced typed literal** (`DATE '…'`, [grammar.md](grammar.md) §36): the
  context-free counterpart — the keyword names the type, so the literal carries `date` in *any*
  expression position (`SELECT DATE '2024-01-01'`). It reuses the existing generic
  `identifier string` typed-literal production (one-token lookahead on a following string, like
  `TIMESTAMP '…'` / `INTERVAL '…'`); the string is parsed by the **same** `parse_date` as §2, so
  the `22007` / `22008` codes and every field rule are identical. jed uses the canonical one-word
  keyword only; a `(` after the name (a typmod) is not a typed literal (no `date(p)`).
- **Casts** ([casts.toml](../types/casts.toml)): **deferred**, exactly like timestamp.
  `CAST(x AS date)`, text↔date, and the date↔timestamp/timestamptz conversions are all later
  work. (The string-literal coercion of §2/§5 is **literal adaptation**, not a `(text, date)`
  CAST.)
- **Key encoding** ([encoding.md](encoding.md) §2.1, the int32 codec): `date` reuses the
  fixed-width `int-be-signflip` integer key encoding (width 4) **verbatim** — and, like
  timestamp (and unlike text/decimal/bytea/interval), it is **exercised** this slice, so a
  `date` PRIMARY KEY is **supported** (the bytes already sort in calendar order, infinities
  included).
- **On-disk value codec** (type code **16**, [format.md](../fileformat/format.md)): the same
  4-byte order-preserving integer body as `int32`, behind the presence tag. Adding the type code
  is **additive** within the current `format_version` (the uuid/timestamp/interval/float
  precedent — a new scalar code does not bump the version); a new `date_table.jed` golden pins
  the bytes cross-core (`rust == go == ts == ruby`).
- **Cost** ([cost.md](cost.md)): a date compare node charges **one** uniform `operator_eval`,
  like integer/timestamp — the `# cost:` contract is unchanged.

## 6. Deferred follow-ons

Scoped out of this slice (each its own future slice + obligations), matching the timestamp/
interval precedent of landing the type before its arithmetic and casts:

- **Date arithmetic** — `date + int` / `date - int` → `date`; `date - date` → `int` (days
  between); `date + interval` / `date - interval` → `timestamp`; `date + time` → `timestamp`.
  This is the interval-arithmetic-sized surface (operator catalog rows + resolver overloads +
  evaluator kernels).
- **Casts** — `text ↔ date`, `date ↔ timestamp` / `timestamptz` (the latter unblocks
  cross-family `date < timestamp` comparison, §4), and `date(p)`-style typmods (there are none).
- **Clock-relative literals** — `today` / `tomorrow` / `yesterday` / `now` / `epoch` (on the
  entropy/clock seam, [entropy.md](entropy.md), like the deferred timestamp `now` literal).
- **Date functions** — `make_date`, `EXTRACT`/`date_part`, `date_trunc`, `current_date`.

## 7. Determinism traps (the cross-core checklist)

1. **Reuse the timestamp calendar** — `days_from_civil` / `civil_from_days` are the *same*
   functions timestamp uses (1970 epoch); do not fork a second copy. The civil↔days path uses
   **truncating** division paired with the Hinnant `-399`/`-146096` adjustment.
2. **Date portion only** — compute the day from `(astro, month, day)` directly; never from an
   instant. `24:00:00` does **not** advance the day, and an offset is **never** applied (the two
   places date diverges from timestamp's field handling).
3. **int32 range, int32 sentinels** — finite is `i32::MIN+1 ..= i32::MAX-1`; `i32::MIN` /
   `i32::MAX` are `-infinity` / `infinity`; a finite parse onto a sentinel, or beyond the int32
   window, traps `22008`. Checked first in both parse and render.
4. **TS day field** — held as `bigint` like every other integer in the TS core (uniform-bigint
   discipline), converted at the int32 encode/decode boundary.
5. **Era mapping** — `BC` ⇒ `astro = 1 − displayed`; render inverts it. No astronomical year 0
   on input. 4-digit zero-pad, full width beyond 9999.
6. **Field validation** — month/day (leap Feb-29)/hour/minute/second ranges; `:60` rejected;
   only exactly `24:00:00` accepted for the hour. All trap `22008`; malformed syntax `22007`.
7. **Resolve-time parse** — a bad literal in `WHERE` traps before any scan, deterministically.
8. **Own family** — a distinct `Value` variant and type code (16); never collapse to the int32
   variant (results render via the value's own `render()`, which needs the type). `date ×
   timestamp` is `42804`.
