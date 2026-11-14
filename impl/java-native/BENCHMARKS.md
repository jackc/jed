# Java experiment measurements

The final Java experiment passes the matched 24-workload, five-actor comparison
and improves aggregate allocation substantially, but it remains slower than Go
and Rust on most measured workloads. Selective value classes show no broad
performance advantage over ordinary classes on the same JDK 28 build. These
results support keeping Java 25 as the baseline and Valhalla as an optional
experiment; they do not establish performance parity or justify requiring an
unreleased JDK. The semantic baseline is
`3e5962f9affc06c768df1688765162a65a893848`, format v53.

## Final comparison

Final source `419675aa4fa4b2afeaa9a645d6ebc3e68e38579e77858bb1816e12dd2848c5fc`
passed all 42 acceptance gates before measurement. The benchmark then completed
three serial, rotating process repeats for Java 25, ordinary JDK 28, the selective
value-class build, and frozen Go/Rust peers. All 360 workload records and 72
separate Java retained-memory records match rows, checksums and logical costs.
Source, policy and actor integrity checks passed after the retained probes.

This is a **shared-host, bounded-warmup diagnostic**, not an idle-host or
converged steady-state result. The safe five-second observer captured all nine
timed and three retained JVMs as this benchmark's `PerformanceDiagnostics`
processes, with no unidentified or overlapping sampled Java workers. An unrelated
Go test used one CPU before timing; non-actor `setup-pfor` and `go-pfor` processes
subsequently used approximately 1–1.5 CPUs during portions of the run. Across 61
samples between evidence start and completion, total sampled non-actor CPU had a
median of 104% and maximum of 151% of one CPU, on a 24-core host. Those workers
showed no sampled physical I/O. Their detailed ownership was unavailable after
exit, and sampling cannot exclude short-lived work. No unrelated process was
stopped. All repeats are retained; no repeat was selected for a favorable result.

The machine is an Intel Core Ultra 9 285K, with default affinity across CPUs
0–23. Databases used ext4 on `/dev/mapper/ubuntu--vg-ubuntu--lv`, synchronous
commits and the fsync OS path. This is not a measurement of hardware durability
guarantees beyond that OS contract. Toolchains are Temurin 25.0.5+8, OpenJDK
28-ea+24, Go 1.27.1 and Rust 1.99.0. Java 25 versus ordinary JDK 28 changes both
compiler and VM (`javac 28 --release 25` for the latter). The value build uses the
same JDK 28 compiler/VM with release 28 and preview enabled, selectively changing
`SqlValue`, `SqlUuid` and `SqlInterval`; it does not redesign collections for
specialized flattened storage.

Each final time is the conventional median within each process (averaging the
two middle samples for even counts), then the median of three process medians.
Ratios below 1 favor the numerator. The raw driver's upper-median summary is
retained unchanged; `summary.json` recomputes the conventional medians from raw
samples. The concurrent-read sample is 256 queries across four newly launched
threads, including session/preparation and result drain.

| Workload | Java 25 µs | Java 25 / Go | Java 25 / Rust | JDK 28 / Java 25 | Value / ordinary JDK 28 |
| --- | ---: | ---: | ---: | ---: | ---: |
| scalar | 5.48 | 9.59 | 3.30 | 0.96 | 1.04 |
| parallel_point | 1,693.73 | 7.62 | 4.59 | 1.00 | 0.97 |
| point | 7.05 | 5.08 | 2.86 | 0.98 | 1.00 |
| range | 63.30 | 3.27 | 2.13 | 1.47 | 0.97 |
| secondary | 151.84 | 3.37 | 3.15 | 1.35 | 0.79 |
| aggregate | 1,916.20 | 7.78 | 13.91 | 0.94 | 1.03 |
| join | 24,178.32 | 4.20 | 5.33 | 0.98 | 0.99 |
| sort | 829.82 | 0.74 | 1.82 | 1.02 | 1.09 |
| grouped | 3,866.38 | 1.84 | 2.23 | 0.91 | 1.04 |
| distinct | 1,680.74 | 0.65 | 1.17 | 0.99 | 1.03 |
| distinct_aggregate | 4,455.06 | 3.73 | 4.27 | 0.95 | 1.03 |
| window | 283.52 | 4.29 | 6.11 | 1.09 | 1.01 |
| ranking | 2,262.31 | 0.94 | 1.59 | 1.02 | 1.09 |
| text_filter | 1,059.47 | 2.94 | 3.50 | 1.00 | 1.01 |
| float_aggregate | 1,660.05 | 8.40 | 10.73 | 0.95 | 1.03 |
| decimal_aggregate | 2,431.33 | 0.41 | 4.63 | 1.00 | 1.00 |
| expression_projection | 45.58 | 1.97 | 1.75 | 1.00 | 1.07 |
| json | 89.26 | 1.41 | 2.27 | 1.18 | 0.99 |
| array | 68.18 | 1.78 | 2.14 | 1.03 | 1.11 |
| rollback | 157.35 | 7.56 | 8.31 | 1.08 | 0.99 |
| durable | 2,249.71 | 1.43 | 1.25 | 1.02 | 1.00 |
| text_rollback | 120.90 | 4.08 | 4.28 | 1.05 | 1.02 |
| text_durable | 1,841.43 | 1.16 | 1.11 | 1.21 | 0.89 |
| cold_open | 497.35 | 2.26 | 2.14 | 1.22 | 0.91 |

