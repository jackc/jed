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

The core seam units, all weight `1`:

| unit | fires when |
|---|---|
| `storage_row_read` | one row is read from a table store during a scan |
| `page_read` | one B-tree node (page) is touched while scanning a store |
| `row_produced` | one row is emitted into a query result set |
| `operator_eval` | one interior expression node is evaluated |

(`page_read` was **added** in P6.3 when the store became a page-backed B-tree — §3
"`page_read`" — *alongside* `storage_row_read`, not a rename; the later
`aggregate_accumulate` unit, [../cost/schedule.toml](../cost/schedule.toml), is metered in
the aggregates path.) The weights are uniform on purpose — phase 1 proves the seam reads
cost from **data**; tuning the numbers later is a data-only change touching no executor code.

## 3. Accrual rules (the cross-core determinism contract)

These rules are the contract. They must be implemented **identically** in Rust, Go, and
TS; any deviation diverges the count and fails the corpus.

- **`storage_row_read`** is charged once per row pulled from a store, at the top of the
  executor scan loop, **before** the filter runs — in `SELECT`, `DELETE`, and `UPDATE`.
  It is charged in the **executor loop, not inside the storage iterator**: the Rust store
  returns a lazy iterator while Go/TS materialize a sorted slice, so charging in storage
  would diverge the (future) abort *point*. The executor loop is the one place all three
  cores agree.
- **`page_read`** is charged once per B-tree node (page) in a table's store when that store
  is scanned, as a block **before** that table's `storage_row_read`s — the dedicated
  subsection below gives the rule (a full scan touches every node, so the charge is the
  tree's structural node count).
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

### `page_read` — the pages a scan touches

The store is a **page-backed copy-on-write B-tree** (P6.1, [storage.md](storage.md) §6): each
table's rows live in a tree of fixed-size pages, and the node boundaries are a §8 byte contract
— *the same tree shape in every core* (the in-memory B-tree **is** the on-disk one, node-for-page).
So the number of pages a scan touches is **deterministic and byte-identical across cores**,
exactly the property cost requires.

- **A full table scan walks the whole tree**, so it charges `page_read` once per **node**
  (interior *and* leaf) in that table's tree — its structural **node count**. The executor has
  no index or point-lookup path yet: every `SELECT` / `DELETE` / `UPDATE` scan reads the entire
  store (the same loop that charges `storage_row_read` per row), so it touches every page. An
  **empty table** (no root) has zero nodes and charges no `page_read`.
- **`page_read` is charged as a block, before that table's `storage_row_read`s** — read the
  pages, then the rows within them. Charged at the **same three sites** as `storage_row_read`
  (the `SELECT`/JOIN materialization, the `DELETE` scan, the `UPDATE` phase-1 scan), once per
  table-scan *execution*. The total is order-independent, but fixing the block-before-rows order
  pins the future abort point (§6) identically across cores.
- **It composes exactly like `storage_row_read`.** A **JOIN** materializes each base table
  once, so it charges each table's node count once (Σ over the relations — a self-join counts
  the table twice, once per alias). A **set operation** charges each operand's scans
  (`lhs + rhs`). An **uncorrelated** subquery (folded once) charges its tree once; a
  **correlated** subquery re-scans its inner table **per outer row**, charging that node count
  each time — identical to how those forms already compose `storage_row_read`.
- **Logical, not physical.** `page_read` counts the tree's structural node count — a *logical*
  page access — **not** a physical disk fetch. A future buffer pool / demand-paging cache for
  larger-than-RAM files (CLAUDE.md §9) serves a page from memory or disk transparently; the
  cost is identical either way, so the deterministic cost stays cache-independent (§13).

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

