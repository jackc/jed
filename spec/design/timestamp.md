# The `timestamp` / `timestamptz` types — design

> The reasoning behind the datetime types. The **data is authoritative**
> ([../types/scalars.toml](../types/scalars.toml) — the types, families, encoding method;
> [../types/compare.toml](../types/compare.toml) — comparability/ordering;
> [../types/casts.toml](../types/casts.toml) — casts (deferred);
> [../functions/catalog.toml](../functions/catalog.toml) — comparison operators;
> [../fileformat/format.md](../fileformat/format.md) — the on-disk value codec + type codes;
> [../encoding/timestamps.toml](../encoding/timestamps.toml) — the parse/render byte vectors).
> This doc is the *why* and the precise calendar/parse/render arithmetic the three cores must
> reproduce **byte-identically** (CLAUDE.md §2, §8). When a decision here changes, change the
> data and here in the same edit, and update [CLAUDE.md](../../CLAUDE.md) §4 if it revises a
> commitment.

`timestamp` (zoneless wall clock) and `timestamptz` (UTC instant) are the datetime types. They
are the first case where **two distinct SQL types share one physical representation** — an
`int64` microsecond instant — differing only in semantics and on-disk type code.

This slice deliberately ships **no time-zone database and no named zones**. The split is the
PostgreSQL *instant* model, not the SQL-standard *offset-bearing* model:

- **`timestamp`** = a zoneless wall clock. No zone, ever; never converted.
- **`timestamptz`** = an absolute UTC instant. Input text **may** carry a numeric offset
  (`Z`, `±HH`, `±HH:MM`, `±HH:MM:SS`), which normalizes the value to UTC and is then
  **discarded** — only the UTC instant is stored, no offset retained.

Any zone-aware behavior beyond offset-on-input belongs to the host (native value projection at
the embedding boundary) or to a much later slice introducing a host-supplied, version-pinned
tz database as an explicit, named execution input. Keeping zones out of the engine keeps it
fully deterministic and byte-identical across cores (CLAUDE.md §8/§13) with zero external data
dependency. The non-goal is wire/`pg_catalog` fidelity (CLAUDE.md §1); the goal is PG's
*observable* datetime behavior on the surface we implement.

## 1. Representation — int64 microseconds since the Unix epoch

A value is an **`int64` count of microseconds** since `1970-01-01 00:00:00`, proleptic
Gregorian, **no leap seconds** (a day is always 86 400 s). `timestamp` measures the wall
clock; `timestamptz` measures UTC. Microsecond resolution matches PG; `int64` µs spans
≈ ±292 277 years around 1970, far more than the calendar needs.

We **own this range** — there is no need to match PG's exact 4713 BC … 294276 AD bounds (which
fall out of PG's 2000-01-01 epoch). The binding constraint is the `int64` itself, checked on
input (§2).

**Infinity sentinels.** The two extreme `int64` values are reserved:

- `i64::MIN` (`-9223372036854775808`) = **`-infinity`**
- `i64::MAX` (`9223372036854775807`) = **`+infinity`**

Finite values therefore occupy `i64::MIN + 1 ..= i64::MAX − 1`; a finite parse that would land
exactly on a sentinel traps `22008` (§2). This is PG's `DT_NOBEGIN`/`DT_NOEND` scheme, and it
is the reason infinity costs almost nothing here: signed-`int64` comparison already gives
`-infinity < every finite < infinity`; the `int-be-signflip` key encoding already sends
`i64::MIN` → all-zero (sorts first) and `i64::MAX` → all-ones (sorts last); the 8-byte on-disk
codec stores them verbatim. So **ordering, key encoding, and storage handle infinity for
free** — only parse and render special-case it.

This is a deliberate, documented **difference from `decimal`**, which *excludes* NaN/±Infinity
([decimal.md](decimal.md) §2): there, ±Infinity is a binary-float artifact with no source in an
exact type. A timestamp infinity is the opposite — a genuine, useful, **totally-ordered**
sentinel (open-ended ranges; "valid forever"), and there is no NaN to break the total order. So
timestamps *include* infinity while decimal does not, and both choices serve the same master:
keep the comparison/ordering path total and deterministic.

The two types are **distinct families** that do **not** compare or cast to each other — that
would require a zone. `timestamp × timestamptz` is a `42804` type error
([compare.toml](../types/compare.toml)).

## 2. Calendar arithmetic — the determinism hotspot