The resident aggregate change keeps an owned typed state in memory and encodes
it only when crossing the scratch-storage boundary. Updates copy the finite
state before memory admission, preserving the previous state if spilling or
admission fails. Logical memory charges, spill ordering, fault behavior and
encoded scratch format remain unchanged. Its focused host, aggregate and resource
checks passed before the full acceptance run.

The following before/after Java 25 observations use the same workload policy and
conventional-median calculation. Allocation is calling-thread bytes per
execution; MB means 1,000,000 bytes. Timing reflects two separate, qualified
shared-host runs and is not an isolated causal estimate.

| Workload | Allocated MB before → after | Reduction | Median ms before → after |
| --- | ---: | ---: | ---: |
| aggregate | 14.46 → 8.66 | 40% | 5.614 → 1.916 |
| grouped | 22.01 → 15.23 | 31% | 7.881 → 3.866 |
| distinct_aggregate | 18.82 → 15.15 | 20% | 6.188 → 4.455 |
| float_aggregate | 11.85 → 8.18 | 31% | 3.593 → 1.660 |
| decimal_aggregate | 16.33 → 11.81 | 28% | 5.000 → 2.431 |

The allocation reduction is substantial, but ordinary numeric aggregates still
cost about 7.8× Go / 13.9× Rust here; float aggregation is about 8.4× / 10.7×.
Grouped aggregation is about 1.8× / 2.2×. Java is faster than Go in these sort,
DISTINCT, ranking and exact-decimal cases; Rust remains faster there. Durable
numeric/text medians are approximately 1.1–1.4× the peers, with filesystem and
repeat variation limiting that comparison. Startup and cheap-query overhead
remain material.

Value classes reduce measured allocation by about 3% for point reads, and
about 5% for concurrent point batches and secondary reads, relative to ordinary
JDK 28. Concurrent batches include worker-thread allocation. Many other allocation counts are similar; grouped and
DISTINCT aggregate allocation is slightly higher. Most timing ratios cluster
near 1, with both wins and regressions. The apparent secondary-read timing gain
(about 21%) varies with ordinary-JDK process medians of 160–208 µs, versus
154–165 µs for the value build. Range-read value medians span 66–181 µs. These
observations do not establish a general value-class speedup.

Compilation continues during measured phases: 3.12 seconds total for Java 25,
3.97 seconds for ordinary JDK 28 and 4.53 seconds for the value build, across
three repeats. Even after 5000 warmups, scalar-query late thirds remain roughly
14–18% faster than early thirds. Some EA/value numeric aggregate thirds differ
by a factor of two despite close process medians. The raw trends therefore do
not support a blanket steady-state claim. No extra repetitions or favorable
subsets were used to hide this variation.

Fresh-process startup medians are 77.9 / 81.4 / 84.1 ms for Java 25 / JDK 28 /
value, compared with 2.16 ms Go and 1.09 ms Rust. Whole-process peak RSS spans
1.29–1.97 GiB across Java variants/repeats, 26.4–27.7 MiB for Go and
15.6–15.9 MiB for Rust. RSS includes setup, warmup, all workloads and JVM memory;
it is neither the minimum required heap nor the live database working set.
Measured GC counters total 56 / 61 / 54 collections and 146 / 114 / 123 ms for
the three Java builds. Go records 3187 collections and 164 ms of pauses; its
process-wide counter scope differs. Rust allocation/GC metrics are unavailable,
not zero.

