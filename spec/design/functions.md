# Function / operator catalog — design

> The reasoning behind the function / operator catalog. The **catalog is authoritative**
> ([../functions/catalog.toml](../functions/catalog.toml)); this doc is the *why*. When a
> decision here changes, change it in the catalog and here in the same edit, and update
> [CLAUDE.md](../../CLAUDE.md) if it revises a load-bearing commitment.

The catalog is canonical shared data (CLAUDE.md §5): each entry names an operator, its
operand contract, result type, and NULL behavior. It is the single place the operator
semantics are stated, so the per-language cores — and the future codegen "middle path"
that emits their stubs — descend from one contract instead of N hand-written ones.

## 1. Role & scope

The comparison and null-test operators were **backfilled**: the three cores
([impl/rust](../../impl/rust), [impl/go](../../impl/go), [impl/ts](../../impl/ts))
hand-wrote that logic in lockstep first, and an authored catalog followed. The
arithmetic and logical operators went the other way — **spec-first** (CLAUDE.md §10/§11),
added to the catalog ahead of the parser/executor code in the general-expression slice.
The catalog now lists:

| Kind | Operators | Result |
|---|---|---|
| `logical` | `AND` `OR` `NOT` | `boolean` |
| `comparison` | `=` `<>` (`!=`) `<` `>` `<=` `>=` | `boolean` |
| `comparison` (NULL-safe) | `IS DISTINCT FROM`, `IS NOT DISTINCT FROM` | `boolean` |
| `null_test` | `IS NULL`, `IS NOT NULL` | `boolean` |
| `arithmetic` | `+` `-` `*` `/` `%`, unary `-` | `promoted` |
| `function` (scalar) | `abs` `round` (§9), `make_interval`/`make_timestamp`/`make_timestamptz` (§11), `uuid_extract_version`/`uuid_extract_timestamp`, `uuidv4`/`uuidv7`, `now`/`clock_timestamp` (§12), `num_nulls`/`num_nonnulls` (VARIADIC, array-functions.md §12) | per-function |
| `aggregate` | `COUNT` `SUM` `MIN` `MAX` `AVG` | `i64` / `decimal` / widened (§8) |
| `set_returning` | `generate_series`, `unnest` (§10, array-functions.md §9) | a row **set** (§10) |

The **aggregates** are not operators — they collapse a set of rows into one value, have no
infix symbol or precedence, and widen their result type by the operand type — so they live
in a **separate `[[aggregate]]` array** with their own field set, not as `[[operator]]`
rows (§8, [aggregates.md](aggregates.md)). The not-equal operator `<>` (name `ne`) is the
3VL negation of `=`: it propagates NULL exactly as `=` does (`NULL <> NULL` is unknown), one
catalog row per operand family like the other comparisons. `<>` is its canonical symbol; the
PostgreSQL spelling `!=` is a **lexer-level alias** that folds to the same operator, so it gets
no catalog row of its own (see [grammar.md](grammar.md) §4). The catalog must stay descriptive:
it must not list an
operator no core implements, nor omit one a core has — and a new operator is added here
**first**, in the same change that adds its parser/executor code and conformance entries.

The catalog defines what operators *do*; it does **not** restate how scalars compare or
promote. That division is load-bearing and is the subject of §4.

## 2. Result types: `boolean` and `promoted`

The `boolean` scalar type now exists ([types.md](types.md) §1) as the first non-integer
type. Comparisons, null tests, and the logical connectives all produce a real
`boolean` — projectable in the SELECT list (`SELECT a = b`), consumable in `WHERE`, and
ordered `false < true`. Three-valued logic rides on the same type: `unknown` is simply a
**NULL boolean**, so `{true, false, NULL}` *is* Kleene's three states with no separate
non-storable value. The reserved id `truth` that earlier slices used for comparison
results is therefore **retired** — once `boolean` is a scalar id there is exactly one
three-valued domain, and `result = "boolean"` resolves like any other scalar.

The `result` field is one field that holds *either* a scalar id from
[../types/scalars.toml](../types/scalars.toml) *or* a reserved non-scalar id. The one
remaining reserved id is:

- `promoted` — the common promoted operand type of an arithmetic operator: `i16 + i16
  → i16`, `i16 + i64 → i64` (the higher-`rank` operand wins, per the promotion
  tower in [../types/compare.toml](../types/compare.toml)). It is reserved rather than a
  concrete scalar because the actual result type is computed per call from the operands.

One unified field means every consumer (and the coherence checker) validates the result
the same way — "a known scalar id, or a known reserved id" — whether `boolean` or
`promoted`.

The comparisons and the null tests both carry `result = "boolean"`. Their difference is
not in the result *type* but in NULL handling (§3): a null test always lands on a definite
`true`/`false`, expressed by its `null` field, not by a second result id.

## 3. NULL: propagation vs detection

The three-valued NULL logic itself lives in [../types/compare.toml](../types/compare.toml)
`[null]`. The catalog records, per operator, *which side of it the operator falls on*, in
the `null` field:

- `propagates` — any NULL operand makes the result `unknown` (a NULL boolean). The
  comparisons are here: `NULL = NULL` is `unknown`, equality is not reflexive across NULL,
  and a row whose predicate is `unknown` is excluded just like `false`. The arithmetic
  operators and unary `NOT` are also `propagates` (`NULL + 1` is `NULL`, `NOT NULL` is
  `NULL`).
- `detects` — the operator inspects NULL-ness and **always** returns a definite boolean,
  never `unknown`. The null tests are here: `IS NULL` / `IS NOT NULL` are the sanctioned
  way to observe a NULL.
