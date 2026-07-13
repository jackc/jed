# Working TODO — Cost as a plan input (Path B)

> **Temporary multi-session implementation checklist.** The canonical roadmap item is
> “Cost as a plan input (the strategic investment — Path B)” in [TODO.md](TODO.md), and the
> canonical architectural record remains [CLAUDE.md](CLAUDE.md) plus `spec/design/*`.
> This file records execution order and handoff state only. Move every settled decision into
> the relevant canonical document as it is made. Delete this file after the work is landed and
> the remaining follow-ons, if any, have been folded back into `TODO.md`.

The ordered phase ladder carries initial Path B through **P8**. **P9** is the later
statistics-quality refinement already anticipated by `TODO.md`; **P10** is the final landing gate.

## Standing rules for every slice

- [ ] Spec behavior before executor code. Keep estimator algorithms hand-written in Rust, Go,
  and TypeScript; only mechanical constants and fixtures belong in shared generated data.
- [ ] Land behavior in all three native cores in lockstep.
- [ ] Keep plan choice, estimated values, actual metered cost, and EXPLAIN spelling deterministic
  and cross-core identical.
- [ ] Re-pin every affected `# cost:` assertion in the same slice that changes the selected plan.
- [ ] Add a NoREC relation in every slice that enables a new optimization or plan choice.
- [ ] Keep planning unmetered. Estimates must not charge the runtime meter or affect `max_cost` /
  `lifetime_max_cost` enforcement.
- [ ] Preserve PostgreSQL result behavior. EXPLAIN and jed's deterministic estimator remain
  jed-owned surfaces and are not PostgreSQL-oracle output.
- [ ] Add no dependency without explicit human confirmation.
- [ ] Update relevant specs, `CLAUDE.md`, `TODO.md`, website documentation, and examples as each
  user-visible or standing decision lands.

## P0 — Ratify and specify deterministic plan choice

**Goal:** settle the contract before any statistics or estimator implementation.

- [x] Add a dedicated estimator design section/document and link it from
  [planner.md](spec/design/planner.md), [cost.md](spec/design/cost.md), and
  [explain.md](spec/design/explain.md).
- [x] Ratify the “spec the plan” branch of class P in
  [determinism.md](spec/design/determinism.md): independent cores must select the same plan rather
  than ledgering per-core plan/cost divergence.
- [x] Update `CLAUDE.md` with the ratified rule that cost identity includes deterministic
  plan-estimator identity.
- [x] Define the estimator's complete input set: query structure, catalog facts, transactional
  statistics, and any admitted storage-structure facts.
- [x] Decide literal-versus-parameter behavior. Planning occurs before parameter binding today, so
  parameter estimates need a deterministic generic rule unless the pipeline is deliberately
  revised.
- [x] Define `est_rows` and `est_cost` representations, including zero, unknown/unavailable facts,
  maximum values, and rendering.
- [x] Define exact arithmetic: integer or fixed rational operations, rounding direction at every
  step, checked multiplication/addition, and saturation/overflow behavior identical across cores.
- [x] Define the estimate as named runtime-cost-unit counts plus their weighted total from
  `spec/cost/schedule.toml`; do not invent wall-clock-only weights inside the planner.
- [x] Record the consequence that currently unmetered work, including sorting, is invisible to the
  Path B objective. If such work must influence plan selection, first add and re-pin a runtime cost
  unit explicitly.
- [x] Define a total candidate order and final tie-break, including access-path kind, lowercased
  index name, physical relation order, and join algorithm.
- [x] Define supported and fallback estimation rules for every current plan-node kind.
- [x] Define a deterministic resource bound for join search so untrusted SQL cannot trigger an
  exponential unmetered planner.
- [x] Decide the shared data/fixture locations and wire their coherence checks into `rake verify`.

**Exit gate:** the estimator, statistics lifecycle, candidate ordering, cache-validity inputs, and
search limits are fully specified without relying on a core implementation as the authority.

**Status:** complete on 2026-07-13. `bundle exec rake verify` passes; no engine behavior changed.