Separate requested-GC probes retain about 7.6–8.1 MB of Java 25 heap with the
aggregate/join/grouped/durable databases live and 4.3–4.6 MB after close. Ordinary
JDK 28 uses about 6.8–7.2 MB live and 3.7–4.0 MB after close; the value variant is
similar. Cold-open probes retain roughly 7.4–8.2 MB even after close in this
measurement. These observations exclude native/JIT/metaspace memory and do not
establish a leak or guarantee that a GC request completed. Raw per-case values
are retained.

Final local evidence is
`.dev/java-native/benchmarks/matched-20261114-2890055-1txi9l/`:
`evidence.json`, `summary.json`, conventionally recomputed `baseline-summary.json`,
`context.jsonl`, `context-summary.json`, `observer.py`, `summarize.rb`,
`summarize-context.py` and `run.log`. The retained repository archive is
`impl/java-native/evidence/benchmark-final/`. The observer emits PID/start-time
identity, safe ownership fields and an exact main-class whitelist; it never emits
full arguments or environment. Both calculation scripts are retained so the
summary and context interpretation can be reproduced. Earlier evidence below
remains historical and is not overwritten by this final capture.

## Matched procedure

`mise run java-native:benchmark` builds Java 25, ordinary JDK 28, and the selective
value-class variant from one immutable source capture. Independent public-API
Go and Rust drivers build offline from the pinned revision. The task retains
source, executable, dependency-lock, input and toolchain identities. Every actor
must match row counts, result checksums and logical execution costs.

The 24 workloads cover scalar/point/range/secondary reads, four-worker concurrent
reads, joins, sorting, grouping, DISTINCT, windows, text, floats, exact decimals,
JSON, arrays, rollback, durable numeric/text updates, and cold database opens.
Every actor uses 256 MiB cache and work-memory budgets, 8192-byte pages and
synchronous durable commits. Filesystem identity is recorded. Cold opens retain
the host's warm OS cache. Query drain, rendering and checksum work are timed.
Concurrent-read samples include thread launch, per-worker session/preparation,
and 64 queries per worker; they are not an isolated engine throughput measure.

Smoke runs use 64 rows and verify correctness. Diagnostic runs use 4096 rows,
serial actor execution, rotating actor order and at least three process repeats.
Warmup is bounded: 5000 cheap queries, 200 other queries, and 100
transaction/open/concurrent executions. Raw warmup and measured samples,
early/late measured medians, Java compilation time, GC counters and process peak
RSS remain in the evidence. Warmup counts alone do not establish steady state;
compilation and sample trends must be inspected before describing results that
way. Startup is measured in separate fresh processes. Accepted timing runs must
have no competing correctness jobs or profilers.

Java allocation counters distinguish the calling thread and concurrent workers;
Go's allocation counter is process-wide. The safe, dependency-free Rust driver
does not install an unsafe global allocator, so Rust allocation counters are
unavailable. Optional Java retained-heap probes run in separate processes and
request GC before execution, with a live database, and after close. They exclude
native/JIT/metaspace memory, and a GC request does not guarantee collection.

## Correctness smoke evidence

The full 24-workload smoke passed across all five actors, including separate
retained-heap probes for the three Java variants:

- Source `f7a00be5d2c7a2b5c5c62288c7e88f1bb068179e94470d0c59f1901fa09c078f`.
- Local evidence: `.dev/java-native/benchmarks/matched-20261114-1797083-1x7020/evidence.json`.
- Database filesystem: ext4 on `/dev/mapper/ubuntu--vg-ubuntu--lv`.

Later six-workload smoke captures checked updated drivers and optimizations
against Go/Rust; these are narrower checks, not replacements for final full
validation. Smoke timings were collected during correctness work and are not
accepted comparison evidence.

## Allocation diagnostics

JFR profiles ran on Java 25 with 4096 setup rows and 20 warmup executions per
workload. The first used 100 measured executions; subsequent profiles used 50.
Correctness jobs were active concurrently. The following numbers are observed
calling-thread allocated bytes per execution, except concurrent reads which sum
the calling thread and four worker threads per 256-query batch. They are not
heap retention or peak-memory measurements. Separate source snapshots and JVM
compilation state also differ; these observations are not isolated causal tests.

| Workload | Initial profile | Cached integer bounds | Decoded-leaf reuse | Probe-template reuse |
| --- | ---: | ---: | ---: | ---: |
| Join | 906.1 MB | 800.9 MB | 134.9 MB | 81.1 MB |
| Grouped aggregate | 37.9 MB | 35.3 MB | 22.9 MB | 21.0 MB |
| Exact-decimal aggregate | 31.8 MB | 29.1 MB | 16.2 MB | 15.8 MB |
| Concurrent point-read batch | 68.2 MB | 58.2 MB | 6.4 MB | 6.3 MB |

