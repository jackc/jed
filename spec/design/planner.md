# The planner: explicit optimizer-pass structure — design

> How a SELECT becomes an executable plan, and where each optimization lives. The planner
> is a **deterministic rule engine** whose cost-based single-relation and join choices have
> landed: it resolves
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
| 1 | **scan bounds** | per base relation (not SRF/derived): inventory and estimate every legal path; one-base-relation and eligible P7 two-base-relation SELECTs consume the complete inventory, while other joins and mutations retain §5.1's explicit staged boundaries | `relBounds[i]` | cost.md §3 "bounded scan", "index-bounded scan", "GIN-bounded scan", "GiST-bounded scan", "canonical interval sets" |
| 2 | **index-nested-loop** | a join inner base relation (INNER/CROSS/LEFT right side, not lateral/CTE) with a PK / leading B-tree comparison or GIN/GiST query operand from a bare **earlier sibling** column in ON or WHERE | `relINLBounds[i]` | cost.md §3 "JOIN" (per-outer-row seek/gather) |
| 3 | **hash join** | exactly two non-lateral inputs; INNER/LEFT ON contains one or more same-type, key-encodable bare-column equalities across the inputs; no inner INL; every remaining ON conjunct is a non-trapping leaf equality/inequality | `hashJoin` | cost.md §3 "hash JOIN" (`hash_build`/`hash_probe`; ON only for bucket candidates) |
| 4 | **ORDER BY via PK scan order** | single base relation, non-aggregate, column-only keys: the ORDER BY is a one-direction PK prefix (ASC) or the full PK (DESC ⇒ reverse scan), collation-matching the stored key | `pkOrdered`, `pkReverse` | cost.md §3 "ORDER BY satisfied by primary-key order" (sort elided; with LIMIT, a top-N) |
| 5 | **single-relation pipeline choice / ORDER BY via secondary-index order** | one base relation: compose every access candidate with its natural PK/index order, add every eligible order-only B-tree walk when LIMIT is present, and minimize cumulative scheduled cost through LIMIT/OFFSET; exact index-order shape/type gates remain | `relBounds[0]`, `pkOrdered`, `pkReverse`, `indexOrder` | estimator.md §9.1; cost.md §3 "ORDER BY satisfied by secondary-index order" |
| 6 | **bounded costed join search** | P7's two-base shape and P8's maximal hard-fenced base INNER/CROSS islands: exhaustive Pareto-frontier left-deep DP through 8 movable relations, deterministic cheapest-next above it; ordinary access, physically-dependent INL, and safe ON-equijoin hash are chosen per step | `relationOrder`, `joinSteps`, `relBounds`, `relINLBounds` | estimator.md §9.2/§10; cost.md §3 "JOIN" |
| 7 | **join sort-elision** | a selected fence-free left-deep INNER/CROSS tree, a LIMIT, forward driver-PK ORDER BY with no key beyond that PK, no eager non-PK bound on the driver; the materialized left subtree feeds a streaming final join step | `joinPkOrdered` | estimator.md §10.4; cost.md §3 "JOIN" (the join top-N) |
| 8 | **blocking ORDER BY top-k** | rules 4–7 did not elide the sort; plain SELECT (no DISTINCT, aggregate/group, or window), ORDER BY + constant LIMIT; checked `K = OFFSET + LIMIT` (`LIMIT 0` ⇒ K=0) | `topK` | cost.md §3 "blocking ORDER BY top-k" (full scan/evaluation and cost retained; sort work reduced) |

Data-flow dependencies fixing the order: rules 2–3 first form the staged legacy join choice; rule 5
reads the complete rule-1 inventory and subsumes rule 4's provisional single-relation order decision;
rule 6 replaces the staged join fields for eligible P7/P8 shapes and evaluates each candidate's
query-specific order property; rule 7 records the winning candidate's join sort-elision; rule 8 reads
the three preceding sort-elision decisions. Rules 4–6
that select scan order remain mutually exclusive by their gates; hash join preserves the same
physical-outer then physical-inner candidate enumeration and may compose with join sort-elision.

The physical fields live in a dedicated sub-struct of the plan (`phys` — Go
`physicalPlan`, Rust/TS `PhysicalPlan`), so the stage boundary is visible in the type: the
logical fields plus the `relMasks` annotation are stage 1's output, `phys` is stage 3's.
`relationOrder` maps physical positions to source ordinals; it never rewrites resolved logical
column slots. `joinSteps[position - 1]` records the newly-ready authored `ON` ordinals and the
algorithm for appending that physical relation. P7 is the two-entry instance of the same P8 shape.