### P0 human decision checkpoint

These decisions materially affect later slices. The maintainer approved all six recommendations on
2026-07-13; they are now canonical in [estimator.md](spec/design/estimator.md).

- [x] **Optimization objective.** Path B minimizes the existing observable runtime
  cost schedule exactly as `TODO.md` says. It does not add private planner-only weights for
  currently unmetered work such as sort comparisons, dedup bookkeeping, or row concatenation. If
  those operations should influence selection, expand the runtime schedule in an explicit earlier
  slice and re-pin its costs first.
- [x] **Parameter sensitivity.** Keep physical planning before parameter binding.
  Literals may use value-aware statistics when those exist; `$N` uses a deterministic generic
  selectivity. A custom parameter-sensitive plan can be a later feature rather than reshaping the
  pipeline and prepared-plan cache in initial Path B.
- [x] **Initial selectivity constants.** Borrow PostgreSQL's row-count-only defaults
  but encode them as exact rational data — equality `1/200`, one-sided inequality `1/3`, and a
  paired range `1/200` — with exact unique-key rules overriding the defaults.
- [x] **Join-search bound.** Use exhaustive deterministic left-deep dynamic programming
  for reorderable islands of at most 8 relations, then deterministic greedy cheapest-next for
  larger islands. This borrows PostgreSQL's default join-collapse boundary without adopting its
  randomized GEQO fallback.
- [x] **Estimator inputs and cache validity.** Admit only catalog/statistics facts and
  storage facts visible without leaf I/O (for example exact resident-skeleton node counts and tree
  height). Cache them with a relation-scoped estimator-input fingerprint, including attachment
  identity, rather than a database-wide generation that invalidates prepared plans after unrelated
  table writes.
- [x] **DML scope.** Initial cost-based selection applies to SELECT. UPDATE/DELETE use
  the behavior-neutral candidate inventory but retain their current fixed policies until a
  dedicated slice decides mutation visitation/error-order consequences.

## P1 — Transactional per-table row counts

**Goal:** make the first estimator input exact, cheap, transactional, and available after reopen.

The stores already maintained exact counts when built from empty, but a disk-loaded B+tree skeleton
deliberately carried an unknown count so open did not walk every leaf. v28 now persists the count
without restoring that leaf walk.

- [x] Define the row-count range and byte encoding in `spec/fileformat/format.md`.
- [x] Allocate the next `format_version` and add the count to each table catalog entry.
- [x] Write the exact count in from-scratch serialization and incremental catalog rewrites.
- [x] Load the count alongside `root_data_page` and install it into the disk-loaded `PMap` /
  table-store skeleton in all three cores.
- [x] Preserve the invariant `root_data_page == 0` iff the persisted count is zero; reject corrupt
  mismatches where they can be detected without a leaf walk.
- [x] Maintain the count through INSERT, INSERT … SELECT, DELETE, UPDATE re-keying, UPSERT paths,
  cascades, ALTER rewrites, CREATE/DROP, and statement rollback.
- [x] Verify explicit-transaction rollback restores the old count just like other snapshot state.
- [x] Cover main, attached, in-memory, file-backed, and session-temporary table domains.
- [x] Ensure post-open mutations keep maintaining the loaded known count rather than reverting it
  to unknown.
- [x] Add a byte-exact golden fixture isolating the new table-catalog field.
- [x] Add cross-core golden write/read tests and corruption tests.
- [x] Add transactional tests for commit, rollback, reopen, deletes to zero, and failed mutations.
- [x] Add a regression proving file open remains O(interior spine) and does not fault table leaves
  merely to obtain the count.

**Exit gate:** every visible snapshot carries an exact table count, rollback restores it, and every
core writes and reads byte-identical files without regressing open behavior.

**Status:** complete on 2026-07-13. `format_version` 28 stores `row_count` as a nonnegative signed
`i64` end-to-end (Rust `i64`, Go `int64`, TypeScript `bigint`), with an exact-version clean break and
no v27 migration path. `bundle exec rake ci` passes after regenerating and independently verifying
all 61 file-format fixtures. P2 and P3 are complete below; P4 is the next implementation slice.

