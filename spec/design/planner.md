# The planner: explicit optimizer-pass structure — design

> How a SELECT becomes an executable plan, and where each optimization lives. The planner
> is a **deterministic rule engine** whose cost-based single-relation and join choices have
> landed: it resolves
> the query into a **logical plan**, applies **rewrite rules** (WHERE contradiction detection and
> WHERE/ON predicate pushdown — §3), and then runs **physical/access-path selection** — a fixed, ordered list of discrete rules,
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
rewrite rules            → logical plan         (stage 2 — rewriteWhere, §3)
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
| WHERE rewrite | `rewriteWhere` (rewrite.go) | `rewrite_where` (executor/rewrite.rs) | `rewriteWhere` (rewrite.ts) |
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

## 3. Stage 2 — rewrite rules

Stage 2 sits at a fixed position in each core's `planSelect`, after the logical plan and its
`relMasks` annotation are complete and before `optimizeSelect`. Its one driver, `rewriteWhere`, runs
contradiction detection (§3.1) and, when that does not fire, predicate pushdown of the WHERE (§3.2) and
of each join's ON (§3.3). It records its decision in a `pushdown` annotation of the plan and **never
rewrites `filter` or an ON**: the complete WHERE and every complete ON remain the input every
access-path detector, index-nested-loop detector, hash-key detector, join-dependency walk, touched-set
walk, and selectivity rule reads, so bounds and estimates see exactly the predicates they saw before.
Execution reads the annotation: a contradiction reads nothing, a pushed filter runs inside its
relation's scan, and only the **residuals** (`postJoinFilter`, and each join's residual ON) are
evaluated over joined rows. The one rewrite that does change a predicate is §3.2's **body pushdown**,
and it changes only a derived table's *body*, which is planned again as if the conjuncts had been
written there.

The contract every rewrite rule meets:

- **Plan→plan and pure** — it transforms the logical plan before parameters are bound; it
  never executes anything and never reads a parameter value.
- **Results-identical, and never a new error.** The same rows on every core. A rewrite may
  evaluate an expression on rows the unrewritten plan would not reach only when that expression is
  structurally unable to trap; it may skip evaluations the unrewritten plan performs — exactly as a
  scan bound already does — so an error that plan would raise can go unraised. Error visitation, and
  the point where a cost ceiling aborts (54P01), are defined by the selected (rewritten) plan, as for
  every physical choice ([estimator.md](estimator.md) §1).
- **Cost-identical, or an explicitly decided cost change.** Cost is an observable cross-core
  contract (CLAUDE.md §13, cost.md §1). Textbook rewrites — constant folding, CSE,
  short-circuiting — **drop `operator_eval` charges**, so each rule records its cost decision here
  and re-pins the affected `# cost:` corpus entries in the same change; never a silent apply.
- **A NoREC relation in the same change** ([conformance.md §8](conformance.md)) — the
  metamorphic sweep does not discover new optimizations on its own.

General constant folding, CSE, and redundant-recheck elimination remain unimplemented; each would
need its own cost decision under this contract.

### 3.1 Contradiction detection (`query.where_contradiction`)

Flatten the WHERE's top-level AND-chain (nested ANDs included; an OR, NOT, or any other node is one
opaque conjunct). The WHERE is a **contradiction** when any of these plan-time proofs holds:

1. a conjunct is the literal `FALSE` or a literal `NULL`;
2. a conjunct is a comparison (`= < <= > >=`) between two literals that is never TRUE — either side
   NULL, or a same-kind literal pair whose order fails the operator; text literals are compared only
   by `=` (byte identity — every jed collation is deterministic, [collation.md](collation.md) §6),
   never by ordering, which a collation may decide;
3. among the conjuncts that compare a **bare column** with a literal, the estimator's contradiction
   inventory ([estimator.md](estimator.md) §7.1) finds a NULL literal or two comparisons on the same
   column with no common value (`x > 5 AND x < 3`, `x = 1 AND x = 2`, `x = 1 AND x > 1`), under the
   same literal comparison and text-equality restriction as rule 2.

Rule 3 deliberately admits only bare columns: a column has one value per row, while a structurally
equal expression may not (`random() > 0.9 AND random() < 0.1` can be TRUE). Parameters are never
proven (planning is pre-bind), nor are disjunctions or non-column operands; a missed proof only
forgoes the optimization. Contradictions over a scan key's parameters remain the runtime empty-bound
rule of cost.md §3.

