# Scalar planner work estimation experiment

This explicitly requested optimization follows the
[multiway LEFT-join pass](OUTER_JOIN_FOLLOWUP.md). The Ruby core remains
experimental, frozen and opt-in. Normal tests, benchmarks, CI and future
feature work do not include it.

**Decision: reject this candidate.** It reduces estimator dispatch and allocations
but does not establish a useful JIT throughput gain. The measured candidate and
its evidence are retained in archival commit
`d8084fb6e44bbf3f825bf7ff61d1bdcb16e3451a` for reproduction. The original
estimator is restored, with library hash identical to the baseline below.
The candidate-specific test lives in that archival commit. The following
implementation and measurements describe the candidate, not a retained optimization.

## Measured implementation

The existing stack-guarded `expression_work` method now counts ordinary scalar
nodes directly instead of dispatching each node through five additional
estimator layers. Literal/column/parameter/outer/source/star nodes contribute
zero own units; unary/binary/cast/numeric-context nodes contribute one.
All arguments still contribute, including arguments on zero-unit leaves and
duplicate occurrences of the same child.

The fast path accepts exact `Expression` instances with nil or plain-array
arguments. It applies the original correlated-subplan metadata surcharge after
visiting children, retaining metadata lookup order and live cost reads.
Other kinds, subclasses and custom argument containers use the existing
estimator chain. Special descendants reach that chain at their original position. Host-function costs,
parameter-sensitive quantified arrays, membership state, correlated subplan
costs and aggregate short-circuit behavior retain their existing handling.
The existing depth counter, twelve-level Fiber slicing and exception cleanup
remain in place.

There is no prewalk, new cache or retained estimate. Every call observes the
current tree. The previous profile's 93 LEFT and 231 range work-estimation
calls included recursive child visits, so those counts supported reducing
dispatch without establishing a useful identity-cache hit rate. No SQL
behavior, host API, physical-plan policy or dependency changes.

## Evidence and method

The baseline is pushed commit
`eb2c83db21bc496ea6c28be02fa3467b8e100f94`, library SHA-256
`62e562806cb2fca0e30792f1c90c3546f9f847c94a1af4651d9b3a376fce3705`,
archived at `/tmp/jed-work-estimation-before/impl/ruby-native`.
New evidence belongs in
[`work-estimation-followup`](../../bench/ruby-native/results/work-estimation-followup/).
Earlier artifacts remain unchanged.

Final library SHA-256:
`ee1bc36689278e2bf6f7f2ce04bdc7f9fadae4429cb9ff6d20136f6bc2ebed6b`.

The unchanged nine-job [multiway plan](../../bench/ruby-native/compare/multiway-plan.json)
uses the same 1,000-row fixture, SQL, parameter sequence, warmups, iterations and
five samples per process. Each runtime runs serial before/final/final/before
processes for prepared cursors, ordinary query cursors and eager execute. All
results are drained. Paired times average the two process medians for each
source; raw samples and slower cells are retained. Process ranges are
observations, not a statistical significance test.

Interpreter and YJIT use Ruby 4.0.7; ZJIT uses Ruby 4.1.0-preview1. The ZJIT
comparison therefore also changes Ruby version. The library does not enable
a JIT. Separate fresh supported-core and actual Rust Ruby-wrapper measurements
characterize their different public APIs and collection overhead. Wrapper
preflights check exact values, names, types and costs. Allocation profiles run
last using reused `execute_prepared` statements; the timed eager lane uses
repeated `execute(SQL)`.

Direct, untimed diagnostics use a private 32-row fixture across four APIs in
memory and file storage. They compare values, metadata, error/cost prefixes,
physical plans and EXPLAIN against the original estimator, including catalog
changes. Internal probes exercise current parameter/context state, mutable
arguments and metadata, aggregate short circuits and exception recovery.
The focused test also checks metadata default-proc and subclass lookup order.
No new shared SQL behavior requires a corpus entry.

