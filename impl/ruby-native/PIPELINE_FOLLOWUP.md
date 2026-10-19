# Compiled single-table Ruby pipelines

Measured 2026-10-19. This explicitly requested update extends the frozen Ruby
experiment's runtime compiler from expression methods into single-table row
loops. No dependency, public API, supported-core implementation, or default task
selection changes.

Historical report: the subsequent [correctness follow-up](CORRECTNESS_FOLLOWUP.md)
fixes the metadata, stack-depth, correlated-cost, and multi-page LIMIT defects
identified here. These measurements and original failure reports remain intact;
the follow-up separately records corrected costs and API-specific memory audits.

## Result and scope

The implementation fuses predicate evaluation and projection into the existing
scan loop. The benefit after expression compilation is modest: focused stable-YJIT
controls show approximately 3–13% faster eager scans and 1–6% faster ordinary lazy
queries. This is an additional improvement over the previously committed compiler,
not another multiplication by its earlier expression speedup.

Storage access, cursor delivery, cost accounting, and result ownership remain
substantial work. Generated loops do not make the complete query one compiled
method: they stop at the existing scanner and output-sink interfaces. Aggregation,
joins, blocking sorts, DISTINCT, host calls, profiling, and finite query-memory
accounting retain their existing execution. Eligible ordered scans preserve
streaming OFFSET/LIMIT; scalar and exact-point paths retain their specialized
implementations.

## Alternating controls

The initial full workload sequence had small gains, mixed results, and an apparent
CASE regression. To distinguish fusion from unrelated source changes and runtime
history, four fresh-process runs use the **same final source** with only the
internal pipeline switch changed: disabled, enabled, enabled, disabled. The
expression compiler stays enabled in every run. Each job retains five timing
samples and its full warmup/iteration schedule. GC remains enabled.

The table reports the mean of the two per-run medians, in microseconds per complete
query. It is a small alternating experiment, not a confidence interval.

| Query | Eager disabled | Eager fused | Speedup | Ordinary lazy disabled | Ordinary lazy fused | Speedup |
|---|---:|---:|---:|---:|---:|---:|
| Scalar control | 5.26 | 5.27 | 1.00× | 5.38 | 5.39 | 1.00× |
| Arithmetic filter | 310.38 | 284.22 | 1.09× | 447.24 | 437.90 | 1.02× |
| Arithmetic projection | 527.24 | 511.60 | 1.03× | 824.53 | 810.10 | 1.02× |
| CASE projection | 488.43 | 463.67 | 1.05× | 845.78 | 800.29 | 1.06× |
| Plain scan | 250.02 | 221.34 | 1.13× | 513.38 | 483.18 | 1.06× |
| Projection/LIMIT 20 | 66.91 | 63.12 | 1.06× | 91.08 | 89.84 | 1.01× |
| Projection/OFFSET 500/LIMIT 20 | 176.89 | 167.87 | 1.05× | 228.48 | 220.35 | 1.04× |

Every eager scan's two enabled timings are faster than both disabled timings.
The CASE regression does not reproduce. Prepared results are also retained, but
their 7–9% average gains on arithmetic/CASE are inflated by a slower final disabled
run; they are not stronger evidence than the eager and ordinary-query controls.
Absolute timings differ materially from the larger workload sequence, so tables
from different sequences should not be combined as if only fusion changed.

Raw controls: [disabled A](../../bench/ruby-native/results/pipeline-followup/fusion-control-1-disabled.json),
[enabled B](../../bench/ruby-native/results/pipeline-followup/fusion-control-2-enabled.json),
[enabled B](../../bench/ruby-native/results/pipeline-followup/fusion-control-3-enabled.json),
[disabled A](../../bench/ruby-native/results/pipeline-followup/fusion-control-4-disabled.json).

## Whole-query source and cross-core comparisons

This is the original full 15-query sequence, in microseconds per prepared query.
Ruby is stable 4.0.7 YJIT; supported-core columns are contemporary after controls.
Every lane agrees on values/checksums and consumed row counts. This adapter does
not validate cross-core costs; the separate binding checks below expose the
existing LIMIT cost gap.

