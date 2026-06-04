# Cost accounting — design

> The reasoning behind the deterministic cost-accounting seam (CLAUDE.md §13). The
> canonical **data** is [../cost/schedule.toml](../cost/schedule.toml) (the unit
> weights); this doc is the *why* and — because cost is a cross-core contract with no
> reference implementation (§2) — the precise **accrual rules** every core must obey.
> The schedule is validated by [../cost/verify.rb](../cost/verify.rb) (`rake verify`).

A first-class use case is **safely evaluating untrusted, user-supplied queries**
(CLAUDE.md §13). That requires the engine to **deterministically meter the cost of
executing a query** and, eventually, to **abort when a caller-supplied ceiling is
exceeded**. This slice builds the **seam** — the cost counter threaded through the
executor, expression evaluator, and storage reads — while the executor is still small.
The ceiling + abort is deferred (§6); the seam is what is expensive to retrofit, so it
goes in now.

## 1. Why cost is a shared contract, not an implementation detail

Because there is no reference implementation (CLAUDE.md §2), the only thing that says two
cores agree is that they produce identical results on the same shared tests. Cost is no
different: the cost of a given `(query, database state)` is **fully deterministic** and
**byte-identical across every core**. This makes it a CLAUDE.md §8 divergence hotspot,
and so it is **asserted in the conformance corpus** (the `# cost:` directive —
[conformance.md](conformance.md)), not merely in per-core tests. A divergence in any
core's counting is a failing corpus entry the day it appears.

## 2. The unit schedule is data

The cost units and their weights live in [../cost/schedule.toml](../cost/schedule.toml)
(data over code, CLAUDE.md §5), emitted into each core as `@generated` constants by
[../../scripts/gen_costs.rb](../../scripts/gen_costs.rb) — the same codegen "middle path"
as the operator catalog ([codegen.md](codegen.md)). The accrual **sites** (which line in
the executor/evaluator/storage fires which unit) are hand-written per core; §5 forbids
codegenning the evaluator. Only the **weights** are shared data.

Three units this slice, all weight `1`:

| unit | fires when |
|---|---|
| `storage_row_read` | one row is read from a table store during a scan |
| `row_produced` | one row is emitted into a query result set |
| `operator_eval` | one interior expression node is evaluated |

The weights are uniform on purpose — phase 1 proves the seam reads cost from **data**;
tuning the numbers later is a data-only change touching no executor code.

## 3. Accrual rules (the cross-core determinism contract)

These rules are the contract. They must be implemented **identically** in Rust, Go, and
TS; any deviation diverges the count and fails the corpus.

- **`storage_row_read`** is charged once per row pulled from a store, at the top of the
  executor scan loop, **before** the filter runs — in `SELECT`, `DELETE`, and `UPDATE`.
  It is charged in the **executor loop, not inside the storage iterator**: the Rust store
  returns a lazy iterator while Go/TS materialize a sorted slice, so charging in storage
  would diverge the (future) abort *point*. The executor loop is the one place all three
  cores agree.
- **`row_produced`** is charged once per row that survives the filter and is projected
  into a `SELECT` result set, at projection time (post-filter, post-`ORDER BY`, **and
  post-`LIMIT`/`OFFSET`**). `LIMIT`/`OFFSET` slice the sorted rows *before* the projection
  loop, so a row skipped by `OFFSET` or excluded by `LIMIT` is scanned and filtered (it
  pays `storage_row_read` + its filter `operator_eval`s) but charges **no** `row_produced`
  or projection cost — only the windowed rows do. `DELETE` / `UPDATE` emit no rows and so
  charge no `row_produced`.
- **`operator_eval`** is charged once per **interior** expression node — `cast`, `neg`,
  `not`, `arith`, `compare`, `and`, `or`, `is_null`, `distinct`. **Leaf nodes — `column`
  and the constants (`int`/`bool`/`null`) — charge nothing.** Charging leaves would make
  cost track how many literals the parser happened to fold, an accidental property; cost
  must track genuine evaluation work.
