# Range function & operator surface

> The function/operator surface over the six range types (`spec/design/ranges.md` is the type
> axis — value model, codec, comparison). Delivered in slices **RF1–RF4** (ranges.md §11).
> Every behavior here is oracle-pinned against `postgres:18`. This doc grows one section per
> slice; **RF1 (the accessor functions) and RF2 (the constructor functions) have landed; RF3–RF4
> (the operator surface) are deferred.**

The polymorphic machinery is shared with the array function surface (`array-functions.md` §2):
one type variable **ELEM**, bound from the polymorphic argument slots and read back into the
reserved result codes. Ranges add a third pseudo-family, **`anyrange`**, alongside `anyarray`
and `anyelement`.

- **`anyrange`** (arg slot) — matches any range argument and binds `ELEM :=` its element type
  (the same definitive binding `anyarray` does, resolved in the same pass, *before*
  `anyelement`). A non-range where `anyrange` is required, or an element-type conflict, is
  `42883` (`undefined_function` — no matching overload), exactly as the array surface reports.
- **`anyrange`** (result code) — `ELEM`-range, i.e. `Range(ELEM)` (used by the RF4 set operators
  and `range_merge`; `42P18` if ELEM is undeterminable because every polymorphic argument was an
  untyped `NULL`). The RF2 **constructors** do *not* use this code — each carries a **concrete**
  range id (`i32range`, …) in its result, since the constructor name already fixes the type.
- **`anyelement`** (result code) — reads `ELEM` back (the `lower`/`upper` bound value type).

These are catalog **contract tokens** interpreted by the hand-written resolver (CLAUDE.md §5),
not real families in `scalars.toml`. `spec/functions/verify.rb` admits them in
`arg_families`/`result` via the `POLYMORPHIC_FAMILIES` / `RESERVED_RESULTS` allowlist.

## 1. Accessor functions (RF1)

Seven **pure, STRICT** readers of a range value's parts. `null = "propagates"`: a SQL-`NULL`
range yields `NULL`. None can error (no `errors`). Catalog rows: `spec/functions/catalog.toml`
(`kind = "function"`, `arg_families = ["anyrange"]`).

| Function | Result | Returns |
|---|---|---|
| `lower(anyrange)` | `anyelement` (= ELEM) | the lower bound value; **`NULL`** if the range is empty or unbounded below |
| `upper(anyrange)` | `anyelement` (= ELEM) | the upper bound value; **`NULL`** if the range is empty or unbounded above |
| `isempty(anyrange)` | `boolean` | is this the empty range |
| `lower_inc(anyrange)` | `boolean` | is the lower bound inclusive (`[`) |
| `upper_inc(anyrange)` | `boolean` | is the upper bound inclusive (`]`) |
| `lower_inf(anyrange)` | `boolean` | is the lower bound infinite/unbounded |
| `upper_inf(anyrange)` | `boolean` | is the upper bound infinite/unbounded |

**Edge cases (oracle-pinned).** For the **empty** range: `isempty` is `true`, and *every other*
reader is `false` (the booleans) or `NULL` (`lower`/`upper`). For an **infinite** bound:
`lower`/`upper` is `NULL`, the matching `*_inf` is `true`, and the matching `*_inc` is `false`
(an infinite bound is never inclusive). These follow directly from the canonical value
representation — the empty range stores both bounds `None` with both inclusivity flags `false`,
and an infinite bound stores `None` with its `_inc` flag `false` — so a reader of the stored
flag already yields PG's answer, **except** `lower_inf`/`upper_inf`, which must guard `empty`
first (the empty range is *not* infinite on either side).

Worked examples (`int4range`, canonical `[)`):

| value | `lower` | `upper` | `isempty` | `lower_inc` | `upper_inc` | `lower_inf` | `upper_inf` |
|---|---|---|---|---|---|---|---|
| `[1,5)` | `1` | `5` | f | t | f | f | f |
| `empty` | `NULL` | `NULL` | t | f | f | f | f |
| `(,5)` | `NULL` | `5` | f | f | f | t | f |
| `[1,)` | `1` | `NULL` | f | t | f | f | t |
| `(,)` | `NULL` | `NULL` | f | f | f | t | t |

**Divergence — no string `lower`/`upper`.** PostgreSQL overloads `lower(text)`/`upper(text)`
(case folding). jed has no string case-folding functions yet, so `lower`/`upper` are
**range-only** here; `lower('abc')` is `42883`, not a lowercased string. This is a documented
divergence (jed owns its surface, CLAUDE.md §1); if string `lower`/`upper` land later, the
resolver gates by operand type, exactly as PG does.

**Per core.** The resolver is `resolve_range_func` (parallel to `resolve_array_func`), gated by
`is_range_func_name` (any `kind="function"` catalog row mentioning `anyrange`) and wired into
`resolve_func_call` after the array gate. The `anyrange` binding pass lives in `match_poly`
(before `anyelement`) and the `anyrange` result arm in `poly_result_type`. The eval kernel
(`eval_range_func` / `RangeFunc`) reads the operand range value — hand-written per core,
identical by construction (the value is self-describing). No on-disk change (RF1 is a query
surface only).

