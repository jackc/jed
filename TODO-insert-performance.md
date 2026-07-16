# Temporary Insert Performance Plan

> Temporary working plan for reducing INSERT allocation traffic, recovering the Rust
> single-process performance lost when shared file coordination starts a background thread, and
> carrying the generally useful optimizations across the Rust, Go, and TypeScript cores. This file
> is intentionally separate from `TODO.md`. Delete it after the work lands and move durable decisions,
> measurements, and remaining follow-ons into the canonical design docs and `TODO.md`.

## Outcome

Make repeated prepared INSERTs substantially cheaper without changing SQL results, errors, cost,
selected plans, snapshot semantics, file bytes, or host APIs.

The immediate motivating lane is `insert_rollback`: 1,000 executions of one prepared single-row
INSERT inside one transaction, followed by rollback. On Linux/glibc, starting the shared-file arrival
probe changes glibc from its single-threaded fast paths to multithread-safe paths. The probe does not
add work to each INSERT; it makes the INSERT path's existing allocation volume more expensive.

The intended end state is:

- `locking=auto` continues to select safe shared multi-process coordination.
- `locking=exclusive` continues to be the explicit maximum-performance, single-process option.
- The allocation-heavy INSERT path becomes fast enough that shared coordination's glibc transition
  is small in the common benchmark rather than a roughly 25% regression.
- Rust, Go, and TypeScript share the same optimized execution shapes where useful, while each core
  uses ownership mechanisms appropriate to its language.
- No third-party dependency is required. Any later proposal for an arena, allocator, or benchmark
  profiler dependency must receive explicit approval under `CLAUDE.md` §14 before it is added.

## Current evidence

The existing roadmap already records the Rust copy-on-write deep-clone problem: Rust rebuilds an
INSERT path by deep-cloning `Vec<Vec<u8>>` keys and rows, while Go's slice copy and TypeScript's array
copy retain references to existing entries. See `TODO.md` under “Bench-driven perf follow-ons.”

A temporary Rust counting-allocator probe around the current `insert_rollback` lane measured the
following approximate traffic for 1,000 inserted rows. The instrumentation was removed after the
measurement.

| phase | allocation/reallocation calls | requested bytes |
|---|---:|---:|
| parameter binding and pre-`insert_rows` resolution | 38,000 | 3.3 MiB |
| INSERT validation and temporary structures | 370,000 | 50 MiB |
| table and secondary-index tree mutation | 306,000 | 39 MiB |
| total | 716,000 | 93 MiB |

Interpretation:

- This is about 716 allocation/reallocation calls and 93 KiB of cumulative requested bytes per row.
- Roughly 75% of the calls requested 16 bytes or less.
- Requested bytes are allocation traffic, not peak or retained memory.
- The counter itself perturbs timing, so use it only for allocation counts and relative attribution;
  use the ordinary benchmark harness for elapsed time.
- This measurement is Rust-specific. Do not project the count onto Go or TypeScript: their decoded
  leaves shallow-copy existing key/row references and therefore avoid much of Rust's deep allocation.

Relevant current code:

- Rust: `impl/rust/src/pmap.rs::decoded_parts` and `node_insert` deep-clone decoded leaf contents and
  rebuild the root-to-leaf path.
- Go: `impl/go/pmap.go::decodedParts` and `nodeInsert` allocate new outer slices/path nodes but retain
  existing key and row backing objects.
- TypeScript: `impl/ts/src/pmap.ts::decodedParts` and `nodeInsert` use shallow `.slice()` copies and
  allocate replacement arrays/path nodes.
- Before Slice 1, all three `insertRows` implementations built multi-row validation collections for
  a one-row prepared execution.
- Before Slice 2, prepared-plan caching targeted reusable SELECT plans. A prepared INSERT still
  performed substantial resolution and maintenance setup on every execution.

## Non-negotiable invariants

Every slice below must preserve all of these. A speedup that weakens one is rejected.

