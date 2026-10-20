# Multiway LEFT-join resolution follow-up

> Historical report: raw evidence and run-specific drivers are on the preserved
> experiment branch. See the [archive and retrieval instructions](../../bench/ruby-native/ARCHIVE.md).
> Reproduction commands below refer to that original checkout.

This explicitly requested optimization follows the
[expression traversal pass](EXPRESSION_WALK_FOLLOWUP.md). The Ruby core remains
experimental, frozen and opt-in. Normal tests, benchmarks, CI and future
feature work do not include it.

## Implementation

Eligible three-through-eight input INNER/CROSS/LEFT chains reuse resolved
expressions and aliased base-table source metadata within the existing bounded
statement cache. Existing resolution limits, contextual parameter checks and schema/environment
invalidation rules remain unchanged. No rows or parameter values enter the
retained resolution cache.

The review also found that the older table-column map could grow when a public
statement's aliases changed repeatedly, and its frozen descriptor hashes still
borrowed mutable alias strings. That map now admits at most eight table/alias entries
and owns its strings through the existing source-metadata snapshot helper.
It shares the source map's 4,096-unit and 64 KiB allowance. Oversized metadata
uses fresh resolution. This tightens retention without adding a new allowance.

Each invocation recomputes physical-plan eligibility from its current syntax.
Multiway LEFT queries always run ordinary physical selection, including when
routed planner syntax changes from INNER to LEFT after retaining an INNER
winner. The planner continues to enforce fixed outer-join boundaries and
recompute row estimates, access paths and algorithms for the current bindings.
The existing structural memo within a single plan search remains local to that
search and distinguishes fixed outer edges from reorderable inner edges.

RIGHT/FULL, USING/NATURAL, lateral and non-base sources retain their existing
fallbacks. No SQL behavior, host API or dependency changes.

## Evidence