- `kleene` — the three-valued connective truth table, used by binary `AND` / `OR`. NULL
  does **not** simply propagate: a *dominant* operand absorbs it — `FALSE AND NULL` is
  `FALSE`, `TRUE OR NULL` is `TRUE` — and only when no operand dominates does the result
  go `unknown` (`TRUE AND NULL` is `unknown`, `FALSE OR NULL` is `unknown`). This is why
  `AND`/`OR` cannot be `propagates`. (`NOT` is the one logical operator that genuinely
  propagates, so it carries `propagates`, not `kleene`.) The truth tables themselves live
  in [types.md](types.md), not as catalog data — the catalog records only which discipline
  each operator falls under.

- `none` — the **non-strict** discipline (the array builders `array_append`/`array_prepend`/
  `array_cat`, [array-functions.md §4](array-functions.md)): the resolver does **not**
  short-circuit a NULL argument — the kernel inspects NULL-ness itself (a NULL array argument is
  the identity/empty, not a propagated NULL). Distinct from `null_safe` (a *comparison* result
  rule); `none` says the resolver does no NULL handling at all.

- `null_safe` — NULL is a **comparable value**, not a poison: the result is **always** a
  definite boolean, never `unknown`. `IS NOT DISTINCT FROM` is NULL-safe `=` — `NULL IS
  NOT DISTINCT FROM NULL` is TRUE, `1 IS NOT DISTINCT FROM NULL` is FALSE, and two present
  integers compare exactly as `=` would. `IS DISTINCT FROM` is its negation. This is the
  one discipline that separates these two operators from the propagating comparisons:
  their operand contract (`integer × integer`, `promote`) and `boolean` result are
  identical to `=`; only the NULL handling differs. The cores implement it by short-
  circuiting the two NULL cases (both-NULL → "same", one-NULL → "distinct") and otherwise
  deferring to three-valued `=`, which is definite when neither side is NULL.

## 4. Operand resolution by reference, not duplication

A single comparison operator accepts many operand pairs: `i16 = i64`, `i32 < i16`,
and so on. The catalog expresses this with **operand families** plus a **resolution
reference**, not an enumerated overload per type pair:

```
arg_families   = ["integer", "integer"]
arg_resolution = "promote"
```

`arg_resolution = "promote"` means "reconcile a mixed-width pair by the promotion tower in
[../types/compare.toml](../types/compare.toml)" — widen both to the common (higher-`rank`)
type, then compare as one integer. The catalog states the operator's *contract*; the
*reconciliation* is deferred to the table that owns it. `compare.toml` already holds the
promotion strategy (`max-rank`), the comparability matrix, and the NULL logic; restating
any of it here would duplicate canonical data and drift.

The rejected alternative was an **enumerated** catalog: one entry per concrete
`(left, right)` pair (~45 rows for five operators over the nine integer pairs). It is
flat but re-encodes the promotion tower into the catalog, grows quadratically as families
(decimal, text, …) arrive, and creates two places that must agree about which pairs are
comparable. Family + reference keeps it one row per operator forever.

The coherence checker ([../functions/verify.rb](../functions/verify.rb)) enforces the
division: every `arg_families` entry must be a real `family` in `scalars.toml`, and a
`promote` resolution must name an operand pair that `compare.toml` actually lists as
comparable with a promotion rule for the family.

## 5. `<=` and `>=` are primitive, definable as Kleene-OR

The cores implement `<=` and `>=` directly, and the catalog lists them as primitive
`comparison` operators with the same family signature as `<` and `=`. They are *equal to*
`(< OR =)` and `(> OR =)` under three-valued (Kleene) OR — which is why a NULL operand
makes them `unknown` exactly as `<` and `=` do: `or(unknown, unknown)` is `unknown`, never
`false`. That equivalence is genuine reasoning, recorded here, but it is **not a data
field**: the catalog describes what the cores do (evaluate a primitive), and a
`derived_from` edge would be the catalog's only derivation, premature machinery for one
case, and would imply a rewrite the cores do not perform.

## 6. Precedence is authored data

`precedence` is an integer on every operator (higher binds tighter) and is the **single
source of truth** for the expression precedence tower. The hand-written parsers
([grammar.ebnf](../grammar/grammar.ebnf) `expr`) descend in exactly this order — a parser
whose precedence disagrees with the catalog is non-conformant. The tower:

| Level | Operators | precedence |
|---|---|---|
| OR | `or` | 10 |
| AND | `and` | 20 |
| NOT | `not` | 30 |
| comparison + null test | `eq` `lt` `gt` `le` `ge`, `is_null` `is_not_null` | 35 |
| additive | `add` `sub` | 40 |
| multiplicative | `mul` `div` `mod` | 50 |
| unary minus | `neg` | 60 |

Comparison and the null tests are **non-associative** at their level (`a = b = c` is a
syntax error, `42601`); every other binary level is left-associative; the prefix operators
(`not`, `neg`) are right-associative. The codegen emits `precedence` into each core's
operator descriptor table so the value is one authored number, not three hand-copied ones.

## 7. Arithmetic result type and overflow boundary

An arithmetic operator's `result = "promoted"` is the higher-`rank` operand type (the same
promotion tower comparisons use, §4). The computed value is range-checked against **that**
type, so `i16 + i16` overflows at the `i16` boundary (`30000 + 30000` traps `22003`),
not at `i64`'s — the type-faithful behavior, matching PostgreSQL's `smallint + smallint`.
This is a deterministic, cross-core-identical decision (CLAUDE.md §8): every core computes
in 64-bit, traps `22003` if the 64-bit op itself overflows, *and* traps `22003` if the
in-range 64-bit result falls outside the declared result type. `div`/`mod` additionally
trap `22012` on a zero divisor (a defined, deterministic abort, not a wrapped value).

## 8. Deferred fields and the growth rule

One optional field rounds out the operator schema — recently moved from reserved to **live**, the
model for additive, data-only growth:

- `cost` — a **per-operator** evaluation cost base, now **live**. The cost-accounting seam landed
  first (CLAUDE.md §13): the unit schedule is data in [../cost/schedule.toml](../cost/schedule.toml)
  with a single **uniform `operator_eval`** weight, the *why* in [../design/cost.md](../design/cost.md).
  This optional `cost` field is now codegen'd into `OperatorDesc` and **read by the evaluator**: an
  operator node charges `operator_cost(name)` — the operator's own `cost` if authored, else the
  uniform `operator_eval` (cost.md §3). It is a **name-level static base** (overloads sharing a name
  must agree — verify.rb) and **size-independent**: argument-size-dependent cost lives in the
  size-scaled units (`decimal_work` / `varlen_compare` / `collate` / `regex_step` / …), never here.
  **No built-in sets a non-default `cost` today** — every weight is the uniform default, so cost is
  unchanged — but tuning a built-in's base, or giving a host-defined function a static weight
  (cost.md §6), is now a **pure data-only change**: add the integer in catalog.toml, regenerate, done.
  (The evaluator reads it at the arithmetic / comparison / logical operator arms; extending it to the
  remaining operator-bearing nodes — null-test, `IS DISTINCT FROM`, `||`, the scalar functions,
  json/array/range operators — is the identical one-line pattern per arm, an additive follow-on.)
  The seam was designed as one coherent schedule, not a per-operator constant bolted on — which is
  why the field could be made live later with no reshape.

Reserved values and kinds still to be authored spec-first with their own executor slices
([../../TODO.md](../../TODO.md)):

- The `function` kind is now **substantially authored**: `abs`/`round` (§9), `make_interval`
  with named + `DEFAULT` arguments (+ the un-defaulted siblings `make_timestamp`/`make_timestamptz`,
  §11), the uuid extractors/generators + clock functions
  (§12), and the text casing functions **`lower(text)`/`upper(text)`** (collation Slice 3e) all
  landed as `[[operator]]` rows with `kind = "function"`, plus `generate_series` as a
  `set_returning` row (§10). `lower`/`upper` are **overloaded** — the same names are the range
  accessors (range → element); the resolver branches on the argument type. They **fold ASCII only**
  by default and consult the **loaded Unicode property tables** for full Unicode casing — the
  SQLite-style baseline, with no casing table built into the engine ([collation.md §16](collation.md));
  `ILIKE` (case-insensitive `LIKE`) folds the same way. Further scalar functions — `ceil`, `floor`,
  `mod`, `sign`, the text `length`, `initcap`, and the like — are follow-on slices that reuse the mold.

**The polymorphic array functions are authored (`kind = "function"`, over `anyarray`/`anyelement`).**
AF1 — `array_ndims`/`array_length`/`array_lower`/`array_upper`/`cardinality`/`array_dims` and the
non-strict builders `array_append`/`array_prepend`/`array_cat` — reuses the scalar-function mold but
adds the **`anyarray`/`anyelement` pseudo-families** (admissible in `arg_families`) and the reserved
result codes `anyarray`/`anyelement` (a type variable `ELEM`, bound by structural unification and
read back into the result). The dispatch (the unification + the kernels) is hand-written per core;
`verify.rb` admits the tokens as a small allowlist. The full design — the resolution algorithm,
the literal-adaptation rule, and the per-function semantics — lives in
[array-functions.md](array-functions.md). That surface is now **complete**: `||`/`unnest`/`@>`/`&&`/
`ANY`/`ALL` and finally **`VARIADIC`** (AF6, §12 — the `num_nulls`/`num_nonnulls` built-ins) all landed.

**Aggregates are authored (`kind = "aggregate"`).** `COUNT`/`SUM`/`MIN`/`MAX`/`AVG` landed
in a **separate `[[aggregate]]` array**, not as `[[operator]]` rows, because they do not fit
the operator mold on three counts: (1) the **result widens by the operand type** —
`SUM(i16/i32) → i64`, `SUM(i64) → decimal`, `MIN/MAX → the input type` — expressed
by two reserved result ids, `sum_widen` and `same_as_input`, alongside the concrete `i64`
/`decimal` (`COUNT → i64`, `AVG → decimal`); (2) a fifth **NULL discipline**, `aggregate`
— NULL inputs are *skipped* (except `COUNT(*)`, which counts every row), and an empty or
all-NULL group yields `NULL` for `SUM/AVG/MIN/MAX` but `0` for `COUNT`; (3) **`COUNT(*)`
takes no expression** (`arg = "star"`), and there is no infix symbol, precedence, or
`arg_resolution`. The coherence checker validates aggregates on a separate branch
([../functions/verify.rb](../functions/verify.rb)), and the codegen emits a separate
`AGGREGATES` descriptor table. The full semantics — the widening table, the empty-set
rules, the `GROUP BY` / `HAVING` rules, the cost accrual — live in
[aggregates.md](aggregates.md). DISTINCT inside an aggregate (`COUNT(DISTINCT x)`) folds only the
distinct argument values ([aggregates.md](aggregates.md) §5); `FILTER (WHERE cond)` folds only the
rows for which `cond` is TRUE ([aggregates.md](aggregates.md) §11).

The `null_safe` discipline is now **authored**: `IS [NOT] DISTINCT FROM` (`kind =
"comparison"`, `null = "null_safe"`) landed once the `boolean` type gave the result a
home (§3). Like the null tests it is a keyword operator with no punctuation `symbol`, so
the catalog checker exempts a `null_safe` comparison from the "comparison must carry a
symbol" rule ([../functions/verify.rb](../functions/verify.rb)).

## 9. Scalar functions (`abs`, `round`)