- **Published snapshots are immutable.** A committed snapshot, pinned reader, open cursor, writable
  CTE read pin, or any other previously exposed root must never observe later mutation.
- **One committed version plus one working set.** Do not introduce row versions, a WAL, or a second
  transactional model.
- **Two-phase DML behavior.** All validation and deterministic error ordering remain before writes
  where the current contract requires it. A failed statement/block exposes no partial change.
- **Writable CTE shared snapshot.** Every sub-statement reads the pre-statement pin while writes
  accumulate into the working set in the existing deterministic order.
- **Cursor stability.** A cursor opened before a later write continues to observe its original root.
- **Exact tree shape and file bytes.** Page-fit arithmetic, split points, merge rules, key order,
  page IDs, dirty-page serialization, golden images, and cross-core round trips remain unchanged.
- **Exact deterministic cost.** Allocation and planning are unmetered; the optimization must not add,
  remove, or reorder metered work or change a cost-ceiling abort point.
- **Exact results and errors.** Values, row multisets, `ORDER BY`, structured error fields, error
  precedence, affected-row counts, and transaction poisoning remain unchanged.
- **Fresh per-execution state.** Parameters, defaults, entropy, clock reads, sequence state,
  privileges, limits, temp resolution, and transaction state are never captured in a reusable plan.
- **Memory safety.** No new `unsafe`, cgo, or FFI in a core.
- **No format bump.** These are in-memory execution optimizations. If an implementation idea would
  change persisted bytes, stop and split it into a separately designed format slice.

## Measurement protocol

- [x] Record a clean “before” run from the exact commit being optimized.
- [x] Pin the benchmark process to one CPU and run on an otherwise idle host.
- [x] Alternate configurations rather than running all samples of one configuration first:
  `shared, exclusive, shared, exclusive, ...`.
- [x] Run at least five measured pairs after one discarded warmup pair.
- [x] Record median, p90, p99, minimum, checksum, compiler/runtime versions, CPU, OS, and glibc
  version where applicable.
- [x] Use the ordinary benchmark result for timing. Keep allocation probes separate so their atomic
  counters or runtime tracing do not contaminate elapsed-time conclusions.
- [x] For Rust, collect allocation/reallocation calls and cumulative requested bytes for the whole
  1,000-row transaction and, temporarily, the three phases in the evidence table.
- [x] For Go, use a focused `testing.AllocsPerRun`-style benchmark or existing Go allocation tools;
  also record GC count/bytes for the full lane.
- [x] For Node/TypeScript, use V8 allocation/GC tracing outside the timed run and record total allocated
  bytes and GC activity. Do not compare V8 object counts directly with Rust allocator calls.
- [x] Store durable before/after evidence in `spec/design/benchmarks.md`; do not commit temporary
  allocator instrumentation to an engine core.

Primary benchmark matrix:

- [x] `insert_rollback` on `small` — the motivating one-row prepared INSERT with one secondary index.
- [x] A one-row prepared INSERT into a table with no secondary indexes, separating table-tree cost
  from index maintenance.
- [x] A one-row prepared INSERT with several indexes, including expression/partial/unique where
  available, to make sure the fast path scales without skipping maintenance.
- [x] Integer-only and variable-width text rows; include a long-value case to ensure the optimization
  does not regress compression/overflow behavior.
- [x] Batch sizes 1, 10, 100, and 1,000 at the transaction level.
- [x] A true multi-row SQL statement so the optimized one-row path does not regress the existing
  statement-batch implementation.
- [x] `insert_commit_durable` to catch fsync/write-path regressions.
- [x] `secondary_update` and `secondary_pointset_delete` to measure reusable tree/DML improvements
  beyond INSERT without allowing them to expand the first slice.

Regression lanes:

- [x] Hot prepared primary-key lookup.
- [x] Cold primary-key ramp and checksum path.
- [x] Full scan and aggregate.
- [x] Concurrent readers.
- [x] Durable one-row write.
- [x] The native-TS versus Node/Rust wrapper subset, because Rust-core INSERT changes affect the
  wrapper comparison.

