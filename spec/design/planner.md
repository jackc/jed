# The planner: explicit optimizer-pass structure — design

> How a SELECT becomes an executable plan, and where each optimization lives. The planner
> is a **deterministic rule engine** (no statistics, no cost-based choice yet): it resolves
> the query into a **logical plan**, applies **rewrite rules** (none exist yet — §3), and
> then runs **physical/access-path selection** — a fixed, ordered list of discrete rules,
> each a single function owning its gate and its action. This doc is the contract all three
> cores implement in lockstep (CLAUDE.md §2). It exists because the passes used to be fused
> into one `planSelect` function per core; the observable behavior — which plan is chosen,
> what it costs ([cost.md §3](cost.md)), how it renders ([explain.md §4](explain.md)) — is
> **unchanged** by the split and stays pinned by the conformance corpus.

## 1. The pipeline

A query statement moves through these stages, per core:

```
parse                    → AST
resolve                  → the LOGICAL plan     (stage 1 — planSelect's body)
rewrite rules            → logical plan         (stage 2 — empty today, §3)
physical selection       → physical decisions   (stage 3 — optimizeSelect, §4)
—— the planner ends here ——
bind params              → values for $N
fold uncorrelated        → subquery results as constants
execute                  → rows                 (exec-time lane selection, §6)
```

Entry points (each core spells the same decomposition in its own convention):

| stage | Go | Rust | TS |
|---|---|---|---|
| resolve → logical plan | `planSelect` (planner.go) | `plan_select` (executor/planner.rs) | `planSelect` (executor.ts) |
| physical selection | `optimizeSelect` (optimize.go) | `optimize_select` (executor/optimize.rs) | `optimizeSelect` (optimize.ts) |
| access-path mechanisms | access_path.go | executor/access_encode.rs | executor.ts (`detect*`) |

Two stages that look planner-shaped deliberately are **not** in the planner:

- **`foldUncorrelatedInPlan` is not a rewrite rule.** It *executes* uncorrelated subqueries
  once and folds their results in as constants — execution needs bound parameters, so it
  runs **after** bind, between the planner and the executor. A stage-2 rewrite rule is a
  pure plan→plan transform that runs before binding.
- **Exec-time lane selection is not physical selection** (§6). The executor's dispatch
  (`execSelectEmit`) picks *how* to run the chosen plan — streaming vs. buffered vs.
  vectorized — from facts that are not plan properties: whether the meter is unmetered,
  whether the store is file-backed, the session's `work_mem`. The plan cache shares one
  plan object across sessions with different meters and stores, so these gates structurally
  cannot move to plan time. Lane choice never changes the rows **or the cost** (the
  streaming/spill invariance contracts — [streaming.md §6](streaming.md),
  [spill.md §6](spill.md)); plan choice changes both.

## 2. Stage 1 — the logical plan

Resolve builds the logical plan: the FROM scope (tables, CTEs, SRFs, derived tables,
LATERAL), join predicates, WHERE, GROUP BY / aggregates / grouping sets, window specs,
HAVING, projections, ORDER BY, DISTINCT, LIMIT/OFFSET — every clause bound to typed,
slot-indexed resolved expressions. The invariant that defines the stage boundary:

> **Resolve decides names, types, and errors — never an access path.** Every physical
> field of the plan is zero-valued when resolve hands it over; only stage 3 writes them.