The **mechanisms** the rules call — `detectScanBound`, `detectINLBound`,
`buildIndexAccessPredicate`, `orderSatisfiedByPK`, `orderSatisfiedByIndex`, interval-set
reduction — are shared pattern-matching/encoding machinery, not rules; they also serve
UPDATE/DELETE planning and exec-time eligibility checks, and live with the access-path
code, not in the optimizer pass.

**EXPLAIN** renders every rule's decision ([explain.md §4](explain.md)): rule 1 as the
scan's access-path detail, rule 2 as the `Index-nested-loop` prefix, rule 3 as `Hash Join`, rules
4–6 as the sort-elision note (`ordered: pk ordered` / `index order: <index>` / `join pk ordered`) —
and rule 7 as `Sort keys=N, top-k=K`, which makes each rule corpus-assertable without touching
internals.

## 5. Access-path inventory and staged selection (rule 1)

For each base relation, the access-path machinery inventories every legal candidate. P6 cost-selects
the complete set for eligible one-base-relation SELECTs (§5.2); every deferred shape still uses
the legacy selector, whose fixed precedence is:

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

Every core builds one complete, policy-free inventory, then applies an explicit **consumer policy**;
there are no separate SELECT, UPDATE, DELETE, and EXPLAIN detection ladders. This P3 plumbing is
behavior-neutral and leaves the selected physical `ScanBound` union unchanged.

The inventory contains one candidate for each legal physical access path: PK tuple, every eligible
ordered B-tree index, every eligible GiST index, every eligible GIN index, PK interval set, every
eligible ordered-index interval set, and an explicit full scan. A host-attached relation has only the
full candidate until bounded attachment execution is scoped correctly. A relation without a WHERE
also has only the full candidate.

Each candidate carries these explicit planning facts:

- a collision-free identity `(kind, lowercased_index_name)`, with an empty name for PK, PK interval,
  and full paths;
- the existing executor `ScanBound`, absent only for full scan;
- its scan-order capability: reversible table-storage-key order, or forward order in one named
  B-tree index; and
- the complete resolved WHERE as the required residual filter, absent only when there is no WHERE.

Inventory order is the P0 total access-path order: PK, ordered B-tree, GiST, GIN, PK interval,
ordered-index interval, full. Index-bearing candidates of the same kind sort by raw UTF-8 bytes of
their already-lowercased catalog name. Catalog container or map iteration must not affect this order.
This order is the future equal-estimate tie-break; the legacy selector below deliberately preserves
two older precedence exceptions and does not simply take the inventory's first element.

- **SELECT** admits PK, ordered B-tree, GiST, GIN, PK interval-set, and ordered-index interval-set
  candidates in the §5 order.