## P2 — Statistics-aware prepared-plan cache validity

**Goal:** a cached plan must remain identical to a freshly planned query after estimator inputs
change.

- [x] Define a deterministic estimator-input fingerprint for the relations a plan references.
- [x] Include every fact that can affect selection: row counts, admitted structural page facts,
  later histogram/NDV versions, database/attachment identity, and relevant catalog generation.
- [x] Extend prepared-plan cache entries and hit validation in Rust, Go, and TypeScript.
- [x] Keep the fingerprint relation-scoped where practical so unrelated table changes do not
  invalidate a plan unnecessarily.
- [x] Handle attached databases explicitly; do not validate only against the main database's
  catalog/statistics state.
- [x] Keep temporary-relation plans uncacheable under the existing rule unless a complete temp
  fingerprint is deliberately specified.
- [x] Ensure working-transaction statistics never populate a committed cache entry.
- [x] Verify rollback leaves the committed fingerprint and cache validity unchanged.
- [x] Add prepared-versus-fresh tests that alter relevant and irrelevant table counts.
- [x] Assert cached and fresh executions choose the same EXPLAIN plan and accrue the same actual
  cost.

**Exit gate:** every cache hit is result-, plan-, estimate-, and actual-cost-identical to a fresh
planning pass over the same visible snapshot.

**Status:** complete on 2026-07-13. All three cores store and compare the exact source-order tuple
field-by-field using opaque snapshot identity/revision tokens (never a collision-prone hash). Focused
tests cover relevant and irrelevant count changes, count-return-to-prior-value invalidation,
working-state fill exclusion, rollback, every DML disposition, per-attachment identity/generation,
and fresh-versus-refilled EXPLAIN/row/actual-cost parity. `bundle exec rake ci` passes. No file-format
or SQL result change; P3 is complete below and P4 is next.

## P3 — Deterministic all-candidate inventory, behavior-neutral

**Goal:** expose all legal choices without changing which plan runs yet.

- [x] Refactor `detectScanBound` / `detect_scan_bound` / `detectScanBound` into a candidate
  inventory plus a separate selector.
- [x] Enumerate full scan, PK bound, every eligible ordered B-tree index, GiST, GIN, PK interval
  set, and ordered-index interval set candidates.
- [x] Preserve each consumer policy explicitly: SELECT and the existing UPDATE/DELETE ordering.
- [x] Give every candidate a canonical identity and deterministic ordering independent of maps or
  host iteration.
- [x] Retain a legacy selector that reproduces today's fixed precedence and lowest-lowercased-index
  tie-break exactly.
- [x] Make scan-order capabilities and required residual filters explicit candidate properties.
- [x] Keep physical plan fields and executor behavior unchanged in this slice.
- [x] Add shared/cross-core inventory cases for multiple usable indexes and mixed access methods.
- [x] Run existing EXPLAIN, cost, NoREC, and CI suites with zero output or cost re-pins.

**Exit gate:** every core inventories the same candidates, while the legacy selector proves the
refactor is plan-, result-, EXPLAIN-, and cost-neutral.

**Status:** complete on 2026-07-13. Rust, Go, and TypeScript inventory the same canonical identities
independently of catalog iteration and retain the complete WHERE plus explicit scan-order facts on
each candidate. Shared EXPLAIN cases pin the old SELECT/mutation choices; per-core white-box tests
pin complete mixed-method inventory and both legacy exceptions. No output or cost was re-pinned;
`bundle exec rake ci` passes. P4 is next.

## P4 — Base-relation estimator in shadow mode

**Goal:** estimate every base access candidate without using estimates to select it.

- [x] Author shared estimator constants/facts as language-neutral data; generate only mechanical
  constants, never planner control flow.
- [x] Author `spec/cost/estimator_vectors.toml` with inputs, per-unit counts, `est_rows`, weighted
  `est_cost`, and expected tie keys.