The initial recording identified repeated lowercase/catalog lookup in
`SqlType.contains`. Cached bounds still derive from the generated scalar catalog;
that frame disappeared from subsequent top allocation/CPU samples. Repeated
leaf decoding then dominated. A decoded-leaf memo attached to the existing pager
page, keyed by codec identity and discarded with that page, removed leaf decoding
from the top samples. It adds no independent unbounded row cache.

Sequential base-table probes now reuse their already-resolved predicate and
binding templates while creating fresh scans, spools and metrics. Arbitrary
lateral derived plans retain their factory path. Scratch type decoding also
caches the enum's immutable values array instead of cloning it per field. The
fourth profile records these changes. The host suite, 1696 encrypted join,
lateral, subquery and memory records, and 284 memory-only records passed after
the changes. Aggregate scratch-state serialization remains a visible cost. Measured phases with only 20 warmups still accumulated 141–1274 ms of
compiler activity across these profiles, supporting the longer warmup and trend
reporting now used by the diagnostic task.

Local profile identities:

| Profile | Java source SHA-256 | Evidence directory under `.dev/java-native/benchmarks/` |
| --- | --- | --- |
| Initial | `f7a00be5d2c7a2b5c5c62288c7e88f1bb068179e94470d0c59f1901fa09c078f` | `matched-20261114-1797083-1x7020/jfr-20261114-1891811-kuzeku` |
| Integer bounds | `d61a5274a09c09d07f45d100a62e9688737cc4c29bdbd38b684f32b90c751f7c` | `matched-20261114-1905123-ub2931/jfr-20261114-1907287-frbdok` |
| Leaf reuse | `7dc4e3e33dbf4f92cf64bb6f4eb60ce1aeedf9091d36ca177c519a3e257cd260` | `matched-20261114-1913654-aki5u6/jfr-20261114-1915201-de1dwo` |
| Probe reuse | `dfa2ed91cf048099be8ab87698bafb129fa2ea6104b7914524bdaa981d417fc6` | `matched-20261114-2193164-2o2bm7/jfr-20261114-2194987-bcghzr` |

Each directory retains `profile.json`, the exact input, JFR recording, event
summary, raw driver output and `hotspots.json`. JFR allocation weights are
sampling estimates. The hotspot summaries cover startup/setup/warmup as well as
execution; the allocation table instead uses the driver's measured-phase counters.
These local artifacts have not yet been promoted to retained acceptance evidence.

## Durable-write diagnostics

Source `7b7d215edb3980596877f240f931f4fee76074f5c625aa295bbc7f6544b73d8b`
adds explicit JFR workload/phase events, allowing measured samples to exclude
setup and warmup. Numeric and text single-row updates, with and without rollback,
passed the four-workload matched smoke against Go/Rust. Files remain on the same
ext4 filesystem; concurrent correctness work still excludes timing conclusions.

On 4096 rows, a 20-warmup/50-execution recording observed about 2.06 MB allocated
per numeric commit, 2.05 MB per text commit, and 0.65–0.66 MB per rollback. A
100-warmup/500-execution recording observed 4.90–4.91 MB per commit while rollback
allocation stayed near 0.65 MB. Measured-only samples consistently identify leaf
encoding and byte-buffer writes/copies. At this snapshot, `payloadSize` performed
full leaf encoding even when only its length was needed.

An isolated public-API probe measured each group of 50 commits through 1000
updates. Allocation stepped once near update 225, then remained flat: roughly
2.10 to 5.90 MB with the secondary index and 1.20 to 5.01 MB without it. Updating
the value to itself stayed near 1.23 MB. The changed integer crosses the packed
region's one-byte offset range at this point, suggesting an encoding/layout
transition rather than unbounded history retention. That explanation is an
inference requiring storage-path confirmation. Write timing must account for
these layout transitions; a tiny initial-write sample is not representative.

Local evidence under `.dev/java-native/benchmarks/`:

- `matched-20261114-2202532-cx2oyy/evidence.json`: four-workload peer-checked smoke.
- `matched-20261114-2202532-cx2oyy/jfr-20261114-2204583-riwtuy`: shorter write profile.
- `matched-20261114-2202532-cx2oyy/jfr-20261114-2208350-8l8xb7`: longer write profile.

