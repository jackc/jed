# Implicit argument coercion — design

> Status: **landed in all three cores** (slices 1–4 of §9): the compare.toml cost table, the pass
> over built-in and host functions, the catalog cleanup, and the capture check. The resolver
> consistency cleanup (§9 slice 5) is an optional later refactor. The decisions are in §8: integer
> arguments resolve to `decimal` (§6.3), the table lives in compare.toml (§3), and host functions
> take part (§4.2). Corpus: `expr/arg_coercion.test` (capability `func.arg_coercion`); host
> behavior: each core's host-function tests.

## 1. Problem

The generic scalar-function path ([functions.md §9](functions.md)) matches each argument to its
row's declared family **exactly**. Matching is by family, so within a family it is already free:
`abs(i16)` and `abs(i64)` both match `abs(integer)`, and `f32` matches a `float` slot. What it never
did, before this pass, was cross a family boundary. So:

| Call | jed before the pass | PostgreSQL |
|---|---|---|
| `sqrt(2)`, `ln(2)`, `exp(1)` | `42883` | `float8` value |
| `sign(2)`, `scale(5)` | `42883` | `float8` / `int4` value |
| `sin(1)`, `cbrt(27)`, `to_timestamp(1700000000)` | `42883` | `float8` / `timestamptz` value |
| `power(2, 10)` | `42883` | `float8` value |
| `round(5)`, `ceil(5)` | `5` (`numeric`), via jed-only integer overload rows | `5` (`float8`) |
| `power(2.0, 3)` | `numeric`, via `arg_resolution = "promote"` | `numeric` |

Two workarounds each covered part of this:

- **Stand-in overload rows.** `round`, `ceil`, `ceiling`, `floor`, `trunc` carried an `(integer)`
  row returning `numeric` that PostgreSQL does not have, only to make `round(5)` work.
- **Per-row `arg_resolution = "promote"`** on six two-argument rows (`power`/`pow`/`log` over
  `decimal`, `power`/`pow`/`atan2` over `float`). It reconciled a mixed numeric pair, and nothing
  else.

Every new numeric function had to pick one of these by hand, or leave the integer call as `42883`.
(Range constructors such as `numrange(1, 10)` were never affected: they resolve in their own
resolver, [range-functions.md](range-functions.md).)

## 2. Goal and non-goals

**Goal.** One resolution step, driven by shared data, that resolves a call matching no row exactly
by the cheapest admissible argument conversions. It is modeled on DuckDB's binder: every implicit
conversion has a cost, the candidate with the lowest total wins, and a tie is an error.

**Non-goals.**

- **No new conversions.** The pass uses only conversions jed already performs implicitly
  elsewhere: widening within the integer and float towers and the `integer → decimal` promotion
  ([casts.toml](../types/casts.toml) `mode = "implicit"`, [compare.toml](../types/compare.toml)),
  and a numeric literal's adaptation to its context ([types.md](types.md) §6, [float.md](float.md)
  §6). A float *value* still never meets an integer or decimal value (the float island), and text
  never becomes a number or date.
- **No PostgreSQL preferred-type heuristics.** PostgreSQL ends overload resolution with category
  preferences (`float8` among numerics, `text` among strings). [functions.md §9.1](functions.md)
  already declines them for unknown-typed arguments. This pass declines them too: a tie is `42725`.
- **No change to operators.** Infix operators keep the compare.toml promotion rules.
- **No change to an exact match.** A call that resolved by an exact row, a host function, or §9.1
  resolves the same way. The pass runs only where the answer used to be `42883`.

## 3. Data: the argument-coercion table

The `[[arg_coercion]]` section of [compare.toml](../types/compare.toml), next to the promotion and
comparability rules it extends. Costs are per **type**, because host-function slots name exact types
(§4.2). Its rows, in file order (comments abridged):

