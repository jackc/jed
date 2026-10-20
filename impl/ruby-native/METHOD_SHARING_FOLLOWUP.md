# Sharing compiled Ruby methods

This follow-up starts at `fc12f427`, after the complete cursor correctness and
simple literal LIKE audits. Ruby remains a frozen, explicitly selected experiment.
No dependency, public API or normal test/benchmark/CI task changes.

## Implementation

The five compiler families now share generated classes through a process-wide
64-entry LRU, keyed by concrete kernel superclass and exact Ruby source. Each
materialization still constructs a fresh frozen kernel referencing its Program's
immutable pool; the method cache retains neither. Thus
`id + 1` and `id + 2` can use the same generated method without sharing constants,
bindings, execution context or result ownership. Ordering Procs remain bound to
their individual kernels. Existing second-use admission and per-plan/kernel
limits remain in place.

Cache access and compilation use a mutex; query execution does not. A child
process replaces the inherited cache and mutex before new materialization.
Already-live inherited kernels remain valid. Concurrent first child admissions
can duplicate an initial compilation before converging, without mixing data.
The cache owns only frozen source and generated classes, never pools or engines.
It admits at most 64 × 32 KiB of source; this does not bound Ruby's retained
machine code or the classes kept alive by existing kernels.

## Evidence

Baseline library SHA-256:
`3e749562c6b764681d6917604cf02aa73b3194cfdbb8c5e756616588de91dc88`.
Final library SHA-256:
`fec686e7484eda683c1692576175671b6c11fb5accaca78b6ccb71466f0ee70a`.
Commands, runtime identities, source hashes, raw samples and bounds are retained
in [method-sharing-followup](../../bench/ruby-native/results/method-sharing-followup/).

Whole-query controls retain the existing 1,000-row fixture and schedules, with
five samples per process and source ABBA order for interpreter, YJIT and ZJIT.
Prepared queries, direct cursor queries and eager execution are checked and
measured separately. Results are fully consumed and exact values, metadata and
costs are checked before timing. GC remains enabled.

Direct diagnostics distinguish equal source with different pools from genuinely
different sources. Compile/first-call latency, warm materialization, allocations,
JIT method identity and cache churn are separate observations from steady query
latency. The process bounds remain 512 MiB RSS and 2 GiB address space.

## Results

Sharing improves repeated method creation and reuse of JIT code. It does not
produce a broad steady-state query speedup.

The following ranges span seven consumers across all five compiler families.
Each entry is a median of 30 observations, in microseconds. Materialization
reuses an already-emitted Program; complete builds include emission and method
materialization for a new constant pool, with parsing/resolution outside timing.
These direct before/final processes are separate observations, not ABBA estimates.

| Runtime | Materialization before → final, µs | Complete new-pool build before → final, µs | Build speedups |
| --- | ---: | ---: | ---: |
| Interpreter 4.0.7 | 16.48–43.00 → 0.81–1.09 | 27.20–86.22 → 8.03–26.08 | 2.68–3.52× |
| YJIT 4.0.7 | 16.13–53.95 → 0.78–0.89 | 38.80–115.70 → 5.63–22.61 | 3.44–7.12× |
| ZJIT 4.1 preview | 11.93–30.23 → 0.73–1.05 | 48.34–96.01 → 7.29–35.03 | 2.30–7.36× |

Every new-pool build improves in these 21 direct comparisons. All seven donor
methods have direct JIT evidence in each JIT runtime before recipient creation.
New recipients have identical classes, method owners and UnboundMethods, with
separate kernels/pools. They therefore reuse the donor's compiled method; the
baseline creates and compiles separate methods. Ordering has no value pool and
retains a separate comparator Proc for each kernel. Cold first-build/first-call
observations, warmup trajectories, allocations and individual samples remain in
the diagnostic artifacts.

The seven whole-query controls vary in elapsed time by −4.13% to +4.76% in the
interpreter, −1.93% to +8.10% under YJIT, and −14.59% to +4.40% under ZJIT.
Four comparisons have both final process medians above both baseline medians:
interpreter prepared PK (+4.76%), YJIT direct-query many-groups (+4.59%), YJIT
direct-query prefix LIKE (+5.10%), and ZJIT prepared prefix LIKE (+4.40%). YJIT
prepared arithmetic projection averages +8.10% but its process medians overlap
the baseline range. These costs remain part of the result; compilation savings
do not establish neutrality for every warmed query.

