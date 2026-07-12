# Temporary plan: finish rule-based planner extensions

Status: planning artifact only. Delete this file when the work is incorporated into `TODO.md` and
the relevant design documents.

## Goal and scope

Complete the open work named by `TODO.md` under **Rule-based extensions
(results-identical, no statistics)**, including the items cross-referenced to their home sections:

1. ordered B-tree scans for `UPDATE`/`DELETE`, including the already-deferred secondary-index
   point-set mutation path;
2. composite-primary-key point/prefix pushdown;
3. point-set/range intersection and unions containing range disjuncts;
4. `LIMIT` streaming combined with bounded index scans;
5. `join_pk_ordered` combined with index-nested-loop (INL);
6. GIN and GiST sibling bounds for INL;
7. the `ORDER BY` + `LIMIT` top-k heap;
8. an in-memory hash-join operator selected by a deterministic structural rule.

This plan does **not** introduce statistics, cost estimates, cost-based access-path selection, or
join reordering. It also does not absorb unrelated follow-ons from the broad home TODO bullets
(ordered index directions, partial-index inference, new opclasses, variable-width index tails,
etc.). Grace-hash spill remains the next storage-maturation slice after the in-memory hash operator;
it is not required to finish the rule-based planner section, but the operator must leave that seam.

The current tree already has PK point-set `UPDATE`/`DELETE`, plus GIN- and GiST-bounded mutations.
The missing DML work is ordinary ordered-B-tree access predicates and secondary-index point sets;
do not reimplement the landed paths.

## Invariants for every feature slice

- Update the authoritative design first (`planner.md`, `cost.md`, and the access-method/type design
  document concerned), then the capability manifest and `EXPLAIN` vocabulary where observable.
- Implement Rust, Go, and TypeScript in the same slice. A plan/cost change does not land in only one
  core.
- Keep the original predicate as a residual recheck unless the spec explicitly proves and prices
  recheck elision. This is the main results-identical safety property.
- Specify deterministic candidate ordering, deduplication, NULL/empty behavior, access-path
  precedence, exact accrual sites, and `max_cost` guard points before executor work.
- Add a focused shared conformance suite with rows, `EXPLAIN`, and `# cost:` pins. Add the required
  NoREC relation and run the PostgreSQL oracle check for the results/errors that are PG-comparable.
- Re-pin costs only where the new rule actually fires. Contrast cases must prove that every old
  fallback still chooses the old plan and retains its cost.
- Update `TODO.md` as each named slice lands; remove stale narrowing text from design docs and
  capability descriptions in the same change.
- Per slice, run targeted tests during development, then `rake verify`, `rake conformance`,
  `rake corpus:norec_sweep`, and `rake ci` before integration. Performance slices also get a
  checksum-equivalent before/after benchmark run.

## Dependency order

```text
Phase 0: neutral access-path plumbing
  -> Phase 1: ordered B-tree DML
  -> Phase 2: composite-PK tuple bounds
       -> Phase 3: bound-set algebra
       -> Phase 4: bounded LIMIT streaming
            -> Phase 5: join top-N + INL
                 -> Phase 6: GIN/GiST sibling INL

Phase 7: generic top-k heap (independent after Phase 0)

Phases 1-7
  -> Phase 8: deterministic hash join
```

The order deliberately freezes one generalized bound representation and one executor row-source
contract before adding combinations. Otherwise each combination would grow its own detector,
candidate ordering, and cost loop in all three cores.

## Phase 0 — behavior-neutral access-path consolidation

Land this as a pure refactor with **zero** corpus cost re-pins.

### Slice 0A: one bound inventory and precedence function

- Replace the duplicated SELECT-versus-DML detection ladders with shared access-path candidates plus
  an explicit consumer eligibility mask. Preserve today's observable precedence exactly:
  single-column PK -> ordered B-tree -> GiST -> GIN -> point set -> full scan for SELECT, while DML
  enables only the paths it already supports until Phase 1.
