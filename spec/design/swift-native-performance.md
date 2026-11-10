# Swift execution architecture investigation

Authorized 2026-11-06 after the completed native Swift experiment. This work is
explicitly opt-in and does not promote Swift, change its semantic baseline, or
select production delivery. The original frozen implementation `cec8170bfc33f087fa98949ae661b1eaa5490218`
and its complete benchmark/verification evidence remain preserved. The starting
checkout for this investigation is local-only `56972d2f`.

The original question was why the experimental Swift engine took geometric-mean
times of 9.1990× Rust, 7.5125× Go and 2.1292× TypeScript on the same 143 workloads.
The language-neutral contracts remain authoritative. Changes must preserve
values, types, diagnostics, logical costs, resource schedules, cancellation,
durability and byte encodings. No unsafe arithmetic, unchecked builds, alternate
storage path, fixture weakening, dependency addition or workload-specific result
shortcut is authorized by this investigation.

## Completed result

The structural implementation is complete and frozen at local-only
`43481e2047da1cee34a627b910c8a7c7c59c473f`. It preserves the semantic target and
experimental status. [BENCHMARKS.md](../../impl/swift-native/BENCHMARKS.md) now
leads with direct peer timings, followed by controlled Swift and aligned Ruby
comparisons. All selected results pass exact validation; no slower result or
failed setup is discarded.

The final 22-workload, four-core, three-round diagnostic matrix gives equal-weight
Swift/peer geometric means of **5.4065× Rust, 5.1083× Go and 1.0423× TypeScript**;
Swift has lower medians on 0/22, 0/22 and 13/22 respectively. These are not an
updated result for all 143 workloads. In the separate eight-workload alternating
Swift comparison, lookup improves **1.93×**, fixed aggregation **2.74×**, array
COUNT **11.55×** and array containment **12.58×**. All eight medians improve;
durable insertion's 1.18× result has overlapping round ranges.

The aligned three-workload Ruby/YJIT comparison is mixed: Ruby wins lookup
(16.757 versus 23.620 µs), while Swift wins full aggregation by 23.81× and
durable insertion by 2.71×. The runner preserves Ruby's normal compiler loading
and defaults; its file aggregate uses the existing streaming fold, while the
separate compiled-aggregate path declines file-backed execution. These are
implementation comparisons, not evidence of an inherent language ranking.

