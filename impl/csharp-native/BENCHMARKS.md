# C# experiment diagnostic measurements

## Refreshed core — 2026-11-14

The refreshed NativeAOT actor is **81.9× faster on integer
aggregation, 98.1× on floating-point aggregation,
31.4× on joins and 41.5× on sorting** than the original
actor in the matched diagnostics below. Allocation falls substantially.
C# now beats Go in some measured workloads, but **does not match or beat Go on
every workload**. Production delivery and supported-core promotion remain
separate decisions.

Final source: `6f0da47cc73888be3557572ea523a52e80d8c64a131fad80e4a95943c7073bf3`.
Semantic baseline: `6b2355d98a7fd3738c60474779d1d63548f350dc`, format v53.
All four immutable actors passed the complete [acceptance matrix](VERIFICATION.md).
The original NativeAOT actor remains historical engine34 source
`7124c0e6a67e3fb3955ef6df359faaa94d8d375702b887b72cc439a851a51594`;
it was rerun against the **current pinned Go peer and identical extended input**.
This comparison does not reuse the old nine-workload timings or old Go peer.

### Method and reproducibility

Linux x86-64, Intel Core Ultra 9 285K (24 logical CPUs), .NET SDK 11.0.100/runtime
11.0.0 and Go 1.27.1. Each workload uses a fresh 4,096-row file with 8,192-byte
pages and prepared syntax. Draining owned rows, rendering values and hashing
results are inside the timed operation. Writes verify final count/sum outside
timing. Raw records include SQL, setup, iterations, every sample, allocation,
GC collections, checksums, compiler/runtime settings and source/artifact/policy hashes.

Final measurements run sequentially after all builds and correctness jobs stop.
All processes inherit **CPU0 affinity**, a performance core (reported maximum
5.5 GHz), so C# and Go cannot be scheduled onto different core classes. Three
rounds use the fixed order original AOT, refreshed default AOT, default JIT,
runtime-async AOT, runtime-async JIT. Each invocation runs C# then its independently
built Go peer. All **204 refreshed and 51 original paired workload checks** match.
Each actor performs 10× the original warmup count, equally for C# and Go.
No frequency lock, forced GC or randomized run order is used. Timing remains
diagnostic; raw per-run observations retain variability.

**JIT measurements set `DOTNET_TieredCompilation=0`** to use optimized resident
code immediately. Their startup and timing do not characterize default tiered-JIT
warmup. AOT does not set that variable. Refreshed AOT explicitly selects
`OptimizationPreference=Speed`; original and refreshed use SDK-default instructions.
This compares delivered source/build configurations, not a source-only compiler
experiment. Runtime-native async is a separately compiled experimental variant.

**Files are on `/tmp`, which is tmpfs on this host.** File ownership and
`synchronous=on` barriers remain enabled. Commit times measure the engine's commit
path on tmpfs, not physical-disk durability latency. Cold open uses a warm OS cache.
These cases do not measure encryption throughput, data larger than cache, spill
throughput or concurrency. Startup includes process launch and an initial SELECT.

Allocation includes diagnostic rendering/checksumming and managed runtime work,
not just the engine. In particular C#'s diagnostic checksum encodes each rendered
value and separator to a fresh UTF-8 array; Go hashes strings directly. That
harness is unchanged for historical comparability. It contributes to the row-heavy
allocation gap; these measurements do not isolate its CPU contribution. The
aggregate/write gaps cannot be explained by large result checksums.

The [machine-readable summary](measurements/refresh-summary.json) links all
12 final runs and three original-AOT runs with hashes. Tables show medians of
three per-run sample medians, in microseconds per operation. Ratios are medians
of **paired** C#/Go ratios; greater than 1 means C# took longer. Independent
medians need not divide to the displayed paired ratio.

### Latency against current Go