Civil ↔ instant conversion must be **byte-identical across cores**, so it is pinned here as
integer pseudocode (Howard Hinnant's `days_from_civil`/`civil_from_days`, the algorithm behind
C++20 `<chrono>`): branch-light pure integer math, valid for the whole astronomical year range,
no library dependency (CLAUDE.md §14). All `/` and `%` below on the *civil* path are **truncating**
where the inputs are non-negative; the **instant decomposition** path uses **floor (Euclidean)**
division/modulo — called out explicitly, because Rust/Go `%` are truncating and would corrupt
pre-1970 / BC values.

```
days_from_civil(y, m, d):                 # proleptic Gregorian → days since 1970-01-01 (=0)
    y  -= (m <= 2)
    era = (y >= 0 ? y : y-399) / 400       # truncating int division
    yoe = y - era*400                      # [0, 399]
    doy = (153*(m + (m > 2 ? -3 : 9)) + 2)/5 + (d-1)
    doe = yoe*365 + yoe/4 - yoe/100 + doy
    return era*146097 + doe - 719468       # 719468 shifts the epoch to 1970-01-01

civil_from_days(z):                        # inverse
    z  += 719468
    era = (z >= 0 ? z : z-146096) / 146097
    doe = z - era*146097                    # [0, 146096]
    yoe = (doe - doe/1460 + doe/36524 - doe/146096) / 365   # [0, 399]
    y   = yoe + era*400
    doy = doe - (365*yoe + yoe/4 - yoe/100) # [0, 365]
    mp  = (5*doy + 2)/153                   # [0, 11]
    d   = doy - (153*mp+2)/5 + 1            # [1, 31]
    m   = mp + (mp < 10 ? 3 : -9)           # [1, 12]
    return (y + (m <= 2 ? 1 : 0), m, d)
```

```
micros_from_civil(y,mo,d,h,mi,s,us):       # CHECKED; trap 22008 on i64 overflow OR sentinel hit
    days = days_from_civil(y, mo, d)
    secs = days*86400 + h*3600 + mi*60 + s
    t    = secs*1_000_000 + us             # us already includes any rounding (§3)
    if t == i64::MIN or t == i64::MAX: trap 22008   # reserved for ±infinity
    return t

civil_from_micros(t):                      # FLOOR (Euclidean) division/modulo throughout
    us   = floor_mod(t, 1_000_000)         # always 0..999_999
    secs = floor_div(t, 1_000_000)
    sod  = floor_mod(secs, 86400)          # 0..86399
    days = floor_div(secs, 86400)
    (y,mo,d) = civil_from_days(days)
    return (y, mo, d, sod/3600, (sod%3600)/60, sod%60, us)
```

Per core: Rust `i64::div_euclid`/`rem_euclid`; Go a small floor-div/mod helper; TS does **all µs
math in `bigint`** (`number` loses precision past 2⁵³, and JS `%` truncates), with a `bigint`
floor helper. `micros_from_civil` uses **checked** multiply/add and traps `22008` on `int64`
overflow for extreme years or if a finite result would equal a reserved sentinel.

## 3. Parsing — a text literal adapting in a timestamp context

A `'…'` literal stays a generic text literal and is parsed in the executor's coercion layer
(resolve time), exactly like `bytea` — **no lexer/parser change**. The accepted grammar and
every field validation are pinned here and by [timestamps.toml](../encoding/timestamps.toml).

```
input  := special
        | [era_pre] date [ (' '|'T') time ] [offset] [' ' era]
special := ('+'|'-')? 'infinity'           # case-insensitive
era      := 'BC' | 'AD'                     # case-insensitive
date     := year '-' month '-' day         # year 1..6 digits (magnitude)
time     := hour ':' minute [ ':' second [ '.' frac ] ]
offset   := 'Z' | ('+'|'-') HH [ ':' MM [ ':' SS ] ]
```

Rules:

- **Special values (checked first).** `infinity` and `-infinity` (case-insensitive, optional
  leading `+` on `infinity`) parse directly to the `i64::MAX` / `i64::MIN` sentinels — no date,
  offset, or era. The **clock-relative** specials (`now`, `today`, `tomorrow`, `yesterday`) and
  the `epoch` / `allballs` aliases are **not** accepted this slice (deferred with `now()`); they
  trap `22007`.
- **Date-only** input → time defaults to `00:00:00`. Both `' '` and `'T'` separators are
  accepted. Surrounding whitespace is trimmed; interior spacing is strict.