| Query | Ruby before | Ruby after | Go | Rust | TypeScript |
|---|---:|---:|---:|---:|---:|
| Scalar | 4.62 | 5.39 | 0.91 | 1.40 | 2.09 |
| PK hit | 4.42 | 4.19 | 1.36 | 2.26 | 4.58 |
| PK miss | 3.32 | 3.44 | 1.12 | 1.79 | 2.92 |
| Filtered scan | 253.80 | 262.69 | 162.57 | 112.25 | 305.18 |
| Aggregate | 297.51 | 302.07 | 65.02 | 52.99 | 165.11 |
| Group | 666.80 | 694.17 | 255.24 | 172.62 | 480.41 |
| Sort/LIMIT | 901.33 | 926.42 | 207.44 | 132.55 | 341.97 |
| Arithmetic filter | 442.53 | 441.12 | 211.76 | 151.26 | 404.92 |
| Arithmetic projection | 813.96 | 832.14 | 272.02 | 217.18 | 566.20 |
| CASE projection | 805.44 | 893.52 | 244.60 | 183.28 | 504.44 |
| Aggregate argument | 702.87 | 734.14 | 265.67 | 218.55 | 528.33 |
| Computed sort | 1006.05 | 1042.03 | 317.34 | 206.04 | 418.11 |
| Plain scan | 526.08 | 513.28 | 138.16 | 110.89 | 260.19 |
| Projection/LIMIT | 86.62 | 88.28 | 7.14 | 5.49 | 15.10 |
| Projection/OFFSET/LIMIT | 206.57 | 222.22 | 104.44 | 74.23 | 200.91 |

These source comparisons are mixed, including changes in unmodified scalar,
aggregate, group, and sort paths. They do not establish a broad prepared-query
speedup. The alternating controls above isolate the incremental pipeline effect
more directly and retain the initial observations instead of replacing them.
In particular, generating the loop does not remove the large setup cost of the
small LIMIT query or establish Go/Rust parity.

Matching per-runtime before → after observations:

| Query | 4.0 interpreter | 4.1 preview YJIT | 4.1 preview ZJIT |
|---|---:|---:|---:|
| Arithmetic filter | 1333.32 → 1205.36 | 686.23 → 666.70 | 841.59 → 734.11 |
| Arithmetic projection | 2284.49 → 2178.20 | 1159.93 → 1260.54 | 1380.39 → 1315.99 |
| CASE projection | 2229.90 → 2150.05 | 1209.22 → 1267.08 | 1352.89 → 1244.63 |
| Plain scan | 1384.94 → 1154.34 | 723.46 → 706.35 | 880.39 → 675.32 |
| Projection/LIMIT | 197.08 → 191.94 | 74.50 → 78.71 | 88.89 → 85.26 |
| Projection/OFFSET/LIMIT | 600.87 → 564.01 | 292.60 → 287.04 | 371.81 → 318.18 |

The interpreter benefits, especially on plain projections. ZJIT shows larger
whole-query changes on some shapes, but unmodified group/sort controls also became
10–15% faster in that run. Those changes cannot all be attributed to fusion.
Preview YJIT is mixed; no runtime is uniformly superior for this workload.

## Ruby binding and cold SQL

Cells below are **native eager / Rust binding**, in microseconds per query, from
the larger 11-job API sequence. Full native prepared/ordinary measurements are
preserved in the summary. The native scalar, PK, and host paths are controls;
host-containing queries do not enter the new pipeline tier.

| Query | 4.0 YJIT | 4.1 preview YJIT | 4.1 preview ZJIT |
|---|---:|---:|---:|
| Scalar | 5.40 / 7.81 | 5.47 / 7.87 | 5.35 / 8.22 |
| PK hit | 3.95 / 10.19 | 4.09 / 10.16 | 3.96 / 10.54 |
| Arithmetic filter | 415.25 / 355.06 | 409.20 / 343.71 | 383.91 / 364.26 |
| Arithmetic projection | 721.84 / 687.13 | 752.46 / 652.97 | 719.22 / 697.46 |
| CASE projection | 782.53 / 762.14 | 656.79 / 723.22 | 667.15 / 780.01 |
| Plain scan | 297.93 / 673.75 | 284.17 / 655.09 | 292.70 / 683.92 |
| Projection/LIMIT | 68.43 / 27.71 | 61.19 / 26.91 | 65.58 / 28.78 |
| Projection/OFFSET/LIMIT | 223.22 / 97.09 | 218.58 / 94.79 | 210.84 / 100.32 |
| 1,000 host row callbacks | 952.23 / 1361.27 | 991.00 / 1305.66 | 945.56 / 1359.84 |
| 1,000 rows through host batches | 1190.32 / 877.72 | 1229.82 / 832.35 | 1117.71 / 911.09 |