| Workload | Optimized JIT µs (×Go) | Optimized JIT runtime-async µs (×Go) | NativeAOT µs (×Go) | NativeAOT runtime-async µs (×Go) |
|---|---:|---:|---:|---:|
| scalar | 0.90 (2.20×) | 0.80 (1.91×) | 0.80 (1.96×) | 0.70 (1.73×) |
| point | 2.20 (2.53×) | 2.20 (2.49×) | 1.70 (1.93×) | 1.70 (1.79×) |
| range | 36.10 (2.80×) | 35.80 (2.71×) | 35.70 (2.64×) | 34.30 (2.65×) |
| aggregate | 88.50 (0.61×) | 91.30 (0.61×) | 84.50 (0.57×) | 83.05 (0.55×) |
| join | 7387.90 (1.68×) | 6963.60 (1.62×) | 6900.60 (1.60×) | 6422.50 (1.49×) |
| sort | 2045.70 (2.56×) | 1927.20 (2.43×) | 1916.75 (2.39×) | 1905.15 (2.38×) |
| rollback | 53.00 (2.58×) | 51.55 (2.44×) | 46.65 (2.19×) | 46.30 (2.26×) |
| durable | 131.25 (1.53×) | 118.90 (1.46×) | 120.30 (1.40×) | 115.00 (1.32×) |
| cold_open | 284.00 (2.13×) | 264.40 (1.99×) | 245.90 (1.80×) | 221.90 (1.71×) |
| grouped | 2325.45 (1.16×) | 2002.65 (0.97×) | 2112.40 (1.06×) | 1860.45 (0.90×) |
| distinct | 2993.75 (1.36×) | 2696.00 (1.19×) | 2798.70 (1.28×) | 2553.95 (1.15×) |
| distinct_aggregate | 1989.75 (2.28×) | 1723.40 (2.02×) | 1796.70 (2.10×) | 1576.10 (1.79×) |
| window | 84.50 (1.81×) | 79.15 (1.81×) | 76.30 (1.74×) | 71.30 (1.52×) |
| text_filter | 533.95 (2.32×) | 548.95 (2.45×) | 551.10 (2.36×) | 547.70 (2.38×) |
| float_aggregate | 74.50 (0.71×) | 78.40 (0.74×) | 70.20 (0.69×) | 69.85 (0.69×) |
| decimal_aggregate | 2234.90 (0.47×) | 2174.10 (0.45×) | 2121.25 (0.44×) | 2030.80 (0.43×) |
| expression_projection | 48.60 (3.20×) | 47.00 (3.07×) | 45.40 (2.99×) | 45.85 (3.04×) |

### Original versus refreshed NativeAOT

| Workload | Original AOT µs | Refreshed AOT µs | Speedup | Refreshed Go µs | AOT allocated bytes before → after |
|---|---:|---:|---:|---:|---:|
| scalar | 5.70 | 0.80 | 7.12× | 0.41 | 27529 → 2784 |
| point | 61.30 | 1.70 | 36.06× | 0.88 | 62080 → 4936 |
| range | 237.65 | 35.70 | 6.66× | 13.63 | 513304 → 42224 |
| aggregate | 6919.75 | 84.50 | 81.89× | 147.14 | 20130248 → 8136 |
| join | 216643.40 | 6900.60 | 31.39× | 4339.85 | 85650240 → 12680168 |
| sort | 79578.85 | 1916.75 | 41.52× | 801.80 | 7695201 → 354968 |
| rollback | 321.90 | 46.65 | 6.90× | 21.05 | 747632 → 70888 |
| durable | 767.60 | 120.30 | 6.38× | 87.44 | 1462181 → 118599 |
| cold_open | 614.20 | 245.90 | 2.50× | 136.52 | 252210 → 225563 |
| grouped | 10410.15 | 2112.40 | 4.93× | 2013.47 | 33028952 → 1331816 |
| distinct | 8218.65 | 2798.70 | 2.94× | 2188.25 | 18885632 → 2018248 |
| distinct_aggregate | 8324.60 | 1796.70 | 4.63× | 855.47 | 27433144 → 1168824 |
| window | 989.40 | 76.30 | 12.97× | 45.13 | 2522048 → 131144 |
| text_filter | 4767.40 | 551.10 | 8.65× | 233.96 | 12785656 → 231888 |
| float_aggregate | 6886.30 | 70.20 | 98.10× | 99.42 | 20242528 → 19784 |
| decimal_aggregate | 9600.30 | 2121.25 | 4.53× | 4778.22 | 27856816 → 2193464 |
| expression_projection | 282.30 | 45.40 | 6.22× | 15.18 | 673928 → 47304 |

### Allocation and startup

