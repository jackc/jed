# Compiled JOIN predicates

This explicitly requested optimization follows the
[rejected scalar-estimation candidate](WORK_ESTIMATION_FOLLOWUP.md).
The Ruby core remains experimental, frozen and opt-in. Normal tests,
benchmarks, CI and future feature updates do not include it.

## Implementation

The general executor can use the existing bounded scalar compiler for complete
JOIN ON predicates and join-window filters. Each ON tree remains independent,
preserving its original short-circuit position without adding an AND cost.
The file executor uses the same compiler at its existing ordinary and indexed
spill-join predicate calls. Candidate generation, row construction, matching,
NULL extension, output accounting and spill replay remain in the existing loops.

Every acquisition emits the current resolved expression into source and an
immutable data pool. A 64-entry LRU reuses kernels only when both are equal.
The second encounter materializes a kernel; the explicit immediate-compilation
test mode can materialize it on the first encounter. No expression-identity
shortcut is used. The cache owns no rows, parameters, physical winner or execution
owner. The scalar compiler's existing node, depth, source and pool limits apply.
Unsupported complete predicates use the ordinary evaluator.

The general executor skips setup for a known small input: fewer than
eight outer rows for an indexed join, or a product below eight for two exact
Array inputs in a nonindexed join. This avoids emission and ON-program-pair
allocations on singleton joins. Unknown containers and lateral sources retain
ordinary admission without extra length reads; explicit immediate mode bypasses
this performance heuristic. The spill paths retain ordinary kernel admission.

No SQL behavior, host API, dependency or supported-core changes.

## Evidence and method

The baseline is pushed commit
`1657722bc9a82d990b37e1e22a3f54843f35230e`, library SHA-256
`62e562806cb2fca0e30792f1c90c3546f9f847c94a1af4651d9b3a376fce3705`,
archived at `/tmp/jed-join-predicate-before/impl/ruby-native`.
It restores the estimator from the preceding retained LEFT-join optimization.
New evidence belongs in
[`join-predicate-followup`](../../bench/ruby-native/results/join-predicate-followup/);
earlier evidence remains unchanged.

Final library SHA-256:
`f980101e855c807c25c6a7cdb5e55bd0c5b53a1ee5e3f01f918aff843dbcf13f`.

The separate [JOIN-predicate plan](../../bench/ruby-native/compare/join-predicate-plan.json)
preserves the original nine workload objects and schedules, including the
1,000-row fixture and five samples per process. It adds one arithmetic residual:

```sql
SELECT a.id,b.id
FROM items a JOIN items b
  ON b.bucket=a.bucket AND abs(b.score-a.score)<50
WHERE a.id<$1 AND b.id<$1
ORDER BY a.id,b.id
```

The new case uses 100 warmup queries, 20 measured queries per sample and the
fixed parameter cycle `10 + (i * 17) % 91`. It exercises more scalar work per
candidate than the existing equality joins. The original plan file is unchanged.
The verifier checks the first nine entries against it.

Each runtime runs serial before/final/final/before processes for prepared
cursors, ordinary query cursors and eager execute. All results are drained.
Paired times average two process medians per source. Raw samples, process
ranges and all slower cells remain in the evidence; ranges are observations,
not a statistical significance test.

Interpreter and YJIT use Ruby 4.0.7; ZJIT uses Ruby 4.1.0-preview1. The ZJIT
comparison also changes Ruby version. The library does not enable a JIT.
Fresh supported-core and actual Ruby Rust-binding runs use different public
APIs and collection paths and are reported separately from source-change effects.
Exact wrapper preflights precede timing. Profiles run last with reused
`execute_prepared` statements; the timed eager lane uses repeated `execute(SQL)`.

Workers run serially under a 512 MiB sampled RSS guard. Ruby workloads have
at most 2 GiB of address space; supported-core comparison workers retain their
established 16 GiB virtual reserve. The full host suite uses the previously
established 2 GiB allocator reserve with the RSS guard unchanged. These process
guardrails are not a whole-engine memory guarantee.

## Correctness

