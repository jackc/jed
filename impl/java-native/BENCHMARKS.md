# Java experiment measurements

The experiment has matched benchmark infrastructure and diagnostic allocation
profiles. Repeated controlled timing comparisons are pending. These results do
not establish Go/Rust performance parity or a benefit from Valhalla value classes.
The semantic baseline is `3e5962f9affc06c768df1688765162a65a893848`, format v53.

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
