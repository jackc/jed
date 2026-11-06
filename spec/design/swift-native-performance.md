# Swift execution architecture investigation

Authorized 2026-11-06 after the completed native Swift experiment. This work is
explicitly opt-in and does not promote Swift, change its semantic baseline, or
select production delivery. The frozen implementation `cec8170bfc33f087fa98949ae661b1eaa5490218`
and its complete benchmark/verification evidence remain preserved. The starting
checkout for this investigation is local-only `56972d2f`.

The question is why the final experimental Swift engine takes geometric-mean
times of 9.1990× Rust, 7.5125× Go and 2.1292× TypeScript on the same 143 workloads.
The language-neutral contracts remain authoritative. Changes must preserve
values, types, diagnostics, logical costs, resource schedules, cancellation,
durability and byte encodings. No unsafe arithmetic, unchecked builds, alternate
storage path, fixture weakening, dependency addition or workload-specific result
shortcut is authorized by this investigation.

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
   Reuse admitted parameters only when their exact cached schema provenance is
   established, and cache sequence/subquery requirements on the detached plan.
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