Scalar functions are named `f(args)` calls evaluated **per row** — the first being `abs`
and `round`. As §8 reserved, they **reuse the operator mold**: they are `[[operator]]` rows
with `kind = "function"`, sharing the operator field set (`name`, `arity`, `arg_families`,
`arg_resolution`, `result`, `null`, `errors`) and the same overload model — one row per
`(name, arg_families)` signature. Two operator fields are simply **absent**: a function has
no infix `symbol` and no `precedence` (it is called by name, not parsed in the precedence
tower). The coherence checker already accepts this — `function` is in `KNOWN_KINDS`, and the
symbol/precedence requirements are not imposed on it — and the codegen emits the rows into
the generated `OPERATORS` table unchanged (no `gen_catalog.rb`/`verify.rb` change).

**Not operators, not aggregates — a third shape.** A scalar function differs from an
operator only syntactically (call form vs. infix), but differs from an **aggregate**
*semantically*: an aggregate folds a *set* of rows into one value and is legal only in a
projection/`HAVING` over a group (§8, [aggregates.md](aggregates.md)); a scalar function
maps its argument values to one value **per row** and is therefore legal **anywhere an
expression is** — projection, `WHERE`, `JOIN ON`. The shared `function_call` grammar
([grammar.md](grammar.md) §17) is disambiguated at resolve time: an aggregate name collects
into the aggregate context; a scalar-function name resolves to an ordinary per-row
expression node in the current context; any other name is `42883` (`undefined_function`).
A scalar function may still *contain* an aggregate (`abs(sum(x))`) — its argument resolves
in the same context the call sits in, so `sum` inside it is collected in a projection and
rejected `42803` in a `WHERE`.

**`arg_resolution = "none"`.** Unlike a binary arithmetic/comparison operator, a scalar
function does **not** reconcile its arguments to one common type: `round(numeric, integer)`
keeps a decimal value and an integer count side by side. Each argument is matched to its
declared family directly; there is no promotion *between* arguments. (A general implicit
*argument* coercion — e.g. silently widening an `int` argument to `decimal` — is deliberately
**not** built; PG's `round(5)` convenience is provided by explicit integer overloads
instead, below, keeping the type system honest, CLAUDE.md §4.)

**`abs`** carries `result = "promoted"` — for a one-argument function the promoted operand
type is just the operand's own type, so `abs(i16) → i16`, `abs(i64) → i64`,
`abs(numeric) → numeric` (exactly as unary `neg`, §7). Over the integer family the magnitude
is **range-checked at the result type's boundary**: `abs(i16 -32768)` has no positive
`i16` and traps `22003` — the same overflow discipline as `-(i16 -32768)` — so `abs`
carries `errors = ["22003"]`. Over `decimal` it clears the sign and cannot overflow
(`errors = []`).

**`round`** carries `result = "decimal"` and rounds **half away from zero** — the one
engine-wide decimal rounding mode ([decimal.md](decimal.md) §3) — reusing the existing
decimal scale-coercion routine. `round(numeric)` rounds to **scale 0**; `round(numeric, n)`
rounds to **`n` fractional places** (PG parity includes a negative `n`, rounding to the left
of the point: `round(150, -2) → 200`). Two **integer overloads**, `round(integer)` and
`round(integer, integer)`, return `numeric` so that PostgreSQL's `round(5) → 5` works
without an implicit coercion pass — they are authored as concrete catalog rows. All of
`round`'s forms `propagate` NULL (any NULL argument → NULL), as does `abs`. The **decimal**
overloads carry `errors = ["22003"]`: a round-up carry can push a value at the integer-digit
format cap over it ([decimal.md](decimal.md) §2/§4 — exactly PG); the integer overloads
cannot (an i64 is at most 19 digits). `round`'s scale argument clamps to `max_scale`
(16383) like PG `numeric_round`.

**Cost.** A scalar-function call charges one `operator_eval` ([cost.md](cost.md)) — the same
uniform per-evaluation weight every operator charges — with its arguments charging their own
costs recursively. The cost is deterministic and cross-core-identical (CLAUDE.md §13), and
is asserted in the conformance corpus alongside the rows.

## 10. Set-returning functions (`generate_series`) — the fourth function mold

A **set-returning function (SRF)** is the fourth function shape, distinct from the three
above:

- an **operator** maps infix operands to one value;
- a **scalar function** (§9) maps named arguments to one value **per row**;
- an **aggregate** ([aggregates.md](aggregates.md)) folds a **set** of rows into one value;
- an **SRF** *expands* its arguments into a **row set** — zero or more rows per call.

