# Primary-key lookup and Ruby JIT follow-up

> Historical report: raw evidence and run-specific drivers are on the preserved
> experiment branch. See the [archive and retrieval instructions](../../bench/ruby-native/ARCHIVE.md).
> Reproduction commands below refer to that original checkout.

This report preserves the first follow-up's measurements. The explicitly requested
[second lookup pass](LOOKUP_SECOND_PASS.md) records subsequent improvements and
repeated measurements against Go.

This is the explicitly requested follow-up to the [initial experiment](REPORT.md).
The initial 58× gap against Go included substantial avoidable Ruby work. This pass
removes repeated planning, type parsing, structural hashing, and cursor work,
uses stdlib Zlib for CRC32, and measures a separately built Ruby 4.1.0-preview1
with actual YJIT and ZJIT activation. File primary-key reads improve from
**162.15 to 54.12 µs with Ruby 4.0 YJIT**, and to **43.84 µs with Ruby 4.1 YJIT**.
The best result is about **19× Go**, down from the original reported 58× gap.
The core remains a frozen, opt-in experiment;
normal tests, benchmarks, CI, and future feature work do not include it.

## Source and measurement contract

The comparison starts from commit `c3ba8e69f556af0dbd5e24876a9092a557cc8c10`, whose
Ruby library hash is
`05b5aa0e67adf3fd2c19f99059397772d141ed0f0d3a6094eefde9702c5adbfb`.
That is the initial report's completed optimized core, not its earlier working
baseline or incomplete prototype. The final library hash for this follow-up is
`b7275e0ee4b632191e078d24ee3134961b607fdad778f5575754b2b4cf7b0456`.
Timing workers load immutable copies and check the hash before and after each run.
The supported cores and canonical SQL/parameter/checksum adapters are unchanged.

Host: Intel Core Ultra 9 285K, x86-64 Linux, ext4; Ruby 4.0.7 and the local
4.1.0-preview1 build, Go 1.27.1, Rust 1.99.0, Node 26.10.0. Timing lanes run
sequentially, without concurrent test/build work from this task. The host is not
dedicated or CPU-isolated; small timing differences should not be overinterpreted.

## Why lookup was slow

The diagnostic runs the canonical four-column primary-key query against the
million-row file, after 50,000 warmup calls. Five shorter timing batches separate
lazy cursor construction from draining the result. TracePoint call counts are
collected afterward; allocation tracing is separate. Traced timings are never
presented as benchmark results.

Before this follow-up, a lookup entered about 835 Ruby methods and allocated
about 1,307 objects. Several costs were unrelated to finding the row:

- A cursor helper created a new AST with `OFFSET 0` every time. This defeated
  identity-based shape caches and repeated recursive expression inspection.
- Resolved point queries still invoked general access-path selection. They now
  retain an eligible integer-primary-key point descriptor and perform one descent.
  Competing indexes, general predicates, ordering, limits, and budget/profile
  paths retain the ordinary planner and charge schedule.
- Leaf cache keys recursively hashed column descriptions on every field access.
  They now use schema identity, retaining that schema in the bounded cache entry
  so object-ID reuse cannot alias a different schema.
- Already-canonical scalar type names repeatedly went through regex parsing, and
  integer checks rebuilt large bounds. Finite canonical-name and bound tables
  eliminate that work; aliases, modifiers, arrays and custom types keep the
  original normalization path.
- Cursor construction and execution coerced primitive bindings twice. A cursor
  now reuses its privately copied primitive bindings; temporal/container values
  retain their metering-sensitive path. Exhausting `each` no longer constructs
  a `StopIteration` backtrace, while public `next` preserves its exception API.
- Read-only cursor cleanup refreshed unchanged primary indexes. It now skips
  that work only for an unchanged cursor engine. Rollback snapshots, pins and
  mutation invalidation remain intact.
- Fixed-width integer bytes now use Ruby's `pack`/`unpack`, and storage enumeration
  skips sorting/allocation work when no attachments exist.

The original diagnostic measured **161.48 µs/query**. Before the final two small
changes it measured **57.20 µs**, with about **506 allocations** and **358 Ruby
method calls**. The final two changes reduce allocations further to **432/query**
(67% below the original). Their isolated timing samples overlap the preceding
version, so this report does not attribute a precise latency gain to them.

