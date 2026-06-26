# Aggregate functions, `GROUP BY`, and `HAVING` — design

> The reasoning behind aggregation. The **data + grammar are authoritative**
> ([../functions/catalog.toml](../functions/catalog.toml) — the `[[aggregate]]` rows;
> [../grammar/grammar.ebnf](../grammar/grammar.ebnf) — `function_call`, and later
> `group_by` / `having`); this doc is the *why* and — because there is no reference
> implementation (CLAUDE.md §2) — the precise **cross-core contract** every core must
> reproduce: result types, NULL / empty-set behavior, grouping, and cost. When a decision
> here changes, change the data/grammar and here in the same edit.

An **aggregate** collapses a *set* of rows into a single value: `COUNT(*)` counts them,
`SUM(x)` totals a column, `AVG(x)` averages it, `MIN`/`MAX` take the extremes. This is the
engine's first construct that is neither per-row (an expression) nor structural (a clause)
but a *fold over rows*, and the first **function-call** syntax ([grammar.md](grammar.md)
§17). PostgreSQL is the behavioral default (CLAUDE.md §1).

## 1. Role, scope, and staging

The five aggregates are `COUNT`, `SUM`, `MIN`, `MAX`, `AVG`
([catalog.toml](../functions/catalog.toml), `kind = "aggregate"`). They landed in **three
vertical slices** (CLAUDE.md §10/§11), each a commit across all three cores — all now landed:

1. **Scalar aggregates with no `GROUP BY`** — the function-call syntax and whole-table
   aggregation: the entire (post-`WHERE`) result is one group → exactly one result row.
   This is what §2–§9 below describe.
2. **`GROUP BY`** — one result row per distinct grouping-key combination, and the
   grouping-error rule (§10, [grammar.md](grammar.md) §18).
3. **`HAVING`** — a boolean filter over grouped rows (§10, [grammar.md](grammar.md) §19).

Locked scope decisions: **PostgreSQL widening** for `SUM`/`AVG` (§3); **`DISTINCT` inside an
aggregate** (`COUNT(DISTINCT x)`) has **landed** (§5) — the aggregate folds only the distinct
non-NULL argument values; **`FILTER (WHERE cond)`** has **landed** (§11) — the aggregate folds
only the input rows for which `cond` is TRUE; **`GROUPING SETS` / `ROLLUP` / `CUBE`** and the
**`GROUPING()`** function have **landed** (§12) — one `GROUP BY` names several grouping sets at once.

## 2. What an aggregate computes

An aggregate consumes the **post-`WHERE`** rows of its group and, for the
expression-argument forms, the argument evaluated **per row**. **NULL inputs are skipped**
— the one exception is `COUNT(*)`, which counts every row regardless of NULLs. Over an
**empty or all-NULL** group, `COUNT` returns `0` and `SUM`/`AVG`/`MIN`/`MAX` return `NULL`
(the PostgreSQL behavior). The per-aggregate contract:

| Aggregate | Argument | Result type | NULL inputs | Empty / all-NULL group |
|---|---|---|---|---|
| `COUNT(*)` | (none) | `i64` | counts every row | `0` |
| `COUNT(expr)` | any | `i64` | skipped (counts non-NULL) | `0` |
| `SUM(i16\|i32)` | integer | `i64` | skipped | `NULL` |
| `SUM(i64)` | integer | `decimal` | skipped | `NULL` |
| `SUM(decimal)` | decimal | `decimal` | skipped | `NULL` |
| `AVG(int\|decimal)` | numeric | `decimal` | skipped | `NULL` |
| `MIN`/`MAX` | any ordered | the input type | skipped | `NULL` |

`SUM`/`AVG` require a **numeric** argument (integer or decimal); a non-numeric argument is a
`42883` (no matching aggregate overload) — the catalog has no `SUM(text)`. `MIN`/`MAX` and
`COUNT(expr)` accept **any** scalar (`arg_families = ["any"]`): `MIN`/`MAX` order by the
argument family's comparison rule (integers numeric, text C-collation, decimal by value,
boolean `false < true`, bytea unsigned-byte — [../types/compare.toml](../types/compare.toml)),
and `COUNT` only tests NULL-ness.

## 3. The `SUM` / `AVG` widening (the PostgreSQL model)

Result types are a **function of the operand type**, the reason aggregates need the reserved
`sum_widen` / `same_as_input` result ids ([functions.md](functions.md) §8):

- **`SUM(i16)` and `SUM(i32)` → `i64`.** The running sum accumulates in `i64`; a
  sum that exceeds `i64` traps `22003`. The trap boundary is the **result** type, not the
  input width — `SUM` over many `i32`s that exceeds `i32` but fits `i64` does **not**
  trap, mirroring the arithmetic rule ([functions.md](functions.md) §7).
- **`SUM(i64)` → `decimal`.** Summing 64-bit values overflows `i64` readily, so PG
  widens to `numeric`; jed matches. The running sum accumulates in the exact `decimal`
  domain (each `i64` widened to `decimal` scale 0); it traps `22003` only when the **final**
  result exceeds the decimal cap — never an intermediate ([decimal.md](decimal.md) §2).
- **`SUM(decimal)` → `decimal`**, scale carried exactly (no rounding); traps `22003` at the
  cap of the **final** result only (the order-independent `add_uncapped` fold —
  [decimal.md](decimal.md) §2, [determinism.md](determinism.md) §7), never an intermediate sum.