Performance gates:

- [x] Every slice must reduce either allocation traffic or elapsed time in its targeted lane; keep no
  complexity that produces only noise.
- [x] No unrelated representative lane may regress by more than 5% in repeated paired measurements.
- [x] The final Rust `shared` versus `exclusive` `insert_rollback` gap should be within 5% or normal
  repeated-run noise. If it is not, report the remaining gap explicitly; do not claim the glibc
  regression has been solved.
- [x] Go and TypeScript must not regress even if a Rust-specific representation change gives them no
  corresponding implementation work.

## Slice 0 — lock down correctness and attribution

- [x] Add or identify focused internal tests for exact B+tree shape after a deterministic insert
  sequence, including leaf split, interior split, secondary-index split, overwrite, and rollback.
- [x] Confirm existing golden fixtures cover the same insert sequence across Rust, Go, TypeScript,
  and the Ruby reader/writer.
- [x] Add a focused snapshot-alias test in every core:
  1. create a committed root;
  2. pin it;
  3. perform many inserts into a working root, repeatedly touching the same leaf;
  4. assert the pin remains byte/value-identical;
  5. commit and assert a fresh reader sees the new rows.
- [x] Add a write-transaction cursor test: open a cursor over the working state, perform a later write
  where the host API permits it, and prove the cursor remains stable. If a language's borrowing/API
  rules forbid this shape, document that and test the closest exposed alias.
- [x] Add a writable-CTE test that pins a pre-statement root while two modifying sub-statements touch
  the same leaf; preserve both read-pin semantics and deterministic collision behavior.
- [x] Add an attachment variant so main and attached trees do not accidentally share a mutation
  identity.
- [x] Capture the baseline benchmark/allocation results before implementation.

Evidence is recorded in `spec/design/benchmarks.md` under “INSERT performance slice-0 baseline.” The
existing shared writable-CTE collision corpus already covered the required hidden phase-1 / phase-2
collision, so Slice 0 identifies and retains it rather than duplicating SQL-observable behavior in
per-core tests. The deliberately unsafe TypeScript decoded-leaf alias experiment failed the focused
snapshot guard and was reverted before this slice was finalized.

Exit: correctness tests fail under a deliberately unsafe in-place mutation experiment and pass under
the existing immutable implementation. This proves the tests actually guard the optimization.

## Slice 1 — single-row INSERT execution path in all three cores

### Design

Add a contained fast path selected only when the resolved INSERT source contains exactly one candidate
row and the statement shape is supported. It must run the same operations in the same order as the
general multi-row path, using local values instead of batch collections.

Initial eligibility:

- plain `INSERT ... VALUES (...)`;
- exactly one candidate row;
- no `ON CONFLICT` in the first patch;
- `RETURNING` is allowed only if it can use the same already-validated row without changing when it
  is evaluated;
- writable-CTE execution is allowed only after the read-pin/collision tests prove equivalence;
- otherwise fall back to the existing general path.

Tasks:

- [x] Author the shared operation order in the relevant DML design doc before coding:
  defaults/coercion/NOT NULL → CHECK → PK duplicate probe → UNIQUE probes → compression cost → index
  expression/predicate evaluation → FK/exclusion checks → RETURNING → table write → index writes.
- [x] Confirm that this order exactly matches current behavior, including structured error precedence.
- [x] Rust: avoid allocating `prepared`, `seen_keys`, per-unique `seen_prefixes`, three-level
  `entry_prefixes`, and per-index drain buffers for one row. Use explicit local `Option`/`Vec` values;
  do not add `smallvec` or another dependency.
- [x] Go: avoid one-row maps and string conversions used only for within-batch duplicate detection;
  retain byte slices directly through the validation/write boundary.
- [x] TypeScript: avoid one-row `Set`s, `key.join(",")`, nested prefix arrays, and intermediate mapped
  arrays where a direct local value suffices.
- [x] Keep the multi-row implementation intact as the fallback. Do not duplicate complex constraint
  semantics: extract small shared helpers where that reduces drift, but keep the control flow explicit.