The isolated allocation trace is `.dev/java-native/commit-allocation-history-review.log`.
No durable-write timing or cross-language allocation equivalence is claimed.

The sizing follow-up shares layout decisions between encoding and size-only
calculation, avoiding output buffers and bit packing when only a candidate leaf's
length is needed. Serialization also constructs only the selected plain or
dictionary region. Existing golden bytes and split choices are unchanged. The
full host suite, 172 focused encrypted records, and four matched write smoke
workloads passed. Additional host checks compare measured and encoded lengths
across 72 width/row-count combinations, including nulls, packed arrays,
dictionaries and overflow descriptors.

The follow-up source is
`c06506cef70fb89b0369552d396ce79b46d2868310eb7825e991c590579d1118`.
With the same 4096 rows, 100 warmups and 500 measured executions, calling-thread
allocation changed as follows:

| Workload | Before sizing change | After sizing change |
| --- | ---: | ---: |
| Numeric commit | 4,904,568 B | 2,910,216 B |
| Text commit | 4,910,551 B | 2,917,131 B |
| Numeric rollback | 647,708 B | 360,149 B |
| Text rollback | 657,955 B | 369,126 B |

These observations show about 41% less allocation for commits and 44% for
rollbacks. Measured-phase compilation still accumulated 689–851 ms in the commit
workloads, so the recording does not establish steady-state latency. Its evidence
is `matched-20261114-2473995-qntixl/evidence.json` and profile directory
`matched-20261114-2473995-qntixl/jfr-20261114-2476860-4tj8rp`, under the same local
benchmark directory. Concurrent correctness work remained active.

## Matched baseline before resident aggregate optimization

The frozen `c06506ce` source completed all 24 workloads across five actors and
three process repeats, with exact rows, checksums and logical cost matching.
All 72 separate Java retained-heap probes also matched. Evidence is under
`.dev/java-native/benchmarks/matched-20261114-2680419-j21ev2/`: `evidence.json`
contains raw samples and identities; `summary.json` contains per-case medians,
ratios, allocations, GC, compilation, repeat variation and retained heap.
The context trace is `.dev/java-native/benchmark-controlled-second.context.jsonl`.

This is a bounded-warmup diagnostic on a shared Intel Core Ultra 9 285K host
(24 cores, default affinity across CPUs 0–23), using ext4 on
`/dev/mapper/ubuntu--vg-ubuntu--lv`. The first attempted run was stopped and
rejected after finding two stale Java checks. Those checks were terminated.
The restarted run's ten-second process-name trace shows no Go test and little
non-Java background CPU, but some unidentified Java processes overlap actor
intervals. Their ownership could not be recovered after exit. Therefore this
run does not establish idle-host performance. Sampling also cannot exclude
short-lived interference. File timings exercise ext4 and the synchronous/fsync
OS path; they do not measure hardware durability guarantees beyond that contract.

The Java versions are Temurin 25.0.5+8 and OpenJDK 28-ea+24; peers are Go 1.27.1
and Rust 1.99.0. Java 25 versus ordinary JDK 28 changes both compiler and VM
(the latter compiles with `--release 25`). The value-class build uses the same
JDK 28 compiler/VM with release 28 and preview enabled, selectively changing
`SqlValue`, `SqlUuid` and `SqlInterval`. It does not redesign collections into
specialized flattened storage.

Each historical time below is the median of three process upper medians. Ratios divide the named
actor's time by its comparator: values below 1 favor the numerator. These are
conditional workload observations, not converged steady-state estimates.