## 2. Constructor functions (RF2)

A **concrete-result builder per range type** — `i32range`/`i64range`/`numrange`/`tsrange`/
`tstzrange`/`daterange`, each with a **2-arg `(lo, hi)`** form and a **3-arg `(lo, hi, bounds)`**
form. The `bounds` argument is a 2-character flags TEXT — `'[]'`, `'[)'`, `'(]'`, `'()'` — naming
the lower/upper inclusivity; the 2-arg form defaults it to **`'[)'`** (PG's default). PostgreSQL's
`int4range`/`int8range` spellings are accepted as call names too, as **aliases** of `i32range`/
`i64range` (resolved through the RANGES alias table, not separate catalog rows). Catalog rows:
`spec/functions/catalog.toml` (`kind = "function"`, `result = "<id>range"` — the **concrete**
range id, the `make_interval` precedent of a fixed result rather than a polymorphic ELEM).

| Function | Result | Builds |
|---|---|---|
| `i32range(i32, i32 [, text])` / `int4range(…)` | `i32range` | range of i32 |
| `i64range(i64, i64 [, text])` / `int8range(…)` | `i64range` | range of i64 |
| `numrange(decimal, decimal [, text])` | `numrange` | range of decimal |
| `tsrange(timestamp, timestamp [, text])` | `tsrange` | range of timestamp |
| `tstzrange(timestamptz, timestamptz [, text])` | `tstzrange` | range of timestamptz |
| `daterange(date, date [, text])` | `daterange` | range of date |

**Bounds + NULL (`null = "none"`, non-strict).** Each constructor is **not** strict on its bound
arguments: a **NULL bound is an infinite/unbounded bound**, not a NULL result — `i32range(NULL, 5)`
→ `(,5)`, `i32range(1, NULL)` → `[1,)`, `i32range(NULL, NULL)` → `(,)`. The kernel handles NULL
itself; the resolver does not short-circuit it. The one exception is the **3-arg `bounds` flags**:
a NULL flags argument traps **`22000`** ("range constructor flags argument must not be null"), and
an invalid flags string (anything but the four 2-char forms) traps **`42601`** ("invalid range
bound flags").

**Bound coercion (assignment-style).** Each bound is coerced to the range's element type with the
same discipline as a column store (the element scalar is offered as the literal-adaptation context,
so a bare integer/decimal literal or a string-literal datetime adapts): an integer bound is
range-checked into the element width (`22003` on overflow), an `int → decimal` bound widens, a
`text → timestamp/timestamptz/date` bound parses (`22007`/`22008` on malformed input), and a bound
whose family is not assignable to the element is **`42883`** (no matching overload). This mirrors
jed's INSERT/UPDATE assignment coercion, so it is **more permissive than PostgreSQL's strict
function-argument matching** in two corner cases (documented divergences, not exercised by the
oracle corpus): jed accepts a *wider* integer for a narrower range — `int4range(1::i64, 5)` builds
`[1,5)` (range-checked) where PG rejects the `int4range(bigint, …)` overload — and jed does **not**
accept an unknown string literal for an integer/decimal bound (`int4range('1', 5)` is `42883`,
where PG coerces the unknown literal to integer). Both follow from reusing jed's assignment model;
the canonical spellings (`int4range(1, 5)`, etc.) behave identically to PG.

**Finalization (shared with the cast path).** A constructed range is run through the same
`finalize` kernel as `'[1,5)'::i32range` (ranges.md §4): the order check (`lower > upper` →
`22000`), discrete canonicalization to `[)` (`(x` → `[x+1`, `x]` → `x+1)`, trapping `22003` if the
`+1` steps past the element domain), and empty normalization (equal bounds not both-inclusive →
`empty`). So `int4range(5, 5)` → `empty`, `int4range(1, 5, '[]')` → `[1,6)`,
`numrange(1, 2)` → `[1,2)` (the integer bounds widen to decimal), and
`daterange('2024-01-01', '2024-02-01', '[]')` → `[2024-01-01,2024-02-02)`.

**Per core.** A hand-written `resolve_range_ctor` (parallel to `resolve_range_func`), gated by
`is_range_ctor_name` (a call whose name is a range type name or alias — `range_by_name(name)`
resolves), wired into `resolve_func_call` *after* the range-accessor gate and *before* the generic
scalar gate (the constructor names are `kind = "function"`, so they must be intercepted before the
family-matched scalar path, exactly as the array/range functions are). The resolver reads the
target `RangeDesc` from the call name, resolves each bound with the element scalar as the context
hint, type-checks assignability, and emits a `RangeCtor { elem, args }` node. The eval kernel
(`eval_range_ctor`) coerces each bound to the element (the store-value path), reads the bounds
flags (default `[)`, or the parsed 3-arg text), and calls `finalize`. No on-disk change (RF2 is a
query surface only).

## 3. Boolean operators (RF3) — *deferred*

`@>` `<@` `&&` `<<` `>>` `&<` `&>` `-|-`.

## 4. Set operators (RF4) — *deferred*

`+` (union) `*` (intersection) `-` (difference) and `range_merge`.