| Workload | Optimized JIT B/op | Optimized JIT runtime-async B/op | NativeAOT B/op | NativeAOT runtime-async B/op | Go B/op (paired with AOT) |
|---|---:|---:|---:|---:|---:|
| scalar | 2752 | 2608 | 2784 | 2640 | 1352 |
| point | 4912 | 4768 | 4936 | 4792 | 2008 |
| range | 42200 | 42056 | 42224 | 42080 | 9296 |
| aggregate | 8040 | 7896 | 8136 | 7992 | 19320 |
| join | 12671448 | 12671304 | 12680168 | 12680024 | 5569719 |
| sort | 354912 | 354925 | 354968 | 354824 | 296152 |
| rollback | 68568 | 68352 | 70888 | 70672 | 126904 |
| durable | 115674 | 114070 | 118599 | 116974 | 279037 |
| cold_open | 223986 | 220389 | 225563 | 221754 | 497771 |
| grouped | 1330384 | 1330240 | 1331816 | 1331672 | 3534269 |
| distinct | 2018240 | 2018096 | 2018248 | 2018104 | 4271607 |
| distinct_aggregate | 1168624 | 1168480 | 1168824 | 1168680 | 1333442 |
| window | 131040 | 130896 | 131144 | 131000 | 183672 |
| text_filter | 231792 | 231648 | 231888 | 231744 | 149144 |
| float_aggregate | 19688 | 19544 | 19784 | 19640 | 2184 |
| decimal_aggregate | 2193272 | 2193128 | 2193464 | 2193320 | 3874820 |
| expression_projection | 47280 | 47136 | 47304 | 47160 | 9296 |

| Configuration | Startup ms | Paired Go ms |
|---|---:|---:|
| jit | 203.16 | 1.12 |
| jit-runtime-async | 182.17 | 1.12 |
| aot | 3.68 | 1.15 |
| aot-runtime-async | 3.55 | 1.11 |
| original-aot | 3.80 | 1.14 |


### Retained allocation-cleanup experiment

The following separately archived comparison is capture19 → capture20, before
the final typed-page and fused-scan changes. It establishes the cleanup's effect;
it is not a claim that those actors certify the final source.


After the capture19 full acceptance and repeated measurements, the final audit
removed unused window scratch/fold containers, runtime hex-key conversions,
redundant grouping-key copies, discarded nonroot leaf metadata and common scalar
dispatch checks. Six additional quiet runs alternate capture19 and final AOT,
three per source. The four-case input covers numeric/text partitioned rankings
and variable-length text rollback/durable updates on a multi-leaf table. All
**24 paired checks** match. It uses the existing actor protocol and 10× warmup;
[cleanup-summary.json](measurements/cleanup-summary.json) links the raw evidence.
The file-host profile does not directly benchmark the nonspill grouping path.

| Workload | Capture19 AOT µs | Final AOT µs | Allocated bytes before → after | Final paired Go µs |
|---|---:|---:|---:|---:|
| ranking_numeric | 6163.95 | 5323.35 | 13920680 → 12523512 | 3452.47 |
| ranking_text | 6475.40 | 6500.55 | 15694424 → 14428328 | 2938.41 |
| text_rollback | 54.10 | 49.20 | 122543 → 122536 | 18.41 |
| text_durable | 177.65 | 169.75 | 239855 → 234183 | 75.47 |


The earlier [capture19 summary](measurements/capture19-summary.json) preserves
its complete repeated measurements separately; it is not used as final-source
acceptance evidence.


### Optimization decisions and remaining differences

The accepted changes cover the measured avoidable work:

- Native resident expression evaluation with retained async continuations; safe
  prepared-plan reuse and public result ownership transfer; compact scalar values.
- Direct page-chunk scans and simple fixed-table filter/projection execution,
  preserving blocking columnar admission, error/cost order and owned results.
- Validated numeric-page COUNT/SUM/AVG kernels with prebound column views and one
  query-owned scratch array. They share exact integer/IEEE accumulators, casts,
  guards and finalization, and retain generic fallbacks for other schemas/plans.
- Native resident group state, exact encoded byte keys, single-allocation integer
  identities, membership-only probes, and fewer unused fold/window buffers.
  Spill timing, row order and deterministic memory charges stay unchanged.
- Bounded Top-K, join binding/copy cleanup, allocation-free LIKE traversal,
  fixed-leaf replacement, lazy row decoding, cached parsed pages, exact payload
  measurement, incremental reclamation/remaps and slicing-by-eight CRC32.

Every retained optimization passed focused parity/invariant checks and the final
cross-core acceptance matrix. Intermediate captures, cleanup comparisons and the
capture20 same-core study are retained in `measurements/`; their source hashes
keep them separate from final evidence. The same-core study exposed real numeric
aggregate overhead and led to the typed-page implementation, rather than accepting
an apparent ratio produced by mixed-core scheduling.

Remaining ratios vary by execution and runtime. Some general expressions,
ordering, joins, text conversion and file lifecycle operations still take longer
than Go. No language/runtime guarantee makes those ratios automatically converge.
There is no claim that all possible optimizations are exhausted. Further changes
need a new measured bottleneck and a proof that they preserve the spec's costs,
error precedence, memory/spill schedule, snapshot ownership and cancellation.
Hardware-counter profiling was unavailable on this host (`perf_event_paranoid=4`);
no host security setting was changed. Removing semantic work, changing the
checksum harness, unsafe code or unapproved dependencies was not used to obtain
these results.

