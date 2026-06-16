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
> **Status: AF1 landed.** The **polymorphic resolution machinery** (§2) plus the
> *scalar-function-shaped* surface — introspection (`array_ndims`, `array_length`, `array_lower`,
> `array_upper`, `cardinality`, `array_dims`) and builders (`array_append`, `array_prepend`,
> `array_cat`) — are implemented across all three cores, oracle-checked
> (`suites/expr/array_functions.test`), capability `func.array`. The remaining slices (§6) —
> `||`, `unnest`, `@>`/`<@`/`&&`, `ANY`/`ALL`, `VARIADIC` — are sequenced follow-ons, each its
> own vertical slice.

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

## 6. Delivery (sub-slices)

The surface lands as ordered, independently-shippable slices, each passing `rake ci` with
oracle-checked conformance — mirroring array.md S0–S5 and composite S0–S6.

- **AF1 ✅** — the §2 polymorphic machinery + the §3 scalar-function surface (`array_ndims`,
  `array_length`, `array_lower`, `array_upper`, `cardinality`, `array_dims`, `array_append`,
  `array_prepend`, `array_cat`). No grammar change. All three cores + `func.array` capability +
  `suites/expr/array_functions.test`.
- **AF2** — the **`||` concatenation operator** (array∥array, element∥array, array∥element — a new
  catalog operator with precedence + polymorphic operator dispatch) and the search/edit functions
  `array_remove`, `array_replace`, `array_position`, `array_positions`.
- **AF3** — **`unnest(anyarray)`** the set-returning function: generalizes the `generate_series`
  SRF machinery ([functions.md §10](functions.md)) to a **polymorphic element-type** output column
  and a per-element row generator (FROM-clause position first; the SELECT-list SRF, `LATERAL`, and
  the `WITH ORDINALITY` form are their own follow-ons).
- **AF4** — the containment/overlap operators **`@>`** (contains), **`<@`** (contained by),
  **`&&`** (overlaps), as polymorphic `boolean`-result operators.
- **AF5** — the **`ANY`/`ALL`** quantified comparisons (`x = ANY(array)`, `x op ALL(array)`) — a
  grammar + resolver + evaluator slice reusing the `IN`-list 3VL membership machinery.
- **AF6** — **`VARIADIC`** call syntax + variadic overload resolution (the `make_interval`-era
  follow-on, [functions.md §11](functions.md), unblocked by the array type).

## 7. Errors

| Failure | Code |
|---|---|
| No array-function overload matches the argument types (incl. element-type conflict `array_cat(int32[], text[])`, or a non-array where `anyarray` is required) | `42883` undefined_function |
| `array_append`/`array_prepend` on a multidimensional array | `22000` data_exception |
| `array_cat` of incompatible dimensionalities | `2202E` array_subscript_error |
| Polymorphic type undeterminable (all polymorphic args untyped `NULL`) | `42P18` indeterminate_datatype |

`22000` (`data_exception`) is registered in [../errors/registry.toml](../errors/registry.toml)
(added with AF1); `2202E`, `42883`, `42P18` already existed.