**Cost decision.** A contradictory SELECT reads **no relation**: no base table page or row, no SRF
generated row, no `cte_scan_row`, no derived or inlined-CTE body, and no FROM-less virtual row. Nothing
is charged for them. Everything above the FROM runs over the empty input exactly as it would over an
empty table: an ungrouped aggregate produces its one row (its `row_produced`), HAVING is evaluated
once on it, GROUP BY/window/DISTINCT/ORDER BY produce nothing. Uncorrelated expression subqueries are
still folded once before execution, as for any query. Each set-operation arm and each subquery is its
own SELECT and is judged independently. The streaming pull lane keeps its lane and simply produces
nothing. Mutations retain their existing runtime empty-bound behavior; a DML contradiction rule is a
follow-on.

### 3.2 WHERE pushdown (`query.where_pushdown`, `query.derived_pushdown`)

With no contradiction, each top-level WHERE conjunct, in source order, is **pushed** toward relation
`i` when all of these hold:

- it references at least one column, and every column it references belongs to relation `i` (an
  outer reference makes it unsafe, below);
- relation `i` is not **lateral** (a correlated LATERAL relation is re-produced per left row; a
  conjunct on it stays in the residual);
- no outer join NULL-extends relation `i`: for the left-deep FROM, `joins[k]` LEFT marks
  `rels[k+1]`, RIGHT marks every `rels[0..k]`, FULL marks both;
- the conjunct is **pushdown-safe**: a tree of AND/OR/NOT over comparisons (`= <> < <= > >=`, except a
  collated ordering comparison), IS [NOT] NULL, IS [NOT] DISTINCT FROM, and bare boolean columns, whose
  operands are bare columns, literals, and parameters. Every such node is structurally unable to
  trap and reads only the current row, so evaluating it on a row the join would have discarded can
  neither raise an error nor change a value. Casts, arithmetic, functions, CASE, subqueries, and
  outer references stay in the residual.

A pushed conjunct then takes the first of these forms that applies:

1. **Body pushdown** (`query.derived_pushdown`) — relation `i` is a derived table whose body is
   **body-pushable** (below) and every column the conjunct references is an output column whose
   select-list item is a bare column of the body. The conjunct is rewritten with each reference
   replaced by that body column, and moves *into* the body. Applies with any number of FROM
   relations, including one (`SELECT … FROM (SELECT …) d WHERE d.k = 5`).
2. **Scan pushdown** — the SELECT has at least two FROM relations. A base table (`query.where_pushdown`)
   or a derived table, CTE reference, set-returning function, or catalog relation
   (`query.derived_pushdown`) runs the conjunct over its own rows before they reach the join.
3. Otherwise the conjunct stays in the residual (a single-relation SELECT gains nothing from a scan
   pushdown: the filter would run on the same rows either way).

A derived body is **body-pushable** when it is a single SELECT — not a set operation, `VALUES`, or a
nested `WITH` — with no aggregate or `GROUP BY`, no window function, and no `LIMIT` or `OFFSET`.
`DISTINCT` and `ORDER BY` are admitted: a pushdown-safe conjunct over bare output columns gives the
same answer for every row in one DISTINCT class (equal values compare equal, and every jed collation
is deterministic), so filtering before or after deduplication keeps the same classes.

