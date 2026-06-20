# Range function & operator surface

> The function/operator surface over the six range types (`spec/design/ranges.md` is the type
> axis — value model, codec, comparison). Delivered in slices **RF1–RF4** (ranges.md §11).
> Every behavior here is oracle-pinned against `postgres:18`. This doc grows one section per
> slice; **RF1 (accessors), RF2 (constructors), RF3 (the boolean operators), and RF4 (the set
> operators + `range_merge`) have all landed — the range function/operator surface is complete.**

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

## 3. Boolean operators (RF3)

The eight PostgreSQL range boolean operators, each returning a **definite boolean** (a range total
order, never composite's 3VL — like the comparison operators of ranges.md §6). All are **STRICT**:
a SQL-`NULL` operand yields `NULL`. Catalog rows: `spec/functions/catalog.toml`
(`kind = "containment"`, sharing `||`'s precedence 37).

| Operator | Overloads | Meaning |
|---|---|---|
| `@>` | `(anyrange, anyrange)`, `(anyrange, anyelement)` | `a` contains range / element `b` |
| `<@` | `(anyrange, anyrange)`, `(anyelement, anyrange)` | range / element `a` contained by `b` |
| `&&` | `(anyrange, anyrange)` | `a` and `b` overlap (share a point) |
| `<<` | `(anyrange, anyrange)` | `a` is strictly left of `b` |
| `>>` | `(anyrange, anyrange)` | `a` is strictly right of `b` |
| `&<` | `(anyrange, anyrange)` | `a` does not extend to the right of `b` |
| `&>` | `(anyrange, anyrange)` | `a` does not extend to the left of `b` |
| `-\|-` | `(anyrange, anyrange)` | `a` and `b` are adjacent |

**Two axes, one dispatch.** `@>`/`<@`/`&&` are **shared with the array containment surface**
(`array-functions.md` §10). The hand-written resolver (`resolve_set_op`) resolves both operands and
dispatches by type: an **array** operand → the array axis; a **range** operand → the range axis
(`resolve_range_op`). The five positional/adjacency operators are **range-only** (a non-range pair
is `42883`). A range operand pairs only with a range over the **same element type** — a cross-element
pair (`int4range @> int8range`) is `42883` (`operator does not exist`), matching PG. The downstream
array GIN-scan planner (`gin_match`) is untouched: it keys off `ArrayFunc` nodes over a GIN-indexed
*array* column, and a range operand produces a `RangeOp` node (ranges are not GIN-indexable), so it
never mis-fires.

**Element overloads.** `@>` and `<@` accept a bare **element** on the non-range side (`range @> 5`,
`5 <@ range`). The element is resolved with the range's element type as the literal-adaptation hint
and coerced assignment-style (reusing RF2's machinery), so `int4range(1,10) @> 5` adapts `5` to `i32`
and `numrange(1,5) @> 5.0` compares decimals. A non-assignable element (a string for an integer
range) is `42883`.

**Empty-range edges (oracle-pinned).** The empty range **contains nothing** and is **contained by
everything** (`'empty' @> r` is false; `r @> 'empty'` is true); it **overlaps nothing**; and it is
**neither strictly-left/right of nor adjacent to** anything (the positional/adjacency operators
return `false` whenever either operand is empty). These fall out of the kernels' explicit empty
guards.

**Adjacency.** Over the canonical representation, `a -|- b` reduces to "`a`'s upper bound value
equals `b`'s lower bound value with **exactly one inclusive**, or vice versa" — the discrete `[)`
canonicalization already folded the integer/date step into the bounds, so no separate discrete path
is needed (`[1,5) -|- [5,9)` is adjacent; `[1,5] -|- [5,9)` overlaps so is not; `[1,5) -|- (5,9)`
has a gap so is not).

**New tokens (lexer).** `<<` `>>` `&<` `&>` `-|-` are added to the lexer, each scanned greedily.
`-|-` is checked **before** the `--` line comment (its middle `|` keeps the two disjoint, but the
order is explicit). jed has **no integer bit-shift**, so `<<`/`>>` are range-only — `5 << 2` is
`42883` (`operator does not exist`), a documented divergence from PG (which computes `20`).

**Per core.** New `BinaryOp` variants (`StrictlyLeft`/`StrictlyRight`/`NotExtendRight`/
`NotExtendLeft`/`Adjacent`) parsed at the `parse_concat` rung; `resolve_set_op` (array-vs-range
dispatch) + `resolve_range_op` (the range axis); a `RangeOp` node carrying the operator and the
range's element scalar (for the element overloads' eval coercion); the eight boolean kernels in
`range.rs` (`range_contains`/`range_contains_elem`/`range_overlaps`/`range_before`/`range_after`/
`range_overleft`/`range_overright`/`range_adjacent`) over a general `cmp_bounds` (PG
`range_cmp_bounds` generalized to compare a lower against an upper). No on-disk change (RF3 is a
query surface only).

## 4. Set operators (RF4)

The three PostgreSQL range set operators plus the `range_merge` function. Each combines two ranges
over a **common element type** into a new range (`anyrange <op> anyrange → anyrange`). All are
**STRICT** (a SQL-`NULL` operand → `NULL`). The operators **reuse the arithmetic tokens** `+`/`*`/`-`
— there is **no new grammar**; a `+`/`-`/`*` with a range operand is dispatched to the range axis at
resolve. Catalog rows: `spec/functions/catalog.toml` (the three operators are `kind = "arithmetic"`
overloads keyed on `anyrange`, like the `@>` array/range overloads in §3; `range_merge` is a
`kind = "function"` row).

| Operator / function | Result | Meaning |
|---|---|---|
| `a + b` | `anyrange` | **union** — the smallest single range covering both; `22000` if the result would not be contiguous |
| `a * b` | `anyrange` | **intersection** — the overlap of `a` and `b`; `empty` when they are disjoint |
| `a - b` | `anyrange` | **difference** — the part of `a` not covered by `b`; `22000` if `b` splits `a` in two |
| `range_merge(a, b)` | `anyrange` | like `+` but **spans any gap** between the ranges silently (never errors) |

**Element/type match.** Both operands must be ranges over the **same** element type. A range paired
with a non-range (`int4range + 5`), or a cross-element pair (`int4range + int8range`), is **`42883`**
(`operator does not exist`, matching PG). A bare untyped `NULL` beside a range is taken as a NULL
range (the eval is STRICT, so the result is `NULL`).

**Contiguity errors (oracle-pinned).** `+` (union) raises **`22000`** ("result of range union would
not be contiguous") when the two ranges neither overlap nor are adjacent — the union would span a gap
and is not a single range. `-` (difference) raises **`22000`** ("result of range difference would not
be contiguous") when `b` lies strictly inside `a` (`a.lower < b.lower` **and** `a.upper > b.upper`),
which would leave two disjoint pieces. `*` (intersection) and `range_merge` **never** error.

**Empty-range edges (oracle-pinned).** Union and `range_merge`: an empty operand yields the other
unchanged (`empty + r = r`; both empty → `empty`). Intersection: a disjoint, merely-adjacent, or
either-empty pair → `empty`. Difference: an empty subtrahend (or a `b` disjoint from `a`) yields `a`
unchanged; a `b` wholly covering `a` yields `empty`.

**`range_merge` is union, relaxed.** PG implements `range_merge(a, b)` as `range_union(a, b, strict =
false)` — the same union kernel without the contiguity check. So `range_merge(int4range(1,5),
int4range(10,20))` is `[1,20)` (spanning the gap), where `int4range(1,5) + int4range(10,20)` is
`22000`.

**Per core.** No grammar change and no new lexer token. The dispatch is at the top of
`resolve_binary`'s `Add | Sub | Mul` arm: once the operands are resolved, a range operand routes to
`resolve_range_set_op` (which checks the common-element rule and builds the result `Range` type), and
the numeric/temporal arithmetic below never sees a range. `range_merge` is a function call, gated by
`is_range_func_name` (it has `anyrange` arg families) and special-cased in `resolve_range_func` to emit
the **same** `RExpr::RangeSetOp` node with `op = Merge` (rather than a scalar-accessor `RangeFunc`).
The three set kernels (`range_union` with a `strict` flag, `range_intersect`, `range_minus`) live in
`range.rs`, built over the same `cmp_bound`/`cmp_bounds` bound comparison as the boolean operators
(§3) plus a small `make_range` helper (PG `make_range`, minus the discrete canonicalize step the
operands already satisfy — the result bounds are taken from already-canonical operand bounds, so
discrete results stay canonical `[)` by construction). No on-disk change (RF4 is a query surface
only).