- **`AVG(any numeric)` → `decimal`**, computed as **`sum::decimal / count::decimal`** — the
  running sum is **always** accumulated in `decimal` (even for integer input), then divided
  by the integer count widened to `decimal`. The quotient's scale follows the exact decimal
  division `select_div_scale` and **half-away-from-zero** rounding ([decimal.md](decimal.md)
  §4) — the single hardest cross-core function. Worked: `AVG` of integers `1, 2` is `3 / 2`,
  where `select_div_scale` gives `rscale = 16`, so the result renders `1.5000000000000000`;
  `AVG` of `10, 20, 30` is `60 / 3 = 20.0000000000000000`. `AVG` over an empty/all-NULL
  group is `NULL` (count `0`, no division).

`COUNT` is always `i64`. Every core must widen **identically** — this is a CLAUDE.md §8
divergence hotspot, asserted in the corpus.

## 4. Whole-table aggregation (no `GROUP BY`) — the single-row result

When the select list contains an aggregate and there is **no `GROUP BY`**, the entire
(post-`WHERE`) result is **one group**, so the query produces **exactly one result row** —
*even over an empty table*. `SELECT COUNT(*) FROM empty` is `0` (one row), and `SELECT
SUM(x), MIN(x) FROM empty` is `NULL NULL` (one row), never the empty result. (This is the
key contrast with `GROUP BY` over an empty table, which produces **zero** rows — slice 2,
§10.)

Because there is exactly one output row and no grouping keys, **a non-aggregated column in
the select list is illegal**: `SELECT a, COUNT(*) FROM t` is `42803` (`grouping_error`) —
`a` has no single value over the group. This is the degenerate case of the general
grouping-error rule (§10): with no `GROUP BY`, the set of legal "grouping keys" is empty, so
*only* aggregates and constants may appear outside an aggregate. A literal is fine
(`SELECT 1, COUNT(*) FROM t`), and aggregates may be combined with constants and operators
(`SELECT SUM(x) + 1, COUNT(*) * 2 FROM t`).

This rule also covers the **FROM-less** aggregate ([grammar.md](grammar.md) §34): the input
is the one virtual row, so `SELECT COUNT(*)` is `1`; with a false `WHERE` the single group
still emits (`COUNT` → `0`, the others → `NULL`), exactly the empty-table case above.

## 5. Function-call syntax (see [grammar.md](grammar.md) §17 for the full rule)

`function_call ::= identifier "(" "DISTINCT"? ( "*" | expr ) ")"`. Only the five aggregate names
resolve; an unknown name is **`42883`**. `COUNT(*)` is the row counter (`*` is accepted only
by `COUNT`). Aggregate names are **not reserved** — a one-token lookahead (bare identifier
immediately followed by `(`) distinguishes `count(*)` the aggregate from `count` the column,
byte-identically across cores.

### `DISTINCT` inside an aggregate (`COUNT(DISTINCT x)`)

A leading `DISTINCT` makes the aggregate fold only the **distinct** argument values: `COUNT(DISTINCT
x)` counts how many distinct non-NULL values of `x` the group has; `SUM`/`AVG(DISTINCT x)` total /
average the distinct values; `MIN`/`MAX(DISTINCT x)` are unchanged (de-duplication does not move the
extremes, but the form is accepted for symmetry with PostgreSQL). De-duplication runs **before** the
fold, **value-canonically** — the same `Eq`/`Hash` the group-key bucketing uses, so `1.5` and `1.50`
are one value and `-0.0` and `+0.0` are one value — keeping the **first occurrence in scan order**.
NULL arguments are skipped exactly as without `DISTINCT` (so an all-NULL / empty distinct group is
`COUNT` `0` and `SUM`/`AVG` `NULL`). It composes with `GROUP BY` (de-duplication is per group).

The deliberate restrictions, all matching PostgreSQL (oracle-verified):