- Keep attachment restrictions, partial-index implication gates, expression-index gates, collation
  safety, fixed-width suffix gates, and deterministic lowest-name tie-breaking visible in the
  candidate/eligibility layer rather than scattered in executors.
- Do not move physical DML planning into SELECT's `SelectPlan`; give mutation planning a small typed
  physical plan of its own so `EXPLAIN UPDATE/DELETE` can eventually render the selected bound
  without re-detecting it.

### Slice 0B: one access-path execution result

- Normalize every bound executor to produce storage-key plus row (or a streaming equivalent), exact
  up-front `(page_read, value_decompress, access-method work)` units, and deterministic row order.
  SELECT may discard keys; mutations retain them for phase-2 writes.
- Preserve today's guard/accrual order and eager-versus-streaming decisions. This slice changes no
  plan and no cost; its purpose is to stop later features from adding a fourth copy of the scan loop.

Exit gate: all existing cost pins and `EXPLAIN` output are byte-for-byte unchanged in all cores.

## Phase 1 — ordinary B-tree access paths for `UPDATE`/`DELETE`

This is the lowest-risk missing feature and closes the stale split where GIN/GiST mutations are
accelerated but ordinary secondary indexes are SELECT-only.

### Slice 1A: contiguous ordered-index bound

- Enable the existing maximal equality-prefix plus optional trailing-range `IndexBound` for mutation
  target scans, with the same partial-index implication, expression-index, collation, and suffix
  eligibility rules as SELECT.
- Gather `(storage_key, old_row)` from the pre-mutation index state, residual-recheck the full WHERE,
  then run the existing two-phase validation/write machinery. PK-changing UPDATE and indexed-column
  UPDATE must remain safe because candidate collection completes before writes.
- Define cost as the matching SELECT scan block minus final `row_produced`, with `RETURNING` adding
  its normal projection/production work. Index maintenance remains unmetered.

### Slice 1B: secondary-index point-set mutation

- Enable the already-existing `IndexKeySet` mechanism for `UPDATE`/`DELETE` after contiguous bounds
  and GIN/GiST, matching SELECT's last-resort rule.
- Deduplicate candidate storage keys defensively even though distinct leading scalar values normally
  address disjoint row sets. This future-proofs expression/partial generalization without allowing a
  row to be updated twice.
- Include hit, miss, duplicate/NULL list member, non-unique index, PK-rekeying UPDATE, `RETURNING`,
  rollback-on-error, and fallback contrasts in the corpus.

Exit gate: the Secondary-indexes `UPDATE/DELETE` follow-on and the OR/IN secondary point-set DML
follow-on can both be removed from `TODO.md`.

## Phase 2 — composite-primary-key tuple bounds

Do this after Phase 1 so the new tuple bound automatically serves SELECT and mutation consumers
instead of creating another single-purpose PK path.

### Slice 2A: tuple-bound representation and exact point lookup

- Generalize `PkBound` from one scalar member to an ordered vector of PK members. Encode a complete
  equality tuple directly to the table storage key.
- Specify conflicts (`a=1 AND a=2`), NULL/out-of-range values, parameter/correlated sources,
  collation matching, and promoted-comparison rejection member by member.
- Cover SELECT, correlated subqueries, independent join-relation bounds, UPDATE, and DELETE through
  the shared consumer seam. Preserve the full residual predicate.

### Slice 2B: equality prefix plus optional range on the next member

- Reuse the secondary-index access-predicate model: maximal equality prefix over `(pk1, pk2, ...)`,
  optionally followed by range terms on the next member. Build `[prefix, prefix-successor)` for a
  pure prefix and the correct inclusive/exclusive endpoints for a trailing range.
- Keep DESC scan/order behavior unchanged; this slice narrows the storage range only.
- Extend INL to composite PKs only when the join predicate supplies a usable leading tuple prefix;
  do not invent skip-scan over a non-leading member.
- Add point, partial-prefix, prefix+range, out-of-declaration-order PK, mixed-width/collated member,
  mutation, correlated, INL, `ORDER BY`, miss, contradiction, and no-leading-member fallback cases.