## Historical original freeze — 2026-11-13

Measured 2026-11-13 on Linux x86-64, Intel Core Ultra 9 285K (24 logical CPUs),
.NET SDK 11.0.100 / runtime 11.0.0 and Go 1.27.1. The experiment is frozen;
production delivery and supported-core promotion remain separate decisions.

These nine workloads show substantial C# time and allocation overhead against the
pinned Go peer. NativeAOT reduces startup and point/range latency here, but takes
longer for aggregation and sorting. Runtime-async compilation is not a consistent
performance improvement. These are measurements of this implementation and host,
not conclusions about the languages' attainable performance.

## Method and provenance

The actors use verified engine34 source
`7124c0e6a67e3fb3955ef6df359faaa94d8d375702b887b72cc439a851a51594`, semantic
baseline `b1a6affca7725e0b07a4f734eddb1c569ef76bcf`. Later harness35 changes only
repair simulation digest bookkeeping; they do not alter the measured engine.
The [verification record](VERIFICATION.md) records acceptance and the final
metadata-only freeze proof. Both AOT publications use the SDK-default instruction set.

All correctness jobs had finished before measurement. Runs were sequential:
three default JIT runs, three runtime-async JIT, three default NativeAOT, then
three runtime-async NativeAOT. Each invocation runs C# followed by an independently
compiled pinned Go actor. Every invocation captures the C# actor directory and its
dependencies, verifies their hashes before and after execution, and records build, policy,
input and actor identities. The Go executable hash is identical in all 12 runs:
`d334c76d9a5d2b6ff01e7c324e104b0c9da0395fd48d5edddff0aa568788128f`.
All 108 paired workload checks match name, mode, iteration count, row count and
checksum. Write cases also verify final table contents outside the timed section.

Each workload creates a fresh 4,096-row file, uses 8,192-byte pages, and performs
its declared warmup before sampling. Ordinary queries and writes reuse prepared
syntax. Result draining and checksum rendering are timed. The scalar lane is a
literal multiplication; point and range lanes read primary keys; aggregation
computes count/sum; the join is an equal-primary-key self join; sort returns the
first 100 rows ordered by value descending then id. The complete SQL and warmup/
iteration counts are in every raw JSON file.

**The files were on `/tmp`, which is tmpfs on this host.** Native file ownership,
page I/O and `synchronous=on` barriers remain enabled, but the durable-update lane
measures the engine's synchronous commit path on tmpfs, not physical-disk
persistence latency. Cold open uses a warm OS cache. This run does not measure
SSD durability, encryption, large/out-of-cache data, spill throughput or concurrency.
No affinity, frequency lock, forced GC, or randomized run order was used. The
machine's heterogeneous cores, scheduler and short warmups can affect samples.

## Paired latency

Each time is the median of three per-run sample medians, in microseconds per
operation. Each parenthesized ratio is the median of the three paired C#/Go
median ratios; **greater than 1 means C# took longer**. Since the medians are
computed independently, a ratio need not equal the quotient of displayed times.
All per-run C# and Go medians and ratios are in [summary.json](measurements/summary.json).

| Workload | JIT µs (×Go) | JIT runtime-async µs (×Go) | NativeAOT µs (×Go) | NativeAOT runtime-async µs (×Go) |
|---|---:|---:|---:|---:|
| scalar | 26.80 (47.60×) | 11.20 (25.23×) | 6.00 (9.33×) | 9.40 (16.41×) |
| point | 162.20 (121.41×) | 163.30 (120.47×) | 57.30 (43.21×) | 57.00 (41.56×) |
| range | 440.05 (21.69×) | 438.85 (21.76×) | 229.90 (10.47×) | 207.40 (10.49×) |
| aggregate | 4666.60 (19.30×) | 4802.45 (19.50×) | 6246.70 (26.64×) | 6753.05 (27.34×) |
| join | 178273.50 (32.20×) | 174484.50 (30.98×) | 201911.00 (36.23×) | 200096.30 (35.29×) |
| sort | 43985.75 (43.79×) | 43602.00 (43.36×) | 76181.90 (75.17×) | 74655.55 (72.12×) |
| rollback | 281.85 (10.01×) | 286.05 (9.97×) | 298.95 (10.38×) | 304.80 (10.25×) |
| durable | 845.70 (4.95×) | 795.10 (4.51×) | 808.30 (4.59×) | 777.40 (4.44×) |
| cold_open | 734.90 (2.83×) | 690.80 (2.72×) | 653.30 (2.26×) | 643.70 (2.35×) |