**Body form.** A derived body's pushed conjuncts — the WHERE's, in source order, after any §3.3 ON
conjuncts pushed to the same relation — are appended to the body's own WHERE as a left-deep AND:
`((W AND c1) AND c2)`, or `(c1 AND c2)` when the body has no WHERE. The body is then **planned again
from its syntax** with that WHERE, so its own stage 2 (contradiction; pushdown, including into its
own derived tables) and stage 3 (access paths — the pushed conjunct can now bound the body's key)
treat it exactly as if it had been written in the body. Planning a body again has no other effect:
in particular the statement's CTE reference counts, and so every CTE's inline/materialize mode, are
unchanged. A conjunct that moved into a body is evaluated nowhere in the outer SELECT.

**Scan form.** The conjuncts scan-pushed to a relation form its **pushed filter**; all conjuncts that
were neither scan- nor body-pushed form the **residual**. Each is rebuilt as the **left-deep AND of its
conjuncts in source order** (so a filter of `n` conjuncts has `n − 1` AND nodes; the original WHERE's
AND nodes are not charged anywhere). Ordinary column slots are unchanged in the plan's copy; execution
evaluates a copy rebased to the relation's own row.

**Execution.** A base table's pushed filter runs wherever its rows are read — the materialized scan,
the per-outer-row index-nested-loop fetch, and the bounded spill lane — on each row the access path
admits, after its `storage_row_read` and before it is admitted to the query-memory row account or
reaches a join: a row that is not TRUE is dropped there. Any other relation is first produced exactly
as before (a derived or inline-CTE body runs, a materialized CTE's buffer is copied with its
`cte_scan_row` charges, an SRF or catalog relation generates its rows), with its rows reserved as
before; its pushed filter then runs over those rows in production order and releases each rejected
row's reservation before the join reads the relation. The residual, when present, is evaluated on
each joined row exactly where the complete WHERE used to be; when nothing remains there is no
post-join filter at all.

**Cost decision.** A scan-pushed conjunct charges its `operator_eval` (and size-scaled comparison
units) once per **admitted or produced row** of its relation — repeated per outer row for an
index-nested-loop inner — instead of once per **surviving joined row**; rows it rejects never reach
the ON predicate, hash build/probe, the residual, or (for a base table) the row account. A
body-pushed conjunct charges wherever the replanned body evaluates its WHERE — per body row, or per
admitted base row when the body pushes it further — and, when it bounds the body's access path, the
rows it excludes are never read, projected, or charged at all. The residual charges as the old WHERE
did, over joined rows. Both rules are unconditional (PostgreSQL's choice), so the change is usually a
reduction but is not monotone: when a join discards more rows than it multiplies, evaluating the
conjunct on every base row can cost more than evaluating it on the few joined survivors. The
estimator models the split ([estimator.md](estimator.md) §8.3), so cost-based join search sees it, and
the actual meter remains authoritative. Because only non-trapping conjuncts move, pushdown never raises
an error the unrewritten plan would not; it can leave unraised an error on a row the moved conjunct
now rejects first — an ON-predicate error on a pair, or a body's select-list error on a body row —
the same narrowing a scan bound already performs.

EXPLAIN renders a scan-pushed filter on its relation's node (`Scan`, `Subquery`, `CTE Scan`, `SRF`,
`Catalog Scan`), a body-pushed conjunct inside the body's own plan, and the residual as the Filter node
([explain.md](explain.md) §5).

**Not pushed into a body.** A CTE reference is scan-pushed only, even when its CTE is inlined: whether
a CTE is inlined is decided from its reference count after the whole statement is planned, so
pushing into an inlined CTE's body needs a per-reference body specialization (with its own EXPLAIN
and estimate attribution) and is a follow-on. So are pushdown into a grouped body (a conjunct over
grouping columns), into each arm of a set operation, and through a lateral relation.

### 3.3 ON pushdown (`query.on_pushdown`)

Each join `joins[k]` with an ON flattens the ON's top-level AND-chain. A conjunct that is
pushdown-safe and references exactly one relation `i`, which is not lateral, is pushed toward `i`
when the join kind allows it:

- **INNER**: `i` is either input — `rels[k+1]`, or any `rels[0..k]`;
- **LEFT**: `i` is the right input `rels[k+1]` (the NULL-extended side);
- **RIGHT**: `i` is one of the left inputs `rels[0..k]` (the NULL-extended side);
- **FULL**: never (both sides are preserved).

A conjunct that filters the *preserved* side of an outer join is never pushed: it decides which
preserved rows match, not which survive. A left input `i ≤ k` additionally must not be NULL-extended
by an earlier join `joins[0..k-1]` (the §3.2 rule applied to that prefix): filtering a NULL-extended
relation before its own outer join would manufacture NULL-extended rows that the ON — perhaps
`x IS NULL` — would then judge differently. A later join never matters: it consumes this join's output,
which pushdown does not change.

A pushed ON conjunct takes the same body or scan form as a WHERE conjunct (§3.2); unlike the WHERE,
ON pushdown always has at least two relations, so the scan form is always available. A relation's
pushed filter, and a derived body's appended conjuncts, list the pushed conjuncts in **source order**:
each join's ON conjuncts in join order, then the WHERE's. Each join's remaining ON conjuncts form its
**residual ON**, the left-deep AND in source order; when none remains the join has no ON predicate at
all (an INNER join then pairs every surviving row, a LEFT/RIGHT join NULL-extends a preserved row only
when the other side has no surviving row).