Exit gate: remove the composite-PK point/prefix pushdown follow-on from `TODO.md` and the narrowing
from `constraints.md`.

## Phase 3 — bound-set algebra for OR/IN

Model this once as a canonical sorted, disjoint list of key intervals. A point is `[k,k]`; a range is
an interval. Canonicalization happens after parameter/sibling values are encoded, because only then
can intervals be ordered, clipped, merged, or found empty.

### Slice 3A: intersect a point set with a co-present range

- For `key IN (...) AND key <range>`, encode/deduplicate the points, construct the contiguous range,
  discard points outside it, and probe the survivors in key order.
- Apply to PK and leading ordered-index columns, and to mutation consumers already enabled by
  Phase 1. Empty intersection reads and costs zero; the full WHERE still rechecks admitted rows.
- This simpler slice fixes the intersection/canonicalization contract before union introduces
  overlapping intervals.

### Slice 3B: union point and range disjuncts

- Recognize a pure OR tree whose leaves all bound the same key: equality, `<`/`<=`/`>`/`>=`, and
  `BETWEEN` (including any point leaves). Mixed columns, `NOT`, non-bound expressions, promoted
  comparisons, or unsafe collations fall back unchanged.
- Encode, sort, merge overlapping/touching intervals, remove duplicates, and scan each disjoint
  interval once. Candidate rows must be emitted once in key order even when source disjuncts overlap.
- Cost is the sum of each **canonical disjoint interval's** bounded-scan units, not the source
  disjunct count. Specify whether adjacent closed integer intervals merge based on encoded key-space
  adjacency; use the same byte rule in all cores rather than host arithmetic.
- Start with PK and B-tree leading columns. Do not fold GIN/GiST predicates into this interval type;
  their candidate-set algebra is opclass-specific.

Exit gate: remove both OR/IN range follow-ons from `TODO.md`; `EXPLAIN` renders a stable interval-set
summary rather than source-tree accidents.

## Phase 4 — `LIMIT` streaming over bounded access paths

The result sequence must equal today's eager bound followed by WHERE, ORDER BY handling, OFFSET, and
LIMIT. `page_read` blocks remain defined by the chosen bound unless the spec explicitly changes them;
only per-candidate fetch/filter/projection work may short-circuit.

### Slice 4A: contiguous PK and ordered-index bounds

- Turn a contiguous ordered index range into a bounded row source. Combine `WHERE` access bounds with
  `ORDER BY` satisfied by that same index, removing `ruleOrderByIndexScan`'s current `relBounds == nil`
  gate when the bound and requested order are compatible.
- Also stream `LIMIT` without blocking ORDER BY through the exact candidate order the eager path
  currently returns. OFFSET counts residual-filter survivors, not raw index entries.
- Cover equality prefix, trailing range, composite PK prefix, empty/miss bounds, residual rejects,
  non-unique ties, OFFSET, collation, and the incompatible-order fallback.

### Slice 4B: canonical interval/point sets

- Stream Phase 3's sorted disjoint intervals sequentially and stop after `OFFSET+LIMIT` survivors.
  Do not revisit a row at interval boundaries.
- Preserve the defined sum-of-probes/interval `page_read` contract. Decide explicitly whether
  unstarted later intervals charge their block; prefer charging on first pull, consistent with the
  existing row-source seam, and re-pin costs cross-core.

### Slice 4C: GIN and GiST candidate gathers

- Keep each opclass gather complete when completeness is needed for intersection/union/descent, but
  stream the resulting storage-key-ordered candidate set into point lookup + residual filtering and
  stop those per-row reads at the LIMIT window.
- GIN still charges every posting entry actually combined; GiST still charges every node/descent
  visited to form the candidate set. Only table point-lookups, `storage_row_read`, residual work, and
  output work after the stop disappear.
- Add contrast cases where ORDER BY is incompatible and therefore still requires a blocking sort.

Exit gate: remove the ordered-index, GIN, and GiST "no LIMIT-streaming combination" narrowings from
their design/TODO text.

