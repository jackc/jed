# Indexed-JOIN bound preparation

This explicitly requested follow-up investigates repeated structural work in
indexed JOIN probes. The preceding
[scalar-function pass](JOIN_FUNCTIONS_FOLLOWUP.md) leaves the range workload
with 34 probes, 101 comparison extractions and 354 structural comparisons
per query. Those counts include planning and are not CPU-time attribution.

The Ruby core remains experimental, frozen and opt-in. Normal tests,
benchmarks, CI and future feature updates do not include it.

## Implementation

The candidate adds a private execution copy of a selected ordinary primary
or secondary JOIN access. Known exact-Array outer inputs of zero or one row
skip setup. Other inputs keep the first probe unchanged; the second probe
attempts bounded admission, then lazily retains comparison lists at their
existing extraction sites.

The scan loops continue to evaluate, normalize and encode current bound
values. Empty-bound checks retain their complete comparison lists, including
duplicate equalities and contradictory ranges; the planner's
equality-truncated access parts are not a substitute. Scan comparisons use
the complete scan predicate separately. Partial-index implication,
contradiction checks, row enumeration, charges and errors remain in their
existing paths.

The structure admits up to 32 index parts/keys and a 192-node expression walk
of depth at most 32, under a 4,096-unit descriptor allowance. It covers plain
integer, boolean, text and decimal types. Unusual metadata, unsupported
expression shapes, interval accesses and GiST/GIN accesses keep their
ordinary behavior. Rejected admission is not retried on subsequent probes.
Cached list containers and synthetic primary-key expressions are frozen.

Retained physical plans receive no execution state. The private structure
holds expression references and descriptors, never bound values, rows or
encoded keys, and ends with its JOIN execution. No new SQL semantics, host
API, dependency or supported-core change is introduced.

## Evidence and method

The baseline is pushed commit
`25a1a7cce500266ac6f665ea920496e746382102`, library SHA-256
`405f5536969b71c80fa30bd45d962bda33af642b0185039d76e6443005c35336`,
archived at `/tmp/jed-join-bounds-before/impl/ruby-native`.
Final library SHA-256:
`66afc525fcc2f4d0290ef70d03cc2f95e7521bd467029ecc653fe7b418f55341`.
New evidence belongs in
[`join-bounds-followup`](../../bench/ruby-native/results/join-bounds-followup/).
Earlier reports and measurements remain unchanged.

The existing [13-workload plan](../../bench/ruby-native/compare/join-functions-plan.json)
keeps its SQL, 1,000-row fixture, parameter cycles, warmups, iteration counts
and five samples per process. Its repeated range probes are the primary
target; singleton, empty and other query shapes expose setup overhead.
Secondary and composite index behavior receives separate untimed coverage.

Each runtime runs serial before/final/final/before processes across prepared
cursors, ordinary query cursors and eager execute. All results are drained.
Paired times average two process medians per source; raw samples, process
ranges and every slower observation remain available. Range separation is
descriptive, not a statistical significance test.

Interpreter and YJIT use Ruby 4.0.7; ZJIT uses Ruby 4.1.0-preview1. Comparing
the JITs also changes Ruby version. The library does not enable a JIT.
Fresh supported-core and actual Ruby Rust-binding observations use different
public APIs and collection paths and are separate from source-change effects.
Exact wrapper preflights precede timing. Profiles run last using reused
`execute_prepared` statements; the timed eager lane repeats `execute(SQL)`.

Workloads run serially under a 512 MiB sampled RSS guard. Focused checks have
1 GiB of address space; host, corpus and Ruby measurement workers have 2 GiB.
Supported-core comparisons retain their established 16 GiB virtual reserve.
These process guardrails are not whole-engine memory limits.

## Correctness

The new focused host test observes actual structural reuse and compares
enabled/disabled execution across all four APIs in memory and file modes.
It covers primary, secondary, composite, partial and expression indexes;
NULL, contradictory and duplicate bounds; width and decimal normalization;
deferred source-read faults; exact statement/lifetime cost prefixes; nested
probes; suspended snapshots; finite query-memory allowances; admission and
descriptor bounds; fallback and ownership release.

The ordinary file LEFT fixture uses the regular indexed executor. A separate
DISTINCT query positively exercises `spill_index_join` and fewer structural
extractions, while retaining exact outcomes and costs. Entering that executor
does not establish that scratch bytes were written. Development fixture
corrections fixed a private-method invocation and the earlier assumption that
ordinary file LEFT necessarily used the spill executor; no core change was
needed.

The baseline/final YJIT diagnostic smokes passed before formal correctness.
One earlier baseline attempt failed in diagnostic cleanup because a local
was initialized only inside a nested block. The corrected pair found no
masked earlier error and produced identical complete semantic payloads.
The exact failed helper and bounded report remain under `smoke-fixture/`;
successful smokes are under `harness-smoke/`. No core or focused-test changes
were required.

All six focused suites passed under the interpreter, YJIT and ZJIT at the
frozen source revision. Their largest sampled RSS was 87.2 MiB.
All 70 host-test files passed; the complete host worker peaked at 314.3 MiB.

All four complete corpus lanes passed **77,154/77,154 records**:

| Storage / API | Runtime | Passed records |
| --- | --- | ---: |
| Memory / query cursor | ZJIT | 20,297 |
| Memory / execute collector | ZJIT | 20,297 |
| File / query cursor | YJIT | 18,280 |
| File / execute collector | YJIT | 18,280 |

There are no failures, unsupported records, timeouts, memory-limit exits or
additional skips. Existing authored storage-mode skips remain unchanged.
The largest corpus worker used 191.3 MiB sampled RSS. Corpus lanes force
immediate compiler admission; focused and direct diagnostics exercise the
normal cache policy. All formal correctness workers exited before formal
measurement work began.