```toml
# widening within a tower (value)
[[arg_coercion]]
from = "i16"
to   = "i32"
cost = 1

[[arg_coercion]]
from = "i32"
to   = "i64"
cost = 1

[[arg_coercion]]
from = "i16"
to   = "i64"
cost = 2

[[arg_coercion]]
from = "f32"
to   = "f64"
cost = 1

# an integer constant narrower than its i64 default, admitted only when the value fits
[[arg_coercion]]
from = "integer_literal"
to   = "i32"
cost = 1

[[arg_coercion]]
from = "integer_literal"
to   = "i16"
cost = 2

# integer -> decimal (value)
[[arg_coercion]]
from = "i16"
to   = "decimal"
cost = 3

[[arg_coercion]]
from = "i32"
to   = "decimal"
cost = 3

[[arg_coercion]]
from = "i64"
to   = "decimal"
cost = 3

# a numeric constant into the float island (literal only)
[[arg_coercion]]
from = "integer_literal"
to   = "f64"
cost = 4

[[arg_coercion]]
from = "integer_literal"
to   = "f32"
cost = 5

[[arg_coercion]]
from = "decimal_literal"
to   = "f64"
cost = 4

[[arg_coercion]]
from = "decimal_literal"
to   = "f32"
cost = 5
```

That is the whole table.

**Cost bands.** The costs form three bands, and verify.rb checks the ordering between them:
staying in the tower (1–2) is cheaper than leaving it for `decimal` (3), which is cheaper than
entering the float island (4–5). Within a band, a nearer type is cheaper. The bands are what make
a host's `f(i64)` beat a host's `f(decimal)` for an `i16` argument, and `decimal` beat `float` for
an integer.

**Built-in slots ignore the first band.** A built-in row's slot names a family, and every member
type of that family matches it at cost 0 (as today: `abs(i16)` matches `abs(integer)`). A family
slot's cost for an argument outside the family is the cheapest row into any member type, and the
argument adopts that member: an integer constant in a `float` slot costs 4 and becomes `f64`.

**Checks** ([../functions/verify.rb](../functions/verify.rb) check 18). Every value row (a scalar
`from`) is backed by a `mode = "implicit"` `[[cast]]` row in casts.toml, and only a literal row
enters the float family from outside it. Costs are positive integers, each pair appears once, and
the band ordering holds. `scripts/gen_catalog.rb` emits the table into each core's operators module
beside `OPERATORS` (`ARG_COERCIONS` / `argCoercions`).

**Why compare.toml.** compare.toml owns promotion, which this table extends to function arguments.
Six of its rows are literal adaptations, which are not casts and so have no natural casts.toml
form. The verify.rb check above still ties the value rows to casts.toml's `mode` lattice.

**Why `decimal` is cheaper than `float`.** This one ordering is the design's main value judgment.
It makes an integer argument prefer a `decimal` overload over a binary `float` overload, where
PostgreSQL picks `float8`. §6 weighs the trade-offs and states the decision.

## 4. The algorithm

Resolution of a generic-path call `f(a₁ … aₙ)` becomes:

1. **Exact built-in match** (unchanged), including §9.1's unknown-argument rule. A `42725` from
   here is final; the pass never breaks an ambiguity.
2. **Host function, exact signature** (unchanged; [extensibility.md](extensibility.md) §4.2).
3. **Coercion pass** (replaced the old per-row `promote_scalar_pair`):
   1. **Candidates.** Every `kind = "function"` row with the call's name and arity, and every
      host function with that name and arity (§4.2). Named arguments and defaults are placed
      first, as today ([functions.md §11](functions.md)).
   2. **Per-position cost.** For argument `aᵢ` against its slot:
      - `0` if `aᵢ`'s type matches the slot: a member of a built-in slot's family, the exact type
        of a host slot, or anything in an `any` slot;
      - `0` if `aᵢ` is unknown-typed (a bare `NULL` or `$N`; §9.1) and the slot is a built-in one;
      - the cheapest `[[arg_coercion]]` row from `aᵢ`'s source into the slot's type (into any
        member type, for a family slot), as §3 describes;
      - otherwise the signature is **not a candidate**. An unknown-typed argument in a host slot
        is not a candidate, as today ([functions.md §9.1](functions.md)).
   3. **Score** a candidate as the sum of its per-position costs. Keep the minimum-score set.
   4. **Select.** An empty set is `42883`, as today. With one candidate, select it. With several:
      1. if any are built-in, drop the host candidates (§4.2);
      2. apply §9.1 step 3 to the unknown positions, if any;
      3. if that does not leave exactly one, the call is **`42725`**.

      None of these steps looks at catalog or registration order.
