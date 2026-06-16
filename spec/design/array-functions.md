# Array function / operator surface — design

> The array **function/operator surface** — `array_length`/`cardinality`/`unnest`/`||`/`@>`/
> `&&`/… — and the **polymorphic `anyarray`/`anyelement` resolution** that the whole surface
> needs. This is the deferred `0A000` follow-on listed in [array.md §12](array.md) and
> [TODO.md](../../TODO.md). The array **type** (`T[]`: codec, comparison, subscript, slices,
> multidim) landed in array.md S0–S5; this doc is the **function layer over that type**. The
> catalog is authoritative ([../functions/catalog.toml](../functions/catalog.toml)); this doc is
> the *why*. PostgreSQL semantics are the default (CLAUDE.md §1), pinned against the live
> `postgres:18` oracle — several array-function NULL/shape rules are subtle (§3) and must be
> oracle-checked, not guessed.
>
> **Status: AF1 + AF2 + AF3 landed.** AF1 — the **polymorphic resolution machinery** (§2) plus the
> *scalar-function-shaped* surface: introspection (`array_ndims`, `array_length`, `array_lower`,
> `array_upper`, `cardinality`, `array_dims`) and builders (`array_append`, `array_prepend`,
> `array_cat`). AF2 (§8) — the **`||` concatenation operator** and the **search/edit functions**
> (`array_remove`, `array_replace`, `array_position`, `array_positions`). AF3 (§9) — the
> **`unnest(anyarray)` set-returning function**, the engine's second FROM-clause SRF: it expands an
> array into one row per element at the bound element type. All three are implemented across all
> three cores, oracle-checked (`suites/expr/array_functions.test`,
> `suites/expr/array_concat_search.test`, `suites/query/unnest.test`), capabilities `func.array` +
> `func.unnest`. The remaining slices (§6) — `@>`/`<@`/`&&`, `ANY`/`ALL`, `VARIADIC` — are sequenced
> follow-ons, each its own vertical slice.

## 1. Why a new layer

The array *type* is a finished structural type — `Type::Array(Box<Type>)`, a compact value
codec, btree-NULL comparison, subscripting and slices ([array.md](array.md)). What it lacks is
the **function surface** every real array workload needs: introspect a value's shape
(`array_length`), grow it (`array_append`, `||`), search it (`@>`, `array_position`), and explode
it into rows (`unnest`). PostgreSQL exposes ~30 array functions plus the `||`/`@>`/`<@`/`&&`
operators and the `= ANY(array)` / `op ALL(array)` quantifiers.

The thing standing between jed and all of that is **polymorphism**. Every one of these functions
is declared in PostgreSQL over the pseudo-types `anyarray`/`anyelement`: `array_append(anyarray,
anyelement) → anyarray`, `unnest(anyarray) → setof anyelement`. jed's catalog resolution
([functions.md §4](functions.md)) is **exact-family-match** — a slot names a concrete `family`
from [../types/scalars.toml](../types/scalars.toml) (`integer`, `text`, …) and an argument matches
iff its family equals the slot. Arrays are not a `family` (they are a *structural* type whose
identity is "array of E"), so the catalog cannot even *name* an array-accepting function, let
alone tie a result type to an argument's element type. **AF1 builds that missing machinery** and
then spends it on the simplest, highest-value functions; the rest reuse it.

## 2. Polymorphic resolution — `anyarray` / `anyelement`

Two new **pseudo-families** are admissible in a `[[operator]]` row's `arg_families`, and two new
reserved `result` codes:

| token | as `arg_families` slot | as `result` |
|---|---|---|
| `anyarray` | matches any **array** argument; binds `ELEM := its element type` | the bound array type `ELEM[]` |
| `anyelement` | matches any argument; binds/checks `ELEM := its type` | the bound element type `ELEM` |

These are **not** real families — they are deliberately **absent from scalars.toml** (they are
not storable types, have no id, no codec). They are *catalog contract tokens* interpreted by the
**hand-written resolver** per core (CLAUDE.md §5 — the dispatch is hand-written, only the registry
data is shared). `verify.rb` admits them in `arg_families`/`result` as a small allowlist and
otherwise leaves them alone.