- **No short-circuit.** Both operands of every binary node (`and`, `or`, `compare`,
  `arith`, `distinct`) are **always** evaluated before the node charges its own
  `operator_eval`. This is already true — the Kleene helpers (`and3`/`or3`/`boolAnd`)
  are pure functions over already-computed operand values, never control flow. The seam
  **must not introduce** a short-circuit: skipping the RHS in one core when the LHS is
  FALSE/NULL would drop that core's operand evals and diverge the count.
- **`CASE` is the one deliberate exception to no-short-circuit.** A `CASE`
  ([grammar.md](grammar.md) §23) charges its own `operator_eval` for the node, then evaluates
  its `WHEN` conditions **in source order, stopping at the first one that is TRUE** — a FALSE
  or NULL/UNKNOWN condition falls through. Only the conditions tested **up to and including the
  match**, plus the **selected** result (the matching `THEN`, or the `ELSE`, or nothing for an
  implicit `ELSE NULL`), are evaluated and charged; later arms are **not** evaluated. This
  short-circuit is *required* by PostgreSQL semantics — `CASE WHEN a = 0 THEN 0 ELSE 1 / a END`
  must not divide by zero on the `a = 0` rows — so it is a sanctioned exception, not a seam
  violation. It stays deterministic per `(query, db state)` because the evaluation order (first
  match wins, conditions left to right) is fixed across cores, so which arms accrue is itself
  deterministic. (A consequence, like `DISTINCT`'s, is observable: `CASE WHEN true THEN 0 ELSE
  1/0 END` succeeds and costs *less* than the eager form would, because the `1/0` arm is never
  reached. The simple form `CASE x WHEN v …` desugars each branch to `x = v`, so the operand is
  evaluated once per tested branch — the same per-branch model as `IN`'s LHS.)
- **Pre-order, LHS-before-RHS.** A node charges itself, then evaluates its left operand,
  then its right. The order does not change the **total** (a sum is order-independent),
  but it fixes the deterministic **abort point** for the deferred ceiling (§6) identically
  across cores.
- **Helpers are not separately charged.** `eval_arith`/`evalArith`, and the `<=`/`>=`
  comparisons' internal `lt3 OR eq3` combinators, are covered by their owning node's
  single `operator_eval`. They are not `RExpr` nodes.

### `SELECT DISTINCT` — the projection-vs-produce asymmetry

`DISTINCT` ([grammar.md](grammar.md) §11) deduplicates the **projected** output, so it must
project *every* filtered row to compute its dedup key — there is no way to know a row is a
duplicate without evaluating its select list. That splits two charges the un-`DISTINCT` path
keeps together:

- **Projection `operator_eval` is charged per *filtered* row**, not per windowed row — for
  each filtered row, every interior projection node fires once. This is independent of
  `LIMIT`/`OFFSET` and of how many rows turn out to be duplicates; the work is genuinely
  done. (Leaf `column`/constant projections still charge nothing, so a bare-column
  `SELECT DISTINCT a` adds no projection cost at all.)
- **`row_produced` is charged per *emitted* row** — the rows surviving dedup **and** the
  window — unchanged from its "one per row in the result set, post-`LIMIT`/`OFFSET`"
  definition (now also post-`DISTINCT`). So `row_produced` always equals the output row
  count.
- **Dedup itself is unmetered**, like the `ORDER BY` sort and the `LIMIT` slice (a dedicated
  dedup-comparison unit could be added later, as for the sort).

A consequence worth stating because it is observable and is a cross-core abort-point contract
(§6): because all filtered rows are projected, a projection that traps fires **even under a
`LIMIT` that would exclude the offending row**. `SELECT DISTINCT 1/a FROM t LIMIT 1` traps
`22012` if *any* filtered row has `a = 0`, whereas un-`DISTINCT` `SELECT 1/a FROM t LIMIT 1`
windows first and does not. The trapping row is deterministic (primary-key scan order), so
all three cores trap identically.

### JOIN — multi-table FROM (the nested-loop contract)

A multi-table `SELECT` ([grammar.md](grammar.md) §15) is a **left-deep nested-loop** join. Its
cost is pinned here because, with no reference implementation, the count is a cross-core contract
(§1). Three rules, each a small extension of the single-table rules above:

- **`storage_row_read` is charged once per physical row as each base table is materialized** —
  total = the **sum of the table cardinalities** (`|A| + |B| + …`), independent of join order or
  fan-out. A row is pulled from its store exactly once (each table is scanned into memory in
  primary-key order); the nested loop then re-reads from that **in-memory** buffer, which is not a
  store and charges nothing. This keeps the existing rule verbatim ("once per row pulled from a
  store, in the executor loop not the storage iterator" — so the Rust lazy-iterator vs Go/TS
  materialized-slice split stays neutralized) and keeps single-table cost identical (one table →
  its cardinality).
- **The `ON`-predicate `operator_eval` is charged per candidate combination** the join evaluates
  it against — for an `INNER JOIN`, once per (running-row × right-row) pair, the `ON` tree's
  interior nodes firing pre-order with **no short-circuit**, exactly like a WHERE. A `CROSS JOIN`
  has no `ON` and charges no join `operator_eval` (it keeps every pair). So `ON` cost =
  |running| × |right| × (interior nodes in the `ON`), deterministic and fan-out-explicit. The
  iteration order — running/left side outer in PK order, right side inner in PK order, left-deep —
  is fixed so the per-combination evals accrue in the same sequence in every core (a §8 surface;
  it fixes the future abort point even though only the total is asserted today).
- **WHERE `operator_eval`** is charged per **surviving combined row** (post-join), and
  **`row_produced`** per emitted output row (post-`LIMIT`/`OFFSET`) — both unchanged; the combined
  row is simply wider. Join materialization buffering, the nested-loop control flow, and row
  concatenation are **unmetered**, like the `ORDER BY` sort and the `LIMIT` slice.

**Worked example.** Tables `a` (3 rows), `b` (2 rows); `SELECT * FROM a JOIN b ON a.k = b.k`, with
2 pairs surviving the `ON`. Materialize `a` → 3 `storage_row_read`; materialize `b` → 2; the `ON`
(`a.k = b.k`, one interior `compare` node — its operands are leaf columns, charging nothing) over
3 × 2 = 6 candidate pairs → 6 `operator_eval`; no WHERE; `*` is bare-column projection (leaves,
charge nothing); 2 emitted rows → 2 `row_produced`. **Total = 3 + 2 + 6 + 2 = 13.** A
`CROSS JOIN` of the same tables emits all 6 pairs and evaluates no `ON`: 3 + 2 + 0 + 6 = **11**.

**OUTER joins charge identically — only the produced-row count grows.** `LEFT`/`RIGHT`/`FULL [OUTER]
JOIN` ([grammar.md](grammar.md) §15) evaluate the `ON` over the **same** `|running| × |right|`
candidate set (so the `ON` `operator_eval` count is unchanged from an INNER join of the same tables);
a row that matches nothing is then **NULL-extended on the absent side and added to the surviving set
without re-evaluating `ON`** — the NULL-extension itself is unmetered, like row concatenation. Those
NULL-extended rows are ordinary surviving combined rows, so they incur WHERE `operator_eval` and
`row_produced` exactly like matched rows. So for the example tables with `SELECT * FROM a LEFT JOIN b
ON a.k = b.k` where 1 `a`-row matches 1 `b`-row and the other 2 `a`-rows match nothing: 3 + 2
materialize, 6 `ON`, no WHERE, and 1 matched + 2 NULL-extended = 3 emitted rows → **3 + 2 + 6 + 3 =
14** (the INNER form of the same query is `… + 1 = 12`; the +2 is the two preserved-left rows).

### What is NOT metered (defined boundary)

Metering covers **execution** — per-row scans, per-row produced, per-row expression
evaluation. It deliberately does **not** meter:

- **Parse / plan / resolve** — these are per-statement (and the literal range-checks,
  type resolution, etc. happen once), not per-row execution.
- **`ORDER BY` sort-internal comparisons** — the sort compares `Value`s directly, not
  through the expression evaluator, so they are outside the `operator_eval` unit. This holds
  for a **multi-key** sort too (each key's comparison is the same direct `Value` compare),
  so adding keys or `NULLS FIRST|LAST` placement changes no cost. (A dedicated
  sort-comparison unit could be added later if wanted; it is not in this slice.)
- **`LIMIT` / `OFFSET` slicing** — selecting the output window is an index slice over the
  already-sorted rows, not evaluation work; like the sort it is unmetered. Its only cost
  effect is *fewer* `row_produced`/projection charges (the excluded rows are never
  projected — see the `row_produced` rule above).
- **`DISTINCT` dedup** — testing whether a projected tuple has been seen is set membership,
  not evaluation, so it is unmetered like the sort and the slice. Its cost effect is the
  asymmetry above: projection `operator_eval` is charged for every filtered row, but
  `row_produced` only for the surviving distinct, windowed rows.
- **Phase-2 row writes** in `UPDATE`/`DELETE` — the two-phase mutation's write pass does
  no eval and produces no row.
- **JOIN nested-loop control flow** — buffering each materialized table, iterating the
  Cartesian/left-deep combinations, and concatenating left+right rows are bookkeeping, not
  evaluation; only `storage_row_read` (per materialized row), the `ON`/WHERE/projection
  `operator_eval`s, and `row_produced` accrue (see the JOIN subsection above).

## 4. Counter representation — exactness across cores (CLAUDE.md §8)

The accrued cost is carried as a signed 64-bit integer: `i64` (Rust), `int64` (Go),
**`bigint` (TS)**. TS must use `bigint`, not `number`: a `number` is an IEEE-754 `f64`,
and a large scan crosses 2^53 where `f64` loses integer precision, silently diverging
from the Rust/Go `i64` totals — exactly the §8 hotspot the type system exists to kill.
The TS core already carries int64 values as `bigint`, so this is consistent. Cost renders
as a plain shortest-decimal integer, matching the `# cost: N` corpus directive.

## 5. The seam shape (so enforcement is additive)

Every accrual routes through a single `Meter::charge(units)` chokepoint per core (a tiny
`Meter` struct threaded by `&mut`/pointer/mutable-object through the executors and the
recursive evaluator). The accrued total is exposed on `Outcome` (both the statement and
query variants — a `DELETE` still accrues scan + filter cost). Centralizing accrual in
`charge` is what makes the deferred enforcement a local change (§6).

## 6. Deferred — enforcement

Not built in this slice; recorded here so the seam shape can be confirmed against it:

- **Caller-set ceiling + deterministic abort.** `Meter` gains a `limit`; `charge` becomes
  the one place that compares `accrued` against it and aborts. Because every unit already
  flows through `charge`, no executor call site is re-threaded — the abort is a ~3-line,
  one-method change. The abort point is deterministic (same `(query, db, ceiling)` → same
  abort) because accrual order is fixed (§3).
- **Cost-ceiling error code.** A new `[[error]]` in [../errors/registry.toml](../errors/registry.toml)
  — a resource/limit class (PostgreSQL uses SQLSTATE class `53` *insufficient_resources*;
  `54` *program_limit_exceeded* is the other candidate). **Not authored now.**
- **A real `page_read` unit.** Storage is whole-image / row-granular today
  ([storage.md](storage.md) §6); `storage_row_read` is the structural storage unit. When a
  paged store lands, **add** a `page_read` unit to the schedule — do not rename
  `storage_row_read` (a row read and a page read are distinct events). Count it as a
  **logical** page access (pages the query touches), **not** a physical disk fetch — so a
  buffer pool / cache for larger-than-RAM files (CLAUDE.md §9) cannot perturb the
  deterministic, cache-independent cost (§13).
- **Per-operator `cost` weights.** A uniform `operator_eval` weight now; the per-operator
  `cost` field in [../functions/catalog.toml](../functions/catalog.toml) stays reserved
  ([functions.md](functions.md) §8). Authoring it later (evaluator preferring the
  operator's `cost`, falling back to `operator_eval`) is purely additive.