- **UPDATE/DELETE** admit PK, ordered B-tree, GIN, GiST, PK interval-set, and ordered-index interval-set.
  Their established GIN-before-GiST order is preserved (unlike SELECT's GiST-before-GIN order), and
  interval sets remain the last resort after every contiguous/opclass bound except for the
  same-key clipping case above. A host-attached target's inventory admits only full scan and routes
  it through its scoped store, unchanged.
- **DML EXPLAIN** renders the same typed mutation physical plan execution consumes. It does not run
  a parallel detector.

The legacy selector preserves two details exactly: a same-key interval set with a direct clipping
range replaces the broader contiguous PK/B-tree bound, and UPDATE/DELETE try GIN before GiST while
SELECT tries GiST before GIN. Within every index-bearing kind, the lowest lowercased name wins.
P3 changes no physical plan field, executor dispatch, EXPLAIN spelling, or actual metered cost.
P4 inventories once per base relation and attaches one estimate per candidate: logical output rows,
access scan rows expressed through scheduled unit counts, weighted cost, and the canonical tie key.
P6 now consumes that vector as the base annotation for §5.2's complete pipeline set; joins and
mutations retain explicit
legacy policies rather than accidentally inheriting a partial cost selector.

### 5.2 P6 single-relation SELECT policy

For a SELECT with exactly one base relation and no join, rule 1 computes the complete inventory and
P4 estimate vector once. Rule 5 then composes every access identity with the natural storage/index
order it can provide, adds every eligible order-only B-tree identity not already present when LIMIT
is present, and compares the complete scheduled estimate through residual filtering, projection,
ordering and LIMIT/OFFSET.
GiST, GIN, both interval-set kinds, PK, every ordered B-tree, and full scan all participate. Multiple
matching ordering indexes participate independently in canonical name order. Multi-relation SELECTs
retain legacy per-relation selection until P7, and UPDATE/DELETE retain §5.1's mutation policy until
their dedicated slice.

Rules 4 and 5 consume each candidate's explicit scan-order property. A PK ORDER BY is
elided only for a table-storage-order candidate; a B-tree bound can always elide only the exact same
index order, while a boundless order-only candidate additionally requires LIMIT. The chosen bound
always retains the complete WHERE as its residual. A plain or ordered LIMIT may change the winner
only through scheduled work it avoids; blocking sort work remains unmetered and contributes no
private planner weight.

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

### 5.3 P7 two-relation SELECT policy

An eligible exactly-two-base-relation INNER/CROSS SELECT consumes both complete ordinary access
inventories and a sibling-bound inventory for each possible physical inner. It compares both source
orientations and all legal nested-loop, hash, and index-nested-loop candidates as complete pipelines.
The hash gate remains the existing safe ON-equijoin gate; WHERE-derived hash keys are not a P7
rewrite. A sibling INL source must lie in the selected physical outer relation, independent of its
original FROM ordinal.

`relationOrder` is physical-position → source-ordinal. The executor materializes/scans by that map,
but combines each pair into the original source relation's global slot interval before evaluating
resolved expressions. EXPLAIN renders the selected physical child order. Exact ties compare source
ordinal sequences, then each physical relation's access identity, then join algorithm, as
[estimator.md §9](estimator.md) specifies.

Join-PK ordering is recalculated per candidate against the selected physical outer. Its deterministic
row-count-only prefix estimate discounts only skipped probe/ON/INL work; ordinary base scans and hash
build remain complete. Outer joins, LATERAL, CTE/SRF/derived inputs, and wider joins keep the staged
FROM-order behavior until P8 or a barrier-specific slice.

### 5.4 P8 N-way SELECT policy

P8 generalizes §5.3 through the bounded state search in [estimator.md §10](estimator.md). Each
authored `ON` tree is scheduled intact at its earliest dependency-complete physical step; every
selected step carries its own nested-loop/INL/hash choice. The executor places each appended base
row into its resolved source slot range before evaluating those source-ordered predicates, so a
physical permutation never changes expression slots.

Outer joins, LATERAL/correlation, SRFs, CTEs, and derived inputs are hard fences. The physical order
may change only inside maximal all-base INNER/CROSS islands and never moves an input or compound
subtree across a fence. DP retains the Pareto frontier required by access-dependent physical row
counts through eight movable relations; larger islands use the one-state deterministic
cheapest-next fallback. The final selected tree alone feeds ORDER BY/LIMIT recomputation, with only
its final join step eligible for N-way streaming top-N.

## 6. Neutrality and determinism

- **Same plan everywhere.** For a given resolved query and visible estimator inputs every core
  must choose the same plan. Today's selector uses structural tie-breaks (the
  lowest-lowercased-name index and FROM-order left-deep joins); Path B replaces those primary
  choices with the exact shared estimate and total candidate order in
  [estimator.md](estimator.md). Neither path may depend on map iteration. Plan choice is observable
  through metered cost and EXPLAIN, both corpus-pinned — a divergent planner is a failing `.test`
  file, not a silent drift.
- **The pass structure is behavior-neutral scaffolding.** Splitting the stages changed no
  gate, no precedence, no tie-break; the corpus (cost pins, EXPLAIN suites, the NoREC/TLP
  sweep) passed unchanged across the refactor, which is the byte-identity proof.
- **The forward hazard is resolved by specifying the plan.** Path B keeps plan identity inside
  the cross-core contract: shared estimator facts, exact arithmetic, complete candidate order,
  and bounded search are specified in [estimator.md](estimator.md) and ratified in
  [determinism.md §8](determinism.md). The algorithms remain hand-written per core. Shared
  estimator vectors, complete-pipeline EXPLAIN rows, actual `# cost:` pins, and NoREC relations detect
  drift.

## 7. Where future passes plug in

- **Predicate pushdown + simplification** (TODO.md) — the first stage-2 rewrite rules,
  under the §3 contract.
- **Plan-time cost estimator** ([estimator.md](estimator.md)) — a stage-3 annotation and selection pass:
  estimate the same units the runtime meter charges for each candidate using the ratified exact,
  cross-core-identical contract. Whole-plan estimates now feed EXPLAIN's `est_rows`/`est_cost`
  columns. P6 consumes complete one-relation access/ordering pipelines; later slices add join search
  ([explain.md](explain.md) §2).
- **Cost-based access-path + join-order selection** (TODO.md) — replaces §5's fixed
  precedence and the FROM-order join tree *inside* stage 3, once the estimator + table
  statistics exist; each enabling slice re-pins affected `# cost:` entries and proves the
  already-ratified plan-identity contract (§6).
- **New physical rules** (the hash join above and later access paths tracked in TODO.md) land as
  discrete rule functions in the §4 inventory, each with its NoREC relation.