- **Era.** A trailing ` BC` / ` AD` is accepted (PG). `BC` maps the displayed year to the
  astronomical year via **`astro = 1 − displayed`** (so `1 BC` = astronomical year `0`,
  `2 BC` = `−1`). `AD` / no era = the displayed year directly. The year token is a magnitude;
  the era sets the sign convention. There is **no astronomical year 0** on input (it is spelled
  `1 BC`).
- **Fractional seconds.** 0–6 digits are taken exactly. **7+ digits are rounded to µs, half
  away from zero** — the engine's one rounding mode ([decimal.md](decimal.md) §3). Compute the
  floor instant for `(y,mo,d,h,mi,s)` with `us = 0`, then add the rounded sub-second µs
  (`0 … 1_000_000`). A rounding result of exactly `1_000_000` simply advances the absolute
  instant by one second — **no special carry code**, because the arithmetic is in absolute µs
  (e.g. `23:59:59.9999996` → next-day `00:00:00`). The rounded sub-second is always a
  non-negative forward fraction, so "half away from zero" is unambiguous for pre-1970 / BC
  instants too.
  - **Documented PG divergence (oracle-checked).** PostgreSQL rounds the timestamp fraction
    **half-to-even** ("banker's rounding") — it parses the fraction through a `double` and
    `rint`, so e.g. PG renders `…00.1234565` as `.123456` (to even) where jed renders
    `.123457` (away). jed keeps its **single exact-integer half-away mode** rather than (a) add
    a second rounding mode just for timestamps, breaking the one-mode invariant, and (b) chase
    a `double`-based result it could not reproduce *deterministically* across cores anyway
    (no float in the value path — CLAUDE.md §8). The divergence appears only on exact sub-µs
    ties; every non-tie input rounds identically to PG.
- **Offset.** For **`timestamptz`**, an offset normalizes to UTC as `utc = wallclock − offset`;
  an absent offset means UTC. For **`timestamp`** (zoneless), an offset is **accepted and
  ignored** — parsed and validated for syntax, but not applied (PG behavior). `Z` = `+00`.
- **`24:00:00`** is accepted (PG) and normalized to next-day `00:00:00`, but **only** exactly
  `24:00:00` with zero minutes/seconds and no fraction; any other `24:xx` traps `22008`.
- **Field validation.** month `1–12`; day valid for the month including the Feb-29 leap rule
  (`y%4==0 && (y%100!=0 || y%400==0)`, on the astronomical year); hour `0–23` (plus the
  `24:00:00` special); minute `0–59`; **second `0–59` — `:60` is rejected (`22008`)**.
  - **Documented PG divergence (oracle-checked).** PostgreSQL accepts *exactly* `:60` (with an
    optional fraction) and **rolls it to the next minute**, cascading through minute/hour/day
    (`12:00:60` → `12:01:00`; `23:59:60` → next-day `00:00:00`). This is lenient overflow
    normalization, **not** leap-second storage. jed **rejects** `:60` for strict-typing
    simplicity — a documented divergence. `:61`, minute `60`, and hour `25` are rejected by both
    engines.

**Errors.** Malformed / unparseable syntax traps **`22007`** (`invalid_datetime_format`).
A syntactically valid but out-of-range field (`month 13`, `day 32`, `:60`, bad `24:xx`), or a
value beyond the representable `int64`-µs range (or onto a sentinel), traps **`22008`**
(`datetime_field_overflow`). Parsing happens at **resolve time**, before any scan, so a bad
literal in a `WHERE` predicate traps deterministically *before* row iteration — mirroring
`bytea`'s `decode_bytea_literal`.

## 4. Rendering — byte-identical canonical text

`Value::render()` emits the canonical text with pure integer→string formatting and explicit
zero-padding — **no locale, no platform date formatting** (the ICU-collation cautionary tale,
CLAUDE.md §8). Checked in order:

1. **Infinity** (before any field formatting): `i64::MIN` → `-infinity`, `i64::MAX` →
   `infinity`. The `timestamptz` `+00` suffix is **not** appended to an infinity.
2. Decompose via `civil_from_micros`, then emit `YYYY-MM-DD HH:MM:SS` (space separator).
   The fractional part is appended **only when the µs field is nonzero**, then trailing zeros
   are trimmed: `2024-01-01 12:00:00`, `…12:00:00.5`, `…12:00:00.001`, `…12:00:00.123456`.
3. **`timestamptz`** appends a fixed **`+00`** suffix (always UTC, whole-hour minimal form):
   `2024-01-01 12:00:00+00`.