- **`DISTINCT` on a window function** is **`0A000`** ("DISTINCT is not implemented for window
  functions") — a window aggregate folds over a *frame*, where per-frame de-duplication is undefined.
- **`DISTINCT` on a non-aggregate (scalar) function** is **`42809`** (`wrong_object_type`, "DISTINCT
  specified, but *f* is not an aggregate function").
- **`agg(DISTINCT *)`** and **`agg(DISTINCT)`** (no argument) are **`42601`** syntax errors —
  `DISTINCT` cannot combine with `*` and requires an argument.
- A wrong argument count stays **`42883`** (`COUNT(DISTINCT a, b)` matches no `count(a, b)` overload),
  `DISTINCT` or not.

**Cost** (the cross-core contract, §8): `aggregate_accumulate` is still charged per `(input row ×
aggregate)` — the argument is evaluated per row to *know* the value to de-duplicate — and only the
actual **fold** (and any `decimal_work` it would charge) is skipped for a duplicate. Because the
first-occurrence set is deterministic (scan order is cross-core identical), the metered cost is
deterministic and identical across cores.

## 6. Where an aggregate may not appear (`42803`)

An aggregate is a fold over a *set* of rows, undefined for a single input row, so it is
rejected in any context that runs **before** grouping or per input row:

- **`WHERE`** — filters input rows before grouping; `SELECT COUNT(*) FROM t WHERE SUM(a) >
  0` is `42803`. (Filtering on an aggregate is `HAVING`'s job — slice 3.)
- **A `JOIN ON`** — same reason, same code.
- **Nested in another aggregate** — `SUM(COUNT(x))` is `42803`.
- **A `GROUP BY` key** (slice 2) — likewise.

PostgreSQL raises SQLSTATE class `42803` (`grouping_error`) for these; jed matches. Matching
is on the **code**, not the message ([conformance.md](conformance.md) §2), so the single
code covers all sites with site-specific message detail.

## 7. NULL handling — the contract

`COUNT(*)` counts rows; **every other aggregate skips NULL arguments**. Concretely, for a
column with some NULLs: `COUNT(c) < COUNT(*)`; `SUM`/`AVG` ignore the NULL rows (an all-NULL
column sums to `NULL`, not `0`); `MIN`/`MAX` ignore NULLs and return `NULL` only when **every**
input is NULL (or the group is empty). This is the standard SQL aggregate NULL rule and the
catalog's `null = "aggregate"` discipline ([functions.md](functions.md) §8). NULL skipping is
*not* three-valued logic — it is "is this argument NULL? then don't fold it."

## 8. Cost accrual (the cross-core contract — [cost.md](cost.md) §3)

Aggregation adds one cost unit, `aggregate_accumulate` (weight 1,
[../cost/schedule.toml](../cost/schedule.toml)), and otherwise reuses the existing units:

- **`storage_row_read`** — per scanned input row, unchanged (the scan is upstream of the
  aggregation stage).
- **The aggregate argument's `operator_eval`s** — charged **per input row**, because the
  argument is evaluated once per row before being folded (a bare-column argument is a leaf
  and charges nothing, like any projection of a bare column). `COUNT(*)` has no argument and
  charges no argument eval.
- **`aggregate_accumulate`** — charged once per `(input row × aggregate)` folded into a
  group. A query with `M` aggregates over `N` post-`WHERE` rows accrues `N × M`.
- **`row_produced`** — per **emitted group row** (one, for whole-table aggregation; one per
  surviving group with `GROUP BY`). Projection `operator_eval`s of the output expressions
  are charged **per emitted group row** (the synthetic row, §9).
- **Unmetered**: the bucketing/hash-insert, and the **finalize** step (including `AVG`'s
  division and the `SUM` widening) — like the `ORDER BY` sort and the `DISTINCT` dedup.

So whole-table `SELECT COUNT(*) FROM t` over `N` rows is `N` (`storage_row_read`) `+ N`
(`aggregate_accumulate`) `+ 1` (`row_produced`) = `2N + 1`. Over an empty table it is `1`
(the one produced row; no scans, no accumulate).

## 9. Determinism (CLAUDE.md §8/§10)

- **The synthetic row.** The resolver splits an aggregate query's select list (and, later,
  `HAVING`) into a flat list of **aggregate specs** plus output expressions that reference
  the computed aggregate results positionally — so the *existing* expression evaluator
  projects the result with no new node type. The aggregate results (and, with `GROUP BY`,
  the grouping-key values) form one synthetic row `[group_key…, agg_result…]` the output
  expressions resolve against by flat index.
- **`AVG` division scale** — the highest cross-core risk: it flows through `select_div_scale`
  + half-away rounding ([decimal.md](decimal.md) §4, §7.2), pinned by the corpus with exact
  rendered strings.
- **`SUM` overflow boundary** — at the **result** type (i64 for the i16/i32 case, the
  decimal cap for the i64/decimal cases); pinned with a value that widens without trapping
  and one that traps.
- **Group ordering / value-canonical keys** — with no `ORDER BY`, group **emission order is
  unspecified** (the corpus compares `rowsort` or adds an explicit `ORDER BY`); the grouping
  itself is deterministic, keyed by the **value-canonical** form so `1.5` and `1.50` share one
  group and `NULL` is its own group ([decimal.md](decimal.md) §5). No hash-map iteration order
  may leak into the *grouping* (which rows group together, the per-group aggregates) — every
  core iterates an explicit insertion-ordered list, never a map — so that result is
  byte-identical cross-core even though emission order is free.

## 10. Staging & deferred

- **`GROUP BY`** (landed) — partitions the post-`WHERE` rows by one or more grouping keys —
  a bare/qualified **column**, a select-list **ordinal**, an output **alias**, or a general
  **expression** (§15) — emitting one row per distinct key
  combination. The **grouping-error rule** ([grammar.md](grammar.md) §18): every
  non-aggregated column in the select list (and `ORDER BY`) must be a grouping key, else `42803`. `NULL` forms its own
  group; decimal keys bucket value-canonically (the displayed key is the first occurrence's
  value). `GROUP BY` over an empty table → zero rows. **`ORDER BY` over the grouped output**
  resolves each key against the grouping keys — a grouping column sorts the group rows
  (after aggregation, before `LIMIT`/`OFFSET`); a non-grouping column is `42803`
  ([grammar.md](grammar.md) §18). **`SELECT DISTINCT` in an aggregate query** (landed) dedups the
  projected grouped output rows (§14).
- **`HAVING`** (landed) — a boolean predicate over grouped rows (§8), evaluated after
  aggregation and before `ORDER BY`; may reference aggregates (even ones not projected — they
  collect into the same synthetic row) and grouping keys; a non-grouped column is `42803`,
  non-boolean is `42804`. Allowed with no `GROUP BY` (filters the single whole-table group),
  and HAVING alone makes a query an aggregate query ([grammar.md](grammar.md) §19).
- **`FILTER (WHERE cond)`** (landed) — restricts which input rows feed an aggregate (§11). On a
  **window** aggregate it is deferred (`0A000`): a pure non-aggregate window function with `FILTER`
  matches PG's own `0A000`, and a window aggregate with `FILTER` (which PG allows) is deferred here
  to a follow-on, a documented divergence.
- **`GROUPING SETS` / `ROLLUP` / `CUBE`** (landed) — one `GROUP BY` computes several grouping sets
  at once, and the `GROUPING()` function reports which columns a row was grouped by (§12).
- **Ordered-set aggregates** (landed) — `mode`, `percentile_cont`, `percentile_disc`, computed over
  the rows sorted by a `WITHIN GROUP (ORDER BY …)` clause (§13).
- **`GROUP BY` by ordinal / alias / expression** (landed) — a grouping key may be a select-list
  ordinal, an output alias, or a general expression, not just a column (§15).
- **Functional-dependency grouping** (landed) — `GROUP BY` a base table's full primary key lets any
  column of that table appear ungrouped (§16).
- **Deferred / out of scope**: **`FILTER` on
  a window aggregate**; **`GROUPING SETS` combined with window functions**; **hypothetical-set
  aggregates** (`rank`/`dense_rank`/`percent_rank`/`cume_dist` `WITHIN GROUP`); and the
  **array-valued** `percentile_cont`/`percentile_disc` fraction form. Each is an additive later
  feature ([../../TODO.md](../../TODO.md)).

## 11. `FILTER (WHERE cond)` — restricting an aggregate's input rows

`agg(args) FILTER (WHERE cond)` folds **only the input rows for which `cond` is TRUE** into that
aggregate (PostgreSQL / SQL-standard). It is a per-aggregate restriction: each aggregate in the
select list (and `HAVING`) carries its own optional filter, applied independently within each
group. `cond` is an ordinary boolean expression over the **input** row (the same scope an aggregate
argument resolves in), evaluated **per row**; a `FALSE` **or `NULL`** result excludes the row (only
`TRUE` keeps it — the WHERE-clause rule, §6 of [grammar.md](grammar.md)). A group whose every row is
filtered out is therefore `COUNT` `0` and `SUM`/`AVG`/`MIN`/`MAX` `NULL`, exactly like an empty
group (§4).

`FILTER` composes with everything aggregation already does: it works whole-table and per `GROUP BY`
group, inside `HAVING` (`HAVING count(*) FILTER (WHERE …) > 1`), and with `DISTINCT` — the **filter
applies first** (restricting the rows), **then** the `DISTINCT` de-duplication (§5), then the fold.

The restrictions, all matching PostgreSQL (oracle-verified):

- **A non-boolean `cond`** is **`42804`** (`datatype_mismatch`, "argument of FILTER must be type
  boolean") — like a non-boolean `WHERE`.
- **An aggregate inside `cond`** is **`42803`** (`grouping_error`, "aggregate functions are not
  allowed in FILTER") — `cond` is a per-input-row predicate, evaluated before aggregation, so the
  filter resolves with aggregates **forbidden**.
- **`FILTER` on a non-aggregate (scalar) function** is **`42809`** (`wrong_object_type`, "FILTER
  specified, but *f* is not an aggregate function").
- **`FILTER` on a window function** is **`0A000`**: a pure non-aggregate window function matches PG's
  own "FILTER is not implemented for non-aggregate window functions"; a window **aggregate** with
  `FILTER` is allowed by PostgreSQL but deferred here (a documented divergence — §10).

**Cost** (the cross-core contract, §8): the filter is evaluated **per input row** (like the
operand), so its own `operator_eval`s are charged per row; `aggregate_accumulate` **and** the
operand's own evaluation are charged **only for a row that passes** the filter (a filtered-out row
contributes nothing, so it accrues no accumulate — contrast `DISTINCT`, where every row accumulates
and only duplicates skip the fold, §5). Because the pass/fold decision is deterministic (scan order
is cross-core identical), the metered cost is deterministic and identical across cores.

## 12. `GROUPING SETS` / `ROLLUP` / `CUBE` and `GROUPING()` — multiple grouping sets

A plain `GROUP BY` partitions the rows by **one** set of columns. `GROUP BY GROUPING SETS (...)`
names **several** at once: each *grouping set* is grouped independently and the results are unioned
(PostgreSQL / SQL-standard). `ROLLUP` and `CUBE` are sugar for common families of sets:

- `GROUP BY GROUPING SETS ((a), (b), ())` → the per-`a` groups, the per-`b` groups, and the
  whole-table grand total, in one result.
- `GROUP BY ROLLUP (a, b)` ≡ `GROUPING SETS ((a, b), (a), ())` — the hierarchical subtotals: the
  prefixes of the column list, longest first down to the empty set (n+1 sets).
- `GROUP BY CUBE (a, b)` ≡ `GROUPING SETS ((a, b), (a), (b), ())` — **every** subset of the columns
  (2ⁿ sets).
- A plain term **cross-products** with the grouping-set forms: `GROUP BY a, GROUPING SETS ((b), (c))`
  ≡ `GROUPING SETS ((a, b), (a, c))`, and several grouping-set clauses multiply. `ROLLUP`/`CUBE` may
  also nest inside `GROUPING SETS`. Each element is a **column group** — a bare column, a
  parenthesized `(a, b)`, or the empty `()` — never an expression/ordinal/alias (the same narrowing
  as plain `GROUP BY`, §10).

**The master grouping columns.** The resolver expands the clause to a flat list of grouping sets and
takes the **ordered union** of every set's columns — a column is *groupable* if it appears in **at
least one** set. A non-aggregated select-list / `ORDER BY` / `HAVING` column must be one of these
master columns, else `42803` (the §6 grouping-error rule, widened to the union). In a row produced by
a grouping set that does **not** include a given master column, that column projects as **NULL** (it
was grouped away, not an actual data NULL — that ambiguity is what `GROUPING()` resolves).

**Empty input / empty set.** An **empty grouping set** `()` always emits **one** row (the grand
total — `COUNT` 0, others NULL, like whole-table aggregation §4), *even over an empty table*; a
**non-empty** set over an empty input emits **nothing**. **Duplicate** grouping sets are **kept**
(`GROUPING SETS ((a), (a))` emits each group twice — PG). With no `ORDER BY` the row order is
unspecified (the corpus uses `rowsort` or an explicit `ORDER BY` over the master columns); the result
**multiset** is deterministic and byte-identical across cores (sets iterated in expansion order, groups
in first-occurrence order — CLAUDE.md §8/§10).

**`GROUPING(c1, …, ck)`** returns an `integer` bitmask reporting, for the grouping set a row came
from, which of its arguments were **grouped away**: bit `(k − 1 − j)` is `1` iff `c_j` is **not** in
that set (so it is NULL by grouping, not by data), `0` if actually grouped. `GROUPING(a)` is `0`/`1`;
`GROUPING(a, b)` is `2·GROUPING(a) + GROUPING(b)`. Each argument must be one of the master grouping
columns, else `42803`. `GROUPING(*)` and `GROUPING()` (no args) are syntax errors (`42601`), and
`GROUPING(...)` does not take `OVER` (`42601`) — it is not a window function. Internally each
`GROUPING()` call is a synthetic trailing column of the grouped row, its value computed per set from
the set's membership mask (no new evaluator node).

**Cost** (the cross-core contract, §8): the post-`WHERE` rows are scanned **once**
(`storage_row_read` unchanged); each row is then folded into **every** grouping set it belongs to, so
`aggregate_accumulate` and the operand `operator_eval`s accrue per `(grouping set × row ×
aggregate)`. `row_produced` is charged per emitted group row across all sets. The `GROUPING()`
finalize is unmetered (like the other finalize steps). Deterministic and identical across cores.

**Bounded expansion.** `CUBE (a, b, …)` of n columns is 2ⁿ grouping sets — an exponential blow-up
from tiny input. The total expansion is capped at **`MAX_GROUPING_SETS` = 4096**; beyond it the
statement aborts `54001` (`statement_too_complex`), jed's structural-complexity gate (the untrusted-
query resource bound, CLAUDE.md §13). This is a **deliberate divergence** from PostgreSQL, which caps
each construct instead (`CUBE is limited to 12 elements`, `54011`, a code jed's registry does not
define); jed bounds the uniform total (`CUBE(12)` = 4096 is fine, `CUBE(13)` = 8192 trips it).

**Window functions** combined with `GROUPING SETS` / `GROUPING()` are **deferred** (`0A000`) — both
want the grouped row's trailing synthetic slots; a single-grouping-set `GROUP BY` with a window
function is unaffected (§10).

## 13. Ordered-set aggregates (`WITHIN GROUP (ORDER BY …)`)

The five aggregates of §1–§9 are **order-independent** — `SUM`/`COUNT`/`AVG` fold commutatively and
`MIN`/`MAX` track an extreme — so a row's *position in a sort* never enters the result. An
**ordered-set aggregate** is the opposite: its result is *defined by* the sorted order of its input,
so the sort key is written explicitly as a `WITHIN GROUP (ORDER BY …)` clause attached to the call
(SQL-standard / PostgreSQL). jed ships PostgreSQL's three built-ins:

| Aggregate | Direct arg | `WITHIN GROUP` input | Result type | What it computes |
|---|---|---|---|---|
| `mode()` | (none) | any sortable | the input type | the most frequent input value |
| `percentile_cont(fraction)` | `f64` fraction | **numeric** | **`f64`** | the **continuous** percentile (interpolated) |
| `percentile_disc(fraction)` | `f64` fraction | any sortable | the input type | the **discrete** percentile (an actual input value) |

**Syntax** ([grammar.md](grammar.md) §17). `agg ( direct_args ) WITHIN GROUP ( ORDER BY sort_key )`,
where `sort_key` is a single key — a bare/qualified **column** *or* a general **expression**
(`ORDER BY a + b`, `ORDER BY abs(x)`) — with the ordinary `ASC`/`DESC` / `NULLS FIRST|LAST` suffix and
an optional **`COLLATE`** (`query.within_group_expr`, the same general-expression key as the query
`ORDER BY`, §10: a column key stays a leaf, any other expression is evaluated per row and the values
are sorted). The key's **collation** drives the sort (`query.ordered_set_collation`): an explicit
`COLLATE` (text operand only — else `42804`), else a text column key's frozen collation, else the
default byte (`C`) order — so for `mode` / `percentile_disc` over text the collation chooses which
tied value is the mode and which value the discrete percentile lands on. One PG divergence
from the query `ORDER BY`: a bare **integer** here is a **constant** (every row sorts equal), *not* an
ordinal — PostgreSQL treats a `WITHIN GROUP` integer as a constant. The `WITHIN GROUP` clause comes
between the argument list and any `FILTER (WHERE …)` / `OVER (…)`. **Exactly one** sort key is allowed
for all three (PG: a second key produces *"function mode(…, …) does not exist"*); a second key →
**`42883`**. An **aggregate** nested in the order key is **`42803`** (aggregates cannot be nested).

**The direct argument vs. the aggregated argument.** A `percentile_*` call has **two** argument
lists. The parenthesized **direct argument** (the fraction) is evaluated **once per group** (§17): it
may be any numeric expression over the **grouping columns** (a literal is the common case), resolved
in the grouped context — so a non-grouped column in it is `42803` — and evaluated against the group's
key values at finalize. The **aggregated argument** is the `WITHIN GROUP` `ORDER BY` key — a column
or a general expression — evaluated **per row** (it is the aggregate's operand). `mode()` has no
direct argument.

**NULL handling.** NULL aggregated values are **skipped**, exactly as for the §7 aggregates: a group
whose every input is NULL (or that is empty) yields **`NULL`** for all three. A **NULL fraction**
yields `NULL` (checked before the range check). Over a non-empty group the aggregated input is the
sorted multiset of non-NULL values; let `N` be its size.

**`mode()`** returns the value that occurs most often; a tie is broken by the **sort order** (the
first such value under the `WITHIN GROUP` ordering, so `ORDER BY x DESC` flips which tie wins). The
result is the input column's own type — `mode()` over `text` is `text`, over `i32` is `i32`.

**`percentile_disc(p)`** returns an **actual stored value** — no interpolation, so it works for any
sortable type and returns that type. Over the `1`-based sorted values it returns the value at row
`K = ceil(p · N)` (and row `1` when `p = 0`), i.e. the smallest `K` with `K/N ≥ p` (PostgreSQL
`orderedsetaggs.c`). Zero-based: `idx = max(0, ceil(p·N) − 1)`.

**`percentile_cont(p)`** **interpolates** between the two bracketing values. Over a **numeric** input
— `i16`/`i32`/`i64`/`decimal`/`f32`/`f64` — each value is widened to `f64` (the correctly-rounded
`decimal→f64` cast, [decimal.md](decimal.md); matching PG's implicit `numeric→float8`) **before** the
sort and the result is **`f64`** (PostgreSQL `float8`). Over an **`interval`** input the interpolation
is done in the **interval domain** (PG `interval_lerp` = `lo + (hi − lo)·pct`, where `interval_mul`'s
field cascade + microsecond `rint` rounding is replicated **byte-identically** — `round_ties_even`,
not half-away) and the result is **`interval`** (`query.ordered_set_interval`). Any other input type
is **`42883`** (no overload), matching PG.
The formula is PostgreSQL's exactly, computed in `f64` so it is **bit-identical** to PG (the same
in-contract determinism exception the window `percent_rank`/`cume_dist` ratios use —
[float.md](float.md) §7, [determinism.md](determinism.md), the `R` render tag):

```
pos    = p · (N − 1)
first  = floor(pos)          second = ceil(pos)
result = (first == second) ? val[first]
                           : val[first] + ((pos − first) · (val[second] − val[first]))
```

The lerp keeps PG's operation order (`lo + (proportion · (hi − lo))`); since each IEEE operation is
individually correctly-rounded and the cores share the operation sequence, the `f64` result is
byte-identical across cores and to PG. `percentile_cont` over a **single** row returns that row's
value; `p = 0`/`p = 1` return the min/max.

**Out-of-range fraction.** `p < 0`, `p > 1`, or `NaN` raises **`22003`** (`numeric_value_out_of_range`,
*"percentile value … is not between 0 and 1"*). Matching PG, the range check fires **per group at
finalize**, *after* the NULL-fraction check but *before* the empty-group check — so a whole-table
`percentile_cont(1.5) FROM empty` (one group) raises `22003`, while `… GROUP BY g` over an empty
table (zero groups) raises nothing.

**Composition.** Ordered-set aggregates compose with `GROUP BY`/`HAVING` (the sort + percentile is
per group) and with `FILTER (WHERE cond)` (the filter restricts the collected rows first, PG —
§11). `DISTINCT` inside the call is rejected (`42601`, PG). **`OVER (…)` is `0A000`** — PostgreSQL
itself does not support an ordered-set aggregate as a window function (*"OVER is not supported for
ordered-set aggregate …"*), so this **matches** PG, not a divergence. An aggregate inside the
`WITHIN GROUP` `ORDER BY` is **`42803`** (*"aggregate function calls cannot be nested"*, PG). A
`WITHIN GROUP` clause on a **non**-ordered-set function — an ordinary aggregate (`sum`) or a scalar
function — is **`42883`** (PG models it as a missing `sum(…, …)` overload), as is an ordered-set
aggregate used **without** `WITHIN GROUP` (PG: *"function mode() does not exist"*).

**Cost** (the cross-core contract, §8). The aggregated argument is evaluated **per row**
(`operator_eval`s) and `aggregate_accumulate` is charged per `(passing input row × aggregate)` — the
identical shape as an ordinary aggregate (a single ordered-set aggregate over `N` whole-table rows is
`N` `storage_row_read` + `N` `aggregate_accumulate` + `1` `row_produced`). The per-group **sort**, the
mode/percentile **finalize**, and the constant fraction evaluation are **unmetered**, like the
`ORDER BY` sort, the `DISTINCT` dedup, and `AVG`'s division (§8).

**Determinism.** The collected values are sorted by the same total order `ORDER BY`/`MIN`/`MAX` use
([../types/compare.toml](../types/compare.toml); `percentile_cont` sorts in PG's `float8` total
order), so the sorted multiset — and therefore mode's tie-break, the discrete index, and the
continuous interpolation — is byte-identical across cores. No hash-map iteration order enters the
result. Result `f64`s are the in-contract correctly-rounded exception (above).

## 14. `SELECT DISTINCT` in an aggregate query

`SELECT DISTINCT` is normally a post-projection dedup of an ordinary query's output rows
([grammar.md](grammar.md) §11). In an **aggregate** query it dedups the *grouped* output: after
`GROUP BY` / `HAVING` / the window stage produce the grouped rows and the query `ORDER BY` sorts
them, the rows are **projected** and **deduplicated by equality** keeping the **first occurrence**,
then `LIMIT`/`OFFSET` is applied — the exact `project → dedup → window` pipeline the non-aggregate
`DISTINCT` uses (§11 of [grammar.md](grammar.md)). So `SELECT DISTINCT count(*) FROM t GROUP BY a`
collapses repeated per-group counts to the distinct multiset of counts.

The **`DISTINCT` `ORDER BY` restriction** applies unchanged ([grammar.md](grammar.md) §11): once
duplicates are collapsed, every `ORDER BY` key must be a **select-list item** (a projected
column/ordinal, a structurally-matching expression, or an output alias), else `42P10` — matching
PostgreSQL (*"for SELECT DISTINCT, ORDER BY expressions must appear in select list"*). This is why
the dedup may run **after** the sort and still be correct: two grouped rows that project to the same
output tuple agree on every select-list item, hence on every order key, so they are adjacent after a
stable sort and the first-occurrence rule keeps exactly one — a sorted, deduplicated result. A `json`
column in the select list is still `0A000` (`json` has no equality — the §11 of [grammar.md](grammar.md)
rule, shared with the non-aggregate path).

**Cost** (the cross-core contract, §8). Every grouped row is **projected** (its projection
`operator_eval`s charged — the §3 asymmetry the non-aggregate `DISTINCT` shares: dedup must see every
row's projected value), and only an **emitted** (post-`LIMIT`) row charges `row_produced`. The dedup
itself is unmetered (like the `ORDER BY` sort and the `DISTINCT` set insert). Deterministic and
identical across cores (the output order comes from the grouped-row iteration / sort, never set
iteration — CLAUDE.md §8/§10). New capability `query.aggregate_select_distinct`.

## 15. `GROUP BY` by ordinal / output alias / general expression

The first `GROUP BY` slice grouped only by a bare/qualified **column** (the same narrowing the query
`ORDER BY` started with). PostgreSQL allows three more grouping-key forms, all landed here:

- **Ordinal** — `GROUP BY 1` names the **1-based select-list position**. Only a *bare integer
  literal* is an ordinal; `GROUP BY 1 + 1` is a constant **expression** (PG). An out-of-range
  position is `42P10` (*"GROUP BY position N is not in select list"*). Under `SELECT *` the ordinal
  names the scope column at that position.
- **Output alias** — `GROUP BY s` where `s` is an `AS` alias (or an item's derived name, e.g.
  `GROUP BY abs` for `SELECT abs(x)`). A bare name resolves an **input column first**, and only if
  there is no such column is the output alias used — **the opposite of `ORDER BY`'s output-first
  rule** (PG). So `SELECT a AS b … GROUP BY b` groups by the table's column `b`, and the projected
  `a` is then a non-grouped column → `42803`.
- **General expression** — `GROUP BY a + b`, `GROUP BY lower(s)`. The expression is **materialized**:
  evaluated **per post-`WHERE` row** into a synthetic grouping column, and a select-list / `HAVING` /
  `ORDER BY` expression that **structurally matches** it resolves to that group's value (`SELECT a+b
  … GROUP BY a+b` projects the group key; `ORDER BY a+b` sorts by it even when it is not selected).
  An **aggregate operand stays per-row** — `sum(a+b)` under `GROUP BY a+b` is the per-row `a+b`
  folded, *not* the single group value (the operand resolves with grouping-expression matching
  **off**, since it is evaluated before the fold). A non-grouped column is still `42803`.

All three forms compose with `ROLLUP` / `CUBE` / `GROUPING SETS` (the term may be an ordinal /
alias / expression in any grouping set). An expression grouping key that has type `json` is `42883`
(no equality), like a `json` column. `GROUPING(…)` arguments stay **columns only** this slice (a
`GROUPING(a+b)` over an expression key is a deferred sub-follow-on).

**Cost** (the cross-core contract, §8). A materialized grouping expression is evaluated **once per
post-`WHERE` row** (its `operator_eval`s charged, like an aggregate operand) and the value cached in
a synthetic column reused across grouping sets; a plain column key adds nothing. The bucketing /
finalize stay unmetered. Deterministic and identical across cores. New capability
`query.group_by_expr`.

## 16. Functional-dependency grouping

The grouping-error rule (§6/§10) requires every non-aggregated select-list / `HAVING` / `ORDER BY`
column to be a grouping key. PostgreSQL **relaxes** it for a **functional dependency**: when the
`GROUP BY` includes every **primary-key** column of a base table T, the PK *determines* every other
column of T (one PK value ⇒ at most one T row), so any column of T — and expressions over them —
may appear ungrouped, with the single per-group value used. jed matches.

The dependency holds **across a join**: `GROUP BY t.id` (t's PK) over `t JOIN u` keeps every `t`
column constant within a group even when several `u` rows match, so `t`'s columns are groupable while
`u`'s (whose PK is *not* grouped) stay `42803`. A **composite** PK requires **all** its members to
be grouped — a partial PK confers no dependency. The relaxation is restricted to a **single grouping
set**: PG rejects the dependency when a `GROUPING SETS` / `ROLLUP` / `CUBE` element omits the PK (a
column grouped away in one set has no single value), so jed applies it only to a plain `GROUP BY`.

**Implementation.** When the (single) grouping set contains a base table's full PK, that table's
remaining columns are added as **extra master grouping keys** — the grouping is **unchanged**, since
each added column is constant within every group, so bucketing by `[pk…, others…]` yields the *same*
partition as by `[pk…]` alone. This makes them ordinary grouping-key slots, so the projection /
`HAVING` / `ORDER BY` resolve them through the normal column path with no new machinery. A CTE /
derived table / SRF has no real PK (a synthetic key), so only base tables contribute. New capability
`query.group_by_functional_dependency`.

## 17. A non-constant ordered-set-aggregate fraction

§13's first slice required the `percentile_*` direct argument (the fraction) to be a **constant**,
folded to an `f64` at plan time. PostgreSQL allows it to be any expression over the **grouping
columns** — evaluated **once per group** (a direct argument, *not* a per-row operand). jed matches:

- The fraction is resolved in the **grouped context** (like the projection), so a grouping column
  binds its synthetic key slot and a **non-grouped column is `42803`** (PG: *"direct arguments of an
  ordered-set aggregate must use only grouped columns"*). An **aggregate** inside the fraction is
  `42803` (PG forbids nesting). A non-numeric fraction is still `42883` (no overload).
- At **finalize** the fraction expression is evaluated against the group's synthetic row (its
  grouping-key values), yielding this group's fraction — so `percentile_cont(p) WITHIN GROUP (ORDER
  BY v) … GROUP BY p` uses each group's own `p`. A **constant** fraction is just the degenerate case
  (the expression ignores the row). The per-group `NULL → NULL` and out-of-range `→ 22003` rules
  (§13) are unchanged, now applied to the evaluated value.
- **Cost.** The fraction evaluation is part of the **unmetered** finalize (like the sort), so the
  metered cost is unchanged from a constant fraction — deterministic and cross-core identical.

New capability `query.ordered_set_nonconstant_fraction`.

## 18. An array-valued `percentile_*` fraction

PostgreSQL's `percentile_cont` / `percentile_disc` accept an **array** of fractions, computing one
percentile per element and returning an **array** — `percentile_cont(ARRAY[0.25, 0.5, 0.75]) WITHIN
GROUP (ORDER BY v)` returns the quartiles as `float8[]`. jed matches:

- The direct argument may be a numeric **array** (any of the §17 forms, but array-typed); the result
  type becomes an **array** of the scalar result type — `float8[]` for `percentile_cont` over numeric,
  the input-type `[]` for `percentile_disc`, `interval[]` for `percentile_cont` over interval (§13).
- The group is sorted **once**; each array element yields one percentile, in element order. A **NULL
  element** yields a **NULL element** in the result (it is not the whole-result NULL of a scalar NULL
  fraction). Every **non-NULL** element is range-checked (`22003`) **before** the empty-group check
  (PG's order), so an out-of-range element fails the whole statement.
- An **empty / all-NULL** group yields **NULL** — the *whole* result, not an array of NULLs (PG).

The array fraction reuses the §17 per-group evaluation (the direct argument is evaluated against the
group's key values) and the same `percentile_disc` / `percentile_cont` / `interval_lerp` kernels, so
it composes with the non-constant fraction and the interval input. New capability
`query.ordered_set_array_fraction`.

## 19. Hypothetical-set aggregates (`rank` / `dense_rank` / `percent_rank` / `cume_dist`)

PostgreSQL's four **hypothetical-set** aggregates answer: *if this hypothetical row were inserted
into the group, what rank would it have?* They share the names of the window ranking functions, but
with a `WITHIN GROUP (ORDER BY …)` clause (and direct-argument values) they are ordered-set
aggregates, routed here rather than to the window path. jed matches:

| Aggregate | Result | Definition over the group sorted by `WITHIN GROUP` |
|---|---|---|
| `rank(args)` | `i64` | `1 +` rows that sort **strictly before** the hypothetical row |
| `dense_rank(args)` | `i64` | `1 +` **distinct** values strictly before |
| `percent_rank(args)` | `f64` | `(rank − 1) / N` |
| `cume_dist(args)` | `f64` | `(#rows ≤ hyp + 1) / (N + 1)` |

`N` is the group's row count (PG's `orderedsetaggs.c` formulas, exactly). Over an **empty** group:
`rank`/`dense_rank` are `1`, `percent_rank` is `0`, `cume_dist` is `1`.

- **Direct args ↔ ordering columns.** The hypothetical row is the parenthesized **direct
  arguments**; their count must equal the number of `WITHIN GROUP` ordering columns, else `42883`
  (PG models it as a missing overload, naming a flag-inflated arg count). Each direct arg is
  evaluated **per group** (like a percentile fraction — §17, so it may reference grouping columns)
  and **coerced to its key's type** (a literal adapts; an incompatible family is `42883`).
- **Ordering.** Each key's `ASC`/`DESC`, `NULLS FIRST|LAST`, and `COLLATE` (§13) are honored when
  comparing a group row's key tuple to the hypothetical row; the first non-equal key decides. NULL
  key values and a NULL hypothetical arg participate via the NULLS placement (no NULL-skip — every
  row counts toward `N`). `dense_rank`'s distinct count is value-canonical (the group-key equality).
- **Composition.** Works whole-table and per `GROUP BY` group, and with `FILTER (WHERE …)` (which
  restricts the counted rows). `DISTINCT` is `42601`; `OVER` (with `WITHIN GROUP`) is `0A000`.

**Cost** (the cross-core contract, §8). Each group row's key tuple is evaluated + buffered
(`aggregate_accumulate` per row, plus the key `operator_eval`s); the per-group hypothetical-row
evaluation, sort comparisons, and rank count are part of the **unmetered** finalize (like the OSA
sort). Deterministic and cross-core identical. New capability `query.hypothetical_set_aggregate`.