- [x] Implement identical arithmetic helpers in all three cores.
- [x] Estimate full scans from row count and admitted structural facts.
- [x] Estimate PK equality/prefix/range candidates.
- [x] Estimate ordered B-tree equality-prefix and trailing-range candidates.
- [x] Estimate GIN, GiST, and interval-set candidates with deterministic fallback rules when row
  counts are the only available statistics.
- [x] Specify selectivity for equality, inequality/range, `IS NULL`, `IN`, `BETWEEN`, AND, OR, and
  unsupported/opaque predicates.
- [x] Estimate residual-filter rows separately from access-path candidate rows.
- [x] Estimate the runtime units affected by the access path, including `page_read`,
  `storage_row_read`, touched-column decompression, access-method work, filter `operator_eval`, and
  produced rows where applicable.
- [x] Keep the legacy selected candidate authoritative; store or test shadow estimates only.
- [x] Cross-check estimates against actual cost in exact/simple cases, while documenting that an
  estimate is not generally required to equal runtime cost.
- [x] Run the shared estimator vectors in Rust, Go, and TypeScript.

**Exit gate:** all cores compute byte-identical estimates for the complete base-candidate fixture
matrix, and no user-visible plan or cost has changed.

**Status:** complete on 2026-07-13. Shared generated facts and 23 canonical arithmetic,
predicate, and all-access-method vectors feed hand-written estimators in Rust, Go, and TypeScript.
Every base-relation inventory is annotated once with exact row count plus resident node-count/height
facts; structural NULL/conflict/range proofs yield zero, and simple full scans cross-check exactly
against actual metered cost. The accepted P4 checkpoint estimates logical output once from the full
WHERE against base `N`; lossy GIN/GiST residual rechecks add `operator_eval` work but do not apply
selectivity a second time. The legacy selector remains authoritative, so plans, EXPLAIN, results,
and actual cost are unchanged. `bundle exec rake ci` passes.

## P5 — Whole-plan estimation and EXPLAIN columns

**Goal:** propagate estimates through the selected plan and make them corpus-assertable.

- [ ] Define whether each node's `est_cost` is local, subtree-cumulative, or both; expose exactly
  one unambiguous contract in EXPLAIN.
- [ ] Propagate estimates through residual filters and projections.
- [ ] Propagate through nested-loop, index-nested-loop, and hash joins in current FROM order.
- [ ] Propagate through aggregate/GROUP BY, HAVING, window, DISTINCT, Sort, and LIMIT/OFFSET.
- [ ] Propagate through SRFs, CTE materialization/references, derived tables, VALUES, set operations,
  and FROM-less SELECT.
- [ ] Cover INSERT/UPDATE/DELETE plan nodes or record an explicit initial narrowing for DML
  estimates.
- [ ] Add `est_rows` and `est_cost` columns to the EXPLAIN result type and renderer in all cores.
- [ ] Specify exact column types, rendering, and sentinel behavior.
- [ ] Keep the EXPLAIN statement's own runtime cost at one `row_produced` per emitted plan row; the
  new cells themselves do not add execution cost.
- [ ] Keep EXPLAIN ANALYZE actual cost/rows separate from estimates.
- [ ] Re-pin every existing EXPLAIN corpus entry to the expanded result shape.
- [ ] Add estimate-focused corpus cases for empty, selective, nonselective, join, aggregate, and
  LIMIT plans.
- [ ] Update `spec/design/explain.md`, website SQL docs, and live examples.

**Exit gate:** plain EXPLAIN exposes deterministic per-node estimates for every supported current
plan shape, and the shared corpus asserts them across all cores.

## P6 — Cost-based single-relation access-path selection

**Goal:** make the first observable plan choices from estimates.

### P6a — PK, full scan, and ordered B-tree

- [ ] Replace the legacy selector for eligible SELECT base relations with minimum estimated cost.
- [ ] Apply the canonical P0 tie-break after estimated cost.
- [ ] Consider access path and required ordering together wherever current ORDER BY/index rules
  interact with the chosen bound.