The baseline is pushed commit
`7a70c9f2543dcf459e8b7ad6dcb4c02cb02499cb`, library SHA-256
`577249f5b49bd67c369ca53eb256d91db73668a072bb1051c819a9698a40fc31`,
archived at `/tmp/jed-outer-join-before/impl/ruby-native`.
Final library SHA-256:
`62e562806cb2fca0e30792f1c90c3546f9f847c94a1af4651d9b3a376fce3705`.
New evidence belongs in
[`outer-join-followup`](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/outer-join-followup`).
Earlier artifacts remain unchanged.

The timing comparison retains the existing nine-job
[multiway plan](../../bench/ruby-native/compare/multiway-plan.json): SQL,
1,000-row fixture, binding sequence, warmups, iterations and five samples per
process. Each runtime uses serial before/final/final/before processes for
prepared cursors, ordinary query cursors and eager execute. Results are fully
drained. Paired times average two process medians per source; all raw samples
and slower cells are retained. Process ranges are observations, not a
statistical significance test.
The plan's historical description calls its LEFT case an unsupported cache
control; this pass makes that same case eligible without changing its workload.

Interpreter and YJIT use Ruby 4.0.7; ZJIT uses Ruby 4.1.0-preview1. Comparing JIT
modes also changes Ruby version. The library does not enable a JIT. Fresh
supported-core and actual Rust Ruby-wrapper measurements characterize their
different public APIs and collection overhead separately from the controlled
Ruby source change. Wrapper preflights compare exact results, names, types and
costs before timing. Allocation profiles run last with reused
`execute_prepared` statements; the timed eager lane uses repeated `execute(SQL)`.

Separate 32-row diagnostics cover five LEFT shapes, an INNER control, FULL
fallback and function fallback through all four APIs, in memory and file
storage. They compare outcomes and error/cost prefixes against fresh resolution,
check current physical-plan signatures, exercise catalog changes, and recount
the retained metadata against its shared allowance.

Workers run serially with a 512 MiB sampled RSS guard. Ruby workloads have at
most 2 GiB of address space; supported-core comparison workers retain their
established 16 GiB virtual reserve. The full host suite uses the same 2 GiB
virtual reserve established by the
[preceding allocator investigation](EXPRESSION_WALK_FOLLOWUP.md#evidence-and-method),
with the resident guard unchanged. These are process guardrails, not a
whole-engine memory guarantee.

## Correctness

All **67 host-test files** passed. The focused suite passed under the
interpreter, YJIT and ZJIT. It verifies matching/unmatched/NULL keys, duplicate
matches, ON versus WHERE, anti-joins, mixed inner/outer chains, exact statement
and lifetime cost/error prefixes, contextual casts, query-memory limits,
catalog/session invalidation, suspended cursors and early close.

Internal probes check that changing routed syntax from INNER to LEFT retains
fresh physical selection even when an old INNER template exists, and that
execution-local extension inputs never share mutable candidate estimates.
They also check metadata ownership/budgets, oversized fallbacks, the eight-input
admission limit, 64-statement eviction and release of session/cursor/binding
references.

All four complete corpus lanes passed **77,154/77,154 records**:

| Storage / API | Runtime | Passed records |
| --- | --- | ---: |
| Memory / query cursor | ZJIT | 20,297 |
| Memory / execute collector | ZJIT | 20,297 |
| File / query cursor | YJIT | 18,280 |
| File / execute collector | YJIT | 18,280 |

There are no failures, unsupported records, timeouts, memory-limit exits or
additional skips. Existing authored storage-mode skips remain unchanged.
The largest corpus worker used 177.0 MiB sampled RSS; the full host suite
peaked at 320.6 MiB. All correctness workers exited before measurements began.

The six formal source/runtime diagnostic reports agree exactly on all
64 workload/API/storage groups, including error/cost prefixes and catalog-change
states. Their current physical winners also agree with fresh resolution.
Both sources passed the actual Rust Ruby-wrapper preflight for all nine timed
workloads and all three native API lanes.

## Results

The warmed three-table LEFT workload improves in all nine runtime/API
comparisons. Both final process medians are below both baseline medians in
each comparison:

| Runtime | Prepared cursor speedup | Ordinary query speedup | Eager execute speedup |
| --- | ---: | ---: | ---: |
| Interpreter | 1.740× | 1.767× | 1.730× |
| YJIT | 1.557× | 1.639× | 1.655× |
| ZJIT | 1.440× | 1.409× | 1.401× |

For YJIT, prepared time falls from 707.771 to 454.562 µs, ordinary query
from 714.663 to 436.082 µs, and eager execute from 689.759 to 416.681 µs.
The profile records **62.910% fewer objects**, from 5,922.9 to 2,196.8 per
query. Base `Engine#compile` calls fall from 110 to zero, source-descriptor
construction from six calls to zero, and base multiway planning remains
one call per execution. There are 36 prepared-root lookups and no retained
LEFT physical winner. These are allocation/call counts, not CPU percentages.

This is a targeted gain, not an across-the-board improvement. Across all
81 paired cells, 19 have separated faster process medians, 24 have separated
slower medians and 38 overlap. Thirty-eight paired means are slower.
All observations remain in
[the complete tables](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/outer-join-followup/tables.md`).

The interpreter's eight eager control workloads are 3.313–5.151% slower,
all with separated medians. YJIT's eight prepared controls are
1.904–5.734% slower, also all separated. Its ordinary-query and eager slower
cells have overlapping ranges. The largest regression is ZJIT's prepared
four-table INNER join: **310.758→378.774 µs, +21.887%**. Its baseline process
medians are 299.190/322.325 µs and final medians 366.973/390.576 µs; final
raw samples span 327.774–608.164 µs. ZJIT prepared scalar and indexed join
also have separated slowdowns of 4.855% and 6.319%.

All eight non-LEFT profile call-count maps are identical between sources.
Their allocations are unchanged except a 0.1-object difference on range.
Scalar and PK do not enter the changed join path. Those facts do not explain
the elapsed regressions, and YJIT call profiles cannot diagnose ZJIT behavior.
The report preserves these limitations; no adaptive rerun replaces a primary
observation.

### Fresh core and binding comparisons

The separate supported-core adapter measures Ruby YJIT at 440.050 µs for
LEFT, versus Go at 3.274 µs, Rust at 3.263 µs and TypeScript at 10.773 µs.
That is still **134.395× Go and 134.879× Rust**. This pass removes much
repeated resolution but leaves a large overall join-performance gap.
Scalar and PK in that adapter are 6.652× and 2.689× Go, respectively.

Through the actual Ruby packages, native prepared LEFT takes 427.147 µs
versus the Rust binding's 48.145 µs: **8.872×** the wrapper's elapsed time.
Native query and eager execute take 435.991 and 432.699 µs. Native prepared
scalar, PK and four-table queries are faster than the wrapper in this run,
at 0.635×, 0.390× and 0.760× its elapsed time. The other prepared join cases
range from 1.281× to 5.893× the wrapper. These package paths have different
planning/collection overhead from the supported-core adapter; their ratios
are separate observations.

All 24 formal commands completed within their guards. Ruby timing and
validation workers peaked at 85.6 MiB RSS, direct diagnostics at 75.9 MiB,
and the supported TypeScript comparison at 177.4 MiB. Timings use memory
storage; file storage is covered by correctness and direct diagnostics.

### Next candidate

Repeated scalar expression-work estimation remains a concrete candidate.
The final LEFT profile still records 93 base `expression_work` calls through
several wrapper layers; range has 231. These are structural work estimates
used repeatedly during fresh physical search. A bounded execution-local memo
for eligible scalar expressions could avoid repeated walks while keeping
parameter-sensitive selectivity, candidate estimates and winners fresh.
It would need to populate at the original call site to preserve error order,
exclude host/subquery/quantified expressions, and expire with the SELECT.
The call counts motivate investigation; they do not establish its speedup.

## Reproduction

The evidence directory contains `check.rb` for focused, host and complete-corpus
checks, and `run.rb` for measurements. Both use explicit
`mise run ruby-native:*` tasks, run serially and refuse to replace existing
reports. Preserve earlier artifacts before an intentional rerun.

Run `check.rb --phase PHASE --source-sha256 FINAL_HASH` for `focus`, `host`
and `corpus`. The correctness driver needs 2 GiB of outer address space and
local Unix-socket access for the existing page-service host tests.

After correctness passes and measured source/helpers are frozen, run
`run.rb --phase PHASE --final-source-sha256 FINAL_HASH` in this order:
`pins`, `verify`, `diagnostics`, `controls`, `cross`, `binding`, `profiles`.
Invoke the drivers through the bounded mise wrapper. Its outer address-space
allowance must be at least 16 GiB for the supported-core comparison;
each workload still has its own 512 MiB RSS guard.

After workers exit, `summarize.rb` verifies source/runtime/helper hashes,
unchanged schedules, exact validation signatures, raw-sample arithmetic and
serial completion intervals. It produces `summary.json`, `tables.md` and
`manifest.json`, preserving parsed values when compacting JSON artifacts.
The manifest covers 74 files totaling 13,010,624 bytes. Its SHA-256 is
`350b70d851c62121b9d95e5bcd065cd5d9032900bc5b913101bcb2598ea0c903`.
