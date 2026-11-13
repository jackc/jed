# C# experiment diagnostic measurements

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