- [ ] Preserve the full WHERE as the residual filter for every candidate.
- [ ] Decide whether UPDATE/DELETE remain on legacy policies for this milestone; document and test
  the boundary.
- [ ] Add EXPLAIN cases where row-count changes flip full/PK/index choices.
- [ ] Add competing-index cases proving name order loses when cost differs and wins only on an exact
  tie.
- [ ] Re-pin every affected `# cost:` entry.
- [ ] Add a new NoREC scenario for cost-selected competing access paths.
- [ ] Benchmark point, range, selective-index, and nonselective-index cases before/after.

### P6b — GIN, GiST, interval sets, and ordering paths

- [ ] Enable cost selection for GIN candidates.
- [ ] Enable cost selection for GiST candidates.
- [ ] Enable cost selection for PK and ordered-index interval sets.
- [ ] Integrate secondary-index ORDER BY/top-N candidates without silently pricing unmetered sort
  work.
- [ ] Add mixed-access-method ties and selectivity flips.
- [ ] Extend NoREC relations and re-pin costs for every newly selectable path.
- [ ] Run affected access-path benchmarks and full cross-core CI.

**Exit gate:** every single-relation SELECT access path is selected from the deterministic estimator,
with exact tie behavior, EXPLAIN coverage, NoREC coverage, and re-pinned actual costs.

## P7 — Costed two-relation join orientation and algorithm

**Goal:** choose the cheaper legal driver and join implementation for the first reorderable join.

- [ ] Introduce a physical relation-order/permutation representation without changing resolved
  logical column slots.
- [ ] Enumerate both orientations of eligible two-relation INNER/CROSS joins.
- [ ] Enumerate nested-loop, index-nested-loop, and hash candidates for each legal orientation.
- [ ] Estimate outer rows, repeated inner scans/seeks, hash build/probe byte work, ON residual work,
  join rows, and downstream rows/cost.
- [ ] Let a sibling-column bound become an INL candidate only when its dependency is already on the
  physical left side.
- [ ] Make hash build/probe orientation explicit; the current fixed right-build behavior becomes a
  candidate rather than an invariant.
- [ ] Treat LEFT/RIGHT/FULL joins, LATERAL, correlated dependencies, and non-reorderable derived/CTE
  shapes as barriers for this slice.
- [ ] Re-evaluate join sort-elision and LIMIT/top-N rules against the selected physical order.
- [ ] Specify plan-dependent error ordering under the ratified deterministic plan contract; keep
  error corpus cases single-offender where required.
- [ ] Add cases where FROM order wins, reverse order wins, INL wins, hash wins, and exact ties occur.
- [ ] Add/revise NoREC and join-commutativity scenarios with total ORDER BY output comparison.
- [ ] Re-pin affected costs and benchmark both orientations/algorithms.

**Exit gate:** eligible two-table INNER/CROSS joins choose the same cheapest orientation and
algorithm in every core, with barriers preserving all other join semantics.

## P8 — Bounded deterministic N-way left-deep join ordering

**Goal:** generalize Path B to multi-relation joins without unbounded planning work.

- [ ] Partition the logical join tree into reorderable INNER/CROSS islands separated by outer-join,
  LATERAL, correlation, and other semantic barriers.
- [ ] Specify and implement the bounded search algorithm chosen in P0 (for example, capped dynamic
  programming with a deterministic fallback).
- [ ] Include physical access path and join algorithm in each partial-plan state rather than choosing
  them independently after join order.
- [ ] Apply canonical state deduplication and tie-breaking independent of hashmap order.
- [ ] Carry estimated rows/cost forward at each left-deep step.
- [ ] Admit an INL edge only after its required sibling relation is present in the left prefix.
- [ ] Preserve resolved expression slots through a physical relation permutation/mapping.
- [ ] Re-run ORDER BY, scan-order, and LIMIT/top-N decisions on the final physical tree.
- [ ] Add 3+-relation tests covering disconnected CROSS products, selective predicates, competing
  indexes, INL dependencies, hash choices, ties, barriers, NULLs, and empty relations.