## Phase 5 — combine two-table join top-N with INL

- Relax `ruleJoinPkOrdered`'s blanket `relINLBounds` rejection only for the right-hand INL shape whose
  per-outer results preserve the nested-loop output order required by `join_pk_ordered`.
- Change the streaming join executor from "materialize right once" to "open the INL bound for the
  current left row", then stop the entire loop at `OFFSET+LIMIT` survivors. A NULL/empty inner bound
  must still produce correct LEFT behavior, although the initial combination should retain the
  current INNER/CROSS `join_pk_ordered` gate.
- Keep the outer scan in forward PK order and forbid an ORDER BY key beyond the outer PK, as today.
  A secondary-index INL may emit several inner rows; prove its order matches the eager nested-loop
  order or keep that subshape gated off until it does.
- Pin cost for hit/miss/NULL sibling values, residual ON/WHERE rejects, OFFSET, secondary-index fanout,
  and the old blocking-sort contrasts. `EXPLAIN` must show both INL and `join pk ordered` rather than
  hiding one decision.

Exit gate: remove the INL + `join_pk_ordered` follow-on.

## Phase 6 — GIN/GiST sibling bounds for INL

These are separate slices because their admissible operands, candidate algebra, and metered work are
different. Both reuse Phase 5's per-outer bounded row source.

### Slice 6A: GIN sibling operand

- Permit the query operand of an accelerable GIN predicate to come from an earlier sibling row,
  evaluated once per outer row: array containment/overlap/equality and scalar `= ANY(array)` only
  where the existing opclass can derive a sound candidate superset.
- Retain constant operands as the existing path; reject an inner array-column query operand and any
  later-sibling reference. Reapply the complete ON/WHERE predicate per candidate.
- Specify NULL, empty array, arrays containing NULL, duplicate terms, posting-list union/intersection,
  LEFT-null-extension soundness, and per-outer cost/guard order.

### Slice 6B: GiST sibling operand

- Permit earlier-sibling query values for the existing range `&&`/`@>` and scalar `=` strategies.
  Build the opclass query once per outer row, descend, fetch candidates in storage-key order, and
  residual-recheck.
- Specify empty range, NULL, lossy candidate, scalar encoding, LEFT-null-extension, and per-outer
  `gist_descent`/page/candidate costs. Multi-column exclusion-only GiST indexes remain ineligible for
  read planning.

For both slices, INL retains precedence over a once-materialized constant bound only when the sibling
bound is usable; an unencodable/unsupported runtime value yields the specified empty/fallback behavior,
never an unsound prune. Add explicit plan precedence tests against PK and ordered-B-tree INL.

Exit gate: remove the GIN/GiST sibling-bound follow-on.

## Phase 7 — bounded top-k for blocking `ORDER BY ... LIMIT`

This phase is independent of bound detection but is best landed after the shared row-source work, so
every scan shape feeds it identically.

### Slice 7A: in-memory stable top-k

- For a blocking ORDER BY with constant `LIMIT`/`OFFSET`, retain only `K = OFFSET + LIMIT` rows in a
  bounded max-heap, then stable-sort the retained rows for output. Guard overflow in `K`; `LIMIT 0`
  avoids heap work but does not silently skip semantically required earlier execution.
- Heap comparison must use the exact ORDER BY comparator plus the original monotonically increasing
  input position as the stable tie-break. Cover ASC/DESC, explicit/default NULL order, collations,
  expression keys, ties, large OFFSET, and `K >= cardinality`.
- Scan, filter, and projection timing/cost must remain the same as the existing full-sort lane. Sort
  and heap bookkeeping remain unmetered, so this slice should require **zero cost re-pins** and should
  return the same error if an ORDER BY expression traps.
- Gate off DISTINCT, aggregate/group, window, and set-operation shapes until each is proven to feed
  the same pre-sort row sequence; add them as contained sub-slices only if the generic sorter seam
  already makes the proof mechanical.

### Slice 7B: spill interaction and benchmark proof