4. **Bare-string-literal fallback** (unchanged; §9.1).

**Adoption.** In the selected row:

- An unknown argument adopts its slot's type, as in §9.1.
- A numeric constant whose target is not `decimal` is resolved again in that type (`f64` in a
  `float` slot; `i32` in a host `i32` slot). A literal is an untyped constant
  ([types.md](types.md) §6), so this is literal adaptation, not a conversion.
- Everything else — a value, or an integer constant bound for a `decimal` slot — keeps its resolved
  expression and converts inside the call: an integer to a wider integer or to `decimal` at scale 0,
  an `f32` to `f64`, as the arithmetic and comparison promotions do. The built-in kernels with a
  `decimal` slot accept an integer value; a host kernel's arguments are converted to its declared
  types before it runs, on the row and batch paths alike, so the batch ABI
  ([extensibility.md](extensibility.md) §4.2.1) is unchanged.

**Cost.** Coercion charges nothing. This matches [cost.md](cost.md) ("the integer → `decimal`
promotion [is a] value conversion inside the … node (no cast, nothing charged)"): `sqrt(int_col)`
costs one `operator_eval` less per row than `sqrt(int_col::numeric)` (`expr/arg_coercion.test`). An
explicit cast still charges its own evaluation, and a host function still charges its declared
cost. One existing charge is unchanged and not part of the pass: a built-in float function whose
result is `f64` widens an `f32` argument through the implicit cast node cost.md "Mixed-width
floats" describes, which charges one `operator_eval`; that argument matched its `float` slot
exactly.

**Result type.** It comes from the selected row's `result`, computed over the *coerced* argument
types. So `abs`'s `"promoted"` sees the post-coercion family.

### 4.1 Worked examples

| Call | Candidates (score) | Selected |
|---|---|---|
| `sqrt(2)` | `sqrt(decimal)` 3, `sqrt(float)` 4 | `sqrt(decimal)` → `1.414213562373095` |
| `sqrt(int_col)` | `sqrt(decimal)` 3; `sqrt(float)` not admitted (a value) | `sqrt(decimal)` |
| `sin(1)` | `sin(float)` 4 | `sin(float)` → `0.8414709848078965` |
| `sin(int_col)` | none | `42883` (the float island; unchanged) |
| `power(2.0, 3)` | `(decimal, decimal)` 3, `(float, float)` 8 | decimal, as §9.2 today |
| `pow(f64_col, 3)` | `(float, float)` 4; decimal not admitted | float, as §9.2 today |
| `power(2, 10)` | `(decimal, decimal)` 6, `(float, float)` 8 | decimal → `1024.0000000000000` |
| `round(5)` (stand-in row removed) | `round(decimal)` 3, `round(float)` 4 | decimal → `5`, as today |
| `round(5, 2)` | `round(decimal, integer)` 3, `round(float, integer)` 4 | decimal |
| `power(NULL, 2)` | `(decimal, decimal)` 3, `(float, float)` 4 | decimal; `NULL` adopts `decimal` |
| `to_timestamp(1700000000)` | `to_timestamp(float)` 4 | `timestamptz` |
| host `f(i64)`; `f(i32_col)` | `f(i64)` 1 | `f(i64)`; today `42883` |
| host `f(i32)`, `f(i64)`; `f(i16_col)` | `f(i32)` 1, `f(i64)` 2 | `f(i32)` |
| host `f(i16)`, `f(i32)`; `f(5)` | `f(i32)` 1, `f(i16)` 2 | `f(i32)`; `f(100000)` admits neither, `42883` |
| host `f(i64)`, `f(decimal)`; `f(i16_col)` | `f(i64)` 2, `f(decimal)` 3 | `f(i64)` |
| host `g(f64)`; `g(2)` | `g(f64)` 4 | `g(f64)`; `g(int_col)` stays `42883` |
| host `sqrt(i64)`; `sqrt(int_col)` | exact host match in step 2 | `sqrt(i64)`; the pass never runs |
| host `sqrt(i32)`; `sqrt(i16_col)` | built-in `sqrt(decimal)` 3, host `sqrt(i32)` 1 | host `sqrt(i32)`: lower score |

### 4.2 Host functions

Host-registered scalar functions ([extensibility.md](extensibility.md) §4.2) take part in the pass.
Before it they matched only an exact scalar signature, so a host `f(i64)` called with an `i32`
column was `42883`; the pass gives them the same implicit conversions built-ins get.

- **Exact matches keep their precedence.** Step 2 (host exact) still runs before the pass. A host
  that registers an exact signature for a call gets it, even where a built-in would match through
  a conversion: a host `sqrt(i64)` serves `sqrt(i64_col)` before `sqrt(decimal)` is considered.
- **Exact types, not families.** A host slot names one scalar type, so within-tower widening has a
  cost (§3's first band). Without it, a host that registers `f(i32)` and `f(i64)` would see
  `f(i16_col)` tie; with it, the nearer `f(i32)` wins.
- **Built-in wins a tie.** When a built-in and a host candidate share the minimum score, the
  built-in is selected. This extends the existing rule that a built-in wins an identical-signature
  collision. A host that wants a call that would otherwise tie registers the exact signature.
- **Unknown-typed arguments** (a bare `NULL` or `$N`) still never match a host slot, as
  [functions.md §9.1](functions.md) says. Neither does the bare-string-literal fallback.
- **Persisted dependencies.** An expression index, CHECK or view over a host call records the
  resolved host signature ([extensibility.md](extensibility.md) §8.1). A call resolved through
  coercion records the selected signature, and reopen verifies it the same way.
- **Upgrades.** A jed release that adds a built-in row can capture a call that resolved to a host
  function through coercion (§7). The §7 capture check reports it. A host's own registrations are
  the host's to manage.

## 5. What it retires

- **The integer stand-in rows** `round(integer)`, `round(integer, integer)`, `ceil(integer)`,
  `ceiling(integer)`, `floor(integer)`, `trunc(integer)`, `trunc(integer, integer)` were deleted.
  The pass selects the `decimal` row, with the same result type, value and cost.
  `abs(integer)` and `to_char(integer, text)` stay, because PostgreSQL has them natively.
- **`arg_resolution = "promote"` on function rows** and `promote_scalar_pair` in each core. Every
  case it handled is a cost-ranked instance (§4.1). The field stays meaningful for operators and
  set-returning functions.
- **Four oracle overrides** that recorded `42883` for an integer call (`sqrt(4)`, `sqrt(2)`,
  `ln(2)`, `power(2, 10)`) became value overrides (§6), as did the corpus's `sign(2)`. Float-only
  calls like `sin(1)` match PostgreSQL outright. The two float-island overrides (`power(f, i)`,
  `width_bucket(f, i, …)`) stay.

## 6. Integer arguments: `decimal` over `float8`

PostgreSQL resolves an integer argument through its preferred numeric type, `float8`. This design
resolves it to `decimal` wherever a `decimal` row exists (§3's costs). §6.1 lists what that changes
for users, §6.2 weighs the two choices, and §6.3 records the decision.

### 6.1 Divergences from PostgreSQL

Wherever PostgreSQL resolves an integer argument through `float8` and jed has a `decimal` row, jed
returns `numeric` instead:

- **Same rendered value, different type:** `round(5)`, `ceil(5)`, `floor(5)`, `trunc(5)`,
  `sign(2)`. jed already did this for `round`/`ceil`/`floor`/`trunc` through the stand-in rows.
- **Different rendered value:** the transcendentals, `sqrt(2)` (`1.414213562373095` vs
  `1.4142135623730951`), `ln(2)`, `exp(1)`, `power(2, 10)` (`1024.0000000000000` vs `1024`). These
  changed from error overrides to value overrides in the ledger.
- **Still an error where PostgreSQL resolves:** an integer or decimal *value* in a float-only
  function (`sin(int_col)`, `power(f64_col, int_col)`). This is the float island, unchanged.

Seen from the user's side, the first group is invisible except to a typed host API. The second is
the real cost of the design: a query copied from PostgreSQL returns a correctly rounded answer
with different digits, where it used to fail loudly.

### 6.2 Trade-offs

**For `decimal`:**

- **It is the only lossless target.** Every `i64` is an exact `decimal`; an `i64` above 2⁵³ loses
  digits as an `f64`. That is why casts.toml makes `int → f64` explicit-only and why
  [float.md](float.md) §6 forbids an integer *value* from silently becoming a float.
- **Literals and columns resolve alike.** Under the float island, `float8` is reachable only from a
  literal. Choosing it would make `sqrt(2)` an `f64` call while `sqrt(int_col)` takes the `decimal`
  row (or fails), so the result type would depend on whether the argument is a literal. With
  `decimal`, both select the same row and type. Giving columns `float8` too would mean relaxing the
  float island, a larger change than this design.
- **Byte-identical results.** The `decimal` transcendentals are a byte-exact port of PostgreSQL's
  `numeric.c` and agree across cores by construction ([decimal.md](decimal.md) §8). The float
  transcendentals sit on the determinism ledger and are compared within a tolerance (`R` tag,
  [float.md](float.md) §8–§9). Choosing `float8` would move ordinary integer calls under that exemption.
- **It matches existing jed behavior.** `round(5)`, `ceil(5)`, `floor(5)` and `trunc(5)` already
  returned `numeric` through the stand-in rows. The hand-written `div`, `gcd`/`lcm` and
  `width_bucket` resolvers already promote integers to `decimal`, never to float.

**For `float8`:**

- **PostgreSQL parity.** Same type and same digits. Ported queries behave identically, and the
  four overrides of §5 disappear instead of turning into value overrides.
- **Speed.** The `decimal` transcendentals are multi-precision loops (Newton `isqrt`, range-reduced
  Taylor and `atanh` series) over base-10⁹ limbs. An `f64` `sqrt`/`ln`/`exp` is one hardware or libm
  call. `sqrt(int_col)` over a large table is much slower.
- **Metered cost.** The `decimal` kernels charge size-scaled `decimal_work` steps ([cost.md](cost.md))
  on top of the call's `operator_eval`. A session under a `max_cost` ceiling reaches it sooner with
  `decimal` than it would with an `f64` kernel.
- **"Exact" does not fully apply.** A transcendental is rounded either way: `decimal` `sqrt(2)` is
  `1.414213562373095` (16 significant digits), `float8` is `1.4142135623730951` (17). What `decimal`
  buys here is determinism, not precision.
- **Display.** `power(2, 10)` renders `1024.0000000000000`, not `1024`.

### 6.3 Decision

✅ **Decided (2026-10-24): `decimal`.** The deciding reasons are the first two: `decimal` is the only
lossless target, and choosing `float8` for literals while the float island keeps columns out would
make a call's type depend on how its argument is spelled. Determinism and consistency with the
existing jed behavior support it.

The speed and cost penalties are real but have a workaround: a caller who wants the float kernel
writes the cast, `sqrt(x::f64)`. `expr/arg_coercion.test` carries a cost entry for an integer
transcendental so the decimal pricing stays visible.

## 7. Risks

- **Overload capture.** Under cost ranking, adding a row can change which row an existing call
  selects. If jed later adds `sqrt(integer)`, `sqrt(int_col)` moves from the `decimal` row to the
  new one. View bodies, CHECKs, defaults and index expressions are stored as source text and
  re-resolved on load ([views.md](views.md)), so their meaning would move with the catalog. The
  capture check guards this: [../functions/coercion_snapshot.toml](../functions/coercion_snapshot.toml)
  records the row the pass selects for every numeric argument tuple (each of `i16`/`i32`/`i64`/
  `decimal`/`f32`/`f64` and the two literal sources, at every numerically typed position) that
  matches no generic-path row exactly, or `42725` for a tie. It is generated by
  [../functions/coercion_snapshot.rb](../functions/coercion_snapshot.rb), an independent Ruby
  implementation of §4 over the built-in catalog, and checked by `mise run verify`: a catalog or
  cost-table change that moves an entry fails until the snapshot is rewritten (`--write`), so every
  capture shows in the change's diff, the way a format change bumps `format_version`. Host functions
  are per environment and not part of it.
- **Persisted expressions.** Every value coercion (tower widening, `integer → decimal`) is
  immutable and never traps, and a literal narrows only when its value fits, so coercion does not
  change an expression index's admissibility
  ([index-dependencies.md](index-dependencies.md)). The planner's trap classification
  ([functions.md §15](functions.md)) treats it as part of the call.
- **Summed scores can trade positions.** Because the score is a sum, a candidate that converts
  fewer arguments can beat one with cheaper conversions everywhere. With two integer constants, a
  `(float, integer)` row (4) beats a `(decimal, decimal)` row (6). No built-in has that shape; the
  capture check would surface one that gains it. The snapshot records no `42725` today.
- **Cross-core drift.** The algorithm is small: a sum, a minimum, and §9.1's existing tie rule over
  codegen'd data. Corpus entries for the built-in rows of §4.1 pin it in all three cores; the host
  rows need per-core host-API tests, since the corpus registers no host functions (CLAUDE.md §10).

## 8. Decisions

1. ✅ **Decided (2026-10-24): integer arguments resolve to `decimal`** (§6.3). The rejected
   alternative dropped the `integer → decimal` value row and kept only the literal-to-float rows,
   which left `sqrt(2)` as an error.
2. ✅ **Decided (2026-10-24): host functions take part** (§4.2), with type-level costs for their
   exact-type slots, and a built-in winning a tie.
3. ✅ **Decided (2026-10-24): the table lives in compare.toml** (§3).

**Not a decision: the resolver-routed functions.** Some functions bypass the generic path for a
hand-written resolver in each core, because their typing does not fit one row per signature: `div`
(its name belongs to the `/` operator's rows), `gcd`/`lcm` (the result takes the wider width of
*both* arguments), `width_bucket` (three operands reconciled together), `make_*` (named arguments
and defaults), and the regex, range, array, JSON and VARIADIC functions. The pass does not touch
them, and nothing about their behavior changes.

`div`, `gcd`/`lcm` and `width_bucket` already hand-code the same numeric rules as §3's table:
integer promotes to `decimal`, and a numeric literal adapts to a float operand. A later
consistency cleanup can have them read those rules from the table instead of repeating them per
core. That is refactoring with no observable change. The polymorphic, regex and VARIADIC resolvers
share nothing with the table.

## 9. Slices

1. ✅ **Data + resolver.** The compare.toml table, verify.rb check 18 and the codegen, and the pass
   in Rust, Go and TS, replacing the old promotion path. Corpus: the `42883` entries flipped,
   `expr/arg_coercion.test` added, the affected oracle overrides rewritten. functions.md §9/§9.2
   rewritten.
2. ✅ **Catalog cleanup.** The stand-in integer rows and the function rows'
   `arg_resolution = "promote"` deleted; the existing round/ceil/floor/trunc corpus (values and
   costs) passes unchanged.
3. ✅ **Host functions.** Host candidates join the pass (§4.2) in all three cores, with per-core
   host-API tests (`coercion_*`) for the host rows of §4.1. extensibility.md §4.2 and api.md §12
   updated.
4. ✅ **Capture check** (§7): coercion_snapshot.rb / .toml, run by `mise run verify`.
5. **Later, optional: resolver consistency cleanup** (§8). `div`, `gcd`/`lcm` and `width_bucket`
   read their numeric reconciliation from the table. No corpus change expected.
