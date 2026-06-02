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
  into a `SELECT` result set, at projection time (post-filter, post-`ORDER BY`). `DELETE`
  / `UPDATE` emit no rows and so charge no `row_produced`.
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
- **Pre-order, LHS-before-RHS.** A node charges itself, then evaluates its left operand,
  then its right. The order does not change the **total** (a sum is order-independent),
  but it fixes the deterministic **abort point** for the deferred ceiling (§6) identically
  across cores.
- **Helpers are not separately charged.** `eval_arith`/`evalArith`, and the `<=`/`>=`
  comparisons' internal `lt3 OR eq3` combinators, are covered by their owning node's
  single `operator_eval`. They are not `RExpr` nodes.

### What is NOT metered (defined boundary)

Metering covers **execution** — per-row scans, per-row produced, per-row expression
evaluation. It deliberately does **not** meter:

- **Parse / plan / resolve** — these are per-statement (and the literal range-checks,
  type resolution, etc. happen once), not per-row execution.
- **`ORDER BY` sort-internal comparisons** — the sort compares `Value`s directly, not
  through the expression evaluator, so they are outside the `operator_eval` unit. (A
  dedicated sort-comparison unit could be added later if wanted; it is not in this slice.)
- **Phase-2 row writes** in `UPDATE`/`DELETE` — the two-phase mutation's write pass does
  no eval and produces no row.

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
  `storage_row_read` (a row read and a page read are distinct events).
- **Per-operator `cost` weights.** A uniform `operator_eval` weight now; the per-operator
  `cost` field in [../functions/catalog.toml](../functions/catalog.toml) stays reserved
  ([functions.md](functions.md) §8). Authoring it later (evaluator preferring the
  operator's `cost`, falling back to `operator_eval`) is purely additive.
