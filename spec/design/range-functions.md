# Range function & operator surface

> The function/operator surface over the six range types (`spec/design/ranges.md` is the type
> axis — value model, codec, comparison). Delivered in slices **RF1–RF4** (ranges.md §11).
> Every behavior here is oracle-pinned against `postgres:18`. This doc grows one section per
> slice; **RF1 (the accessor functions) is the only one landed so far.**

The polymorphic machinery is shared with the array function surface (`array-functions.md` §2):
one type variable **ELEM**, bound from the polymorphic argument slots and read back into the
reserved result codes. Ranges add a third pseudo-family, **`anyrange`**, alongside `anyarray`
and `anyelement`.

- **`anyrange`** (arg slot) — matches any range argument and binds `ELEM :=` its element type
  (the same definitive binding `anyarray` does, resolved in the same pass, *before*
  `anyelement`). A non-range where `anyrange` is required, or an element-type conflict, is
  `42883` (`undefined_function` — no matching overload), exactly as the array surface reports.
- **`anyrange`** (result code) — `ELEM`-range, i.e. `Range(ELEM)` (used by the RF2 constructors
  and the RF4 set operators; `42P18` if ELEM is undeterminable because every polymorphic
  argument was an untyped `NULL`).
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

## 2. Constructor functions (RF2) — *deferred*

`i32range(lo,hi[,bounds])` and the five siblings (+ the PG `int4range`/`int8range` aliases).

## 3. Boolean operators (RF3) — *deferred*

`@>` `<@` `&&` `<<` `>>` `&<` `&>` `-|-`.

## 4. Set operators (RF4) — *deferred*

`+` (union) `*` (intersection) `-` (difference) and `range_merge`.
