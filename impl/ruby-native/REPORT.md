# Ruby core experiment

This is the initial experiment report, preserved with its original measurements.
The explicitly requested [lookup and ZJIT follow-up](LOOKUP_FOLLOWUP.md) records
subsequent optimizations, stdlib CRC32, and Ruby 4.1.0-preview1 measurements.

The independent Ruby port and optimization experiment are complete for the
recorded revision. It remains **experimental and frozen**, outside normal tests,
benchmarks, code generation and CI. Future features must leave it unchanged
unless explicitly requested; promotion requires a separate decision.

Ruby improved substantially, and YJIT helps, but the measured query workloads
remain materially slower than the supported cores. These results support keeping
this as an experiment rather than promoting it on performance grounds.

## Scope and reproducibility

Work began against `d4a01ebdbf3edff0c130b1fdb4c1115df6d1c4bd`. The completed engine
is recorded in commit `7807e37d`; its measured library SHA-256 is
`05b5aa0e67adf3fd2c19f99059397772d141ed0f0d3a6094eefde9702c5adbfb`.
Parser, planner, type system, evaluator, regex VM, storage and page service are
handwritten Ruby. SQL execution does not call another core, FFI or WASM.
Ruby's explicitly approved OpenSSL binding supplies vetted digest primitives;
all 19 digest conformance checks pass. No digest implementation is omitted.

The working baseline is preserved in
[working-baseline.json](../../bench/ruby-native/results/working-baseline.json)
and its verified [source archive](../../bench/ruby-native/results/working-baseline-source.tar.gz),
SHA-256 `d14527fb00cb90489d6eae07fc59fbf7d0a5309ce2e7bae03c7b34f6713008fd`.
It was recorded after the working SQL/host audits and before this optimization
pass. Subsequent stress testing also exposed correctness and resource bugs, so
before/after ratios include those fixes as well as optimization.
The older [prototype measurements](../../bench/ruby-native/results/baseline.json)
and archive remain available, but that port was incomplete and is not the working
baseline used below. File measurements replay the archived working source using
the same final adapter as optimized Ruby.

## Validation

| Audit | Result | Peak sampled worker RSS |
|---|---:|---:|
| Full memory SQL/concurrency corpus | 20,267 / 20,267 | 143.6 MiB |
| Full disk SQL/concurrency corpus, exclusive locking | 18,250 / 18,250 | 157.1 MiB |
| Host suite, 37 test files | Pass | 112.1 MiB |
| Ruby/Go process pairings | 108 / 108 | 231.5 MiB |

The full corpus considers 679 files. Its authored storage-mode rules exclude two
files in memory and 77 on disk (76 explicit disk skips and one memory-only
concurrency requirement). These are not Ruby feature omissions. There are zero
unsupported harness files, timeouts or memory-limit
failures in the successful full audits. Checks include values, types, names,
SQLSTATEs and deterministic costs. The record total includes the shared
concurrency assertions.

[Memory](results/final-memory-conformance.json) and
[disk](results/final-disk-conformance.json) reports identify source `3ba8840c…`.
The only subsequent engine change fixes terminal `25P03` handling in the page
service: after idle revocation, cleanup must not send a second abort. The final
[host](results/final-host.json) and [process](results/final-process.json) suites
verify the measured `05b5aa0e…` source, including a deterministic regression that
failed before the fix. The [initial race failure](results/process-idle-race.json)
is retained. An earlier complete [disk audit with default auto locking](results/audited-disk-auto-conformance.json)
also passed all 18,250 checks.

Additional coverage includes 106 shared file images (97 valid and nine rejected),
65 byte-exact encodings, 38 page-service wire fixtures, 31 regex program fixtures,
41 regex match/cost fixtures, 16 timezone vectors, 46 collation vectors, and
37 logical-memory vectors. Host checks cover prepared statements, independent
sessions, pinned/lazy cursors, settings and grants, scalar/batch host functions,
attachment durability, stored-value sharing, backup/compaction, recovery,
serialization and I/O failures, fork behavior and storage limits. A separate
[guard audit](results/host-gap-audit.json) checked suspicious unsupported branches
against active dispatch paths and the shared spec; no additional Ruby-only
omitted feature was found.

## Memory incident and containment

The maintainer stopped an earlier run at 49 GB. Its exact allocation path was
not established. The old runner kept the entire corpus in one process without
an OS allocation limit. It now runs files/workloads sequentially in isolated
children, with a 512 MiB sampled process-group RSS threshold, a hard address-space
limit, bounded diagnostics and an external deadline. Ruby defaults to 1 GiB of
address space; large/JIT benchmark lanes explicitly use 2 GiB. The mixed-process
harness allows 4 GiB of address space while retaining the 512 MiB RSS threshold.
A watchdog kills descendants if their supervisor exits, including during startup.
RSS is sampled, so it can overshoot the threshold before termination.

Confirmed allocation/retention bugs fixed during the experiment include:

- Closed benchmark handles retained all prior page caches; closed cursors also
  retained engines and snapshots. Both now release them. The 200-open regression
  completed at 154 MiB in its [recorded run](results/working-cold-open-memory.json).
- File scans, index builds and ANALYZE materialized entire populations or row-token
  arrays. They now stream pages and retain bounded state; decoded leaf caches
  share a budget across generations.
- Bounded window queries sorted a million rows before taking 100. They now walk
  primary-key order and retain only the required prefix, including composite and
  descending keys.
- Pending-write accounting rebuilt committed indexes to size one changed entry.
  It now sizes fresh entries only. Both formerly failing update smokes finish
  around 285 MiB.
- GiST commit planning retained padded empty page space and repeatedly rebuilt
  subtree metadata. Compact pending pages and reused metadata preserve exact
  on-disk bytes while bounding that overhead.
- Regex input/replacement handling retained expanded characters or every capture.
  Compact UTF-8 views and streaming replacement avoid those copies.

These fixes do not turn SQL's logical budgets into whole-engine heap caps.
`work_mem` bounds logical operator residency; Ruby objects add overhead.
The harness limits are separate containment for the experiment.

## In-memory query measurements

Host: Intel Core Ultra 9 285K, x86-64 Linux; Ruby 4.0.7, Go 1.27.1,
Rust 1.99.0, Node 26.10.0. All lanes use the same 1,000-row data, prepared
queries, parameter schedule, warmup, five timing batches and 32 independent
checksum cases per query. Every checksum matches. Values below are median
batch time per operation in **microseconds**, lower is better.

| Workload | Working Ruby | Working YJIT | Optimized Ruby | Optimized YJIT |
|---|---:|---:|---:|---:|
| Scalar | 401.92 | 263.95 | 104.35 | 54.74 |
| Primary-key hit | 342.85 | 210.97 | 138.24 | 80.48 |
| Primary-key miss | 352.04 | 198.38 | 129.11 | 74.50 |
| Filtered scan | 12646.52 | 6581.95 | 4678.94 | 3283.27 |
| Aggregate | 12386.73 | 6193.48 | 4486.52 | 3744.41 |
| Group | 10349.34 | 5962.61 | 6468.43 | 5609.54 |
| Sort and limit | 5625.76 | 2325.36 | 2711.57 | 1831.63 |

Interpreter speedups range from 1.60× to 3.85×; YJIT speedups over the working
YJIT baseline range from 1.06× to 4.82×. Final YJIT improves on final interpreter
Ruby by about 1.15×–1.91× across these queries.

| Workload | Optimized Ruby YJIT | Go | Rust | TypeScript |
|---|---:|---:|---:|---:|
| Scalar | 54.74 | 0.78 | 1.39 | 2.21 |
| Primary-key hit | 80.48 | 1.41 | 2.24 | 4.76 |
| Primary-key miss | 74.50 | 0.98 | 1.80 | 2.94 |
| Filtered scan | 3283.27 | 167.86 | 109.81 | 444.19 |
| Aggregate | 3744.41 | 64.98 | 53.35 | 174.92 |
| Group | 5609.54 | 262.72 | 176.55 | 483.25 |
| Sort and limit | 1831.63 | 220.19 | 133.63 | 425.47 |

Optimized YJIT is still roughly 4.3×–25.3× slower than TypeScript here.
These are prepared in-memory queries, not predictions for all applications.
[Raw optimized results](../../bench/ruby-native/results/optimized.json) include
samples, runtimes, source hashes, plan and memory peaks. The host was
not dedicated or CPU-isolated; the benchmark lanes ran sequentially.

## Real-file comparison

These use the original standard SQL, parameter streams and measured iteration
counts: 50,000 warmups plus 50,000 primary-key reads; 10 warmups plus 200 cold
handle opens; 10 warmups plus 300 durable commits. Every lane receives a private
copy of the same file on the recorded **ext4** filesystem, with exclusive locking
and normal synchronous durability. Large and small datasets are approximately
106 MiB and 1.5 MiB. All seven lanes pass row-count/checksum comparisons.

Mean **microseconds per operation**, using the canonical benchmark harness:

| Workload | Working Ruby | Working YJIT | Optimized Ruby | Optimized YJIT | Go | Rust | TypeScript |
|---|---:|---:|---:|---:|---:|---:|---:|
| Primary-key read, large file | 512.99 | 315.18 | 257.51 | 162.15 | 2.79 | 2.39 | 7.05 |
| Cold handle open + read, small file | 63458.13 | 20045.86 | 44895.43 | 5704.20 | 532.15 | 480.00 | 1147.72 |
| Insert + durable commit | 13766.53 | 8868.57 | 9171.87 | 5907.81 | 2469.85 | 2456.06 | 2043.43 |

YJIT matters especially for cold opens: 44.9 ms becomes 5.7 ms (about 7.9×).
Against the working YJIT baseline, file reads improve about 1.94×, cold opens
3.51× and durable inserts 1.50×. Final YJIT durable inserts take about 5.9 ms
versus 2.0–2.5 ms in the supported cores; the read-side gap is considerably larger.