- [x] Ensure phase-2 collision handling still catches an earlier writable-CTE sub-statement that the
  read pin intentionally hid from phase 1.
- [x] Preserve estimator-revision updates, sequence/default flushing, transaction poisoning, cost,
  and affected-row counts.
- [x] Add per-core internal tests only for fast-path selection/invariants; put SQL-observable behavior
  in the shared conformance corpus if new coverage is needed.
- [x] Run the primary and regression benchmark matrices and record the isolated effect.

Evidence is recorded in `spec/design/benchmarks.md` under “INSERT performance slice-1 result.” The
one-row lane improved by 2.5% Rust/shared, 1.6% Rust/exclusive, 2.0% Go, and 11.5% TypeScript. Go
removed five heap allocations per row; V8 allocated 2.2% fewer bytes over the complete lane and ran
one fewer scavenge. Rust's whole-transaction allocator counter could not distinguish the specialized
and forced-batch controls because immutable tree rebuilding dominates. Its shared/exclusive gap is
still 25.6%, explicitly leaving the motivating glibc regression open for later slices. Three paired
control runs put every unrelated representative regression below 5%, with identical checksums.

Exit: all three cores use the one-row path for the motivating benchmark, produce identical answers,
cost, and file bytes, and show a measurable reduction in temporary allocation/GC traffic.

## Slice 2 — cache a safe prepared-INSERT plan in all three cores

### Scope and cache key

Start with plain prepared `INSERT ... VALUES` because its static target/maintenance metadata is useful
across executions and does not depend on row-count estimates. Do not initially cache `INSERT ...
SELECT`, `ON CONFLICT`, writable CTE wrappers, volatile plan state, or UPDATE/DELETE access plans.

The reusable entry contains only immutable, resolution-derived facts such as:

- target database identity and lowercased relation name;
- target column mapping and stored column types;
- primary-key ordinals/types;
- resolved defaults and CHECK expressions that are immutable plan structure, not evaluated values;
- resolved index descriptors, predicates, expression keys, uniqueness, and maintenance order;
- FK and exclusion descriptors needed by INSERT;
- collation identities/versions, not host-global mutable pointers with unvalidated lifetime;
- parameter type expectations and RETURNING projection structure where eligible.

The entry must not contain:

- parameter values;
- evaluated defaults, entropy, clock values, or sequence results;
- a session, transaction, working root, temp table object, pager, or cursor;
- privileges or resource limits;
- pending rows, keys, index prefixes, meters, or mutation-generation tokens.

Use a DML-specific validity signature. The existing SELECT signature includes estimator revisions,
which successful INSERTs intentionally advance; using it unchanged would invalidate the INSERT cache
after every execution. The initial INSERT signature should validate schema/identity facts instead:

- owning core/database identity;
- attachment identity for a qualified target;
- catalog generation or an exact target-schema revision;
- lowercased target relation name;
- executing session's temp-shadow state;
- collation/catalog identities needed by the cached expressions.

Tasks:

- [x] Specify the prepared cache as a tagged entry or separate SELECT/DML slots; do not force a DML
  plan into the SELECT-plan type.
- [x] Define exact INSERT-cache eligibility and invalidation in `spec/design/api.md`.
- [x] Decide whether one `PreparedStatement` may retain one SELECT and one INSERT entry or only the
  entry appropriate to its parsed statement kind. Prefer the smallest explicit surface.
- [x] Re-check privileges, read-only state, transaction state, cost/lifetime limits, parameter count
  and types, and temp shadowing on every execution even on a cache hit.
- [x] Preserve per-execution folding/evaluation for defaults, CHECKs, index predicates/expressions,
  RETURNING, and host-injected seams.
- [x] Rust: extend the `PreparedStatement` cache behind its existing thread-affine `RefCell`/`Rc`
  model; do not broaden `Send`/`Sync` as part of this optimization.