The [before profile](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/lookup-followup/before-profile.json`),
[intermediate profile](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/lookup-followup/after-profile.json`),
[final-source profile](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/lookup-followup/candidate-profile.json`),
and [adjacent control](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/lookup-followup/candidate-control-profile.json`)
retain all samples and checksums. Final-source profiling still sees **zero CRC32,
CRC64, page-CRC or meta-CRC calls per warmed lookup**. Checksums are not the cause
of the remaining hot-read gap. The benchmark's separate answer-checksum replay
takes about 3.35 µs/row, also too little to explain it.

Remaining work includes cursor/snapshot isolation, host value copying, value
decoding, and layered Ruby method dispatch. A larger reduction would require
broader execution/representation changes. These observations describe this
implementation, not a lower bound on Ruby database performance.

## CRC32 uses the standard library

Ruby already exposes the required CRC32 through
[`Zlib.crc32`](https://docs.ruby-lang.org/en/4.0/Zlib.html#method-c-crc32), including
the seed needed to continue across disjoint page spans. With the maintainer's
utility-dependency approval, the handwritten CRC32 implementation is replaced
by Zlib; page/meta checksums avoid concatenating the two spans. No gem was added.
CRC64/ECMA-182 remains Ruby; Zlib does not provide that algorithm. OpenSSL continues
to supply the previously approved digest functions, with no digest omission.

The 8 KiB CRC32 kernel falls from **235.75 to 4.24 µs** without a JIT and from
**15.98 to 4.24 µs** with YJIT. These are kernel measurements, not SQL speedups.
The independent polynomial checks, seeded continuation cases, shared byte images,
recovery checks and page-service wire fixtures verify unchanged encodings.
[Checksum evidence](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/lookup-followup/CHECKSUMS.md`)
records methods, limits and raw results.

## Ruby 4.1 and ZJIT

The installed Ruby 4.0.7 was compiled with YJIT only. `--zjit` warns and exits
successfully without enabling ZJIT, so checking process exit status is insufficient.
Following the requested fallback, Ruby **4.1.0-preview1** was built locally with
`--enable-yjit --enable-zjit`; the repository's normal runtime remains unchanged.
Each activation probe checks the corresponding `RubyVM` enabled flag, verifies
1,000 prepared-query answers, and requires compiled methods for the JIT modes.
Timed runs omit JIT statistics flags.

[Runtime instructions](../../bench/ruby-native/RUNTIMES.md) document the official
release archive, verified SHA-256, offline bundled-gem build, local installation,
and exact activation commands. The [runtime artifact](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/lookup-followup/runtime-build.json`)
retains the failed initial bundled-gem fetch and successful offline retry. Only
existing approved benchmark dependencies were installed from cached archives.

## Final benchmark results

### In-memory prepared queries

Median batch time per operation in **microseconds**, lower is better. Every lane
uses the same 1,000-row data, seven-query plan, five batches and 32 independent
answer-checksum cases per query. The original short warmups are unchanged; these
are not uniformly long-warmed steady-state JIT measurements. The file lookup
comparison below has the canonical 50,000-call warmup.

| Workload | Original 4.0 YJIT | Final 4.0 YJIT | Final 4.1 YJIT | Final 4.1 ZJIT | Go | Rust | TypeScript |
|---|---:|---:|---:|---:|---:|---:|---:|
| Scalar | 54.74 | 30.55 | 24.62 | 28.44 | 0.71 | 1.40 | 1.75 |
| Primary-key hit | 80.48 | 23.80 | 19.45 | 22.96 | 1.61 | 2.23 | 4.21 |
| Primary-key miss | 74.50 | 21.98 | 17.18 | 20.89 | 1.08 | 1.78 | 2.52 |
| Filtered scan | 3283.27 | 1067.33 | 948.94 | 1142.02 | 152.67 | 147.67 | 327.83 |
| Aggregate | 3744.41 | 1029.44 | 983.55 | 1132.18 | 64.91 | 51.94 | 176.24 |
| Group | 5609.54 | 1997.66 | 2060.94 | 2024.84 | 241.60 | 172.80 | 456.30 |
| Sort and limit | 1831.63 | 1928.71 | 1667.78 | 1696.23 | 199.98 | 135.52 | 321.62 |

| Workload | Original 4.0 interpreter | Final 4.0 interpreter | Final 4.1 interpreter |
|---|---:|---:|---:|
| Scalar | 104.35 | 66.92 | 60.05 |
| Primary-key hit | 138.24 | 54.51 | 49.98 |
| Primary-key miss | 129.11 | 48.02 | 43.92 |
| Filtered scan | 4678.94 | 1914.30 | 1828.84 |
| Aggregate | 4486.52 | 1621.75 | 1554.33 |
| Group | 6468.43 | 2931.72 | 2793.23 |
| Sort and limit | 2711.57 | 2824.36 | 2561.44 |

On the same Ruby 4.0 runtime, YJIT primary-key hits improve **3.38×** and aggregate
queries improve **3.64×**. Canonical-type normalization benefits row-heavy queries
as well as point reads. Improvements are not uniform: the 4.0 sort/limit sample is
about 5% slower, while 4.1 improves that workload. YJIT is generally faster than
ZJIT here; their group results are close. Final 4.1 YJIT's memory PK hit still
takes about **12× Go** and **4.6× TypeScript**.

[Ruby 4.0 and supported-core results](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/lookup-4_0_7.json`)
and [Ruby 4.1 results](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/lookup-4_1_preview1.json`)
include all samples, checksums, runtime modes, hashes and memory peaks. Ruby
memory-query workers peak at 73.3 MiB across these runs.

### Canonical real-file workloads

Mean **microseconds per operation**, lower is better. Each cell is one canonical
run with the original iteration count. The original two rows come from the
preserved initial report; all other rows use the final source or unchanged
supported cores in this follow-up.

| Runtime/core | Primary-key read | Cold handle open + read | Insert + durable commit |
|---|---:|---:|---:|
| Original Ruby 4.0.7 interpreter | 257.51 | 44895.43 | 9171.87 |
| Original Ruby 4.0.7 YJIT | 162.15 | 5704.20 | 5907.81 |
| Final Ruby 4.0.7 interpreter | 98.48 | 26807.85 | 5377.50 |
| Final Ruby 4.0.7 YJIT | 54.12 | 3978.46 | 5498.92 |
| Final Ruby 4.1 preview interpreter | 93.71 | 24037.71 | 6465.79 |
| Final Ruby 4.1 preview YJIT | **43.84** | **4203.66** | **4880.64** |
| Final Ruby 4.1 preview ZJIT | 49.75 | 8143.91 | 5626.71 |
| Go | 2.30 | 526.04 | 2012.23 |
| Rust | 2.41 | 446.31 | 1815.47 |
| TypeScript | 7.66 | 1133.29 | 1998.87 |

The source changes give almost **3× faster file lookups on the same Ruby 4.0
YJIT runtime**. Ruby 4.1 YJIT adds another 1.23×, for **3.70×** versus the original
report. The Go gap is about **23.5× with 4.0 YJIT** or **19.0× with 4.1 YJIT**;
the latter is **5.7× TypeScript**. The original 58× difference was not an
unavoidable language penalty, but a substantial gap remains after these fixes.

ZJIT is active and competitive on hot reads, but YJIT wins all three final file
workloads on the same preview runtime. ZJIT cold opens take about twice as long
under the canonical short warmup. Durable-commit results vary more than reads,
and JIT/runtime changes do not improve them uniformly. These measurements support
retaining the experiment, not promoting it on performance grounds.

All **24 file cells** and **56 memory-query cells** completed with matching
row counts/checksums across cores and runtime versions. Ruby file workers peaked
at **337.4 MiB**, below the 512 MiB limit. The
[Ruby 4.0/supported-core file report](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/lookup-followup/final-file-4.0.7.json`),
[Ruby 4.1 file report](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/lookup-followup/final-file-4.1-preview1.json`),
and [cross-report summary](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/lookup-followup/final-summary.json`)
retain the precise totals, source hashes, answer checks, percentiles and resource
measurements. The summary also records hashes of its input artifacts.

## Final validation

| Audit | Result | Peak sampled worker RSS |
|---|---:|---:|
| Full memory SQL/concurrency corpus, Ruby 4.1 ZJIT | 20,267 / 20,267 | 138.3 MiB |
| Full disk SQL/concurrency corpus, Ruby 4.1 YJIT | 18,250 / 18,250 | 200.8 MiB |
| Host/storage suite, Ruby 4.0.7, 39 test files | Pass | 186.7 MiB |

All three audits record the final `b7275e0e…` source before and after execution.
There are zero unsupported harness files, failures, timeouts or memory-limit
failures. The corpus's existing storage-mode rules skip two files in memory and
77 on disk; these are not Ruby feature omissions. The final host suite includes
106 shared file images, 65 byte-exact encodings, 38 page-service wire fixtures,
live owner/client behavior, attachments, snapshots, recovery and resource guards.
New host assertions cover point-plan reuse and invalidation, one-descent lazy
execution, competing-index fallback, cost parity, canonical-type ownership,
cursor bindings and bounded schema-identity caches.

Raw reports: [memory](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/lookup-followup/final-conformance-memory-4.1-zjit.json`),
[disk](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/lookup-followup/final-conformance-disk-4.1-yjit.json`),
and [host](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/lookup-followup/final-host-4.0.7.json`).

## Reproduction and provenance

All tools are explicit experiment tools. The file comparison uses private copies
of the existing approximately 106 MiB and 1.5 MiB standard datasets on ext4,
exclusive locking and normal synchronous durability. It preserves the canonical
50,000 warmup plus 50,000 measured primary-key reads, 10 plus 200 cold handle opens,
and 10 plus 300 durable inserts. A cold open is a new handle, not a flushed OS cache.
Go includes parameter generation in query/commit timing; Ruby, Rust and TypeScript
prepare those parameters immediately before timing. Cold-open parameters are
prepared before timing in every lane. This existing adapter difference is unchanged.

```sh
mise run ruby-native:test
mise run ruby-native:file-compare -- --run --lanes ruby,ruby-yjit,go,rust,ts \
  --report /tmp/file-ruby-4.0.json
RUBY_NATIVE_PHASE=lookup-4_0_7 RUBY_NATIVE_LANES=ruby,ruby-yjit,go,rust,ts \
  mise run ruby-native:compare

ruby_experiment="$PWD/.dev/ruby-native-runtimes/ruby-4.1.0-preview1/bin/ruby"
JED_NATIVE_TIMEOUT=1800 JED_NATIVE_ADDRESS_SPACE_BYTES=17179869184 \
  mise run ruby-native:bounded -- "$ruby_experiment" bench/ruby-native/file_compare.rb \
  --run --lanes ruby,ruby-yjit,ruby-zjit --report /tmp/file-ruby-4.1.json
RUBY_NATIVE_PHASE=lookup-4_1_preview1 RUBY_NATIVE_LANES=ruby,ruby-yjit,ruby-zjit \
  JED_NATIVE_TIMEOUT=1800 JED_NATIVE_ADDRESS_SPACE_BYTES=17179869184 \
  mise run ruby-native:bounded -- env TMPDIR=/tmp "$ruby_experiment" bench/ruby-native/compare.rb
JED_NATIVE_TIMEOUT=900 JED_NATIVE_FILE_TIMEOUT=60 JED_NATIVE_ADDRESS_SPACE_BYTES=2147483648 \
  JED_NATIVE_REPORT=/tmp/memory-zjit.json mise run ruby-native:bounded -- \
  "$ruby_experiment" --zjit impl/ruby-native/bin/conformance.rb
JED_NATIVE_TIMEOUT=900 JED_NATIVE_FILE_TIMEOUT=60 JED_NATIVE_ADDRESS_SPACE_BYTES=2147483648 \
  JED_NATIVE_STORAGE=disk JED_NATIVE_LOCKING=exclusive JED_NATIVE_REPORT=/tmp/disk-yjit.json \
  mise run ruby-native:bounded -- "$ruby_experiment" --yjit impl/ruby-native/bin/conformance.rb
```

The 16 GiB outer address-space reservation permits Go/V8 virtual reservations;
it does not raise the 512 MiB measured-worker RSS limit. Ruby file/JIT workers
use a separate 2 GiB address-space ceiling. Conformance runs one file per child
with a 60-second deadline and 512 MiB sampled RSS ceiling. These are process
guardrails, not a claim that SQL memory budgets cap the whole Ruby heap.

Initial attempts to rebuild the existing memory-comparison adapters were stopped
by the 512 MiB and 1 GiB compiler limits. The successful single-job build peaked
at 1.29 GiB under a separate 2 GiB compilation limit. No measured database worker
received that larger RSS limit. The failed attempts and successful build are
retained in the result directory. The outer supervisor's temporary directory
initially isolated the compiler cache too, causing a subsequent unnecessary rebuild
and another bounded stop. Passing `TMPDIR=/tmp` explicitly to the comparison driver
reuses its existing cache; its database workers still receive isolated workspaces
and their original limits.

Raw artifacts keep their actual source hashes. The `intermediate-*`, `after-profile`,
unprefixed conformance/host and unprefixed file-comparison reports describe earlier
stages, not the final source. Supplemental microkernel and allocation artifacts
have their own [provenance notes](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/lookup-followup/diagnostic-provenance.json`).
The original experiment report and its measurements remain unchanged apart from
the link to this follow-up.