Native and binding answers/metadata agree. Exact native before/after costs agree
for every workload/API. The LIMIT/OFFSET rows have the pre-existing cross-package
cost differences detailed below and are not presented as exact-cost parity.

Fresh SQL comments force parse/plan cache misses. The first lazy LIMIT after-run
was 26% slower, prompting reversed-source repeats. All four results remain visible:

| API / query | Before A | After B | After B | Before A |
|---|---:|---:|---:|---:|
| query / scalar | 93.54 | 100.51 | 95.70 | 94.93 |
| query / arithmetic projection | 1775.14 | 1902.69 | 1778.23 | 1735.38 |
| query / projection LIMIT | 288.01 | 363.73 | 288.56 | 345.52 |
| execute / scalar | 95.62 | 93.28 | 97.81 | 91.53 |
| execute / arithmetic projection | 1327.83 | 1330.16 | 1427.08 | 1326.49 |
| execute / projection LIMIT | 198.77 | 198.64 | 214.77 | 196.75 |
| wrapper / scalar | 7.98 | 8.22 | 7.92 | 8.23 |
| wrapper / arithmetic projection | 685.36 | 713.82 | 676.94 | 707.29 |
| wrapper / projection LIMIT | 28.69 | 30.09 | 28.73 | 29.52 |

The large initial LIMIT slowdown does not repeat consistently. Several native
cold means still moved roughly 4–5% slower, so these samples do not prove zero
cold overhead. They establish **no cold-query benefit**, with substantial visible
variation. First-use queries do not generate a pipeline; parsing, planning, and
execution through the original loop still dominate them.

## Generated loops, JIT evidence, and compilation cost

Direct diagnostics compare the former expression-kernel loop with the fused loop
over 1,000 input rows, excluding storage, planning, and the public cursor/API.
The control uses the original indexed array loop or streaming `each`, selected
outside the measured row loop. Values are microseconds per complete loop:

| Shape | YJIT control | YJIT fused | Speedup | ZJIT control | ZJIT fused | Speedup |
|---|---:|---:|---:|---:|---:|---:|
| Plain stream | 72.46 | 46.47 | 1.56× | 101.66 | 55.63 | 1.83× |
| Arithmetic stream | 261.45 | 240.18 | 1.09× | 303.00 | 278.70 | 1.09× |
| CASE stream | 237.78 | 213.68 | 1.11× | 246.06 | 239.92 | 1.03× |
| OFFSET/LIMIT stream | 43.22 | 36.82 | 1.17× | 59.33 | 48.03 | 1.24× |
| Arithmetic array | 223.10 | 212.95 | 1.05× | 253.72 | 233.21 | 1.09× |

YJIT directly reports compiled instructions for every generated method and its
streaming block. ZJIT's per-job log proves every hot loop was compiled. Its plain
stream wrapper has no separate compiled-method log entry; only the block is
proven in that case. The other generated methods and blocks are logged. The
diagnostic records this distinction rather than equating global JIT activity
with compilation of a particular method.

The final warmed checks repeat values, NULL behavior, partial outputs, division
by zero, overflow, and cost-limit prefixes after timing. The permitted error rows
actually pass their filters, and the OFFSET probe has enough input to reach its
projection failure. Direct-loop allocations are essentially unchanged: fusion
removes dispatch and optional branches, while each output still owns its array.

Repeated compilation medians are **10.6–128 µs** across runtimes. First compilation
was **36 µs–2.13 ms**; first invocation with 1,000-row input was **0.19–6.20 ms**.
These are distinct from steady-state timings and reinforce reuse-based admission.

The churn probe creates 256 pipelines, calls each 200 times, retains 64, then
releases them. All runtimes return to zero live pipeline instances. After GC,
YJIT retains 20 generated classes and a 3.86 MB process code region; ZJIT retains
266 classes, a 4.78 MB code region, and 8.46 MB total JIT memory. These are
whole-process counters, including diagnostic control-expression compilation.
Plan eviction does not promise immediate JIT code/class reclamation.

Peak diagnostic RSS was **64.5 MiB**. Native benchmark workers peaked at
**99.1 MiB**; the largest worker including other cores was **341.5 MiB**
(TypeScript), below the enforced 512 MiB ceiling. Raw diagnostics, logs, resource
observations, and all comparison matrices are in
[`summary.json`](../../bench/ruby-native/results/pipeline-followup/summary.json).