4. **Era / year width.** An astronomical year `≤ 0` renders **BC** with displayed year
   `1 − astro` and a trailing ` BC` (astro `0` → `0001-… BC`); years are zero-padded to **at
   least 4 digits** and printed in full when wider (`10000-01-01`). The ` BC` suffix follows the
   whole datetime — after the `+00` for `timestamptz` (`… 00:00:00+00 BC`).

Because BC/AD and wide-year forms are easy to get subtly wrong, the corpus rows for them are
**bootstrapped from the live PG `db` oracle** (CLAUDE.md §7/§12) and pinned as goldens.

## 5. Comparison and ordering

`timestamp × timestamp` and `timestamptz × timestamptz` compare by the **`int64` instant**
([compare.toml](../types/compare.toml), `via = "none"`): plain signed numeric order, so
`-infinity < every finite < infinity`, `infinity = infinity` is true, and ordering is total
(no NaN). NULL is the largest value (sorts last ascending), three-valued logic throughout — the
existing machinery, unchanged. `infinity IS NULL` is false (infinity is a present, non-null
value). There is **no** cross rule: `timestamp × timestamptz` and datetime × any other family
are `42804`. `IS [NOT] DISTINCT FROM` is the same value comparison with NULL treated as a
comparable value (always definite).

## 6. Literals, casts, keys, cost

- **Literals.** A single-quoted string adapts in a timestamp context (§3) — not a distinct
  token, and not a CAST. With no timestamp context (e.g. `'a' = 'b'` with no datetime column),
  a string literal stays text and compares as text, exactly like `bytea` today.
- **Casts** ([casts.toml](../types/casts.toml)): **deferred**. `CAST(x AS timestamp)`,
  text↔timestamp, and the zone-requiring timestamp↔timestamptz conversion are all later work;
  the latter needs a zone and so never reconciles the two families here.
- **Key encoding** ([encoding.md](../design/encoding.md) §2.1, the `int64` codec): both types
  reuse the fixed-width `int-be-signflip` integer key encoding **verbatim** — and unlike
  text/bytea/decimal it is **exercised** this slice, so a timestamp / timestamptz `PRIMARY KEY`
  is **supported** (the bytes already sort in instant order, infinities included).
- **On-disk value codec** (type codes **8** / **9**, [format.md](../fileformat/format.md)): the
  same 8-byte integer body behind the presence tag.
- **Cost** ([cost.md](cost.md)): a datetime compare node charges **one** uniform
  `operator_eval`, like integer/text — the `# cost:` contract is unchanged.

## 7. Determinism traps (the cross-core checklist)

1. **Floor vs truncating division** — `civil_from_micros` uses **floor (Euclidean)** div/mod;
   a core that uses truncating `%` corrupts every pre-1970 / BC instant. Pin with negative-µs
   vectors.
2. **TS µs math** — all in `bigint`, never `number` (precision past 2⁵³); `bigint` floor helper.
3. **Sub-second rounding** — 7+ digits round half-away-from-zero to µs; the carry into
   seconds/day/year falls out of absolute-µs arithmetic (`23:59:59.9999996` → next day). Use the
   exact-integer half-away test (decimal.md §3), never a float.
4. **Infinity sentinels** — `i64::MIN`/`i64::MAX` are `-infinity`/`infinity`; a finite parse onto
   a sentinel traps `22008`. Checked first in both parse and render; no `+00` on an infinity.
5. **Offset discipline** — `timestamptz` subtracts the offset to UTC then discards it;
   `timestamp` accepts-and-ignores an offset (does not apply it). `Z` = `+00`.
6. **Era mapping** — `BC` ⇒ `astro = 1 − displayed`; render inverts it. No astronomical year 0
   on input. 4-digit zero-pad, full width beyond 9999.
7. **Field validation** — month/day (leap Feb-29)/hour/minute/second ranges; `:60` rejected;
   only exactly `24:00:00` normalizes. All trap `22008`; malformed syntax `22007`.
8. **Render trim** — fractional digits appended only when nonzero, trailing zeros trimmed;
   identical zero-padding of every field across cores.
9. **Two types, one representation** — distinct `Value` variants and type codes (8 vs 9); never
   collapse to the integer variant (results render via `Value::render()`, which needs the type).
   `timestamp × timestamptz` is `42804`.
10. **Resolve-time parse** — a bad literal in `WHERE` traps before any scan, deterministically,
    like `bytea`.
