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

What remains, by expected value:

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