## Implementation and semantic boundary

`CompiledPipeline` reuses the existing handwritten expression emitter, admitting
only complete plain-integer/boolean trees. It emits a fixed Ruby method template
with an indexed `while` for eager array input or the source's existing internal
`each` for streaming input. No SQL identifier, text, literal, or diagnostic is
interpolated into executable Ruby; values stay in a frozen pool. Generated code
captures no engine, bindings, row source, result sink, or session.

Eligible plans try the pipeline tier on reuse. If admission fails, the existing
expression kernels and ordinary executor remain available. Existing plan identity,
schema/publication, parameter-type, environment, timezone, and collation checks
govern reuse. Each plan still retains at most 64 KiB generated source; its fourth
optional slot holds the pipeline. Each method retains the existing 192-node,
32-depth, 256-pool-entry, and 32-KiB source limits. The existing 64-entry plan-cache
bounds remain unchanged. VM/JIT memory is not bounded solely by these source caps.

Array loops retain bulk scan charges before execution and bulk output charges
after successful completion. Streaming loops preserve the exact sequence:
source pull, scan-row charge, predicate, accepted-row OFFSET, projection, output
charge, sink append/cursor suspension, then LIMIT termination. Skipped OFFSET rows
do not evaluate projections, and LIMIT 0 never enters the iterator. Existing scanner
preparation and cleanup remain outside the generated loop. No failure is retried
through the evaluator.

## Verification

All **48 host/storage test files passed** on Ruby 4.0.7, including traced input,
column and output faults; statement/lifetime budget prefixes; warm prepared and
ordinary APIs; cursor suspension; bindings; schema/publication changes; and cache
bounds. Peak RSS was 115.1 MiB. The
[bounded host-test report](../../bench/ruby-native/results/pipeline-followup/host-tests.json)
retains the command and complete output.

Three full-corpus audits used Ruby 4.1.0-preview1 and forced first-use compilation
so one-shot corpus queries exercise generated code. Each considered all 679 files:

| Mode / API / JIT | Passed / attempted records | Pipelines compiled | Peak worker RSS |
|---|---:|---:|---:|
| Memory / execute / ZJIT | 20,267 / 20,267 | 2,179 | 138.4 MiB |
| Disk / execute / YJIT | 18,250 / 18,250 | 2,011 | 179.7 MiB |
| Memory / query / YJIT | 20,099 / 20,170 | 1,187 | 158.7 MiB |

The two execute audits pass completely. They retain the corpus's authored mode
exclusions (two in memory, 77 on disk); no new exclusions were added. Every run
finished without a timeout or RSS-limit hit and verified the implementation hash
before and after execution. Raw reports:
[memory execute](../../bench/ruby-native/results/pipeline-followup/conformance-memory-zjit.json),
[disk execute](../../bench/ruby-native/results/pipeline-followup/conformance-disk-yjit.json),
[memory query](../../bench/ruby-native/results/pipeline-followup/conformance-lazy-yjit.json).

The full lazy audit **does not pass**: it records 71 failures in 16 files. Five
workers exit on `SystemStackError`, leaving 97 fewer attempted records than the
memory execute audit. The original failures remain in the report rather than
being filtered out or labeled unsupported. A separate baseline comparison below
distinguishes these findings from compiler regressions.

## Lazy audit baseline and collection differences

The same 16 affected files were replayed against the immutable pre-change source
and the final implementation, with the same harness, runtime, options, and corpus
bytes. Both attempt 586 records: 515 pass and the **same 71 fail**, with identical
file, line, SQL and error details. The new compiler introduces no failure in this
comparison. The selected-file manifest and both raw reports are retained beside
the full audit:
[comparison and artifact hashes](../../bench/ruby-native/results/pipeline-followup/lazy-baseline-comparison.json),
[corpus manifest](../../bench/ruby-native/results/pipeline-followup/lazy-control-corpus.json),
[before](../../bench/ruby-native/results/pipeline-followup/conformance-lazy-before-control.json),
[after](../../bench/ruby-native/results/pipeline-followup/conformance-lazy-after-control.json).

The full lazy harness calls `Rows#rows`, which enables the engine-owned result
collector and charges its retained rows. The shared corpus's execute-mode memory
thresholds cannot all be applied unchanged to that API. The memory contract
explicitly permits lazy and materialized APIs to retain different buffers and hit
different thresholds ([memory.md §5.4](../../spec/design/memory.md)).