All six formal before/final runtime diagnostics passed. Each covers 56
workload/API/storage groups, including 32 repeated-probe groups and 24
singleton controls, plus exact statement/lifetime budget prefixes and 40
write/statistics/index/EXPLAIN observations. Complete semantic payloads and
physical-plan signatures match across sources and runtimes.

Each final diagnostic traces 9,712 actual saved-list hits and a reduction
from 17,893 to 8,181 original comparison extractions. Its largest observed
structure holds 24 units. Singleton controls perform no preparation.
The separate repeated-secondary and composite fixtures establish reuse in
both storage modes; they are not additional timed workloads.

## Results

**Decision: retain this change for repeated indexed probes.** All nine range
workload/API/runtime cells have both final process medians below both
baseline medians. This is a modest, specific gain, not a general throughput
improvement.

| Runtime | Prepared cursor time, before → final | Prepared elapsed reduction | Ordinary query reduction | Eager execute reduction |
| --- | ---: | ---: | ---: | ---: |
| Interpreter | 2,872.126 → 2,643.521 µs | 7.96% | 8.89% | 9.06% |
| YJIT | 1,419.999 → 1,342.641 µs | 5.45% | 6.81% | 5.44% |
| ZJIT | 1,649.510 → 1,472.874 µs | 10.71% | 4.23% | 6.44% |

The retained YJIT range profile records:

| Observation per query | Before | Final |
| --- | ---: | ---: |
| Allocated objects | 10,559.9 | 9,453.1 |
| Original comparison extractions | 101 | 38.333 |
| Structural key comparisons | 354 | 103.333 |
| Indexed probes | 34 | 34 |

Allocations fall **10.48%**. Call counts average three parameter cases and
can be fractional; they include planning and are not CPU-time attribution.
All twelve other workloads retain identical allocation medians and complete
call maps. The new dispatch branches and method signatures can still affect
execution and JIT behavior; unchanged call maps do not establish that those
workloads are immune to timing changes.

The [complete 117-cell tables](../../bench/ruby-native/results/join-bounds-followup/tables.md)
retain every observation: 28 cells have separated faster process medians,
20 have separated slower medians, and 69 overlap. Sixty-one paired means are
slower. These counts do not estimate the fraction of all SQL improved.

The largest slowdown is ZJIT eager integer outputs, **157.147 → 177.484 µs,
12.941%**, with separated medians. ZJIT ordinary-query computed-key is
9.736% slower with separated medians. The interpreter's largest slowdown is
prepared singleton indexed JOIN, 4.893%; YJIT's is prepared computed-key,
2.089%, both with separated medians. Interpreter controls trend slower
broadly, while YJIT controls are mixed. This run does not establish the cause
of those changes, and no replacement measurements discard them.

Fresh supported-core measurements place the range query at:

| Core / public path | Time per query | Ruby YJIT relative time |
| --- | ---: | ---: |
| Ruby YJIT comparison adapter | 1,293.865 µs | 1.000× |
| Go | 79.567 µs | 16.261× |
| Rust | 58.261 µs | 22.208× |
| TypeScript | 212.986 µs | 6.075× |

The actual Ruby Rust binding takes 232.228 µs for the range query; the native
prepared, query and eager paths take 1,351.624, 1,288.086 and 1,347.465 µs.
Native prepared is **5.820×** the wrapper time. These are separate fresh
public-API observations, not equivalent measurements of just the engine.
The range workload still exceeds 10× Go, and other multi-table shapes remain
much farther away. The experiment is not promoted.

Formal binding/preflight workers peaked at 96.6 MiB sampled RSS, direct
diagnostics at 73.9 MiB and profiles at 71.0 MiB. The fresh Ruby cross-core
worker used 69.7 MiB; the largest supported-core worker was TypeScript at
189.0 MiB. All guards passed.

The verifier checked 24 serial commands, exact semantic signatures,
source/runtime/helper hashes, every paired calculation and lossless JSON
compaction. The manifest covers 82 entries totaling 22,218,171 bytes, including
the preserved smoke correction and pinned helper references. Manifest SHA-256:
`14587c2669affa123d042d07ce77c40b48e8c5e627c7de6d40e569efcb773c87`.

## Remaining opportunity

Fresh N-way planning still allocates candidate state vectors before testing
whether an existing state dominates them. The final range allocation trace
retains 234 arrays at that construction site. A next investigation could count
candidates that an existing frontier state strictly dominates before constructing
those vectors, while preserving all estimates, generation order, ties and
outer-join fences.
The allocation count does not establish a meaningful time saving or the
number of candidates that can be skipped; this remains an unmeasured candidate.

## Reproduction

The evidence directory contains explicit `check.rb` and `run.rb` drivers.
Invoke them through `mise run ruby-native:bounded`; every workload has its own
resource guard. Use at least 16 GiB of outer address space for supported-core
comparisons. Existing page-service host tests also need local Unix-socket
access.

Run `check.rb --phase PHASE --source-sha256 FINAL_HASH` for `focus`, `host`
and `corpus`. After correctness passes and measured source/helpers are frozen,
run `run.rb --phase PHASE --final-source-sha256 FINAL_HASH` in this order:
`pins`, `verify`, `diagnostics`, `controls`, `cross`, `binding`, `profiles`.
The drivers refuse to replace existing reports; preserve earlier artifacts
before an intentional rerun.

After every worker exits, `summarize.rb` verifies source/runtime/helper hashes,
schedules, exact semantic signatures, raw-sample arithmetic and serial
command intervals. It produces the summary, tables and manifest while
preserving parsed values when compacting JSON evidence.