**Worked example.** Tables `a` (3 rows), `b` (2 rows), each small enough to be a single leaf
page; `SELECT * FROM a JOIN b ON a.k = b.k`, with 2 pairs surviving the `ON`. Materialize `a` →
1 `page_read` + 3 `storage_row_read`; materialize `b` → 1 + 2; the `ON` (`a.k = b.k`, one
interior `compare` node — its operands are leaf columns, charging nothing) over 3 × 2 = 6
candidate pairs → 6 `operator_eval`; no WHERE; `*` is bare-column projection (leaves, charge
nothing); 2 emitted rows → 2 `row_produced`. **Total = (1 + 3) + (1 + 2) + 6 + 2 = 15.** A
`CROSS JOIN` of the same tables emits all 6 pairs and evaluates no `ON`: 1 + 3 + 1 + 2 + 0 + 6 =
**13**.

**OUTER joins charge identically — only the produced-row count grows.** `LEFT`/`RIGHT`/`FULL [OUTER]
JOIN` ([grammar.md](grammar.md) §15) evaluate the `ON` over the **same** `|running| × |right|`
candidate set (so the `ON` `operator_eval` count is unchanged from an INNER join of the same tables);
a row that matches nothing is then **NULL-extended on the absent side and added to the surviving set
without re-evaluating `ON`** — the NULL-extension itself is unmetered, like row concatenation. Those
NULL-extended rows are ordinary surviving combined rows, so they incur WHERE `operator_eval` and
`row_produced` exactly like matched rows. So for the example tables with `SELECT * FROM a LEFT JOIN b
ON a.k = b.k` where 1 `a`-row matches 1 `b`-row and the other 2 `a`-rows match nothing: 1 + 3 and
1 + 2 to materialize (one leaf page each), 6 `ON`, no WHERE, and 1 matched + 2 NULL-extended = 3
emitted rows → **(1 + 3) + (1 + 2) + 6 + 3 = 16** (the INNER form of the same query is
`… + 1 = 14`; the +2 is the two preserved-left rows).

### Set operations — `lhs + rhs`, the combine unmetered

A set operation ([grammar.md](grammar.md) §25) — `UNION`/`INTERSECT`/`EXCEPT`, each with an
optional `ALL` — combines the result sets of two operand queries. Its cost is the **sum of the
operand costs and nothing more**:

> `cost(a ⊕ b) = cost(a) + cost(b)`

Each operand is a full `select_core` (or a nested set operation) run through the ordinary query
path, so it **already** charges `storage_row_read` per scanned row, the `operator_eval`s of its
own clauses, and `row_produced` per row it emits (its *pre-combine* output). The set-operation
layer then consumes those materialized rows and does **only set-membership work** — match rows by
the NULL-safe value-canonical key, take the multiset union / intersection / difference, emit the
representative rows — which is **unmetered**, exactly like `DISTINCT` dedup (above), the
`ORDER BY` sort, and the `LIMIT`/`OFFSET` slice. The trailing `ORDER BY` and `LIMIT`/`OFFSET` of a
set operation are likewise unmetered (§ "What is NOT metered"). The integer→`decimal` value
conversion that type unification may apply before keying (§25) is structural, like a JOIN's
NULL-extension, and charges nothing. **No new cost unit** is introduced.