Because it produces rows, an SRF fits neither the operator/scalar result mold nor the
aggregate fold. It lives in its own catalog array
([catalog.toml](../functions/catalog.toml) `[[set_returning]]`, `kind = "set_returning"`)
with its own field set — `arity`, `arg_families`, `arg_resolution`, a reserved `result`, a
fixed output `column` name, and `null = "empty_on_null"`. Two reserved SRF result codes exist:
`set_of_promoted` ("a row set of one column at the promoted integer type of the args" —
`generate_series`) and `set_of_element` ("a row set of one column at the element type bound from
the `anyarray` argument" — `unnest`, the polymorphic SRF, [array-functions.md §9](array-functions.md)).
Like the aggregate dispatch, the resolve path is hand-written per core (dispatched by name —
`generate_series` and `unnest` branches); the catalog row is the shared registry data
(CLAUDE.md §5). The codegen emits a `SET_RETURNING` descriptor table per core (a §8 drift
test cross-checks it), and `verify.rb` validates the array on its own branch (`promote`
there requires each operand family to have a promotion rule — an SRF widens its *own* args
among themselves, it never compares two families, the one divergence from the operator
`promote` check; it also admits the polymorphic `anyarray`/`anyelement` pseudo-families in an
SRF `arg_families` slot, interpreted by the hand-written resolver exactly as for the array
functions).

**`generate_series` (FROM-clause only, integer only).** The first SRF is a row source in the
`FROM` clause ([grammar.md](grammar.md) §35): `generate_series(start, stop)` and
`generate_series(start, stop, step)` over the integer family. It resolves to a **synthetic
one-column relation** — a `Table` built at plan time with a single column whose type is the
**promoted integer type** of the arguments (`generate_series(i16, i32)` ⇒ `i32`;
integer literals default to `i64`). The relation threads through the planner and the
nested-loop join unchanged; the executor, instead of scanning a store, **generates** the
rows in the materialization step. The synthetic table is the only new structure: a §8
borrow-checker note for Rust — it is owned in a `Vec<Box<Table>>` local to the planner so a
`ScopeRel`'s `&Table` reference stays valid (Go/TS are GC-managed).

**PostgreSQL semantics** (oracle-verified): the series runs from `start` toward `stop`
inclusive, stepping by `step` (default `1`); **any NULL argument yields zero rows**;
`start` past `stop` for the step's direction yields zero rows; a **step of zero** is
`22023` (`invalid_parameter_value`, *"step size cannot be equal to zero"*); and an **i64
overflow** while stepping **stops the series cleanly** (no trap — the last representable
element is emitted, then the loop ends). The output column name follows PG's single-column
function-alias rule (§35). The arguments are **implicitly `LATERAL`** (§44): a `$N`, a correlated
outer column, **and** a column of an earlier sibling FROM relation (`FROM t CROSS JOIN
generate_series(1, t.n) g`) are all legal — a sibling reference re-evaluates the SRF once per
left-hand row. Its fixed output columns accept PostgreSQL's column-alias-list form
`AS g(c1, …)` (§35): rename left-to-right, allow a partial list, reject too many names with
`42P10`. Deferred: the SELECT-list SRF position and non-integer variants (§35).

**The second SRF — `unnest(anyarray)`** ([array-functions.md §9](array-functions.md)) reuses this
exact machinery: a FROM-clause synthetic relation, the same implicitly-lateral arg scope, the same
single-column function-alias / explicit-column-list rules, the same `generated_row` cost and
`max_cost` ceiling. It differs
only in (a) its column type — the **bound element type** of its `anyarray` argument (the
`set_of_element` result, the polymorphic analogue of `generate_series`'s `set_of_promoted`), and (b)
its generator — one row per element in the value's flattened row-major order (a NULL array or empty
array → zero rows; a NULL element → a NULL row), rather than a counted series.

**Cost.** Each generated element charges one **`generated_row`** ([cost.md](cost.md) §3),
guarded so a `max_cost` ceiling aborts a runaway `generate_series(1, 10^18)` (or an over-cap
`unnest`) with `54P01` **mid-generation**, before the whole series materializes (CLAUDE.md §13). An
SRF touches no store, so it charges **no** `page_read` / `storage_row_read`. `generated_row` is
distinct from `row_produced` (the result-emission unit): a generated row filtered by a `WHERE` or
dropped by a join still charges `generated_row` but not `row_produced`, so the two diverge
under `WHERE`/`LIMIT`. The arguments charge their own `operator_eval`s once, up front.

**Growth obligation discharged (no NoREC relation).** `generate_series` is a new **row
source**, not an optimization — there is no optimized-vs-unoptimized rewrite pair to
disagree, so the NoREC sweep ([conformance.md](conformance.md) §8) gains no scenario. The
differential cores plus the new conformance file (exact rows, oracle-verified against
PostgreSQL, plus the exact cross-core `# cost:`) are the coverage. Should the planner later
gain an SRF-specific optimization (e.g. a streaming `LIMIT` short-circuit over a generated
series), *that* would owe a NoREC relation.

## 11. Named + optional (DEFAULT) arguments — `make_interval` / `make_timestamp` / `make_timestamptz`

PostgreSQL lets a call use **named notation** (`f(b => 2, a => 1)`) and lets a function
declare **DEFAULT** values so trailing arguments may be omitted. jed had neither at the call
site (it already expresses "optional" the way PG implements most built-ins — by **overloading
on arity**, e.g. `round/1` + `round/2`, `generate_series/2` + `generate_series/3` — which is
separate catalog rows, not a default). Named notation and DEFAULTs landed together, driven by
the first function that needs both: **`make_interval`**.

**The driver — `make_interval(years, months, weeks, days, hours, mins, secs)`.** A scalar
function (one row, `kind = "function"`) whose every parameter is **named** and **DEFAULTs to
0**, returning `interval`. It is the natural first consumer because it is unusable without the
two features: `make_interval(days => 3)` needs named notation to name `days` and DEFAULTs to
omit the other six. PG's `make_interval` is also genuinely named (`pg_proc.proargnames` is
set) and defaulted, so the slice is **oracle-checkable from day one** (postgres:18) rather than
a jed-only divergence — the reason it was chosen over naming an existing built-in like `round`,
whose PG parameters are nameless (naming them would have been a documented §1/§7 divergence).

**`secs` is `f64` (`double precision`), its true PG type** — available since the float
slice ([float.md](float.md)). `years…mins` are the `integer` family; `secs` is `float`. The
value folds into the interval **exactly**: `years/months → months` field (×12), `weeks/days →
days` field (×7), `hours/mins/secs → micros`, grouped `(((hours*60)+mins)*60)*10⁶ +
round(secs*10⁶)` as PG does. The one float step — `secs*10⁶` then a half-away-from-zero round
to an integer — is a single correctly-rounded multiply plus a deterministic round, so the
result is **in-contract** (byte-identical cross-core, compared exactly — *not* an `R`-exempt
float render; the float appears only as an input deterministically folded into an exact
interval). The float→int micro-rounding is jed's one engine-wide mode (half away from zero,
[float.md](float.md) §6) where PG uses `rint` (half-to-even); they can differ only at an exact
half-microsecond tie, which realistic `secs` never hit (the corpus uses exactly-representable
values, so it stays oracle-positive). An `i32` month/day or `i64` micros overflow traps
**`22008`** (`datetime_field_overflow`, *"interval out of range"*), exactly PG — the constructor
uses the same checked arithmetic in every core (Rust `checked_*`, Go `mulAdd`/`mul64`, TS
bigint with per-step i64 checks).