The plan carries one **annotation** computed at the end of stage 1 that is not an
optimization: the per-relation **touched set** (`relMasks` — `computeRelMasks`), the
columns the query statically references. It is a **correctness** input to the lazy/masked
scan ([large-values.md §14](large-values.md)) and a cost input (cost.md §3 "the touched
set"); a wrong mask is a disk-mode NULL-folding bug, not a slow plan. It therefore lives
with resolution, not with the rules.

## 3. Stage 2 — rewrite rules (empty)

There are **no rewrite rules yet**; the stage is a documented seam in each core's
`planSelect` (a fixed position between logical-plan assembly and `optimizeSelect`), not a
no-op driver function. The first occupant introduces the driver — expected: **predicate
pushdown + simplification** (TODO.md), pushing WHERE conjuncts into derived tables / CTEs /
through joins, and detecting contradictions.

The contract a rewrite rule must meet before it lands:

- **Plan→plan and pure** — it transforms the logical plan before parameters are bound; it
  never executes anything.
- **Results-identical** — the same rows and the same errors, on every core.
- **Cost-identical, or an explicitly decided cost change.** This is the sharp constraint:
  cost is an observable cross-core contract (CLAUDE.md §13, cost.md §1). Textbook rewrites
  — constant folding, CSE, short-circuiting — **drop `operator_eval` charges**, so each
  such rule needs an explicit cost decision recorded here and the affected `# cost:`
  corpus entries re-pinned in the same change; never a silent apply.
- **A NoREC relation in the same change** ([conformance.md §8](conformance.md)) — the
  metamorphic sweep does not discover new optimizations on its own.

## 4. Stage 3 — physical/access-path selection: the rule inventory

`optimizeSelect` applies the rules below **in this fixed order** (later rules read earlier
rules' output; the order is part of the cross-core contract). Each rule is one function
owning its **gate** (the structural pattern it requires) and its **action** (the physical
fields it sets). A rule that does not fire leaves its fields zero-valued — the executor
then takes the unoptimized path (full scan, eager sort), which is always correct.

| # | rule | gate (summary) | sets | cost contract |
|---|---|---|---|---|
| 1 | **scan bounds** | per base relation (not SRF/derived): a WHERE conjunct bounds the relation's key per the §5 precedence | `relBounds[i]` | cost.md §3 "bounded scan", "index-bounded scan", "GIN-bounded scan", "GiST-bounded scan", "canonical interval sets" |
| 2 | **index-nested-loop** | a join inner base relation (INNER/CROSS/LEFT right side, not lateral/CTE) whose PK / leading index column compares to an **earlier sibling** column in ON or WHERE | `relINLBounds[i]` | cost.md §3 "JOIN" (per-outer-row seek) |
| 3 | **ORDER BY via PK scan order** | single base relation, non-aggregate, column-only keys: the ORDER BY is a one-direction PK prefix (ASC) or the full PK (DESC ⇒ reverse scan), collation-matching the stored key | `pkOrdered`, `pkReverse` | cost.md §3 "ORDER BY satisfied by primary-key order" (sort elided; with LIMIT, a top-N) |
| 4 | **ORDER BY via secondary-index order** | rule 3 did not fire; a LIMIT; no window/DISTINCT; the ORDER BY is exactly a B-tree index's columns, ASC NULLS LAST, fixed-width PK; an existing bound is allowed only when it walks that same index | `indexOrder` | cost.md §3 "ORDER BY satisfied by secondary-index order" (index-walk top-N) |
| 5 | **join sort-elision** | exactly two non-lateral base relations, INNER/CROSS, a LIMIT, forward outer-PK ORDER BY with no key beyond the outer PK, no eager bound on the outer, no INL bound | `joinPkOrdered` | cost.md §3 "JOIN" (the join top-N) |

Data-flow dependencies fixing the order: rule 4 reads `relBounds[0]` (rule 1) and
`pkOrdered` (rule 3); rule 5 reads `relBounds[0]` and `relINLBounds` (rules 1–2). Rules 3–5
are mutually exclusive by their gates (rule 4 requires `!pkOrdered`; rule 5's two-relation
gate excludes 3/4's single-relation gates).

The physical fields live in a dedicated sub-struct of the plan (`phys` — Go
`physicalPlan`, Rust/TS `PhysicalPlan`), so the stage boundary is visible in the type: the
logical fields plus the `relMasks` annotation are stage 1's output, `phys` is stage 3's.

The **mechanisms** the rules call — `detectScanBound`, `detectINLBound`,
`buildIndexAccessPredicate`, `orderSatisfiedByPK`, `orderSatisfiedByIndex`, interval-set
reduction — are shared pattern-matching/encoding machinery, not rules; they also serve
UPDATE/DELETE planning and exec-time eligibility checks, and live with the access-path
code, not in the optimizer pass.

**EXPLAIN** renders every rule's decision ([explain.md §4](explain.md)): rule 1 as the
scan's access-path detail, rule 2 as the `Index-nested-loop` prefix, rules 3–5 as the
sort-elision note (`ordered: pk ordered` / `index order: <index>` / `join pk ordered`) —
which is what makes each rule corpus-assertable without touching internals.

## 5. Access-path precedence (rule 1's internal order)

For each base relation, `detectScanBound` picks the **first** bound kind that applies —
a fixed precedence, not a costed choice ([indexes.md §5](indexes.md) is the authoritative
selection + execution spec; cost-based selection is a later concern, §7):

1. **PK tuple bound** (maximal equality prefix plus optional next-member range) — the row's own key;
   no second tree.
2. **B-tree index access predicate** — the **lowest-lowercased-name** index yielding a
   non-empty equality-prefix (+ optional trailing range) predicate; column and expression
   keys; a partial index only when a WHERE conjunct structurally implies its predicate.
3. **GiST bound** — a range/scalar operator conjunct over a GiST-indexed column.
4. **GIN bound** — an array-operator conjunct (`@>`, `&&`, `= ANY`, `=`) over a
   GIN-indexed column.
5. **OR / IN interval set** (normally last resort) — a pure same-key disjunction of equality/range
   leaves on a single-column PK or leading index column, runtime-canonicalized to disjoint key
   intervals. A co-present direct range on that same key clips the set; this clipped set deliberately
   wins over the broader contiguous clip alone.
6. Else: **full scan**.

Whatever the bound, the WHERE stays the **residual filter** — a bound only narrows which
rows are scanned, so a superset bound is always sound.

### 5.1 Consumer policies over one bound inventory

The detector is one inventory with an explicit **consumer policy**, not separate SELECT,
UPDATE, DELETE, and EXPLAIN ladders. This is behavior-neutral plumbing for the rule-based
extensions: it makes later eligibility changes one policy edit while preserving today's choices.

- **SELECT** admits PK, ordered B-tree, GiST, GIN, PK interval-set, and ordered-index interval-set
  candidates in the §5 order.
- **UPDATE/DELETE** admit PK, ordered B-tree, GIN, GiST, PK interval-set, and ordered-index interval-set.
  Their established GIN-before-GiST order is preserved (unlike SELECT's GiST-before-GIN order), and
  interval sets remain the last resort after every contiguous/opclass bound except for the
  same-key clipping case above. A host-attached target
  policy-disables every bound and full-scans through its scoped store, unchanged.
- **DML EXPLAIN** renders the same typed mutation physical plan execution consumes. It does not run
  a parallel detector.

Access-path execution has a common key-preserving result: deterministic `(storage key, row)`
candidates plus the exact up-front `page_read` / `value_decompress` / access-method work block.
SELECT compatibility feeds may discard the storage keys; mutations retain them for their phase-2
writes. Per-row `storage_row_read`, residual-filter evaluation, projection, and mutation validation
remain downstream, so this normalization changes neither accrual order nor totals. A full or
contiguous-PK scan may realize the same contract as a pull source rather than an eager vector.

For a single-table LIMIT with no blocking operator, that pull source also covers ordered B-tree
bounds, canonical interval sets, and GIN/GiST candidate keys. Contiguous access paths retain their
up-front structural page block. An interval set charges each disjoint interval on first pull, so a
filled window never starts or charges later intervals. GIN/GiST complete and charge their opclass
gather before table fetch, then stop point-lookups and residual work at OFFSET+LIMIT. An ORDER BY
elides its sort only when the source emits the requested PK order or walks the exact ordering index.

## 6. Neutrality and determinism

- **Same plan everywhere.** For a given (query, catalog) every core must choose the same
  plan: tie-breaks are structural and deterministic (the lowest-lowercased-name index,
  FROM-order left-deep joins), never map-iteration or cost-estimate dependent. Plan choice
  is observable through the metered cost and the EXPLAIN dump, both corpus-pinned — a
  divergent planner is a failing `.test` file, not a silent drift.
- **The pass structure is behavior-neutral scaffolding.** Splitting the stages changed no
  gate, no precedence, no tie-break; the corpus (cost pins, EXPLAIN suites, the NoREC/TLP
  sweep) passed unchanged across the refactor, which is the byte-identity proof.
- **The forward hazard is ratified elsewhere:** as the optimizer grows, independently
  hand-written planners may pick different plans, and cost-identity silently presumes
  plan-identity — the unratified class-**P** fork ([determinism.md §8](determinism.md)).
  Until it is ratified, every new rule keeps plan choice structurally deterministic and
  `# cost:` assertions stay on shapes where all cores plan identically.

## 7. Where future passes plug in

- **Predicate pushdown + simplification** (TODO.md) — the first stage-2 rewrite rules,
  under the §3 contract.
- **Plan-time cost estimator** (TODO.md) — a stage-3 *annotation* pass: estimate the same
  units the runtime meter charges for each candidate, as a spec'd, cross-core-identical
  artifact; feeds the future EXPLAIN `est_rows`/`est_cost` columns (explain.md §7).
- **Cost-based access-path + join-order selection** (TODO.md) — replaces §5's fixed
  precedence and the FROM-order join tree *inside* stage 3, once the estimator + table
  statistics exist; re-pins the affected `# cost:` entries and forces the class-P decision
  (§6).
- **New physical rules** (LIMIT + index-bound streaming, hash join,
  top-k heap — each tracked in TODO.md) land as
  discrete rule functions in the §4 inventory, each with its NoREC relation.