- [x] Go: retain goroutine-safe prepared-statement sharing. Publish immutable DML entries through an
  atomic slot or an equally narrow synchronization mechanism; concurrent valid fills may be
  last-writer-wins.
- [x] TypeScript: store the immutable entry on `PreparedStatement`; avoid retaining an `Engine` or
  session through closures.
- [x] Add an internal cache-hit counter/seam in tests only, not in the public API or deterministic SQL
  cost.
- [x] Assert that the second successful INSERT is still a cache hit even though the relation's
  estimator revision changed.
- [x] Assert misses for another database, detach/reattach, relevant DDL, target drop/recreate, index or
  constraint changes, collation upgrade, and temp shadowing.
- [x] Assert unrelated-table DDL follows the chosen catalog-generation policy (a conservative miss is
  acceptable if documented).
- [x] Assert rolled-back DDL/working-state execution never replaces a valid committed cache entry.
- [x] Assert a cached plan cannot bypass a newly restrictive privilege envelope or read-only session.
- [x] Assert cache hit and fresh resolution produce identical result, error, cost, and tree bytes.
- [x] Benchmark prepared versus one-shot INSERT separately so parse savings are not confused with DML
  resolution savings.

Follow-ons after the INSERT cache is proven:

- [ ] Add `ON CONFLICT` only after its arbiter and per-execution conflict action are cleanly separated.
- [ ] Evaluate UPDATE/DELETE caching separately. Their access plans can depend on estimator revisions,
  parameter shapes, and deterministic visitation/error order, so they may need the full SELECT-style
  relation signature rather than the INSERT schema signature.
- [ ] Evaluate `INSERT ... SELECT` only after its source query plan can compose with the existing
  SELECT cache without capturing per-execution folds.

Evidence is recorded in `spec/design/benchmarks.md` under “INSERT performance slice-2 result.” Against
the Slice-1 prepared/fresh-resolution control, the cached `insert_rollback` lane improved by 5.8%
Rust/shared, 2.8% Go, and 6.7% TypeScript with the same checksum. Temporary allocation probes removed
25 allocator calls and 2,886 requested bytes per row in Rust, 11 allocations per row in Go, and 2.2%
of whole-process V8 allocated bytes. The separately measured one-shot form remained 11.6–18.2% slower
than the cache, but includes parsing and is not used to attribute resolution savings. Paired hot-read,
full-scan, and durable-write controls stayed within 5%; the Rust shared/exclusive gap is still 25.8%,
so the motivating glibc regression remains open.

Exit: repeated prepared INSERTs skip static resolution safely in all three cores, while every listed
invalidation and per-execution gate remains effective.

## Slice 3 — reduce Rust leaf/path deep cloning

This is the largest Rust-specific opportunity and the existing open `TODO.md` item. Prototype the two
contained approaches below against the same tests and allocation probe, then keep only the simpler
one that produces a material improvement.

### Candidate A — shared immutable leaf entries

- [x] Replace deep-cloned Rust key/row ownership in decoded leaves with a representation whose outer
  leaf/path copy retains immutable entry storage cheaply, such as `Arc<[u8]>` keys and appropriately
  shared immutable rows.
- [x] Keep borrowed `key_at` and owned `row_at` behavior unchanged above the representation seam.
- [x] Measure atomic reference-count traffic on hot reads and serial inserts; do not accept a broad
  read regression to improve one write lane.
- [x] Verify large/unfetched values, composites, arrays, ranges, JSON, and variable-width text do not
  gain accidental eager materialization.
- [x] Verify dirty serialization emits identical bytes and does not retain obsolete page blocks.

### Candidate B — mutate uniquely owned dirty nodes in place

- [x] Add an internal mutation helper that may edit a node only when both conditions hold:
  1. `Arc::get_mut` proves the Rust node is uniquely owned; and
  2. the node is dirty/unpublished (`page == 0`).
- [x] If either condition fails, use the existing copy-on-write rebuild unchanged.
- [x] On a unique dirty leaf, insert/replace within its key/row/weight vectors and run the same
  page-fit/split logic.