- [ ] Add a dedicated N-way NoREC/metamorphic relation comparing reordered and deliberately
  non-reorderable equivalent forms.
- [ ] Add planning-limit boundary tests proving large FROM lists remain deterministically bounded.
- [ ] Re-pin actual costs and benchmark representative 3-, 5-, and cap-boundary joins.

**Exit gate / initial Path B milestone:** every eligible left-deep join island is selected from a
bounded, spec'd, deterministic cost search across all three cores; access paths, join choices,
EXPLAIN estimates, actual costs, and NoREC coverage agree.

## P9 — Deterministic NDV and histogram statistics follow-on

**Goal:** improve estimate quality without changing the estimator's determinism contract.

- [ ] Choose the SQL-reachable collection surface, likely an `ANALYZE` vertical slice, and define
  its PostgreSQL relationship/divergences.
- [ ] Define per-column facts: NULL count/fraction, distinct count, histogram bounds, most-common
  values/frequencies, and any value-width facts needed by existing runtime cost units.
- [ ] Define exact versus sampled collection. If sampled, specify a cross-core-identical sampling
  algorithm and seed; do not use host randomness or iteration order.
- [ ] Scan rows in deterministic storage-key order and canonicalize values with shared type rules.
- [ ] Define histogram bucket count, boundary selection, duplicate handling, NULL handling,
  collation behavior, and open/container-type eligibility.
- [ ] Define transactional update, rollback, persistence, invalidation, and deterministic staleness
  policy. Never use wall-clock age.
- [ ] Add statistics to the file format with byte-exact fixtures and corruption validation.
- [ ] Extend the prepared-plan fingerprint with the new statistics facts/version.
- [ ] Feed NDV/histograms into the existing selectivity rules without altering arithmetic or final
  tie-breaking.
- [ ] Add corpus cases where distributions with equal row counts choose different plans.
- [ ] Re-pin costs, extend NoREC, and benchmark skewed/uniform distributions.

**Exit gate:** statistics collection and use are SQL-reachable, transactional, persisted,
byte-identical, deterministic, cache-safe, and demonstrably improve plan selection under skew.

## P10 — Final verification, documentation, and cleanup

- [ ] Run shared estimator fixture verification in all cores.
- [ ] Run file-format goldens and cross-core round trips.
- [ ] Run focused row-count transaction/reopen and prepared-cache suites.
- [ ] Run all EXPLAIN and cost conformance suites.
- [ ] Run the full NoREC sweep.
- [ ] Run `rake verify` and `rake ci`.
- [ ] Run affected access-path and join benchmarks and record before/after results.
- [ ] Update `CLAUDE.md` and every affected `spec/design/*` document to final landed behavior.
- [ ] Update website SQL/EXPLAIN documentation and live examples.
- [ ] Check off or rewrite the canonical Path B entries in `TODO.md`, leaving only genuine
  follow-ons.
- [ ] Remove completed material from this file as canonical docs become authoritative.
- [ ] Delete this temporary checklist once no cross-session handoff state remains.

## Cross-session handoff

Update this short block at the end of each work session.

- **Current slice:** P0 complete; P1 is next but has not started
- **Last completed checkpoint:** canonical estimator data and contract ratified; planner, cost,
  determinism, conformance, EXPLAIN, `CLAUDE.md`, and `TODO.md` synchronized
- **Branch / P0 completion:** `path-b-p0-estimator-contract` / latest commit titled
  `docs(planner): ratify Path B estimator contract`
- **Verification last run:** `bundle exec rake verify` passed 2026-07-13; estimator checker passed;
  `git diff --check` passed
- **Known blockers or open decisions:** none for P0; all six maintainer decisions are recorded above
- **Next action:** begin P1 by specifying the persisted per-table row-count field and format-version
  transition; ask the maintainer before any major file-format compatibility choice