This **follows the `INSERT … SELECT` precedent** (§24, where the wrapping statement adds nothing
to the embedded `SELECT`'s cost), not the single-`SELECT` shape. A deliberate consequence: the
`DISTINCT` invariant "`row_produced` equals the output row count" **does not hold** for a set
operation — the operands charge `row_produced` for their *pre-combine* rows, and the combine that
drops/duplicates rows is unmetered, so the accrued `row_produced` reflects what the operands
produced, not the set operation's final output. This is correct and intended: cost composes from
the independently-metered subqueries.

**Worked example.** Tables `a` (3 rows) and `b` (2 rows), each a single leaf page;
`SELECT x FROM a UNION SELECT x FROM b`. The left operand materializes `a` → 1 `page_read` +
3 `storage_row_read` and emits 3 rows → 3 `row_produced` (a bare-column projection is a leaf,
charging no `operator_eval`): 7. The right operand: 1 + 2 + 2 = 5. The `UNION` dedup is
unmetered. **Total = 7 + 5 = 12**, whatever the number of distinct output rows. `UNION ALL`
(no dedup) costs the **same** 12 — the dedup was already free, so dropping it changes nothing.
The cross-core contract is trivially identical: it is literally the sum of two
independently-deterministic operand costs.

### Subqueries — initplan once, correlated per outer row

A subquery ([grammar.md](grammar.md) §26) — scalar `(SELECT …)`, `x IN (SELECT …)`, or
`EXISTS (SELECT …)` — composes its operand query's cost into the enclosing query with **no new
cost unit**. The subquery runs through the ordinary query path, so it **already** charges its
own `storage_row_read` / `operator_eval` / `row_produced` exactly as any `SELECT` does; the
folding/membership/cardinality machinery is **unmetered**, like `DISTINCT` dedup and the
set-operation combine. How many times that operand cost lands depends on correlation:

- **Uncorrelated** (an "initplan") — executed **exactly once**, at plan setup, and folded into a
  constant. Its cost is added **once**, and the folded constant is a **leaf** (charges no
  `operator_eval` when the outer row evaluates), so a scalar subquery referenced once in `WHERE`
  adds its operand cost once, not once per outer row:

  > `cost(query with uncorrelated s) = cost(query) + cost(s)`

  A globally-uncorrelated subquery is folded once **even when it is nested inside a correlated
  one** (its value never changes), so it too is counted once.

- **Correlated** — re-executed once **per outer row** that reaches its expression node, reading
  the enclosing-row values its plan references. Each execution adds that execution's full
  operand cost (which can vary per outer row, since the correlated values filter the inner scan
  differently), and the subquery node itself — being a real interior operator now, not a folded
  leaf — charges **one `operator_eval`** each time it evaluates. A correlated `IN` additionally
  charges one `operator_eval` per inner result value its membership test compares (the §26 IN
  model). So for a correlated subquery `s` reached by outer rows `R`:

  > `cost(query with correlated s) = cost(query) + Σ_{r ∈ R} (operator_eval + cost(s | r))`

Both are fully deterministic and identical across cores: the same `(query, database)` always
visits the same outer rows in the same order and runs the subquery the same number of times.

The same accounting applies when the enclosing statement is a **`DELETE` / `UPDATE`** (a
subquery in its `WHERE`, or an `UPDATE` assignment RHS — grammar.md §26): an uncorrelated
subquery folds once (operand cost added once, before the scan), and a correlated one re-runs
per **scanned** row that reaches its node, adding `operator_eval + cost(s | r)` each time —
identical to the `SELECT` case, since both mutations drive the same per-row evaluator. The
phase-2 writes evaluate nothing and stay unmetered (below).

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
- **Set-operation combine** — matching rows by the NULL-safe value-canonical key, the multiset
  union/intersection/difference, the integer→`decimal` unification conversion, and the trailing
  `ORDER BY`/`LIMIT`/`OFFSET` are all set-membership / bookkeeping, not evaluation; a set
  operation accrues only its operands' costs (`lhs + rhs`, see the set-operations subsection
  above).

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
- **A real `page_read` unit — ✅ landed (P6.3).** The store is now a page-backed B-tree
  ([storage.md](storage.md) §6), so a distinct `page_read` unit was **added** to the schedule
  (not a rename of `storage_row_read` — both fire on a scan) and is charged per node a scan
  touches. It counts a **logical** page access (the tree's structural node count), **not** a
  physical disk fetch, so the future buffer pool / cache for larger-than-RAM files
  (CLAUDE.md §9) cannot perturb the deterministic, cache-independent cost (§13). Accrual
  rules: §3 "`page_read`".
- **Per-operator `cost` weights.** A uniform `operator_eval` weight now; the per-operator
  `cost` field in [../functions/catalog.toml](../functions/catalog.toml) stays reserved
  ([functions.md](functions.md) §8). Authoring it later (evaluator preferring the
  operator's `cost`, falling back to `operator_eval`) is purely additive.