- [x] On a unique dirty interior node, replace the changed child in place; if a split propagates,
  run the same separator insertion and split logic.
- [x] Never mutate a clean node in place even if it appears uniquely owned; this keeps the durable
  fallback and publication invariant obvious.
- [x] Use `Arc::get_mut`, not an unsafe uniqueness assumption or a manually inspected refcount.
- [x] Check that helper-created temporary `Arc` clones do not accidentally defeat uniqueness before
  the decision point.

### Selection and implementation

- [x] Compare A, B, and a small hybrid. Record allocations, bytes, `insert_rollback`, hot lookup, full
  scan, and compile/code-complexity impact.
- [x] Prefer unique-dirty in-place mutation if it captures most of the gain without changing the
  stored key/row representation.
- [x] Prefer shared entries if uniqueness is rarely available or writable-CTE/cursor aliases make the
  transient path ineffective.
- [x] Do not keep both unless each provides a separately demonstrated gain.
- [x] Apply the chosen mechanism to table and ordered secondary-index B+trees through their common
  `PMap`; do not build an INSERT-only tree.
- [x] Cover remove/rebalance only in a separate follow-up after insert/replace is proven. An INSERT
  optimization must not quietly reshape DELETE.
- [x] Replace the existing open `TODO.md` Rust CoW item with the durable result or a narrower remaining
  follow-on when the slice lands.

Candidate A (`Arc<[u8]>` keys plus `Arc<Row>` values) reduced the allocation probe to 107,571 calls /
36.50 MB and `insert_rollback` to 10.68 ms shared, but changed the representation throughout the
serializer and ran the hot prepared lookup about 4% slower. Candidate B reduced the probe further to
73,952 calls / 5.41 MB and the lane to 2.67 ms shared. The hybrid raised calls to 87,878 and slowed
the lane to 3.11 ms, so only B remains. Five retained Slice-2/current pairs show an 88.2% latency
reduction with identical checksum; the final shared/exclusive gap is 8.6%. Durable evidence is in
`spec/design/benchmarks.md` under “INSERT performance slice-3 result.”

Exit: Rust no longer deep-clones every existing key and row on repeated inserts into the same leaf;
goldens and snapshot tests remain exact; representative reads do not regress.

## Slice 4 — evaluate transaction-owned transient nodes in Go and TypeScript

Go and TypeScript already shallow-copy leaf entries, so first remeasure after Slices 1 and 2. Implement
transient mutation only if outer slice/array and path-node churn remains material.

### Shared ownership model

A node may be mutated in place only when it was created for the current mutation generation and no
root from that generation has subsequently been exposed as an immutable alias. Otherwise the path is
copied and newly created nodes receive the current generation.

Required generation boundaries include:

- forking a working root from committed state;
- publishing a committed root;
- cloning/pinning the pre-statement root for a writable CTE;
- opening a cursor or scan that retains a working root while later writes are possible;
- creating any internal statement-rollback/read pin;
- attachment-root pinning;
- future savepoints, if they land later.

Tasks:

- [x] Inventory every snapshot/root clone in Go and TypeScript before adding an owner field. A plain
  pointer/object reference offers no uniqueness proof.
- [x] Specify a `MutationGeneration`/owner token as private runtime state, never persisted, rendered,
  hashed, or metered.
- [x] Rotate the generation whenever an existing root becomes an immutable alias. Old-generation
  nodes are copied on the next touched path; new path nodes receive the new generation.
- [x] Freeze/clear ownership on commit so published nodes are never mutable.
- [x] Go: keep the token inside the engine/working snapshot and nodes; do not use finalizers, unsafe
  pointer identity, or reflection.
- [x] TypeScript: use an unforgeable private object/token identity rather than a numeric value that
  could collide after wraparound; do not expose it through serialization or structured cloning.
- [x] Keep Packed/clean leaves immutable. The first mutation materializes/copies them exactly as now.
- [x] Preserve the same split/merge builders and exact page-fit rules.
- [x] Add tests that deliberately rotate generations through writable CTE pins, cursors, rollback,
  attachments, and commit.