Cold open means a fresh database handle, not an empty OS page cache. Each cell is
one canonical run; its percentiles, checksums, dataset hashes, filesystem,
source/adapter hashes and resource limits are in
[file-comparison.json](../../bench/ruby-native/results/file-comparison.json).
The first attempt could not start TypeScript because the checkout lacked its
existing native OS-lock helper. The helper was built offline, then only the
three TypeScript cells were rerun; the original failures and retry reports are
retained. The foreign helper build needed a separate 1 GiB process limit (521 MiB
peak); all measured database workers retained their 512 MiB RSS threshold.
File-run artifacts did not record numeric `work_mem`; the launcher clears the
override and uses each core's defaults. The merger preserves that provenance
limitation rather than inferring an unrecorded value.
One existing harness difference remains: Go includes parameter generation in
query and durable-commit timing, while Ruby/Rust/TypeScript generate parameters
immediately before timing. All cold-open lanes generate parameters before
timing. The supported adapters' behavior was preserved.

## Storage kernels

Five-batch medians in **microseconds per operation**, on the checkout filesystem.
Full-snapshot cases include encoding, writes and fsync, then verify rows after
reopening; they exclude SQL planning/execution. They are not the incremental
SQL commit benchmark above. All checksum values remain identical.

| Operation | Working Ruby | Working YJIT | Optimized Ruby | Optimized YJIT |
|---|---:|---:|---:|---:|
| CRC32, 8 KiB | 293.86 | 110.95 | 223.55 | 15.05 |
| CRC64, 8 KiB | 564.42 | 120.69 | 321.26 | 29.45 |
| Full snapshot, 1 row | 7898.92 | 6039.61 | 8214.76 | 6549.14 |
| Full snapshot, 100 rows | 8617.05 | 5870.26 | 9275.66 | 5593.20 |

The YJIT CRC kernels improve about 7.4× (CRC32) and 4.1× (CRC64) over the
working YJIT baseline. Full-snapshot timings do not improve uniformly; the kernel
speedups should not be presented as equal gains for durable operations.

Raw results: [working interpreter](../../bench/ruby-native/results/working-baseline-storage.json),
[working YJIT](../../bench/ruby-native/results/working-baseline-yjit-storage.json),
[optimized interpreter](../../bench/ruby-native/results/optimized-storage.json),
[optimized YJIT](../../bench/ruby-native/results/optimized-yjit-storage.json).

## Standard workload coverage and optimization scope

All **139 standard workloads** have successful one-operation smoke results that
match canonical Go row counts and checksums. This is coverage, not timing of all
139 workloads. The initial complete attempt passed 116 and failed 23; targeted
fixes and retries completed the rest. The [merged evidence](../../bench/ruby-native/results/shared-smoke-comparison.json)
retains all 164 attempts, including 25 failures, and the exact source/configuration
of each success. It combines five source revisions and interpreter/YJIT runs;
it must not be read as one final-source/default-settings run.

Sixteen selected successes use an explicitly recorded 16 MiB `work_mem`, while
the library default remains 256 MiB. This permits large grouping/materialization
workloads to spill before Ruby object overhead reaches the 512 MiB worker limit.
Earlier metadata omitted the `work_mem` field; that is preserved as unknown.
The ANALYZE retry also records a before/after source-hash difference because its
malformed-row-count guard was restored after the worker loaded; subsequent
statistics and full SQL audits include that guard. Neither provenance warning
is silently discarded.

The optimization pass includes bounded parsed/resolved-plan and domain-routing
caches, generation-aware catalog snapshot reuse, simple typed evaluation paths,
shared INSERT/SELECT descriptors, exact fast CRC kernels, streamed scalar
aggregates, sparse paged mutations, shared overflow values, bounded index and
statistics construction, and the allocation fixes above. Cost/error behavior,
memory-budget paths and persisted bytes remain tested. The remaining query gap
calls for deeper execution/representation work rather than more local loop or
allocation substitutions; no further low/medium-effort opportunity was identified
in the inspected paths.

## Remaining limits and maintenance policy

No known implemented-spec feature was deliberately omitted from the Ruby port.
**ZJIT could not be measured**: the benchmark Ruby was built without it. Launchers
check actual activation and never label interpreter fallback as a JIT result.
YJIT was explicitly enabled and measured separately; the library enables no JIT.

File-host tests cover Linux `flock` and Unix page-service sockets. A host without
the required crash-clean lock fails closed. This experiment is not a claim of
testing every Ruby platform, every SQL combination or every workload's throughput.
Resource budgets retain the spec's distinction between logical accounting and
physical heap use.

Only `ruby-native:*` tasks select this experiment. See the [README](README.md)
for APIs and exact commands and the [experiment policy](../../spec/design/ruby-native-experiment.md)
for freezing and promotion. Normal core changes must not update this implementation,
its frozen catalog or these results without an explicit request.