The current source passes **543 native tests and all 749 frozen SQL files**,
241,412 exact resource measurements, 77,612 encrypted records, 2,691 interchange
phases, all saved and generated fault sweeps, 496 mixed-process pairings, both
Swift-client/Rust-owner streams, stress and the six-direction service corpus.
[VERIFICATION.md](../../impl/swift-native/VERIFICATION.md#structural-follow-up-acceptance)
maps each gate to its actual actor and preserves the stale-Go-adapter failure.

The measured gains establish substantial representation/execution overhead in
the original Swift core. They do not establish Rust/Go parity or a Swift
performance ceiling. The remaining source-level opportunities below are
documented hypotheses for separate controlled work, not claimed attribution.

## Second follow-up: copy and dispatch costs (2026-11-09)

The maintainer asked why Swift, usually between Go and Rust, stayed about
five times slower after the structural work above. Sampled release profiles
answer it: the compiler and build are not the cause. On every workload examined,
50–70% of the time was `swift_retain`/`swift_release` and value-witness copies.
The engine is written in a value-semantic style whose per-row and per-statement
copies each cost many atomic reference-count updates. Rust moves or borrows
the same data and Go's collector makes a pointer copy free, so neither pays.

Causes found, in order of measured weight:

- **Large structs copied by value.** `ExpressionContext` holds every statement
  service (about thirty references). Forty row loops copy it per row to rebind
  one closure, and each bounded-aggregate state held its own copy: 48% of
  `group_by_agg`. `SelectPlan`, `DomainSnapshot` and `ScanCandidate` copies
  were about a third of a point lookup.
- **Per-cell and per-row ownership.** Each `StoredCell` carried a full
  `TypeDescriptor`; each scanned row built a node-owning `TreePosition`, a
  page slice and a descriptor copy before decoding one integer.
- **String-typed operators.** Resolved operators are `String`s. Comparing two
  different Swift strings takes the slow Unicode path, and the evaluator built
  an array of six strings per comparison to test membership.
- **Expression nodes copied on dispatch.** Pattern-matching a node copied both
  child expressions and the operand descriptor out of the enum on every
  evaluation.
- **Writes re-validated whole leaves.** Each insert or delete rebuilt its leaf
  through the validating initializer, re-checking every record, and
  `TableStore.put` copied the whole store, forcing a full copy of the
  pending-key dictionary for each staged key.
- **Statements are re-planned and re-admitted every execution.** UPDATE and
  DELETE have no prepared plan; each execution resolves, inventories and
  estimates again, and every statement repeats privilege, domain and catalog
  validation. This is the main remaining cost and is not fixed here.

Changes, all preserving results, costs, errors and bytes:

- `ResolvedExpression` and `SelectPlan` keep their fields in a copy-on-write
  box; a copy is one retain. `ExpressionContext` keeps row-varying fields
  inline and boxes the rest. Aggregate states hold only the meter and the
  collation key.
- `StoredCell` carries its fixed width, not a descriptor; decoding takes the
  column type from the owning tree. Packed leaves keep plain-integer region
  facts and decode common scalars straight from page bytes.
- Cursors hand out leaf **runs**; the packed aggregate fold holds each leaf
  once and still polls, charges and checkpoints per row in the old order.
- The evaluator classifies an operator's bytes once and compares or adds
  integer pairs directly. `ByteReader` reads integers without sub-slices.
- Leaf edits derive the new leaf from the old one, and a clean page is adopted
  as writer records without re-validation. `TableStore` copies only the tree
  for failure atomicity. An unbounded cost meter skips ceiling arithmetic.

One round of the 22 diagnostic workloads, all four actors on one CPU with
identical checksums ([CSV](../../impl/swift-native/BENCHMARK_FOLLOWUP_20261109.csv)):

| Group | Swift before / Rust | Swift after / Rust | Swift after / Go | Speedup |
|---|---:|---:|---:|---:|
| Scans and aggregates (12) | 4.96× | 2.63× | 2.68× | 1.89× |
| Per-statement lookups and writes (8) | 9.00× | 7.81× | 7.26× | 1.15× |
| All 22 | 5.75× | 3.85× | 3.85× | 1.49× |

`full_scan_agg` goes from 83.5 to 37.0 ms (Rust 21.7, Go 13.9),
`varscan_text_isnull` from 12.3 to 4.4 ms and `group_by_agg` from 2.98 to
1.67 s (Rust 0.65, Go 1.03). Point lookups improve only 1.1× (22.0 µs against
2.5 µs) and row writes 1.07–1.4×. The run used a tmpfs data directory, so the
two durable workloads measure engine overhead without a real barrier and are
not comparable with the earlier disk-backed table. This is a single-round
diagnostic, not a replacement for the three-round matrix above.

Verified on this source: 543 native tests and all 749 frozen SQL files (21,597
records, 2,271 cost assertions), the threaded stress run with the frozen final
checksum, and 897/897 Swift–Go file interchange phases. The cross-core resource
accounting, encrypted, simulation, process and page-service gates were not
re-run.

What remained after this pass, by expected value (the first three items were
taken up in the third follow-up below):

- **Prepared mutation plans.** Cache the resolved preparation and selected
  access for UPDATE and DELETE as INSERT and SELECT already do, and cache
  statement admission (privileges, targets, sequence-writer need) against the
  schema signature. A profiled prepared DELETE spends about 40% of its time
  re-planning and about 30% in statement admission and snapshot handling.
- **Statement-scoped copies.** `DomainSnapshot`, `TableStore`, `ScanCandidate`
  and `SessionSettings` are still copied field by field several times per
  statement, and each statement's first staged key copies the transaction's
  pending-key dictionary.
- **Ownership tokens for dirty nodes.** Interior nodes are still path-copied
  per mutation, a retain per child. Go mutates nodes its generation owns.
- **Typed, batched folds.** The per-row fold still retains each aggregate
  state and decodes through `SQLValue`. With no cost ceiling and no
  cancellation token, charges could be batched per leaf as the peers do.
- **Operator codes.** Resolved operators should be an enum; the byte
  classifier here covers only the evaluator's hot path.
- Dynamic exclusivity checks on class-held arrays are roughly a tenth of a
  filtered scan's samples. `-enforce-exclusivity=unchecked` would remove them; it was not used.

## Third follow-up: per-statement and bulk-write costs (2026-11-10)

The second pass left lookups and row writes about eight times slower than
Rust. Profiles of a prepared point SELECT, INSERT and DELETE showed no single
hot spot: each statement repeated work whose result could not have changed,
and each write copied far more of the tree than it edited.

Causes found:

- **Snapshot, store and candidate structs were still copied field by field.**
  `DomainSnapshot` (fifteen fields), `TableStore`, `ScanCandidate`,
  `CatalogTable` and `SessionSettings` are passed and stored many times per
  statement. They were about a fifth of a point lookup.
- **UPDATE and DELETE resolved their SQL on every execution** (23% of a
  prepared single-row DELETE), unlike INSERT and SELECT.
- **Every write validated the whole catalog.** Updating a row count went
  through `updateCatalog`, which re-validates every reference and re-resolves
  every composite type. Every statement also re-borrowed the main domain's
  types into the temporary domain and re-ran the table's writability check.
- **A sequence-writer walk ran for every write**, though its answer matters
  only when every target is temporary (8% of a DELETE).
- **Each edit copied multi-reference elements.** A writer leaf's record array
  held three references per record and an interior node's child array three
  per clean child, so one row edit cost roughly a thousand reference-count
  updates and as many again when the old nodes were freed.
- **Bulk writes path-copied the interior nodes once per row.** A 10,000-row
  DELETE spent two thirds of its time copying and freeing child arrays.
- **Candidate costing hashed strings.** Each access candidate built a
  string-keyed dictionary and looked up every unit of the cost schedule in it;
  each candidate also rebuilt the same contradiction check.

Changes, all preserving results, costs, errors and bytes:

- The five structs above keep their fields in a copy-on-write box, as
  `SelectPlan` does. `PackedRecord` shares one immutable box and a clean
  child's payload is `indirect`, so either copies with one retain.
- `PreparedMutationPlan` caches UPDATE/DELETE resolution against the same
  schema signature as `PreparedInsertPlan`, with the same eligibility (a base
  table outside the temporary domain, reusable expressions, no dynamic-zone
  index). Privileges, roots and index contexts are rebound per execution. The
  **access path is still selected on every execution**: its estimator inputs
  change with every write, so there is no signature under which to reuse it.
- DML records a row-count change without whole-catalog validation; type
  borrowing and the writability check are skipped while the catalogs (and, for
  the latter, the environment) are the same objects that passed before. The
  sequence walk runs only when it can decide the outcome.
- A writer leaf holds its records in chunks of at most 32 (`RecordList`), so
  an edit copies one chunk and the chunk list.
- **Edit batches.** One statement's run of `put`/`remove` on its private store
  marks the interior nodes it creates with a token and updates those nodes in
  place afterwards. The steps and the resulting tree are exactly those of the
  unbatched edits; only the copying differs. A batch never touches a node that
  existed before it, so earlier snapshots are unaffected. The cost is the
  failure guarantee of a removal: an I/O or corruption error while rebalancing
  can leave the batch's own tree half edited, so a batch whose edit throws is
  discarded, which every caller already does with a failed statement's store.
  GIN edits stay unbatched.
- Pending-key accounting uses a persistent hash trie (`PendingKeyMap`). A
  statement's first staged key no longer copies every key its transaction has
  staged. This did not move the 1,000-row rollback workloads; it removes a
  quadratic cost from longer transactions.
- Candidate cost comes from six cached schedule weights; the candidates of one
  inventory share their contradiction check. MIN/MAX compare integers without
  a type descriptor, and the variable-width cost preflight reads its store's
  facts once rather than per row.

Two alternating rounds of the 22 diagnostic workloads for the two Swift builds
(best of two) and one round for the peers, all on one CPU with identical
checksums ([CSV](../../impl/swift-native/BENCHMARK_FOLLOWUP_20261110.csv)).
"Before" is the second follow-up's source:

| Group | Swift before / Rust | Swift after / Rust | Swift after / Go | Speedup |
|---|---:|---:|---:|---:|
| Scans and aggregates (12) | 2.62× | 2.56× | 2.59× | 1.03× |
| Per-statement lookups and writes (8) | 7.44× | 3.86× | 3.72× | 1.93× |
| All 22 | 3.78× | 2.92× | 2.95× | 1.29× |

`point_lookup_pk` goes from 21.7 to 12.3 µs (Rust 2.6, Go 2.1),
`insert_rollback` from 39.0 to 19.7 ms per 1,000 rows (Rust 2.9),
`delete_rollback_serial` from 58.5 to 27.3 ms (Rust 2.8) and
`delete_range_rollback` from 102.3 to 20.9 ms (Rust 8.1, Go 12.9).
`full_scan_minmax` goes from 61.0 to 47.6 ms; the other scans are unchanged
within 3%. As before the data directory is tmpfs and this is a diagnostic, not
a replacement for the three-round matrix.

Verified on this source: 546 native tests and all 749 frozen SQL files (21,597
records, 2,271 cost assertions), the threaded stress run with the frozen final
checksum, and 897/897 Swift–Go file interchange phases. Three new unit tests
cover what the corpus cannot: `PendingKeyMap` and `RecordList` against the
plain collections they replace, and batched edits against unbatched ones node
for node across leaf merges and splits. That last test caught a real defect
during development (a parent's node count went stale when its child had been
edited in place), which the SQL corpus had not. The saved fault-simulation
regressions and the encrypted SQL corpus also pass for Swift alone. The
cross-core resource accounting, cross-core encrypted matrix, Rust and
TypeScript interchange, generated simulation sweeps, process and page-service
gates were not re-run.

What remained after this pass (the path-copy and select-setup items were
taken up in the fourth follow-up below):

- **Access selection per write** is now the largest single cost of a
  single-row UPDATE or DELETE (about a fifth). The estimator is specified
  cross-core, so the route is making inventory and estimation cheaper, not
  skipping them.
- **Single-row writes still path-copy.** A batch helps only within one
  statement: each statement keeps its predecessor's tree for statement
  atomicity and for cursors pinned in the transaction, so its first edit copies
  the root-to-leaf path, a retain per child. Chunked child arrays would cut
  that; a transaction-wide token is unsafe for the reasons above.
- **Select setup.** A point lookup descends the tree twice (cost preflight,
  then the scan), compiles its packed predicate and rebuilds its read bundle
  per execution; together about half of the remaining 12 µs.
- The typed-fold, operator-enum and exclusivity items of the second follow-up
  are unchanged.

## Fourth follow-up: scans, grouping and lookup setup (2026-11-10)

After the third pass scans were still about 2.6 times slower than Rust.
Profiles showed that most scan predicates and the plain aggregate fold still
went through per-row objects, and that general grouping rebuilt its
accumulators for every row.

Causes found:

- **Only text and bytea equality, nullness and array operators had an encoded
  predicate.** An integer comparison such as `amount > $1` fell back to the
  generic evaluator through the row frame, which cost a third of a filtered
  scan. The encoded predicates that did exist read each cell through a
  `StoredCell` slice, several reference-count updates per row.
- **The variable-width cost preflight built a cell per row** to learn that an
  inline value has no large-value facts.
- **The ungrouped fold retained its states and re-read its meter per row**:
  about three quarters of `full_scan_agg` was reference counting.
- **General grouping made, restored, packed and freed an accumulator object
  per aggregate per row**, compared the function name against string sets on
  each pack and restore, encoded each group key twice and looked it up three
  times, and copied the expression context through a method reference
  (`map(current.evaluate)`).
- **A point lookup descended the tree twice**, once for the cost preflight and
  once for the scan, and recompiled its predicate through a private meter.
- **Names hash a byte at a time.** `SQLText` keys most catalog dictionaries.

Changes, all preserving results, costs, errors and bytes:

- An encoded integer predicate: a bare integer column against an integer
  literal or parameter, for all six comparisons. It declines on NULL and on a
  writer leaf, charging nothing, exactly as the other encoded atoms do. Text,
  bytea and array atoms read a clean page's bytes in place.
- The preflight walks leaf runs and skips inline and NULL cells without
  building them.
- **Leaf-at-a-time fold.** When the statement has no cost ceiling and no
  cancellation token, nothing can observe one row's charge apart from the
  next. An ungrouped, unfiltered fold of COUNT, integer SUM and integer
  MIN/MAX then computes a whole leaf and adds the leaf's charges once. An
  overflowing sum or count makes that leaf fall back to the row-at-a-time
  path, which reports the error at its exact row. With a ceiling or a token
  the row-at-a-time path runs as before.
- General grouping reuses one scratch accumulator per aggregate (a plain
  accumulator's whole state is the tuple `restore` installs), decides each
  state's tuple shape once in the plan, and carries the key's encoding and
  resident entry from the lookup to the store. `AggregatePlan` and
  `ResolvedAggregateCall` are boxed.
- A single-key access keeps the row position found by the cost preflight, or
  looks it up once when no variable-width column makes the preflight visit
  the row. The charge order is unchanged.
- `SQLText` and the spill map's key hash eight bytes per step with the same
  seeded hasher. Interior nodes hold their children in fixed chunks of 32
  (`ChildList`), so a single-row write copies one chunk and the chunk list.

Two alternating rounds for the two Swift builds (best of two) and one round
for the peers, one CPU, identical checksums
([CSV](../../impl/swift-native/BENCHMARK_FOLLOWUP_20261110B.csv)). "Before" is
the third follow-up's source:

| Group | Swift before / Rust | Swift after / Rust | Swift after / Go | Speedup |
|---|---:|---:|---:|---:|
| Scans and aggregates (12) | 2.57× | 0.79× | 0.80× | 3.25× |
| Per-statement lookups and writes (8) | 4.01× | 3.69× | 3.41× | 1.09× |
| All 22 | 2.97× | 1.52× | 1.50× | 1.96× |

`full_scan_agg` goes from 37.2 to 5.1 ms (Rust 21.3, Go 14.5),
`full_scan_agg_filter` from 107.8 to 53.3 ms (Rust 54.5), `varscan_text_eq`
from 8.2 to 2.5 ms (Rust 2.4), `varscan_project_needle` from 23.2 to 4.5 ms
(Rust 8.5), `group_by_agg` from 1.70 to 1.05 s (Rust 0.67, Go 1.05) and
`point_lookup_pk` from 12.4 to 10.4 µs (Rust 2.5). The two plain aggregate
scans now beat both peers because of the leaf-at-a-time fold; that says
nothing about the peers' design, which keeps per-row charging there.

Verified on this source: 548 native tests and all 749 frozen SQL files (21,597
records, 2,271 cost assertions), the threaded stress run with the frozen final
checksum, 897/897 Swift–Go interchange phases, the 18 saved fault-simulation
regressions and the encrypted SQL corpus for Swift alone. Two more unit tests
cover `ChildList` against a plain array and the leaf-at-a-time fold against
the row-at-a-time one. The existing packed-aggregate tests already compare an
unlimited statement with a cost-limited one for equal rows and equal cost. The
cross-core resource accounting, cross-core encrypted matrix, Rust and
TypeScript interchange, generated simulation sweeps, process and page-service
gates were not re-run.

What remains, by expected value:

- **Per-statement overhead.** A single-row INSERT or DELETE is still six to
  eight times Rust (19 and 23 µs against 3). No one item dominates: access
  selection is about a fifth of a DELETE, and the rest is statement admission,
  snapshot and settings handling, index maintenance and record encoding in
  pieces of a few percent each. A point lookup is four times Rust for the same
  reason.
- **General grouping** is at Go's speed and 1.6 times Rust. It still packs
  and restores a tuple per aggregate per row and encodes a generic key.
- **Filtered and grouped folds** do not use the leaf-at-a-time form.
- The operator-enum and exclusivity items of the second follow-up are
  unchanged.

## Investigation and controlled changes

1. Inspect captured compiler commands, flags, libraries, host controls and
   benchmark adapters. Distinguish a build/runtime handicap from additional engine
   work. Parameter timing and cold/concurrent semantics stay explicit.
2. Profile immutable release actors on representative lookup, variable-column,
   predicate, aggregate and write paths. Debugger samples identify call paths,
   not CPU percentages or benchmark measurements. Keep profiling and timing
   separate; do not change machine-wide governors or tracing policy.
3. Test value representation independently. The original `indirect` declarations
   box scalar payloads as well as recursive containers. Selective indirection
   keeps primitive values inline while retaining boxes for recursive or large
   payloads. This changes physical layout only; deterministic logical memory
   accounting continues to use the shared spec, not `MemoryLayout`.
4. Audit cursor and expression environment ownership. Per-row copies of retained
   tree/context structures and temporary getter closures are structural costs.
   Eliminate redundant ownership work while preserving immutable snapshots,
   lazy page access, error visitation, host callbacks and thread isolation.
5. Compare packed execution with the supported cores. Existing Swift columnar
   eligibility often drives ordinary row decoding and recursive expressions;
   peers have encoded-span predicates, selective gathers and reusable scratch
   rows. Broader packed execution uses semantic operations and the same admission,
   charge, null, fallback and error contracts, without matching benchmark SQL.
6. Separate dirty records from their persisted page encoding. The initial Swift
   tree constructs, pads, checksums and validates a new packed leaf on every
   mutation; the supported cores retain decoded dirty leaves until serialization.
   An immutable dirty-leaf form can remove repeated encoding without changing
   snapshot ownership, split/merge policy, overflow ownership or commit ordering.
   Mutable builders and ownership tokens are a separate decision: an implicitly
   copied Swift value must never retain permission to mutate an aliased snapshot.
7. Treat prepared-plan requirements as immutable facts of the cached plan.
   Reuse admitted parameters on generic hits only when their exact cached schema
   provenance is established, and cache sequence/subquery requirements on the detached plan.
   Keep miss, custom-plan and uncacheable admission/error order unchanged.
   Read bundles own their snapshot map and deterministic pin order; execution
   creates a scalar input only when its selected backend needs one.

## Established causes and boundaries

The retained release actor uses `-O` and whole-module optimization, with matching
Swift 6.4.2 libraries and no sanitizer or testability flags. The investigation
found no build-mode explanation for the gap. Current cgroups impose no CPU or
memory limit; historical clocks and thermal conditions remain unknown. New
comparisons use the same logical CPU for all actors without changing governors.

Selective value indirection alone improves the controlled fixed aggregate by
1.44× but barely changes the lookup. Removing redundant scan ownership and page
counting, then using reusable packed positions, improves array COUNT by 7.11×
and text nullness by 4.43× in the intermediate comparison. These isolated stages
establish that representation and additional engine work matter; neither is a
whole-suite parity claim. The intermediate packed frame also regresses the
fixed aggregate relative to the value-only stage. The final implementation
therefore resolves aggregate dispatch once and uses a compact operand vector
for unfiltered folds, preserving the original decode and checkpoint order.

Dirty leaves retain immutable **encoded** record vectors. They do not eagerly
decode SQL values, and their validated nonpending spans remain usable by packed
predicates. Physical PAX packing, checksums and validation happen at persistence
boundaries. Snapshot copies remain immutable, and logical accounting continues
to use the shared schedule rather than physical allocation counts.

The existing adapters have a timing asymmetry: query/durable parameter generation
is inside Swift and Go timers but outside Rust, TypeScript and Ruby timers.
Retained peer actors are not silently modified. This affects interpretation of
small parameterized operations but cannot explain parameter-free scan gaps.
The supplementary Ruby wrapper explicitly includes generation to match Swift;
its tooling adjustment and unchanged core source are archived separately.

Ruby's older reports are separate evidence: source revisions, lock/storage
behavior, timing statistics and generated execution kernels differ. Do not infer
an inherent language ranking from those reports. Direct claims require an
aligned comparison.

## Evidence and acceptance

Use canonical workloads and datasets, capture source/actor identities, retain
all selected results including slower lanes, and check checksums and row counts.
Focused experiments label their membership and cannot replace a full-suite
performance claim. Compare new builds against the retained original actor under
the same conditions, with repeated alternating runs. Keep unrelated builds and
profiling idle during measurements.

For representation and execution changes, run the full frozen SQL/native suite,
exact cross-core accounting and affected byte/storage/host gates. Add direct
tests only for invariants unavailable to the corpus. Broader verification follows
the affected paths; earlier actors do not establish later source correctness.
Record accepted changes, rejected hypotheses, commands and results before
refreezing the resulting snapshot.

## Remaining structural work identified by source review

These are concrete implementation differences, not a measured allocation or CPU
breakdown of the final actor. They explain where another controlled experiment
could begin; this follow-up does not claim they account for the entire remaining
peer gap.

- The first two items of this list (direct nullness, leaf-scoped traversal)
  were done in the second follow-up above.
- The admitted integer-group packed lane builds key arrays and encodes a generic
  byte key for each survivor. Rust/Go use integer keys plus a separate NULL
  sentinel. Selecting the key representation once could remove this work while
  preserving first-occurrence order and exact logical reservations.
- Variable-width cost preflight traverses the input before execution. Peers can
  accumulate large-value facts in their packed fold and batch charges in an
  unlimited lane. Fusion in Swift needs error/cancellation-cost validation;
  equal totals on successful queries are insufficient.
- General bounded grouping restores and repacks aggregate state through the
  spill representation. That broad algorithm also exists in Go, but Swift adds
  row-context capture, argument arrays and full state objects. Retaining typed
  resident accumulators until spill is a larger representation change that
  must preserve memory admission, thresholds, fold order and cleanup.
- Prepared execution still copies a large `SelectPlan` while rebinding physical
  roots. Separating immutable plan metadata from per-execution bindings is a
  larger design change than making the entire mutable plan a reference type.

Do not use unsafe memory access, weaker checks, changed accounting or generated
SQL-specific shortcuts to obtain a language-performance ranking. Ruby's approved
runtime-compiled kernels also differ materially from interpreted execution;
the supplementary measurements compare these engines, not abstract languages.