- [x] Measure GC bytes, pauses, and elapsed time. If the gain is marginal, retain immutable shallow
  copies and document why Rust alone received the deeper tree optimization.

Rust may use the same conceptual generation in documentation, but its implementation should continue
to require `Arc::get_mut` as the final uniqueness proof.

The Slice-3 commit `7f66dd58` was retained as the control. Five CPU-2-pinned processes reduced the
Go lane from 11.661 ms to 3.821 ms (-67.2%) and the TypeScript lane from 16.182 ms to 14.983 ms
(-7.4%), with identical checksums. Go's median 30-transaction probe reduced allocated bytes 89.5%,
GC cycles 92.1%, and GC pause 91.4%; V8's complete-lane trace reduced between-GC allocated bytes
53.4% and scavenges from 46 to 29. Hot PK lookup, full scan, and durable-write paired controls all
stayed within 2%. Durable evidence and the exact token/alias inventory are in
`spec/design/benchmarks.md` and `spec/design/transactions.md`.

Exit: either Go/TypeScript gain a proven transient path under an explicit aliasing invariant, or the
slice closes with measurements showing their existing shallow-copy approach is preferable.

## Slice 5 — integration and final decision gate

- [ ] Run focused unit tests after each core change.
- [ ] Run the shared SQL conformance and cost suites after each cross-core slice.
- [ ] Run byte-exact goldens and cross-core file round trips after every tree representation/mutation
  change.
- [ ] Run `bundle exec rake unit`.
- [ ] Run `bundle exec rake concurrency:race` and `go test -race ./...` after ownership/transient work.
- [ ] Run `bundle exec rake concurrency:process` because the motivating default is shared file access.
- [ ] Run TypeScript typecheck and browser/OPFS tests; the TS core shares the executor/tree even though
  OPFS itself remains exclusive.
- [ ] Run formatting, lint, verification, and finally the full `bundle exec rake ci` gate.
- [ ] Run the complete primary/regression benchmark matrix with checksum agreement.
- [ ] Repeat the CPU-pinned alternating Rust `shared`/`exclusive` A/B and record the final delta.
- [ ] Rerun the native Node/Rust wrapper comparison for affected write lanes.
- [ ] Update `spec/design/benchmarks.md` with methodology, before/after numbers, allocation/GC evidence,
  and rejected prototypes.
- [ ] Update `spec/design/api.md` with the prepared-INSERT cache contract and invalidation rules.
- [ ] Update `spec/design/transactions.md`, `packed-leaf.md`, or `bplus-reshape.md` with any durable
  transient-mutation ownership rule.
- [ ] Update `TODO.md`: remove the Rust CoW item if complete and retain only measured follow-ons.
- [ ] Correct shared-locking documentation so it states the measured Rust/glibc behavior truthfully.
- [ ] Update `CLAUDE.md` only if a standing architectural decision changed. These optimizations should
  preserve, not relax, immutable published snapshots.
- [ ] No `/web` update is expected because there is no user-facing API or SQL behavior change.
- [ ] Delete this temporary file after its durable content has moved to the canonical docs.

## Commit sequence

Keep the history reviewable and bisectable. Each commit must pass its focused tests and include its
own before/after benchmark evidence in the commit message or accompanying design update.

1. Measurement/correctness guards and benchmark corpus additions.
2. Single-row INSERT fast path across Rust, Go, and TypeScript.
3. Prepared-INSERT cache specification, tests, and all three implementations.
4. Rust copy-on-write allocation reduction.
5. Go/TypeScript transient mutation only if measurements justify it.
6. Final benchmark record, canonical documentation/TODO updates, and deletion of this file.

Do not combine the shared-lock protocol implementation and all performance work into one opaque
commit. The current `shared-file-locking` branch may carry the sequence so the final A/B is available,
but every optimization must remain independently reviewable and revertible before fast-forwarding
`master`.