- Use top-k before external spill so a finite K within `work_mem` creates no runs; if K exceeds the
  budget, fall back to the existing external merge sorter rather than inventing a second spill
  format.
- Add/refresh the `order_by_limit` benchmark, require checksum equality across old/new lanes and all
  cores, and record memory/time improvement. No dependency may be added.

Exit gate: remove the bench-driven top-k follow-on.

## Phase 8 — deterministic in-memory hash join

Do this last. It changes join evaluation cardinality and cost, competes with INL, and creates the
operator seam later grace-hash spill will bound.

### Slice 8A: spec and cost decision (must precede implementation)

- Define the first eligible shape narrowly: a two-input INNER equijoin between bare columns of the
  same resolved type, no lateral dependency, with no usable INL on the inner. Build the
  right/FROM-order input and probe the left; do not use cardinality or statistics. Expression keys
  and promoted cross-type comparisons wait until their evaluation and hash-canonicalization rules
  are separately specified.
- Put the rule after scan bounds and INL. Fixed precedence: usable INL wins; otherwise an eligible
  hash equality wins; otherwise nested loop. Tie-breaking among multiple equality conjuncts is source
  order (or another explicitly spec'd structural rule), never hashmap order.
- Decide cost as part of the design. At minimum add deterministic build/probe work proportional to
  rows/keys so `max_cost` bounds hashing; define whether the selected equality's `operator_eval` is
  charged per probe/candidate or represented by the new hash units. Add weights to shared spec data,
  not per-core constants.
- Preserve error behavior by keeping the first slice to a predicate shape whose skipped nonmatching
  pair evaluations cannot trap. Additional ON conjuncts and expression keys are later sub-slices only
  after their evaluation/error order is specified.

### Slice 8B: operator with deterministic output

- Use the existing NULL-safe/value canonicalization machinery only where it matches ordinary `=`;
  SQL NULL join keys never match. Hash buckets must retain right input order, and probing must retain
  left input order, reproducing nested-loop enumeration for matching pairs. Never iterate a hash map
  to emit rows.
- Reapply the full ON predicate to bucket candidates, then the normal residual WHERE. ORDER BY,
  LIMIT/OFFSET, projection, and row production remain downstream and unchanged.
- Add collision-forcing unit tests per core (an internal invariant the corpus cannot express), plus
  shared conformance for NULLs, duplicates/many-to-many, empty inputs, typed keys, ORDER BY, LIMIT,
  cost ceiling, EXPLAIN, INL precedence, and nested-loop fallbacks.

### Slice 8C: broaden only after the base operator is pinned

- Add LEFT join while preserving unmatched-left emission; then multiple equality keys; then safe
  residual ON conjuncts. Keep RIGHT/FULL and more-than-two-relation hash edges deferred unless their
  preserved-side and left-deep ordering contracts are explicitly designed.
- Factor build/probe storage behind a bounded interface and carry stable row sequence numbers so the
  later grace-hash spill slice can partition without changing results, order, or cost. Do **not** add
  spill in this planner slice.

Exit gate: `EXPLAIN` and the capability manifest expose the hash operator, the old nested-loop-only
narrowing is removed, the in-memory operator is benchmarked against nested loop, and `spill.md` names
grace-hash spill—not operator creation—as the remaining hash-join storage work.

## Final closeout

After all phases are green:

1. Re-read the Rule-based extensions subsection and every referenced home bullet; delete completed
   follow-on text rather than leaving checked archaeology.
2. Ensure `planner.md`'s ordered rule inventory and access-path precedence match all three cores and
   `EXPLAIN` exactly.
3. Run the full PG oracle check for every new PG-comparable corpus file, then `rake ci` from a clean
   worktree.
4. Run the relevant benchmark corpus (`order_by_limit`, indexed mutations, INL/top-N, GIN/GiST INL,
   equijoin) and verify cross-engine/core checksums before comparing timing.
5. Confirm the next open strategic work remains clearly separated: cost estimator/statistics/join
   reordering in Path B, predicate rewrite infrastructure, and grace-hash spill in storage maturation.