**The unification algorithm** (one shared shape, hand-written in each core's
`lookup_scalar_overload`). For a candidate overload whose `arg_families` contains a polymorphic
slot, against the call's resolved argument types `arg_tys`:

1. `elem := None` (the type variable `ELEM`).
2. **Pass A — `anyarray` slots.** For each slot `i` that is `anyarray`: if `arg_tys[i]` is an
   array type `E[]`, `unify(elem, E)`; if it is the `NULL` type (a bare untyped `NULL`), defer
   (contributes no binding); otherwise this overload **does not match**.
3. **Pass B — `anyelement` slots.** For each slot `i` that is `anyelement`: if `arg_tys[i]` is
   `NULL`, defer; else `unify(elem, arg_tys[i])`. (Two passes because `anyelement` can appear
   *before* its binding `anyarray` — `array_prepend(anyelement, anyarray)`.)
4. **Pass C — concrete slots.** Every non-polymorphic slot matches by the existing
   `family_matches` (e.g. `array_length`'s second `integer` slot).
5. `unify(elem, X)`: if `elem` is unbound, set it to `X`; if already bound, require structural
   type equality (`int32 == int32`, `text == text`, composite by catalog ref) — a conflict
   (`array_cat(int32[], text[])`) makes the overload **not match**.

If no overload matches, the call is `42883 undefined_function` (jed is strict — no implicit
inter-family coercion, exactly as elsewhere; PG agrees here, e.g. `array_cat(int[], text[])` is
`42883`). The matched overload's `result` is then computed from the binding: `anyarray →
Array(elem)`, `anyelement → elem`. If `elem` is still unbound at this point — every polymorphic
argument was an untyped `NULL` (`array_append(NULL, NULL)`) — the type is undeterminable and the
call is **`42P18 indeterminate_datatype`** (a documented jed posture; PG happens to default the
unknowns to `text`, but jed's literals are strictly typed so this only arises for a degenerate
bare-`NULL` pair and is never in the oracle corpus).

A bare untyped `NULL` argument, once the overload is chosen, is **coerced to the resolved
polymorphic type** (its `NULL` value carries the bound element/array type forward), so
`array_append(xs, NULL)` appends a typed-NULL element.

**Literal adaptation (two-pass resolution).** jed types an undecorated integer literal as `int64`
(CLAUDE.md §8 / types.md §6), where PostgreSQL types a small one as `int4`. So `array_append(xs,
40)` over an `int32[]` column would, naively, see element `40` as `int64` and reject the overload.
jed resolves this with its standard **literal adaptation**, applied in two passes: pass 1 resolves
the arguments with no hint to discover the array's element type; if that element is a scalar, pass
2 **re-resolves the polymorphic-slot arguments with it as the expected-type hint**, so a bare
integer/decimal literal element (or the elements of an untyped `ARRAY[…]` constructor argument)
adapts — with a range check — to the array's element type. `array_append(int32[], 40)` lands on
`int32` (`array_append(int32[], 5000000000)` is `22003`), and `array_cat(int32[], ARRAY[7,8])`
adapts the constructor to `int32[]`. The hint is the element type of the **first** `anyarray`
argument; the concrete `integer` dimension slot of `array_length`/`lower`/`upper` keeps its pass-1
resolution. (To make this honor a context hint, the `ARRAY[…]` constructor now threads its `ctx`
down to its elements — previously every caller passed `None`, so 1-D inference is unchanged.)

**Result-type plumbing.** `lookup_scalar_overload` now returns the matched descriptor **and the
bound `elem`**; `scalar_result_type` gains the two reserved codes and consults `elem`. This is the
one structural change to the resolution path; everything downstream (the per-row evaluator, the
`# types:` contract, cost) is unchanged.

## 3. AF1 functions — the scalar-function-shaped surface

All nine are ordinary `kind = "function"` rows ([functions.md §9](functions.md)): called by name,
evaluated **per row**, valid anywhere an expression is. They need **no grammar change** — the
`function_call` production already parses them — so AF1 is purely (a) the §2 machinery + (b) nine
value kernels. Every rule below is **oracle-pinned** (`postgres:18`).

| function | signature | result | NULL discipline |
|---|---|---|---|
| `array_ndims` | `(anyarray)` | `int32` | `propagates` |
| `array_length` | `(anyarray, integer)` | `int32` | `propagates` |
| `array_lower` | `(anyarray, integer)` | `int32` | `propagates` |
| `array_upper` | `(anyarray, integer)` | `int32` | `propagates` |
| `cardinality` | `(anyarray)` | `int32` | `propagates` |
| `array_dims` | `(anyarray)` | `text` | `propagates` |
| `array_append` | `(anyarray, anyelement)` | `anyarray` | `none` (non-strict) |
| `array_prepend` | `(anyelement, anyarray)` | `anyarray` | `none` (non-strict) |
| `array_cat` | `(anyarray, anyarray)` | `anyarray` | `none` (non-strict) |

### 3.1 Introspection (`array_ndims`/`length`/`lower`/`upper`/`cardinality`/`dims`)

Pure readers of the value's shape header (`ndim`/`dims`/`lbounds` — array.md §4). They
**`propagate` NULL** (a `NULL` array, or a `NULL` dimension argument, yields `NULL`), and beyond
that return `NULL` for an out-of-shape request (PG; oracle-pinned):

- `array_ndims(a)` — the dimension count; **`NULL` for the empty array `{}`** (`ndim 0`).
- `cardinality(a)` — the **total** element count (product of `dims`); **`0` for `{}`** (not
  `NULL` — the one introspector that distinguishes empty from absent).
- `array_length(a, d)` — the length of dimension `d`; `NULL` if `a` is empty or `d ∉ [1, ndim]`
  (so `array_length('{1,2,3}', 2)` and `array_length('{1,2,3}', 0)` are `NULL`).
- `array_lower(a, d)` / `array_upper(a, d)` — dimension `d`'s lower / upper bound; `NULL` if `a`
  is empty or `d` out of range. (`('[2:4]={7,8,9}')` → `array_lower 2`, `array_upper 4`.)
- `array_dims(a) → text` — the bound spec `[l1:u1][l2:u2]…` (no trailing `=`, unlike `array_out`'s
  prefix); `NULL` for `{}`. `'{10,20,30}'` → `[1:3]`; a 2×3 value → `[1:2][1:3]`.

The dimension argument's family is **`integer`** (jed's integer family — int16/int32/int64). PG's
is strictly `int4`, so jed accepts `array_length(xs, 1::int64)` where PG raises `42883`; a **minor
documented divergence** (§5), invisible to the corpus (integer literals resolve fine). The result
is always **`int32`** (PG's `integer`), regardless of the array's element type.

### 3.2 Builders (`array_append`/`array_prepend`/`array_cat`)

These return a new array (`result = anyarray`) and are **non-strict** — the catalog `null`
discipline `none` means the resolver does **not** short-circuit a `NULL` argument; the kernel
handles `NULL` itself (PG; oracle-pinned). This is the surface's one genuinely new NULL rule:

- `array_append(a, e)`: if `a` is **`NULL` or empty**, the result is the 1-D singleton `{e}`
  (lower bound 1). Otherwise `a` must be **empty or 1-dimensional** — a multidimensional `a` is
  **`22000`** (PG *"argument must be empty or one-dimensional array"*). Otherwise the result is
  `a`'s elements followed by `e`, the lower bound **preserved** and the upper bound grown by one
  (`'[2:4]={7,8,9}'` ∥ `10` → `'[2:5]={7,8,9,10}'`). A `NULL` element `e` is appended as a real
  NULL element (`{1,2}` ∥ `NULL` → `{1,2,NULL}`).
- `array_prepend(e, a)`: the mirror — `e` is placed first, the lower bound **preserved**, upper
  grown by one (`array_prepend(0, '{1,2,3}')` → `'{0,1,2,3}'` lb 1; `array_prepend(6,
  '[2:4]={7,8,9}')` → `'[2:5]={6,7,8,9}'`). Multidimensional `a` → `22000`.
- `array_cat(a, b)`: identity-aware concatenation. `NULL`/empty acts as the identity (`a` NULL →
  `b`; `b` NULL → `a`; **both NULL → `NULL`**; `a` empty → `b`; `b` empty → `a`). Otherwise it
  concatenates along the **outer** dimension, matching PG `array_cat`'s three cases on
  `(ndims_a, ndims_b)`:
  - **equal** `N == N`: outer length = `a.dim[0] + b.dim[0]`; the **inner** dims `dims[1..]` must
    be equal or it is **`2202E`** (PG *"cannot concatenate incompatible arrays"*); result lower
    bounds = `a`'s.
  - **off-by-one** `N == M+1`: the `(M)`-D operand is one outer slice of the `(N)`-D one; its dims
    must equal the other's `dims[1..]` (`2202E` otherwise); the outer length grows by one; result
    lower bounds = the **higher-dimensional** operand's.
  - **otherwise** (`|N−M| > 1`): `2202E`.

  In every case the flattened element list is simply `a.elements ++ b.elements` (row-major,
  outer-first), so a 2×2 ∥ 1-D-of-2 appends a row.

### 3.3 Cost

No new cost units. Each AF1 call is one interior expression node → one `operator_eval`
([cost.md](cost.md), [functions.md §9](functions.md)); the per-element walk inside a kernel is
unmetered, exactly as the array constructor / subscript / comparator walks are (array.md §9). The
arguments charge their own `operator_eval`s recursively. Deterministic and cross-core-identical
(CLAUDE.md §13), asserted with `# cost:` in the corpus.

## 4. The `null = "none"` discipline

The catalog `null` field gains a fifth value, **`none`**: "the operator/function is **not**
NULL-strict; the kernel inspects NULL-ness itself and the resolver must **not** auto-return NULL
on a NULL argument." It is the data-shaped expression of the non-strict builders (§3.2). At
resolve, a function's row sets a per-node `propagate_null` flag (`true` for every existing
discipline, `false` for `none`); the per-row evaluator skips its blanket NULL short-circuit when
the flag is clear and calls the kernel with the raw (possibly NULL) argument values. `propagates`
is unchanged for the six introspectors. (This is distinct from the comparison-only `null_safe`:
`null_safe` is a *comparison* result rule; `none` says *the resolver does no NULL handling at
all*.)