Workers run serially under a 512 MiB sampled RSS guard. Ruby workloads have at
most 2 GiB of address space; supported-core comparison workers retain their
established 16 GiB virtual reserve. The full host suite uses the same 2 GiB
virtual reserve established by the preceding
[allocator investigation](EXPRESSION_WALK_FOLLOWUP.md#evidence-and-method).
These process guardrails are not a whole-engine memory guarantee.

The first correctness supervisor exited before starting any checks because
the supplied digest used component-sorted paths instead of the canonical
lexically sorted file paths. Its report is retained as
`correctness-supervisor-wrong-pin.json`; the corrected invocation pins the
unchanged library using `NativeRubySource.digest`.

## Correctness

All **68 host-test files** passed. The focused suite passed under the
interpreter, YJIT and ZJIT. Its internal tests verify duplicate/nested child
visits, mutation between calls, special-node behavior, metadata lookup and
error order, deep Fiber slicing, and caller-depth restoration after failure.
Public-API tests compare plans, values, types and exact statement/lifetime
cost/error prefixes against the original estimator in memory and file storage.

All four complete corpus lanes passed **77,154/77,154 records**:

| Storage / API | Runtime | Passed records |
| --- | --- | ---: |
| Memory / query cursor | ZJIT | 20,297 |
| Memory / execute collector | ZJIT | 20,297 |
| File / query cursor | YJIT | 18,280 |
| File / execute collector | YJIT | 18,280 |

There are no failures, unsupported records, timeouts, memory-limit exits or
additional skips. Existing authored storage-mode skips remain unchanged.
The largest corpus worker used 179.7 MiB sampled RSS; the full host suite
peaked at 372.4 MiB. All correctness workers exited before measurements began.

## Results

The prepared targets show modest interpreter improvements and essentially flat
JIT timings:

| Runtime | Range before → candidate (µs) | LEFT before → candidate (µs) |
| --- | ---: | ---: |
| Interpreter | 2,928.385 → 2,816.162 | 762.104 → 742.353 |
| YJIT | 1,387.001 → 1,384.585 | 445.700 → 445.401 |
| ZJIT | 1,621.549 → 1,624.761 | 460.181 → 464.548 |

The interpreter's 27 paired means all improve by 0.6–5.1%, including unaffected
scalar and PK controls. That makes attribution uncertain. YJIT prepared range
improves 0.174% and LEFT 0.067%; both process ranges overlap. ZJIT prepared
range and LEFT are 0.198% and 0.949% slower, also overlapping. ZJIT eager LEFT
is **4.069% slower**, with both final medians above both original medians.

All 81 cells remain in the [complete tables](../../bench/ruby-native/results/work-estimation-followup/tables.md):
20 have separated faster process medians, six have separated slower medians,
and 55 overlap. Thirty paired means are slower. The largest regression is
ZJIT prepared computed-key: **251.731 → 303.223 µs, +20.455%**. Other
separated regressions include ZJIT ordinary indexed join (+6.211%), ZJIT
ordinary PK (+3.39%), YJIT prepared four-table join (+3.498%) and YJIT
ordinary three-table miss (+1.18%). No replacement runs discard these results.

The profile establishes a small deterministic reduction in work:

| Workload | Objects/query before → candidate | Reduction | Base estimator calls |
| --- | ---: | ---: | ---: |
| Range | 10,558.3 → 10,410.3 | 148 / 1.402% | 231 → 1 |
| LEFT | 2,196.8 → 2,136.8 | 60 / 2.731% | 93 → 0 |

The stack-guarded entry still runs 231/93 times. All five lower layers have
the same call-count reduction as the base estimator. The other seven jobs
have identical allocation counts and complete call maps between sources,
including computed-key, scalar and PK, which make no warmed work-estimator
calls. Those facts do not explain the elapsed regressions; a YJIT call profile
also cannot diagnose ZJIT timing. Call counts are not CPU-time percentages.

The six formal source/runtime diagnostic reports agree on all 72 API/storage
groups and 26 internal helper cases, including exact outcomes, costs and plans.
Both sources passed the actual Rust Ruby-wrapper preflight.

The fresh supported-core adapter measures candidate Ruby YJIT LEFT at
418.531 µs versus Go 3.206 µs, Rust 3.116 µs and TypeScript 10.638 µs:
130.561× Go and 134.328× Rust. In the separate actual Ruby-package comparison,
native prepared LEFT is 440.392 µs versus the Rust binding's 51.567 µs,
or 8.540× its elapsed time. These public APIs and collection paths differ;
their ratios are separate observations, not controlled source-change effects.

All 24 formal commands completed within their guards. The reduction of
1.4–2.7% in allocations, without a clear JIT throughput gain, is insufficient
to justify duplicating estimator logic. The original implementation is the
baseline for the next candidate.

## Next candidate

Compile complete JOIN ON predicates at their existing evaluation sites using
the bounded scalar compiler. That could reduce per-candidate interpretation
while keeping candidate generation, row assembly and output accounting intact.
The previous range profile recorded 460 evaluator calls per query versus 27
for LEFT. Those totals are not CPU fractions, and a focused trace must establish
how many belong to the join predicates before estimating a benefit.

Each complete predicate would need independent admission and whole-predicate
fallback, preserving NULL rejection and the existing short circuit between
ON trees without introducing an extra AND charge. Candidate/read order, exact
cost and lifetime prefixes, unmatched outer rows, LIMIT behavior and file spill
replay must remain unchanged. Retained kernels must not capture rows, engine
owners, parameter values or physical winners. This is a candidate for another
pass, not an implemented change here.

## Reproduction

The evidence directory contains `check.rb` for focused, host and complete-corpus
checks, and `run.rb` for measurements. Both use explicit
`mise run ruby-native:*` tasks, run serially and refuse to replace existing
reports. Check out archival commit `d8084fb6e44bbf3f825bf7ff61d1bdcb16e3451a` to reproduce this
rejected implementation; the later branch restores the original estimator.
Preserve earlier artifacts before an intentional rerun.

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
The manifest covers 73 files totaling 11,591,466 bytes. Its SHA-256 is
`3713db223c99e41c8f771465b7f9470bdd10f430097db0d6e59546e079ab71c4`.