All **68 host-test files** passed. The focused suite passed under the
interpreter, YJIT and ZJIT. It checks NULL/filter short-circuit behavior,
independent ON trees, hash rechecks, deferred column faults, every small
statement/lifetime cost prefix, the seven/eight-candidate boundary, mutable
expressions, bounded cache ownership, snapshots and spill paths.

All four complete corpus lanes passed **77,154/77,154 records**:

| Storage / API | Runtime | Passed records |
| --- | --- | ---: |
| Memory / query cursor | ZJIT | 20,297 |
| Memory / execute collector | ZJIT | 20,297 |
| File / query cursor | YJIT | 18,280 |
| File / execute collector | YJIT | 18,280 |

There are no failures, unsupported records, timeouts, memory-limit exits or
additional skips. Existing authored storage-mode skips remain unchanged.
The largest corpus worker used 184.5 MiB sampled RSS; the full host suite
peaked at 244.0 MiB. All correctness workers exited before measurements began.

Separate direct diagnostics use a private 32-row fixture across four APIs and
both storage modes. They compare complete outcomes, error/cost prefixes,
metadata, physical plans and EXPLAIN against the original evaluator, and trace
calls to kernels actually returned by the JOIN helper. Small-input budget
cases can use fallback; explicit helper-prefix probes and the focused
immediate-compilation tests cover actual compiled cost boundaries.

The direct file cases are file-backed but do not establish actual spilling:
their observed spill-method and scratch-append counts are zero. Focused tests
explicitly exercise the spill joins and scratch storage. Cache probes verify
the source/pool limits, 64-entry bound and release of execution objects; they
do not claim that Ruby releases all JIT code immediately.

The pre-pin diagnostic smoke initially omitted a dummy parameter slot before
resolving `$1` in a standalone helper. Its failure and script are preserved
under `smoke-fixture/`. Correcting the fixture passed the smoke without changing
the core, tests or correctness checker.

## Results

**Decision: retain this candidate for its arithmetic-residual benefit.**
All nine runtime/API comparisons of that workload have both final process
medians below both original medians. This is a workload-specific improvement,
with measured setup costs on simpler predicates:

| Runtime | Prepared before → final (µs) | Prepared speedup | Query speedup | Execute speedup |
| --- | ---: | ---: | ---: | ---: |
| Interpreter | 5,302.435 → 3,903.480 | 1.358× | 1.345× | 1.315× |
| YJIT | 2,280.515 → 1,910.134 | 1.194× | 1.160× | 1.135× |
| ZJIT | 3,631.461 → 2,447.512 | 1.484× | 1.434× | 1.485× |

The complete [90-cell tables](../../bench/ruby-native/results/join-predicate-followup/tables.md)
retain every observation: 17 cells have separated faster process medians,
33 have separated slower medians and 40 overlap. Fifty-five paired means are
slower. These counts include controls that do not execute the new kernels;
they are not an estimate of the fraction of queries this change accelerates.

YJIT range queries slow by **1.971%, 3.743% and 5.513%** for prepared,
ordinary query and eager execute respectively. Computed-key queries slow by
**5.504%, 8.740% and 10.356%**. Both workloads actually execute new kernels,
so they are not unchanged controls. Their small evaluator savings plausibly
fail to repay fresh emission and cache lookup. The range case uses two
three-node integer equality programs; computed-key uses a five-node program.
The arithmetic residual uses a ten-node program for many more candidates.

Range is mixed on the other runtimes: interpreter prepared is 0.236% slower
with overlapping ranges, ordinary query is 2.411% faster, and execute is
3.004% slower. ZJIT prepared is 3.088% faster, ordinary query is 0.494% slower
with overlapping ranges, and execute is 3.229% slower.

The largest observed regression is ZJIT prepared three-table equality join:
**215.503 → 286.530 µs, +32.959%**, with separated process medians.
Scalar, PK, indexed join, three-table hit/miss, four-table join and LEFT have
identical complete call maps and allocation counts in the before/final YJIT
profiles. Those facts do not explain their elapsed movements and do not
establish a cause for the ZJIT regression. No replacement runs discard it.
A YJIT call profile cannot diagnose ZJIT's generated code or CPU time.