## 5. Ratified decisions & deliberate PostgreSQL divergences

Default is "match PostgreSQL" (CLAUDE.md §1); each divergence is recorded here and, when its
corpus lands, in [../conformance/oracle_overrides.toml](../conformance/oracle_overrides.toml).

1. **Polymorphic resolution by unification, not a coercion lattice** — jed binds one type
   variable `ELEM` by structural equality and rejects conflicts as `42883`; it does **not**
   reproduce PG's `select_common_type` coercion search (which would, e.g., unify `int[]` with
   `bigint` element). This is the same strictness jed already applies to every overload
   (CLAUDE.md §4) and observably matches PG on the in-corpus cases.
2. **`anyarray`/`anyelement` are catalog tokens, not scalars** — absent from scalars.toml; the
   resolver interprets them. (Contrast a real family, which the generic `family_matches` handles.)
3. **The dimension argument is the `integer` family, not strict `int4`** — `array_length(xs,
   1::int64)` is accepted (PG: `42883`). A minor divergence following jed's existing integer-family
   permissiveness; invisible to the literal-only corpus.
4. **Non-strict builders** (`array_append`/`prepend`/`cat` `null = "none"`) — a NULL array
   argument is the identity / empty, not a propagated NULL. PG; oracle-pinned; the surface's one
   new NULL rule.