The unweighted geometric mean of these nine paired ratios is diagnostic only:
JIT 19.85×; JIT runtime-async 18.13×; NativeAOT 14.75×; NativeAOT runtime-async 15.55×. It does not represent an application workload mix.
Small operations were variable: default JIT scalar run medians were 11.4, 26.8
and 27.1 µs; runtime-async JIT scalar medians were 15.4, 10.8 and 11.2 µs. Three
repeats and these warmups do not support a robust compiler-variant ranking for
that lane. No run or sample was removed.

## Startup

Startup includes process launch, engine initialization, an in-memory `SELECT 1`,
and process exit. Each invocation collects five starts; these figures are the
median of the three invocation medians, in milliseconds. This is distinct from
the file-backed `cold_open` workload above.

| Variant | C# startup ms | Paired Go startup ms |
|---|---:|---:|
| JIT | 85.70 | 2.50 |
| JIT runtime-async | 81.03 | 2.44 |
| NativeAOT | 5.28 | 3.08 |
| NativeAOT runtime-async | 5.10 | 3.08 |

## Allocation and GC observations

C# used workstation GC (`server_gc=false`) and Interactive latency in every run.
The following are median allocated KiB per measured operation, not retained heap
or peak RSS. C# uses process-wide `GC.GetTotalAllocatedBytes`; Go uses
`runtime.MemStats.TotalAlloc`. Both include runtime/background allocation and have
different accounting boundaries, so these are diagnostic observations rather
than identical heap-cost units. The Go column is the median across all 12 runs.

| Workload | JIT KiB/op | JIT runtime-async KiB/op | NativeAOT KiB/op | NativeAOT runtime-async KiB/op | Go KiB/op |
|---|---:|---:|---:|---:|---:|
| scalar | 25.13 | 25.16 | 26.88 | 26.74 | 1.32 |
| point | 57.54 | 57.40 | 60.62 | 60.48 | 1.96 |
| range | 478.97 | 478.80 | 501.27 | 501.13 | 9.08 |
| aggregate | 18205.48 | 18212.35 | 19658.45 | 19658.30 | 18.87 |
| join | 79821.88 | 79821.96 | 83643.20 | 83642.99 | 5439.27 |
| sort | 6840.24 | 6840.20 | 7514.81 | 7514.67 | 289.21 |
| rollback | 725.62 | 725.48 | 730.11 | 729.90 | 124.09 |
| durable | 1423.39 | 1421.43 | 1427.89 | 1425.76 | 291.48 |
| cold_open | 243.13 | 240.07 | 246.30 | 242.80 | 488.01 |

Across 50 measured aggregation calls, JIT recorded 49 generation-0 collections
and AOT 54, with no higher-generation collections. Across 25 joins, JIT recorded
109–110 generation-0, 17–22 generation-1 and 5–7 generation-2 collections; AOT
recorded 114–115, 12–16 and 2–7 respectively. Go recorded 62–64 collections over
the paired joins; its collector is not generational. Sort recorded 19 generation-0
collections under JIT and 20 under AOT across 50 calls. Exact collection arrays
for every workload and repeat are retained in the JSON.

The high allocations are a clear follow-up profiling target. No optimization or production-delivery choice is implied by completing
this experiment, and no cross-language memory-efficiency guarantee follows from
these counters.

## Reproduction and raw records

Use the opt-in task with a verified immutable actor, for example:

```sh
mise run csharp-native:performance --variant jit \
  --assembly /absolute/path/to/verified/Jed.Check.dll \
  --output /absolute/path/to/result.json
```

The variants are `jit`, `jit-runtime-async`, `aot`, and `aot-runtime-async`;
AOT takes the published executable instead of a DLL. A full actor directory must
include its build metadata and dependencies. The task runs the nine diagnostic
workloads, not the canonical repository benchmark suite. Setup and compilation
are outside measured samples.

The 12 raw files in [measurements/](measurements/) are named `<variant>-<repeat>.json`.
[summary.json](measurements/summary.json) records their SHA-256 hashes, each paired
run median, allocations and GC counts. Absolute commands/capture paths preserve
the original run provenance; they are not portable install paths. The initial
sandbox-only invocation failed before measurement because the file-owner Unix
socket was prohibited; the recorded run used normal host access. No failed or
partial invocation is included as a timing result.
