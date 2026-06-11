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
| `comparison` | `=` `<` `>` `<=` `>=` | `boolean` |
| `comparison` (NULL-safe) | `IS DISTINCT FROM`, `IS NOT DISTINCT FROM` | `boolean` |
| `null_test` | `IS NULL`, `IS NOT NULL` | `boolean` |
| `arithmetic` | `+` `-` `*` `/` `%`, unary `-` | `promoted` |
| `function` (scalar) | `abs` `round` | `promoted` / `decimal` (§9) |
| `aggregate` | `COUNT` `SUM` `MIN` `MAX` `AVG` | `int64` / `decimal` / widened (§8) |

The **aggregates** are not operators — they collapse a set of rows into one value, have no
infix symbol or precedence, and widen their result type by the operand type — so they live
in a **separate `[[aggregate]]` array** with their own field set, not as `[[operator]]`
rows (§8, [aggregates.md](aggregates.md)). `<>` / `!=` are deliberately absent — they do
not exist in the engine (see
[grammar.md](grammar.md) §4). The catalog must stay descriptive: it must not list an
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

- `promoted` — the common promoted operand type of an arithmetic operator: `int16 + int16
  → int16`, `int16 + int64 → int64` (the higher-`rank` operand wins, per the promotion
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

A single comparison operator accepts many operand pairs: `int16 = int64`, `int32 < int16`,
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
type, so `int16 + int16` overflows at the `int16` boundary (`30000 + 30000` traps `22003`),
not at `int64`'s — the type-faithful behavior, matching PostgreSQL's `smallint + smallint`.
This is a deterministic, cross-core-identical decision (CLAUDE.md §8): every core computes
in 64-bit, traps `22003` if the 64-bit op itself overflows, *and* traps `22003` if the
in-range 64-bit result falls outside the declared result type. `div`/`mod` additionally
trap `22012` on a zero divisor (a defined, deterministic abort, not a wrapped value).

## 8. Deferred fields and the growth rule

One field is designed but **deliberately not authored yet**, so its absence is intentional:

- `cost` — a **per-operator** evaluation cost. The cost-accounting seam has now landed
  (CLAUDE.md §13): the whole unit schedule is authored as data in
  [../cost/schedule.toml](../cost/schedule.toml) — storage row reads, rows produced, and a
  single **uniform `operator_eval`** weight — with the *why* in
  [../design/cost.md](../design/cost.md). The per-operator `cost` field **here** stays
  **reserved** as the additive tuning hook: the evaluator charges the uniform `operator_eval`
  for every operator this slice, and authoring per-operator weights later (the evaluator
  preferring an operator's own `cost`, falling back to `operator_eval`) is a pure data-only
  change. The seam was designed as one coherent schedule, not as a per-operator constant
  bolted on here — adding a field to a data table later is cheap; designing the seam in
  fragments is not.

Reserved values and kinds still to be authored spec-first with their own executor slices
([../../TODO.md](../../TODO.md)):

- The `function` kind is now **partly authored**: the first scalar functions, `abs` and
  `round`, landed as `[[operator]]` rows with `kind = "function"` (§9). Further scalar
  functions — `ceil`, `floor`, `mod`, `sign`, the text `length`/`lower`/`upper`, and the
  like — are follow-on slices that reuse the same mold.

**Aggregates are authored (`kind = "aggregate"`).** `COUNT`/`SUM`/`MIN`/`MAX`/`AVG` landed
in a **separate `[[aggregate]]` array**, not as `[[operator]]` rows, because they do not fit
the operator mold on three counts: (1) the **result widens by the operand type** —
`SUM(int16/int32) → int64`, `SUM(int64) → decimal`, `MIN/MAX → the input type` — expressed
by two reserved result ids, `sum_widen` and `same_as_input`, alongside the concrete `int64`
/`decimal` (`COUNT → int64`, `AVG → decimal`); (2) a fifth **NULL discipline**, `aggregate`
— NULL inputs are *skipped* (except `COUNT(*)`, which counts every row), and an empty or
all-NULL group yields `NULL` for `SUM/AVG/MIN/MAX` but `0` for `COUNT`; (3) **`COUNT(*)`
takes no expression** (`arg = "star"`), and there is no infix symbol, precedence, or
`arg_resolution`. The coherence checker validates aggregates on a separate branch
([../functions/verify.rb](../functions/verify.rb)), and the codegen emits a separate
`AGGREGATES` descriptor table. The full semantics — the widening table, the empty-set
rules, the `GROUP BY` / `HAVING` rules, the cost accrual — live in
[aggregates.md](aggregates.md). DISTINCT inside an aggregate is deferred (rejected `42601`).

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
type is just the operand's own type, so `abs(int16) → int16`, `abs(int64) → int64`,
`abs(numeric) → numeric` (exactly as unary `neg`, §7). Over the integer family the magnitude
is **range-checked at the result type's boundary**: `abs(int16 -32768)` has no positive
`int16` and traps `22003` — the same overflow discipline as `-(int16 -32768)` — so `abs`
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
cannot (an int64 is at most 19 digits). `round`'s scale argument clamps to `max_scale`
(16383) like PG `numeric_round`.

**Cost.** A scalar-function call charges one `operator_eval` ([cost.md](cost.md)) — the same
uniform per-evaluation weight every operator charges — with its arguments charging their own
costs recursively. The cost is deterministic and cross-core-identical (CLAUDE.md §13), and
is asserted in the conformance corpus alongside the rows.