The profiles show the specific work removed and the allocation tradeoff:

| Workload | Objects/query before → final | Fast evaluator calls before → final | Integer operation calls before → final |
| --- | ---: | ---: | ---: |
| Range | 10,558.2 → 10,651.1 | 460 → 358 | 85 → 51 |
| Computed key | 887 → 999 | 99 → 49 | 33 → 13 |
| Arithmetic residual | 15,592.2 → 15,784.2 | 4,520.7 → 272 | 1,163 → 54 |

The residual adds 192 setup objects/query (1.231%) while removing thousands
of interpreted dispatches. Four generic evaluator wrapper layers each lose
about 921.7 calls/query. Call counts are diagnostic observations, not
CPU-time percentages. The seven other workloads' allocation counts remain
identical, including LEFT at 2,196.8 objects/query; its small-input gate avoids
JOIN compilation setup.

All six formal direct diagnostic reports agree on all 80 API/storage groups,
complete internal outcomes and exact cost/error prefixes. Every final runtime
executes acquired JOIN kernels in the target cases; baseline and disabled
controls execute none. Cache churn admits 96 distinct pools, retains at most
64 kernels and releases the observed execution objects after their owning
thread exits. Both source revisions passed the actual Ruby Rust-binding
preflight for all ten workloads.

The fresh supported-core run measures arithmetic residual at **1,757.319 µs**
for Ruby YJIT, **393.886 µs** for Go, **768.032 µs** for Rust and
**1,633.398 µs** for TypeScript: Ruby takes **4.461× Go**, **2.288× Rust**
and **1.076× TypeScript** time. This puts this query within the desired
10× Go range. It does not establish universal parity: the existing multiway
point and LEFT joins retain much larger gaps.

In the separate actual Ruby-package comparison, prepared residual takes
**1,835.281 µs** versus the Rust binding's **854.054 µs**, or **2.149×**
its time. Native query and execute take 1,915.503 and 1,943.596 µs.
The same run has native prepared scalar and PK queries faster than the
binding (4.830 versus 7.791 µs and 3.946 versus 10.308 µs). These are
separate public-API and collection-path observations, not controlled
source-change effects.

All 24 formal commands completed serially within their guards. Native/binding
measurement workers peaked at 93.8 MiB sampled RSS; direct diagnostics at
72.0 MiB and profiles at 69.7 MiB. The largest supported-core worker was
TypeScript at 245.2 MiB.

## Remaining opportunities

A separate admission refinement should test avoiding fresh emission for cheap
integer leaf comparisons and small computed-predicate workloads. Any heuristic
needs its own measurements; the current eight-candidate gate does not guarantee
that compilation setup pays for itself. The frozen results above describe the
retained implementation without a later threshold being substituted into them.

Another candidate is execution-local preparation of selected indexed-JOIN
bound structure. The final range profile still makes 34 indexed probes,
101 `key_comparisons` calls and 354 `same_key_expression?` calls/query.
Preparing ordered comparison descriptors once per JOIN execution could avoid
repeated predicate matching while leaving every bound evaluation, normalization,
encoding and charge at its original position.

The selected access descriptor's existing comparison list is not sufficient:
it can omit range checks when equality exists. A future implementation must
preserve the complete ordered comparisons, first-equality choice, duplicate
reads, NULL/contradiction behavior and exact failure prefixes. No bound values,
encoded keys, outer rows or physical winners should survive an execution.
This opportunity is unmeasured and is not implemented by this change.

## Reproduction

The evidence directory contains `check.rb` for correctness and `run.rb` for
measurements. Both use explicit `mise run ruby-native:*` tasks, run serially
and refuse to replace existing reports. Preserve earlier artifacts before an
intentional rerun. Reconstruct the baseline from commit `1657722bc9a82d990b37e1e22a3f54843f35230e`;
the runner verifies its library hash before launching workers.

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
The manifest covers 76 files totaling 16,044,281 bytes. Its SHA-256 is
`a632f324cbc834d5e4cf2486e164e92047cc9b8c4a6622e1aebe2c96743ee9fa`.