An isolated YJIT source ABBA checks scalar, arithmetic projection and prefix LIKE
without the other four query shapes. Elapsed-time changes are:

| Isolated query | Prepared | Direct query | Eager |
| --- | ---: | ---: | ---: |
| Scalar | +0.33% | −3.23% | +1.00% |
| Arithmetic projection | +7.79% | +10.86% | −5.63% |
| Prefix LIKE | +2.04% | −3.01% | +2.05% |

The direct-query arithmetic slowdown is separated: 811.63 → 899.81 µs, with
both final medians above both baselines. The direct-query scalar and prefix LIKE
improvements are also separated; the other ranges overlap. This follow-up does
not erase the original results or establish why JIT/workload context changes
them. The benefit of sharing is reduced method creation and duplicate JIT code;
the measured warmed-query regressions are a tradeoff that the broader checkpoint
must keep visible.

## Code retention

The churn diagnostics use actual second-use admission and the existing 64-entry
kernel-instance LRU: 256 distinct pools with one generated source, or 256 distinct
generated sources. Every variant is checked against the ordinary evaluator.
Revisiting an evicted pool creates a fresh kernel, but reuses the method when its
source remains cached. After the owning thread exits, all three GC observations
report zero live kernel instances in every runtime and shape.

For one source with 256 pools, the final process cache retains one class. The
following process-wide code-region observations include helper methods as well
as generated query code:

| Runtime | Classes after GC, before → final | JIT code region, before → final |
| --- | ---: | ---: |
| Interpreter | 0 → 1 | — |
| YJIT | 16 → 1 | 3.89 → 0.79 MiB |
| ZJIT | 256 → 1 | 3.91 → 0.83 MiB |

ZJIT's total reported JIT memory falls from 7.18 to 1.86 MiB in this pool-diversity
probe. Different query shapes still need different methods: with 256 distinct
sources, the final cache retains 64 classes, while total live anonymous classes
after GC are 64 / 85 / 257 for interpreter / YJIT / ZJIT. YJIT's code region is
12.92 → 12.95 MiB and ZJIT's is 39.68 → 39.70 MiB. This optimization removes
duplicate method work; it does not bound Ruby's retention of distinct JIT code.

## Correctness

All four full-corpus lanes pass with immediate compiler admission and uniform
public API use. Cursor results are drained externally; there is no failure
allowlist, mixed API routing, changed expected result or new corpus exclusion.

| Corpus lane | Records passed | Failures |
| --- | ---: | ---: |
| Memory, ZJIT, execute | 20,267 | 0 |
| Memory, ZJIT, query | 20,267 | 0 |
| Disk, YJIT, execute | 18,250 | 0 |
| Disk, YJIT, query | 18,250 | 0 |

Every lane visits all 679 corpus files. Existing authored mode exclusions remain
2 files for memory and 77 for disk. No worker timed out or hit a memory limit.
The highest corpus worker RSS was 174.76 MiB, under the 512 MiB guard.

All 61 host test files pass in the interpreter, peaking at 196.66 MiB. The new
sharing tests also pass separately under the interpreter and YJIT. They cover
all compiler families and consumer modes, isolated pools and ordering Procs,
exact statement/lifetime error prefixes, deferred reads, memory ownership,
mutable source inputs, cache limits/eviction, concurrent admission, fork safety,
weak-reference lifetime checks, second-use admission, and warmed memory/file
APIs with schema, privilege and publication invalidation.

## Reproduction

Restore `impl/ruby-native` from baseline commit `fc12f427` in a separate directory;
the measurement runner defaults to `/tmp/jed-method-sharing-before/impl/ruby-native`
and accepts `--baseline-source PATH`. Select `controls` and `diagnostics` explicitly
with `bench/ruby-native/results/method-sharing-followup/run.rb --phase PHASE
--final-source-sha256 HASH`, using the final hash above. Its child measurements
use the opt-in mise tasks and per-worker limits. Run the two phases serially;
do not overlap them with tests or other performance work.

The separate `run_audits.rb` requires both source and conformance-harness hashes
and accepts `--mode memory|disk --api execute|query`. The harness hash for this
revision is `9935e8bfaf7d1780b050a77f652291d3e2c1c1ac74669ea35e29d1852ddb53ab`.
`correctness_summary.rb` independently checks the four completed audits, focused
tests and host suite against the current source, corpus, runner and test hashes.