**Execution and cost.** Exactly as §3.2: the pushed conjunct runs on each admitted or produced row of
its relation instead of on every candidate pair, and the residual ON is evaluated wherever the
complete ON was — nested loop, hash join (after key match), index nested loop, N-way step, streaming
and spill lanes. Hash keys, index-nested-loop bounds, and N-way ON ownership are still detected from
the complete ON; the hash keys are always cross-relation equalities and therefore always remain in
the residual. The cost decision is §3.2's: per relation row instead of per pair, unconditional, not
monotone, modelled by the estimator. EXPLAIN shows the residual ON on the join node (`on:conjuncts=N`
/ VERBOSE `on=<expr>`, omitted when none remains) and the pushed conjuncts on the relation, as §3.2.

## 4. Stage 3 — physical/access-path selection: the rule inventory

`optimizeSelect` applies the rules below **in this fixed order** (later rules read earlier
rules' output; the order is part of the cross-core contract). Each rule is one function
owning its **gate** (the structural pattern it requires) and its **action** (the physical
fields it sets). A rule that does not fire leaves its fields zero-valued — the executor
then takes the unoptimized path (full scan, eager sort), which is always correct.

| # | rule | gate (summary) | sets | cost contract |
|---|---|---|---|---|
| 1 | **scan bounds** | per base relation (not SRF/derived): inventory and estimate every legal path; one-base-relation and eligible cost-searched SELECT relations and UPDATE/DELETE targets consume the complete inventory, while fixed barrier inputs retain §5.1's explicit staged boundary | `relBounds[i]` | cost.md §3 "bounded scan", "index-bounded scan", "GIN-bounded scan", "GiST-bounded scan", "canonical interval sets" |
| 2 | **index-nested-loop** | a join inner base relation (INNER/CROSS/LEFT right side, not lateral/CTE) with a PK / leading B-tree comparison or GIN/GiST query operand from a bare **earlier sibling** column in ON or WHERE | `relINLBounds[i]` | cost.md §3 "JOIN" (per-outer-row seek/gather) |
| 3 | **hash join** | exactly two non-lateral inputs; INNER/LEFT ON contains one or more same-type, key-encodable bare-column equalities across the inputs; no inner INL; every remaining ON conjunct is a non-trapping leaf equality/inequality | `hashJoin` | cost.md §3 "hash JOIN" (`hash_build`/`hash_probe`; ON only for bucket candidates) |
| 4 | **ORDER BY via PK scan order** | single base relation, non-aggregate, column-only keys: the ORDER BY is a one-direction PK prefix (ASC) or the full PK (DESC ⇒ reverse scan), collation-matching the stored key | `pkOrdered`, `pkReverse` | cost.md §3 "ORDER BY satisfied by primary-key order" (sort elided; with LIMIT, a top-N) |
| 5 | **single-relation pipeline choice / ORDER BY via secondary-index order** | one base relation: compose every access candidate with its natural PK/index order, add every eligible order-only B-tree walk when LIMIT is present, and minimize cumulative scheduled cost through LIMIT/OFFSET; exact index-order shape/type gates remain | `relBounds[0]`, `pkOrdered`, `pkReverse`, `indexOrder` | estimator.md §9.1; cost.md §3 "ORDER BY satisfied by secondary-index order" |
| 6 | **bounded costed join search** | maximal hard-fenced base INNER/CROSS islands: exhaustive Pareto-frontier left-deep DP through 8 movable relations, deterministic cheapest-next above it; ordinary access, physically-dependent INL, and safe ON-equijoin hash are chosen per step | `relationOrder`, `joinSteps`, `relBounds`, `relINLBounds` | estimator.md §9.2/§10; cost.md §3 "JOIN" |
| 7 | **join sort-elision** | a selected fence-free left-deep INNER/CROSS tree, a LIMIT, forward driver-PK ORDER BY with no key beyond that PK, no eager non-PK bound on the driver; the materialized left subtree feeds a streaming final join step | `joinPkOrdered` | estimator.md §10.4; cost.md §3 "JOIN" (the join top-N) |
| 8 | **blocking ORDER BY top-k** | rules 4–7 did not elide the sort; plain SELECT (no DISTINCT, aggregate/group, or window), ORDER BY + constant LIMIT; checked `K = OFFSET + LIMIT` (`LIMIT 0` ⇒ K=0) | `topK` | cost.md §3 "blocking ORDER BY top-k" (full scan/evaluation and cost retained; sort work reduced) |

Data-flow dependencies fixing the order: rules 2–3 first form the staged fixed join choice; rule 5
reads the complete rule-1 inventory and subsumes rule 4's provisional single-relation order decision;
rule 6 replaces the staged join fields for eligible cost-searched shapes and evaluates each
candidate's query-specific order property; rule 7 records the winning candidate's join sort-elision;
rule 8 reads
the three preceding sort-elision decisions. Rules 4–6 that select scan order remain mutually
exclusive by their gates; hash join preserves the same physical-outer then physical-inner candidate
enumeration and may compose with join sort-elision.

The physical fields live in a dedicated sub-struct of the plan (`phys` — Go
`physicalPlan`, Rust/TS `PhysicalPlan`), so the stage boundary is visible in the type: the
logical fields plus the `relMasks` annotation are stage 1's output, `phys` is stage 3's.
`relationOrder` maps physical positions to source ordinals; it never rewrites resolved logical
column slots. `joinSteps[position - 1]` records the newly-ready authored `ON` ordinals and the
algorithm for appending that physical relation. The two-relation form is the smallest instance of
the same N-way shape.

The **mechanisms** the rules call — `detectScanBound`, `detectINLBound`,
`buildIndexAccessPredicate`, `orderSatisfiedByPK`, `orderSatisfiedByIndex`, interval-set
reduction — are shared pattern-matching/encoding machinery, not rules; they also serve
UPDATE/DELETE planning and exec-time eligibility checks, and live with the access-path
code, not in the optimizer pass.

**EXPLAIN** renders every rule's decision ([explain.md §4](explain.md)): rule 1 as the
scan's access-path detail, rule 2 as the `Index-nested-loop` prefix, rule 3 as `Hash Join`, rules
4–7 as the sort-elision note (`ordered: pk ordered` / `index order: <index>` / `join pk ordered`) —
and rule 8 as `Sort keys=N, top-k=K`, which makes each rule corpus-assertable without touching
internals.

## 5. Access-path inventory and staged selection (rule 1)

For each base relation, the access-path machinery inventories every legal candidate. The cost
selector chooses from the complete set for eligible one-base-relation SELECTs (§5.2) and for
UPDATE/DELETE targets (§5.5); every fixed-policy shape uses the structural selector, whose precedence
is:

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
rows are scanned, so a superset bound is always sound. (In a join, the conjuncts §3.2 pushes to the
relation are rechecked by its scan and the rest after the join; every conjunct is still evaluated.)

### 5.1 Consumer policies over one bound inventory

Every core builds one complete, policy-free inventory, then applies an explicit **consumer policy**;
there are no separate SELECT, UPDATE, DELETE, and EXPLAIN detection ladders. This separation is
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

Inventory order is the canonical total access-path order: PK, ordered B-tree, GiST, GIN, PK interval,
ordered-index interval, full. Index-bearing candidates of the same kind sort by raw UTF-8 bytes of
their already-lowercased catalog name. Catalog container or map iteration must not affect this order.
This order is the cost selector's equal-estimate tie-break; the fixed barrier selector below
deliberately preserves one older precedence exception and does not simply take the inventory's
first element.

- **SELECT** admits PK, ordered B-tree, GiST, GIN, PK interval-set, and ordered-index interval-set
  candidates in the §5 order.
- **UPDATE/DELETE** admit the same candidates and select by cost (§5.5). A host-attached target's
  inventory admits only full scan and routes it through its scoped store, unchanged.
- **DML EXPLAIN** renders the same typed mutation physical plan execution consumes. It does not run
  a parallel detector.

The fixed selector, used only by SELECT barrier inputs, preserves one detail exactly: a same-key
interval set with a direct clipping range replaces the broader contiguous PK/B-tree bound. Otherwise
it takes PK, B-tree, GiST, GIN, PK interval, index interval, then full. Within every index-bearing
kind, the lowest lowercased name wins. (UPDATE/DELETE used a variant of it, trying GIN before GiST,
until §5.5 replaced it.)
The planner inventories once per base relation and attaches one estimate per candidate: logical
output rows, access scan rows expressed through scheduled unit counts, weighted cost, and the
canonical tie key.
The cost selector consumes that vector as the base annotation for §5.2's complete pipeline set;
eligible joins use §5.3/§5.4, mutation targets use §5.5, and hard-fenced shapes retain the explicit
fixed policy rather than accidentally inheriting a partial cost selector.

### 5.2 Single-relation SELECT policy

For a SELECT with exactly one base relation and no join, rule 1 computes the complete inventory and
estimate vector once. Rule 5 then composes every access identity with the natural storage/index
order it can provide, adds every eligible order-only B-tree identity not already present when LIMIT
is present, and compares the complete scheduled estimate through residual filtering, projection,
ordering and LIMIT/OFFSET.
GiST, GIN, both interval-set kinds, PK, every ordered B-tree, and full scan all participate. Multiple
matching ordering indexes participate independently in canonical name order. Eligible
multi-relation SELECTs feed the same inventories into §5.3/§5.4's join search; UPDATE/DELETE targets
use §5.5.

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

### 5.3 Two-relation SELECT policy

An eligible exactly-two-base-relation INNER/CROSS SELECT consumes both complete ordinary access
inventories and a sibling-bound inventory for each possible physical inner. It compares both source
orientations and all legal nested-loop, hash, and index-nested-loop candidates as complete pipelines.
The hash gate remains the existing safe ON-equijoin gate; deriving hash keys from WHERE is not part
of physical selection. A sibling INL source must lie in the selected physical outer relation,
independent of its original FROM ordinal.

`relationOrder` is physical-position → source-ordinal. The executor materializes/scans by that map,
but combines each pair into the original source relation's global slot interval before evaluating
resolved expressions. EXPLAIN renders the selected physical child order. Exact ties compare source
ordinal sequences, then each physical relation's access identity, then join algorithm, as
[estimator.md §9](estimator.md) specifies.

Join-PK ordering is recalculated per candidate against the selected physical outer. Its deterministic
row-count-only prefix estimate discounts only skipped probe/ON/INL work; ordinary base scans and hash
build remain complete. Outer joins, LATERAL, CTE/SRF/derived inputs, and other hard fences keep their
staged authored-order behavior; wider eligible base INNER/CROSS islands use §5.4.

### 5.4 N-way SELECT policy

The N-way policy generalizes §5.3 through the bounded state search in
[estimator.md §10](estimator.md). Each
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

### 5.5 UPDATE/DELETE target policy (`dml.costed_access`)

A mutation target is one base relation with no join, ordering, or LIMIT, so its policy is §5.2
without composition: estimate every inventory candidate's access work plus the complete WHERE per
scan row over the snapshot the mutation scan reads, and take the minimum, keeping the first
candidate in canonical order on an exact tie ([estimator.md §9.3](estimator.md)). Mutation-only
work is per affected row and identical for every candidate, so it is not part of the comparison.
Execution plans after uncorrelated WHERE subqueries fold; DML EXPLAIN plans the unfolded filter.

The selected path's natural emission order — storage-key order, or the named index's order for an
ordered B-tree or ordered-index interval set — is the phase-one visitation order. It therefore
decides which row's WHERE/assignment/CHECK error is reported first, where a cost ceiling aborts,
and the order of volatile per-row assignments such as `nextval`; phase two still validates and writes
the complete batch, and the affected rows and successful end state do not depend on the path. The
NoREC `cost_plan_dml` relation compares cost-selected mutations against forms that defeat every
bound.

## 6. Neutrality and determinism

- **Same plan everywhere.** For a given resolved query and visible estimator inputs every core
  must choose the same plan. Eligible SELECTs use the exact shared estimate and total candidate
  order in [estimator.md](estimator.md); staged barrier policies use their specified
  structural order. Neither path may depend on map iteration. Plan choice is observable
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

- **Further stage-2 rewrites** (TODO.md) — pushdown into inlined CTE bodies, grouped bodies, and
  set-operation arms; DML contradictions; constant folding — each under the §3 contract with its own
  cost decision.
- **New physical rules** (the hash join above and later access paths tracked in TODO.md) land as
  discrete rule functions in the §4 inventory, each with its NoREC relation.