5. **`array_append`/`prepend` reject a multidimensional array `22000`**; **`array_cat` dim
   mismatch is `2202E`** — both match PG's codes exactly (PG uses the bare class-22 `22000`
   *data_exception* for the former).
6. **All-untyped-`NULL` polymorphic call is `42P18`** — `array_append(NULL, NULL)` cannot
   determine `ELEM`; jed raises indeterminate-datatype rather than PG's default-to-`text`. A
   degenerate case, never oracle-checked.
7. **A bare untyped `NULL` in a concrete slot is `42883`, not coerced** — `array_length(xs, NULL)`
   is `42883` (jed's existing strictness — `round(5, NULL)` is likewise `42883`); the cast form
   `array_length(xs, NULL::int4)` resolves and propagates to `NULL`. A divergence from PG (which
   types the bare `NULL` as `int4`), inherited from jed's untyped-NULL posture, not new to AF1.
8. **Element unification adapts literals to the *first* `anyarray` argument** — the literal-adaptation
   hint (above) comes from the first array argument. The realistic shapes (`array_append(col, lit)`,
   `array_cat(col, ARRAY[…])`, `array_cat(col, col)`, `array_cat(ARRAY[…], ARRAY[…])`) all resolve;
   the one shape that does not is an untyped `ARRAY[…]` *first* with a typed array of a different
   (narrower) element type second (`array_cat(ARRAY[1,2], int32[]_col)` → `42883`), where PG would
   find a common type. jed is strict here (the same posture as its array comparison, which requires
   element-family compatibility); put the typed/column array first.
9. **`||` is array-only — text `||` and `int || int` are deferred** (AF2, §8). jed has no text
   concatenation operator and no implicit element→text cast yet, so `'a' || 'b'` and `1 || 2` are
   `42883` (PG would return `'ab'` / `'12'`). The `||` overloads this slice are exactly the three
   array forms; text `||` lands with the string-function surface (types.md §11).
10. **Search/edit comparison is IS NOT DISTINCT FROM** (AF2, §8) — `array_remove`/`array_replace`/
    `array_position`/`array_positions` match an element with NULL-safe equality (jed reuses its total
    element comparator, `value_cmp == Equal`), so a NULL target finds/removes/replaces NULL elements
    and a non-NULL target never matches a NULL element. This is PG's behavior (oracle-pinned), and
    for jed's element types (int/text/…) the total comparator and PG's per-type btree equality agree.
11. **`array_remove`/`array_position`/`array_positions` are 1-D only** (AF2, §8) — a multidimensional
    array argument is `0A000` (PG *"removing/searching … in multidimensional arrays is not
    supported"*), matching PG's code exactly. `array_replace` works on **any** dimensionality (it
    substitutes element-wise, preserving shape). The `array_position` *subscript* result and the
    optional `start` argument are in the array's own lower-bound space (so `array_position('[5:7]=
    {10,20,30}', 20)` is `6`, not `2`); a NULL `start` is `22004`, not a NULL result.

## 6. Delivery (sub-slices)

The surface lands as ordered, independently-shippable slices, each passing `rake ci` with
oracle-checked conformance — mirroring array.md S0–S5 and composite S0–S6.

- **AF1 ✅** — the §2 polymorphic machinery + the §3 scalar-function surface (`array_ndims`,
  `array_length`, `array_lower`, `array_upper`, `cardinality`, `array_dims`, `array_append`,
  `array_prepend`, `array_cat`). No grammar change. All three cores + `func.array` capability +
  `suites/expr/array_functions.test`.
- **AF2 ✅** (§8) — the **`||` concatenation operator** (array∥array, element∥array, array∥element —
  a new operator `kind = "concat"` with precedence 37 + polymorphic operator dispatch that reuses
  the AF1 builder kernels) and the search/edit functions `array_remove`, `array_replace`,
  `array_position`, `array_positions`. One grammar change (the `||` token + a `parse_concat`
  precedence rung); all three cores + `suites/expr/array_concat_search.test`.
- **AF3 ✅** (§9) — **`unnest(anyarray)`** the set-returning function: generalizes the
  `generate_series` SRF machinery ([functions.md §10](functions.md)) to a **polymorphic element-type**
  output column and a per-element row generator. All three cores + `func.unnest` +
  `suites/query/unnest.test`. (FROM-clause position only; the SELECT-list SRF, `LATERAL`, and the
  `WITH ORDINALITY` form remain their own follow-ons.)
- **AF4** — the containment/overlap operators **`@>`** (contains), **`<@`** (contained by),
  **`&&`** (overlaps), as polymorphic `boolean`-result operators.
- **AF5** — the **`ANY`/`ALL`** quantified comparisons (`x = ANY(array)`, `x op ALL(array)`) — a
  grammar + resolver + evaluator slice reusing the `IN`-list 3VL membership machinery.
- **AF6** — **`VARIADIC`** call syntax + variadic overload resolution (the `make_interval`-era
  follow-on, [functions.md §11](functions.md), unblocked by the array type).

## 7. Errors

| Failure | Code |
|---|---|
| No array-function/`\|\|`-overload matches the argument types (incl. element-type conflict `array_cat(int32[], text[])`, a non-array where `anyarray` is required, or text/`int\|\|int` `\|\|`) | `42883` undefined_function |
| `array_append`/`array_prepend` / `array \|\| element` on a multidimensional array | `22000` data_exception |
| `array_cat` / `array \|\| array` of incompatible dimensionalities | `2202E` array_subscript_error |
| `array_remove`/`array_position`/`array_positions` on a multidimensional array (AF2) | `0A000` feature_not_supported |
| `array_position(a, e, start)` with a NULL `start` (AF2) | `22004` null_value_not_allowed |
| `unnest` of a non-array, or with the wrong arity (AF3) | `42883` undefined_function |
| Polymorphic type undeterminable (all polymorphic args untyped `NULL`, incl. bare `unnest(NULL)`) | `42P18` indeterminate_datatype |

`22000` (`data_exception`) is registered in [../errors/registry.toml](../errors/registry.toml)
(added with AF1); `22004` (`null_value_not_allowed`) was added with AF2; `0A000`, `2202E`, `42883`,
`42P18` already existed.

## 8. AF2 — the `||` operator and the search/edit functions

AF2 spends the §2 machinery on the rest of the *expression-position* array surface: the **`||`
concatenation operator** and four **search/edit functions**. Every rule is oracle-pinned
(`postgres:18`). Like AF1 the functions are ordinary `kind = "function"` rows reached through
`resolve_array_func`; the `||` operator is a new `kind = "concat"` with its own precedence and a
hand-written `resolve_concat`.

### 8.1 The `||` concatenation operator

`||` is the **operator spelling of the AF1 builders** — PostgreSQL defines it as three operator
declarations backed by `array_cat` / `array_append` / `array_prepend`, and jed does the same. It is
the surface's one grammar change:

- **Lexer** — a new `||` token (two `|` scanned greedily, like `::`/`=>`); a lone `|` stays a
  `42601` syntax error (jed has no bitwise-or).
- **Precedence** — a new `parse_concat` rung between the comparison level (35) and the additive
  level (40), so `precedence = 37`. This matches PostgreSQL's "any other operator" rung: `||` binds
  **tighter than the comparisons** (`a || b = c` is `(a || b) = c`) and **looser than `+`/`-`**, and
  is **left-associative** (`a || b || c` is `(a || b) || c`). The comparison/`IN`/`BETWEEN`/`LIKE`
  operands all parse at the concat level, so `||` is available inside them.
- **AST + dispatch** — one new `BinaryOp::Concat` node; `resolve_binary` routes it to
  `resolve_concat`.

**`resolve_concat`** is overload resolution over the three `concat` catalog rows, tried **in catalog
order — `(anyarray,anyarray)` [cat], `(anyarray,anyelement)` [append], `(anyelement,anyarray)`
[prepend]** — taking the first whose slots unify (§2 `match_poly`). It:

1. resolves both operands with no hint;
2. computes the element hint from the **first operand that is an array** and re-resolves the
   **non-NULL** operands with it (so `col || 40` adapts `40` to the element type, and `col ||
   ARRAY[…]` adapts the constructor) — a **bare untyped `NULL` operand is deliberately left
   un-adapted** (see below);
3. tries the three overloads in order; the first match selects the kernel (`array_cat` /
   `array_append` / `array_prepend`, called with the operands in source order) and the result type
   (`anyarray` → `ELEM[]`). No match is `42883`.

**Cat-first ordering is load-bearing.** `match_poly` *defers* a bare untyped `NULL` in an `anyarray`
slot (it binds nothing), so `arr || NULL` and `NULL || arr` match the **cat** overload first and
become `array_cat(arr, NULL) = arr` (the NULL array is the identity) — exactly PostgreSQL. A
**typed** null element (`arr || NULL::int32`) is a concrete `anyelement`, never defers, so cat
fails and **append** is chosen → `{…,NULL}`. Leaving the bare NULL un-adapted in step 2 is what
preserves this: adapting it to a typed null would wrongly steer `arr || NULL` into append. (This is
the one place `resolve_concat` differs from `resolve_array_func`, whose overload is *fixed*, so it
adapts the bare NULL into the element slot — `array_append(arr, NULL)` is `{…,NULL}`.)

### 8.2 The search/edit functions

| function | signature | result | notes |
|---|---|---|---|
| `array_remove` | `(anyarray, anyelement)` | `anyarray` | drop every element NOT DISTINCT FROM `e`; **1-D/empty only** (multi-D `0A000`); lower bound preserved |
| `array_replace` | `(anyarray, anyelement, anyelement)` | `anyarray` | substitute every element NOT DISTINCT FROM `from` with `to`; **any dimensionality**, shape preserved |
| `array_position` | `(anyarray, anyelement [, integer])` | `int32` | first match's **subscript** (lower-bound space), else `NULL`; **1-D/empty only** (`0A000`); optional `start` subscript, NULL `start` is `22004` |
| `array_positions` | `(anyarray, anyelement)` | `int32[]` | the `int32[]` of **every** match's subscript (empty `{}` if none); **1-D/empty only** (`0A000`) |

All four are **non-strict** (`null = "none"`): a `NULL` array argument yields `NULL` (or, for
`array_position`/`array_positions`, `NULL`), but a `NULL` search/replacement element is a real
comparable value — matched with the NULL-safe comparator (§5 #10), so it finds/removes/replaces
`NULL` elements. The element is adapted to the array's element type by the same literal-adaptation
hint AF1 uses, so `array_remove(int32[]_col, 2)` adapts `2`.

`array_position`/`array_positions` return jed's `int32` / `int32[]` (PostgreSQL's `integer` /
`integer[]`) regardless of the array's element type, matching the §3.1 introspection convention.
`int32[]` is a new concrete-array `result` code (`<scalar>[]`), read by the resolver as
`Array(scalar)` and admitted by `verify.rb` as a fourth `result` form.

## 9. AF3 — the `unnest(anyarray)` set-returning function

AF3 spends the §2 machinery on the array surface's one **set-returning** function — `unnest`, which
**expands** an array into one row per element. Unlike every prior AF function (a per-row scalar
expression), `unnest` produces *rows*, so it is jed's second **SRF** after `generate_series` and
reuses that machinery wholesale ([functions.md §10](functions.md), [grammar.md §35](grammar.md)): a
FROM-clause row source that resolves to a **synthetic one-column relation**, threads through the
planner and nested-loop join unchanged, and **generates** its rows at the materialization step
instead of scanning a store. Every rule is oracle-pinned (`postgres:18`).

**The one new piece is the polymorphic output column.** `generate_series` types its column at the
*promoted integer* of its args (the `set_of_promoted` result); `unnest` types its column at the
**element type of its `anyarray` argument** — a new reserved SRF result code, **`set_of_element`**,
the SRF analogue of the `anyelement` code (§2). Resolution (hand-written per core in
`resolve_srf`/`resolveSRF`, dispatched by name like the `generate_series` branch):

1. arity is **1** — a wrong count is `42883` (the multi-array `unnest(a, b, …)` PG form is deferred).
2. resolve the single argument with **no hint** (the argument *is* the array; there is no element to
   adapt a literal toward, unlike the AF1/AF2 literal-adaptation hint). Its type must be an
   **array** `E[]` → bind `ELEM := E`, the output column type. A **non-array** (`unnest(5)`) is
   `42883` (no `anyarray` overload — exactly PG). A **bare untyped `NULL`** (`unnest(NULL)`) leaves
   `ELEM` undeterminable → **`42P18`** (jed's polymorphic posture, §5 #6 — PG instead reports
   "function unnest(unknown) is not unique" because it ships several `unnest` overloads; jed has one,
   so the indeterminate case is the honest answer and is **out of the oracle corpus**). A **typed**
   NULL array (`NULL::int32[]`) resolves and yields zero rows at exec.
3. arrays hold **scalar** elements this slice (array-of-composite is the deferred fast-follow,
   [array.md §12](array.md)), so `ELEM` is always a scalar; the synthetic column carries it
   (`int32[]` → an `int32` column, `text[]` → `text`, …).

**The generator** (`unnest_rows`/`unnestRows`) evaluates the single array argument **once** against
the params/outer environment (non-LATERAL, exactly like `generate_series`'s args — a `$N` or a
**correlated outer column** is a legal argument, a **sibling FROM table is not**), then emits one
row per element in the value's **flattened row-major element order**. The semantics fall straight
out of the array value's representation ([array.md §4](array.md)) and are oracle-pinned:

- a **NULL array** argument → **zero rows** (the SRF `empty_on_null` discipline);
- the **empty array** `{}` → **zero rows**;
- a **multidimensional** array → its elements in **row-major order** (`unnest(ARRAY[[1,2],[3,4]])` →
  `1,2,3,4`) — `unnest` flattens, it does not preserve shape;
- a **custom lower bound** is **dropped** (`unnest('[5:7]={10,20,30}')` → `10,20,30`) — `unnest`
  yields *elements*, not subscripts (contrast `array_positions`, §8.2);
- a **NULL element** of a non-NULL array is produced as a **NULL row** (`unnest(ARRAY[1,NULL,3])` →
  `1`, `NULL`, `3`).

Row order without an `ORDER BY` is **unspecified** (CLAUDE.md §8) — the conformance harness compares
the multiset (`rowsort`); PostgreSQL happens to preserve element order, and jed does too, but neither
is contractual. `unnest` composes with `WHERE` / `ORDER BY` / `LIMIT` / cross-join exactly as
`generate_series` does, and the output column follows the same **single-column function-alias rule**
(the alias, else `unnest`).

**Cost.** Each produced element charges one **`generated_row`** ([cost.md §3](cost.md)) — the same
unit `generate_series` charges, guarded so a `max_cost` ceiling aborts a runaway `unnest` (`54P01`)
**mid-generation** before the whole array materializes (CLAUDE.md §13). An SRF touches no store, so it
charges **no** `page_read` / `storage_row_read`; the array argument charges its own evaluation cost
(one `operator_eval` per `ARRAY[…]` constructor node, **zero** for a constant-folded `'{…}'::T[]`
literal) **once**, up front.

**Catalog & registry.** `unnest` is a `[[set_returning]]` row (`arg_families = ["anyarray"]`,
`arg_resolution = "none"`, `result = "set_of_element"`, `column = "unnest"`, `null =
"empty_on_null"`); `verify.rb` admits the polymorphic `anyarray` in an SRF `arg_families` slot and
the new `set_of_element` reserved result. The row is shared registry data (CLAUDE.md §5) cross-checked
into each core's `SET_RETURNING` table by `gen_catalog.rb`; the resolve/generate **dispatch** is
hand-written per core. **Deferred** (each its own follow-on, §6): the SELECT-list SRF position
(`SELECT unnest(…)` is `42883`, like `generate_series`), `LATERAL` (so the natural "explode a column"
`FROM t, unnest(t.xs)` is reached only via a correlated subquery this slice), `WITH ORDINALITY`, the
multi-array `unnest(a, b, …)` form, and array-of-composite elements.