A separate diagnostic drains the same affected files through external `each`
collection, leaving the original audit intact. It passes 558 of 586 records:
46 original collection-budget failures/cascades disappear, while three different
memory-threshold assertions now succeed where the execute-oriented corpus expects
failure. The other 25 failures persist: 19 lazy resolution/typing/metadata failures,
five stack overflows, and one correlated-subquery cost-limit failure. These remain
experimental gaps; no expectation or engine behavior was changed to hide them.
The [diagnostic report](../../bench/ruby-native/results/pipeline-followup/conformance-lazy-external-drain-diagnostic.json)
and [diagnostic adapter](../../bench/ruby-native/results/pipeline-followup/lazy-drain-diagnostic.rb)
record exactly how this comparison differs from the full audit. It is not a
replacement conformance pass.

## Known pre-existing cost gap

The three new scan shapes exposed a storage-meter defect already present in
baseline commit `627cbb631e3a922999c3a0cb3bcba32136fcb345`. Values, column metadata,
and row counts match the Ruby Rust binding, but the LIMIT workloads have different
exact costs. All native APIs agree with each other, and native before/after costs
remain identical; fusion neither introduces nor corrects this defect.

For the six-node benchmark table:

| Workload | Native validation costs | Rust-binding validation costs |
|---|---|---|
| LIMIT 20 | 102, 108, 114, 120, 134 | 106, 112, 118, 124, 138 |
| OFFSET 500/LIMIT 20 | 1104, 1220, 1364, 1555, 1801 | 1106, 1222, 1366, 1556, 1802 |

`storage_stream_rows` charges tree height first, then further structural pages as
the iterator reaches them. The canonical [LIMIT cost schedule](../../spec/design/cost.md)
requires the complete bounded structural page block upfront. The first query
touches only two of six pages, explaining every four-unit deficit; OFFSET reaches
four or five pages, explaining the two-/one-unit deficits. This is an experimental
implementation bug, not an accepted change to the shared contract.

The raw binding harness preserves its failed cost checks. It has not been weakened
to label these comparisons as full conformance passes. Fixing storage metering
also requires budget-failure and storage-fault ordering verification; it remains a
recorded gap outside this compiler change.

## Reproduction

The baseline is preserved in
[`before-source.tar.gz`](../../bench/ruby-native/results/pipeline-followup/before-source.tar.gz)
with a verified restoration manifest. Its implementation SHA-256 is
`e461f1f256a672948823623a88bc20fa949f594ac2708bda5225069de378d473`.
The final implementation SHA-256 is
`0932dbb113e7015a22a72188d2a5a3a29cb3b6a40ba9026518ad3b9e276e83fd`.
Reports check source identity before and after execution.

The [15-query plan](../../bench/ruby-native/compare/pipeline-plan.json) preserves
the previous twelve queries and adds plain projection and projection with
LIMIT/OFFSET over the same 1,000-row fixture. Cross-core adapters retain their
prepared APIs. The separate binding harness measures native prepared, ordinary,
and eager APIs against the available Rust binding, whose API reparses and
materializes results on every call. These are complete API comparisons rather
than isolated FFI measurements.

```sh
RUBY_NATIVE_COMPARE_PLAN=bench/ruby-native/compare/pipeline-plan.json \
  RUBY_NATIVE_PHASE=pipeline-local RUBY_NATIVE_COMPARE_RUBY_ADDRESS_SPACE_BYTES=2147483648 \
  mise run ruby-native:compare

JED_NATIVE_TIMEOUT=300 JED_NATIVE_ADDRESS_SPACE_BYTES=2147483648 \
  mise run ruby-native:test

JED_NATIVE_COMPILATION=immediate JED_NATIVE_TIMEOUT=900 \
  JED_NATIVE_ADDRESS_SPACE_BYTES=2147483648 \
  mise run ruby-native:bounded -- \
  .dev/ruby-native-runtimes/ruby-4.1.0-preview1/bin/ruby --zjit \
  impl/ruby-native/bin/conformance.rb
```

Measurements run sequentially on the same shared Linux x86-64 Intel Core Ultra 9
285K host. Ruby workers enforce a 512 MiB RSS ceiling and 2 GiB address-space
limit. Supported-core workers use the same RSS bound with their larger virtual
reservation allowance. Corpus workers additionally have a ten-second per-file
deadline. No test/build from this task overlaps timed benchmark work.