| Workload | Java 25 µs | Java 25 / Go | Java 25 / Rust | JDK 28 / Java 25 | Value / ordinary JDK 28 |
| --- | ---: | ---: | ---: | ---: | ---: |
| scalar | 5.35 | 9.08 | 3.28 | 0.98 | 1.01 |
| parallel_point | 1,543.59 | 6.89 | 4.21 | 1.08 | 1.00 |
| point | 6.76 | 4.82 | 2.72 | 0.99 | 1.05 |
| range | 60.76 | 2.08 | 2.06 | 1.53 | 0.78 |
| secondary | 158.14 | 3.63 | 3.32 | 1.01 | 0.96 |
| aggregate | 5,613.94 | 22.69 | 41.63 | 1.05 | 1.00 |
| join | 24,653.85 | 4.50 | 5.52 | 1.03 | 1.00 |
| sort | 830.53 | 0.75 | 1.84 | 1.04 | 1.05 |
| grouped | 7,883.60 | 3.64 | 4.75 | 1.00 | 1.06 |
| distinct | 1,593.35 | 0.62 | 1.12 | 1.03 | 1.00 |
| distinct_aggregate | 6,188.47 | 5.41 | 6.03 | 0.95 | 1.02 |
| window | 282.19 | 4.58 | 6.09 | 1.08 | 1.02 |
| ranking | 2,220.08 | 0.90 | 1.59 | 0.98 | 1.10 |
| text_filter | 1,066.66 | 2.97 | 3.54 | 1.00 | 0.98 |
| float_aggregate | 3,593.39 | 18.28 | 23.37 | 0.99 | 1.02 |
| decimal_aggregate | 5,000.62 | 0.88 | 9.68 | 1.00 | 1.01 |
| expression_projection | 44.00 | 1.97 | 1.71 | 1.04 | 0.99 |
| json | 90.71 | 1.41 | 2.42 | 1.11 | 1.02 |
| array | 70.24 | 1.79 | 2.30 | 1.00 | 1.11 |
| rollback | 163.09 | 7.49 | 9.15 | 1.00 | 0.99 |
| durable | 2,150.16 | 1.19 | 1.25 | 1.07 | 1.03 |
| text_rollback | 118.76 | 3.65 | 4.44 | 1.05 | 1.01 |
| text_durable | 2,074.04 | 1.09 | 1.39 | 1.13 | 0.99 |
| cold_open | 497.41 | 2.32 | 2.53 | 1.05 | 1.05 |

The concurrent-read sample comprises 256 queries across four newly launched
threads, not one query. Compilation remains active in the measured Java phases:
2.9 s total for Java 25, 4.5 s for ordinary JDK 28 and 4.7 s for the value build
across all three repeats. Scalar-query late thirds remain 13–23% faster than
early thirds. Range-read process medians vary from 63–98 µs on ordinary JDK 28
and 66–94 µs on the value build; its apparent 22% value-class advantage is not
robust evidence. Durable numeric medians range from 2.00–2.44 ms on Java 25
and 1.24–1.87 ms on Rust. Raw samples and trend fields preserve these variations.

The stable Java build is slower than both peers on most tested cases. Its
largest gaps are ordinary numeric aggregates: about 23× Go / 42× Rust for
count-plus-sum, and 18× / 23× for floating sum. Java is faster than Go in this
sample's sort, DISTINCT, ranking and exact-decimal aggregate cases; Rust remains
faster there. These results identify further implementation work rather than a
reason to require an unreleased Java version.

Selective value classes show no broad latency benefit over ordinary JDK 28.
Their allocation counters improve by roughly 5% for point/concurrent/secondary
reads, remain close for many folds and writes, and worsen for some other cases.
The much larger diagnostic gains earlier in this document came from removing
repeated work and serialization, without depending on value classes. A stable
Java 25 baseline remains reasonable; a future value-class migration can remain
an opt-in experiment. Broader data-layout changes could behave differently and
are not measured here.

Startup medians over 15 fresh processes are 76.3 ms (Java 25), 80.0 ms (ordinary
JDK 28), 82.4 ms (value classes), 1.97 ms (Go) and 1.12 ms (Rust). These measure
launch-to-ready, not first SQL execution. Whole-process peak RSS, including
setup/warmup/all workloads, ranges from 1.36–1.97 GiB for Java, 25.8–27.0 MiB
for Go and 15.5–16.1 MiB for Rust. JVM allocation/GC ergonomics and live heap are
separate measurements; this RSS range is not a minimum embedding requirement.

Calling-thread allocation per execution is about 14.5 MB for Java 25's numeric
aggregate versus 19.4 kB process-wide for Go; joins allocate 83.0 MB versus 8.72 MB.
Java 25/ordinary JDK 28/value builds record 62/69/72 measured GC collections and
166/160/149 ms GC time across all repeats; Go records 3179 collections and
158 ms pause time. Counter scopes differ, and Rust allocation/GC counters are
unavailable. A zero must not be inferred for Rust.

Separate requested-GC probes put Java 25's live heap near 7.6–8.0 MB for the
representative aggregate/join/grouped/commit cases and near 4.3–4.6 MB after
closing; ordinary JDK 28 is around 6.8–7.2 MB live and 3.7–4.0 MB after close.
These are whole-process Java heap observations, including previously loaded
classes and caches, excluding native/JIT/metaspace memory. They neither prove
collection completeness nor bound the peak working set. Full per-case values,
including the cold-open path, remain in the summary.