**The data — `arg_names` + `arg_defaults` (catalog).** Two **optional** fields were added to
the scalar-function (`[[operator]]`) mold ([../functions/catalog.toml](../functions/catalog.toml)):

- `arg_names` — one parameter name per position (length == arity). **Absent ⇒ the function has
  no parameter names ⇒ named notation on it is `42883`** (PG's behavior for `abs`/`round`/the
  aggregates, which simply omit the field — so every pre-existing row is unchanged and
  positional-only).
- `arg_defaults` — integer-literal default strings for the **trailing** parameters (length ≤
  arity; a default may occupy only a trailing slot, like PG). An omitted trailing argument is
  filled with its default at resolve, and the default literal **adapts to its slot's family**
  (so `make_interval`'s `"0"` becomes `f64 0.0` for `secs`, `i64 0` elsewhere).

Both are codegen'd into the per-core descriptor table (`OperatorDesc`) as data (CLAUDE.md §5) —
the resolver **reads** the names/defaults rather than re-hardcoding them. `verify.rb` checks the
shapes (length, no duplicate names, trailing-only defaults, integer-literal defaults) and a
**cross-overload consistency** rule: a parameter name maps to one position across all overloads
of a function, so named→slot resolution is well-defined independent of which arity overload
matches (`make_interval` is single-signature, so this is trivially satisfied today; the rule
guards future named overloads).

**Resolution — normalize-then-dispatch.** A shared per-core `normalize_named_args` step runs
*before* the ordinary family dispatch. Given the call's positional + named arguments and the
catalog row, it builds the positional argument vector of length `arity`: positional args fill
their index in order; each named arg is placed at its `arg_names` index (unknown name `42883`,
duplicate / collision `42601`); every still-empty trailing slot is filled from `arg_defaults`,
and a still-empty *non*-defaulted slot means no overload matches (`42883`). Each resolved
argument is then resolved **with its declared family as the expected-type hint** — this is what
lets a bare numeric literal adapt to the `f64` `secs` slot (reusing the existing float
literal-adaptation path; float is otherwise a strict island), so `make_interval(secs => 1.5)`
and `make_interval(secs => 2)` work like PG instead of erroring as a family mismatch. Fully
positional calls take a fast path identical to before (no names, no behavior change). The
feature is **resolution-only**: the executor, the type system, and the **cost** are untouched —
a named call charges exactly what its positional twin does (one `operator_eval` + the
arguments' own costs), asserted in the corpus (`# cost:`).

**The siblings — `make_timestamp` / `make_timestamptz` (landed).** The timestamp constructors
reuse this exact mold — every parameter **named** (`year`, `month`, `mday`, `hour`, `min`, `sec`),
the `sec` slot `f64` (the float family, folded to micros by the same correctly-rounded multiply +
half-away round, in-contract) — but **none defaulted**: all fields are required (a missing one is
`42883`, no overload), unlike `make_interval`'s all-defaulted seven. They are catalog rows with
`arg_names` but no `arg_defaults`, resolved by the shared `normalize_named_args` + a dedicated
`make_timestamp` resolver (the `make_interval` precedent). A negative `year` is **BC**; field
validation traps `22008` (year magnitude `1..999999` — no year zero, matching the timestamp
parser; `month` 1..12; `day` valid for the month; and the assembled time of day not past 24:00:00,
so `hour = 24` / `sec = 60` are accepted within a day, exactly PG's `make_timestamp_internal`).
`make_timestamptz` is **overloaded on arity**: the 6-arg form interprets the assembled wall clock in
the **session** zone, the 7-arg form in an explicit `timezone` text (an unrecognized zone is
`22023`); both charge one `timezone` unit beyond the call's `operator_eval`, like `AT TIME ZONE`.
The 6-arg form's session-zone dependence makes it `stable` (PG's class); `make_timestamp` is
`immutable`. (`make_timestamptz`'s overload-on-arity is the first time `normalize_named_args` runs
against an arity-selected catalog row — the 7th positional argument or a named `timezone` selects
the 7-arg overload.)

**`make_date` (landed).** The date constructor completes the family: three **named** integer
parameters (PG's exact `year`/`month`/`day`), none defaulted, the same shared resolver. A negative
`year` is BC; year zero / a bad month / a bad day-for-month / a day count beyond the finite i32
window traps `22008`. Built on the same `days_from_civil` core as the date literal
([date.md §6](date.md)); `immutable` — legal in an index expression.

**Deferred (sequenced follow-ons).** General DEFAULT values for *arbitrary* (non-integer)
literals and user-defined functions are not built (jed has no UDFs; built-ins use overloads or
`make_interval`-style 0-defaults). **`VARIADIC`** was blocked
on the `array` type; that has since landed (array.md), so `VARIADIC` **landed** as **AF6** in the
array function surface ([array-functions.md §12](array-functions.md)) — a `VARIADIC` keyword before a
call's final argument plus variadic overload resolution, spent on the engine's first VARIADIC
built-ins `num_nulls`/`num_nonnulls` (a parameter marked `variadic` accepts either a spread of trailing
arguments or a single array via the keyword).

## 12. UUID functions — extractors now, generators on the entropy+clock seam

PostgreSQL's UUID functions split cleanly along jed's determinism contract
([determinism.md](determinism.md) §1), and that split is the build order:

- **Pure extractors (landed):** `uuid_extract_version(uuid) → i16` and
  `uuid_extract_timestamp(uuid) → timestamptz` are deterministic functions of their input
  bits — immutable, fully in-contract, oracle-checked against PostgreSQL 18. They reuse the §9
  scalar-function mold (`[[operator]]`, `kind = "function"`), one row each.
- **Generators (the seam slice):** `uuidv4()` (random) and `uuidv7([shift interval])`
  (wall-clock + random) are **volatile** — they read entropy and the clock, the class-**B**
  case (determinism.md §5). They land on a host-injected **random + clock seam** — two functions
  the host supplies ([entropy.md](entropy.md)) — so they stay *deterministic given the seam
  functions*: tests inject the engine's provided deterministic source + a fixed clock for exact
  cross-core assertions; production's default draws from the OS CSPRNG **per value** (unpredictable)
  + the wall clock.

**The extractors' semantics** (byte 0 is the most-significant of the 16 raw bytes):

- Both **gate on the RFC 4122 variant** — the value is RFC 4122 iff `(byte8 & 0xC0) == 0x80`.
  A non-RFC variant (Microsoft GUIDs `11`, the legacy NCS variant `0`, the all-zero nil UUID)
  makes **both** functions return NULL. NULL input propagates (the `null = "propagates"` rule).
- `uuid_extract_version` returns the version nibble — the high nibble of byte 6, `0..15` — as
  `i16`, for an RFC value.
- `uuid_extract_timestamp` returns the embedded instant as `timestamptz`, for **version 1 and
  7 only**, NULL for every other version. This matches PG 18, which extracts from v1/v7 only —
  **v6 returns NULL there**, a deliberate match to the pinned oracle, not a divergence (a later
  PG may extend the set; jed tracks `REL_18`). v7 reads the first 6 bytes as a 48-bit big-endian
  Unix-millisecond count (`micros = ms * 1000`). v1 reassembles the 60-bit Gregorian 100-ns
  count from time_low/time_mid/time_hi (the version nibble masked off), subtracts the 1582→1970
  epoch offset (`122192928000000000` 100-ns ticks), and **truncates** to microseconds (`/10`,
  toward zero — PG drops the sub-microsecond remainder).

The bit work lives in a small per-core `uuid` module (`uuid.rs`/`uuid.go`/`uuid.ts`), kept
separate from value.rs's text rendering/parsing; the resolver/eval wire it like any scalar
function. Cost is the uniform one `operator_eval` per call.

**The `volatility` field** (catalog schema_version 2). The catalog grows an optional
`volatility` column — PostgreSQL's class, `immutable | stable | volatile`, absent ⇒
`immutable`. Every existing operator/function is `immutable` (and stays so by default, no
re-authoring); the generators are `volatile`; `now()`/`current_timestamp` (the clock seam) is
`stable` and `clock_timestamp()` is `volatile` (below).
It marks a call non-foldable for a future constant-folding/CSE pass. It is **advisory today** —
no such pass exists yet — the same posture §8's `cost` field held before it went live: the spec
states the truth at the point the function is added, and the optimizer slice that needs the
data finds it already there (`cost` has since become live — §8; `volatility` still awaits its
pass). `verify.rb` validates the value set; `gen_catalog.rb` emits it (default `immutable`) into
the descriptor table each core reads.

### Current-time functions — `now()` / `current_timestamp` / `clock_timestamp()`

Three niladic `timestamptz` functions on the host-injected **clock seam**
([entropy.md](entropy.md) §5; the seam's micros are exactly timestamptz's internal representation,
so the value passes straight through). They are oracle-*incompatible* by nature (PG's wall clock
differs), so they are NOT oracle-imported; the corpus pins them under an injected clock
(`suites/expr/clock_functions.test`).

- **`now()`** — **STABLE**. Reads the **statement clock** ([entropy.md](entropy.md) §5): the seam is
  read **once per statement** (`StmtRng.statement_clock_micros`) and reused for every row, so a
  statement's time cannot vary row-to-row (PG's `now()` / `transaction_timestamp()` semantics; jed
  has no cross-statement transaction yet, so statement scope is the whole of it). Rendered UTC with
  the `+00` suffix (jed's timestamptz rendering — a documented PG divergence vs. session-tz display).
- **`current_timestamp`** — the SQL-standard **bare keyword** (no parens), reserved like the
  `true`/`false`/`null` value literals. Pure **parser sugar**: it desugars to a `now()` call node, so
  resolution / execution / cost / volatility / the default `now` column label are all shared. (No
  catalog entry, resolver branch, or executor path of its own. The precision-typmod form
  `current_timestamp(p)` is deferred — a `(` after it falls through to ordinary resolution, 42883.)
- **`clock_timestamp()`** — **VOLATILE**. Reads the seam on **every** call (`StmtRng.clock_now_micros`,
  a fresh read that does *not* touch the statement-clock cache), so it may advance within a statement.
  The reads follow expression-evaluation order. Tested with an injected **advancing** clock
  (the `# clock_advance: start,step` directive, [entropy.md](entropy.md) §6) so the per-call advance
  is deterministic and distinguishable from `now()` cross-core.
- **`current_date`** — the fourth clock reader, returning **`date`**: the statement clock's day in
  the **session zone** — the `'today'` clock-relative literal as a function ([date.md §6](date.md)).
  **STABLE**, sharing `now()`'s once-per-statement clock read, plus one `timezone` unit for the
  zone decomposition. Unlike `current_timestamp` it desugars to its **own** catalog function
  (`current_date()`, default label `current_date`), so the explicit call spelling also resolves —
  where PG rejects it as a syntax error (a documented jed-lenient divergence). Un-indexable
  (`42P17`), like every clock reader.

Each charges the uniform one `operator_eval` per call (independent of the clock value; the
session-zone decomposers add their `timezone` unit). The clock reads are class-**B**
determinism-ledger entries (`now-clock`, `clock-timestamp-clock`, `date-clock-literal`).

## 13. Purity — the built-in surface is safe for untrusted queries

Untrusted SQL is safe to run (CLAUDE.md §13), and this catalog is one of the three pillars
that guarantee it: **the engine provides no built-in that can do bad things.** Every entry
here is **pure and side-effect-free** — it is a total (or NULL-/error-returning) mapping from
input values to an output value, and it touches **nothing else**:

- **No host reach.** No built-in reads or writes the filesystem (no `pg_read_file` /
  `lo_import` / `COPY … TO/FROM` analogue), opens a socket, spawns a process, reads the
  environment, or otherwise escapes the engine. The query surface is curated; an escape hatch
  is **never added**, not merely gated.
- **No hidden state, with one curated exception.** A built-in does not mutate engine state
  outside the value it returns — so evaluation order among side-effect-free nodes is unobservable
  (which is also what lets the planner reorder freely). The **lone exception** is the sequence
  generators `nextval`/`setval` (§14, [sequences.md](sequences.md)), which mutate **in-database
  sequence state** — never host state. They remain untrusted-safe by the §13 criteria: the mutation
  is deterministic (transactional, no seam — sequences.md §5), cost-bounded (the `sequence_advance`
  unit + `max_cost`), and gated to the write path (`25006` on a read-only handle), exactly like the
  `INSERT`/`UPDATE` mutations a query surface already exposes. A query that calls `nextval` advances a
  counter inside the database it is querying; it cannot reach outside it. So "side-effect-free" is
  the rule for *value* functions; the sequence generators are *mutation* functions, curated and
  bounded, not an escape hatch.
- **No unsanctioned nondeterminism.** The **only** window onto the outside world is the
  host-injected **entropy/clock seam** (§12, [entropy.md](entropy.md)): `uuidv4`/`uuidv7` read
  entropy, `now()`/`clock_timestamp()` read the clock, and **nothing else** does. These are
  deterministic-given-the-seam (class-A/B determinism-ledger entries,
  [determinism.md](determinism.md)) and read *only* entropy + the clock — never arbitrary host
  state. There is no general clock/random/locale/PID/host-info built-in beyond them.

This makes purity a **standing rule on catalog growth**, alongside the §8 growth rule and the
§1 "stay descriptive" rule: a proposed function that performs I/O, reaches the host, or
introduces nondeterminism outside the seam **does not belong in the built-in set**. The rule
binds *built-ins only*. **Host/application-supplied functions are explicitly out of scope** —
the engine cannot know what host code does, so a host that registers a function and exposes it
to untrusted queries owns that risk (CLAUDE.md §13); the meter's only structural defense is
the host-function cost contract ([cost.md](cost.md) §6).

This guarantee is pinned as a **conformance regression gate** —
[../conformance/suites/resource/no_escape_hatch.test](../conformance/suites/resource/no_escape_hatch.test),
gated by the `resource.pure_builtins` capability — asserting that the classic PostgreSQL escape
hatches stay absent: a host-reaching function call (`pg_read_file`, `lo_import`, `pg_sleep`,
`current_setting`, `dblink`, `random`, …) is `42883` (undefined_function) and a host-reaching
statement (`COPY … TO/FROM`, `CREATE FUNCTION`, `DO`, `LOAD`, `CREATE EXTENSION`, `SET`, …) is
`42601` (syntax_error). Because the surface is curated (an escape hatch is *never added*), the test
is a tripwire: introducing any of them flips exactly one line from error to ok. It is jed-specific
(PG provides these), so it is not oracle-checked.

## 14. Sequence value functions (`nextval` / `currval` / `setval` / `lastval`) — the stateful built-ins

`nextval('s')` / `currval('s')` / `setval('s', n[, b])` / `lastval()` ([sequences.md](sequences.md))
are the built-ins that (a) resolve a **text argument to a catalog object** (all but `lastval`) and
(b) reach beyond a pure value→value map — `nextval`/`setval` **mutate** the sequence counter (§13's
curated exception). All are `kind = "function"`, `result = "i64"`, `null = "propagates"`,
`volatility = "volatile"`.

- **`nextval('s')`** advances sequence `s` and returns the new i64 value (PG-exact: the first call
  returns `START`, subsequent calls add `INCREMENT`, bounded by `MIN/MAXVALUE` → `2200H` or `CYCLE`
  wrap). Because it mutates, a statement containing it runs on the **write path** (so `SELECT
  nextval('s')` commits a new snapshot) and is `25006` in a read-only transaction. The advance is
  **transactional** — it rolls back with its transaction, jed's documented divergence from PG's
  non-transactional sequences (sequences.md §5, mandated by [determinism.md §5](determinism.md)).
- **`currval('s')`** returns the value `nextval('s')`/`setval('s', n)` last produced **in this
  session** (a per-handle state read, not a snapshot read), `55000` before the first such call this
  session. It is pure-read (no write path).
- **`setval('s', n)`** sets `s`'s counter so the next `nextval` returns `n + INCREMENT`, returns `n`,
  and (like `nextval`) updates `currval`; **`setval('s', n, is_called)`** with `is_called = false`
  makes the next `nextval` return `n` and leaves `currval` untouched. A value outside `[MINVALUE,
  MAXVALUE]` is `22003`. `setval` is a write (transactional, `25006` in a read-only txn) but, unlike
  `nextval`, does **not** update `lastval` (PG; §6). Two overloads (arity 2 / arity 3), like `round`.
- **`lastval()`** returns the value the most recent `nextval` (of **any** sequence) produced in this
  session, `55000` before the first `nextval`; a pure session read (no name argument, no write path),
  unaffected by `setval`. It is the first 0-arg sequence function (the `now()` precedent).
- A missing sequence is `42P01`; a NULL argument propagates NULL. The name argument is the bare
  sequence name (the PG `'s'::regclass` form, regclass implicit).

These stay within the §13 untrusted-query guarantee: no host reach, deterministic, and cost-bounded
(`nextval`/`setval` each charge one `sequence_advance` unit, [cost.md](cost.md)).
