# Ruby lookup: second optimization pass

This explicitly requested continuation brings the canonical file primary-key
lookup **within 10× Go with both YJIT and ZJIT**. Across three runs per Ruby JIT,
the median is **16.99 µs on Ruby 4.0.7 YJIT**, **17.64 µs on Ruby 4.1 preview
YJIT**, and **18.88 µs on Ruby 4.1 preview ZJIT**. Go's median across five runs
is **2.69 µs**. These are medians of complete canonical run means, not individual
query percentiles. The improvement is specific to eligible lazy point reads;
other query shapes and durable writes do not receive the same speedup.

The core remains a frozen experiment, excluded from normal tests, benchmarks,
CI, and future feature updates unless specifically requested. This work adds no
dependency and does not promote the experiment to supported status. The
[first follow-up](LOOKUP_FOLLOWUP.md) and [initial report](REPORT.md) retain their
historical measurements and original coverage record.

## Source and measurement contract

Baseline commit: `222b81aeddfd97547b042c629d7c3f4fb4617554`.
Baseline Ruby library SHA-256:
`b7275e0ee4b632191e078d24ee3134961b607fdad778f5575754b2b4cf7b0456`.
Final Ruby library SHA-256:
`00fa6ca66444430c9715160c1a59c6d00c6432a4f83a810fb2b8e0b3a1041ae3`.
Timing workers load immutable copies and record the source hash before and after
execution. The SQL, parameters, datasets, checksums, adapters, supported cores,
and JIT build are unchanged from the previous follow-up.

Host: Intel Core Ultra 9 285K, x86-64 Linux, ext4; Ruby 4.0.7 and the separate
4.1.0-preview1 build, Go 1.27.1, Rust 1.99.0, Node 26.10.0. Timings run
sequentially without concurrent tests or builds from this task. This is a shared
host, so repeat ranges matter more than small differences between Ruby JITs.

## What changed

`query` and `query_prepared` can now construct a small `PointRows` cursor for
bare-column/star projections over an exact integer primary-key equality. It
retains the resolved point plan, immutable table version, bindings, and reader
pin. It avoids duplicating the entire engine and entering a Fiber plus the
general execution stack for a single row. `execute` and `execute_prepared` retain
their ordinary eager execution path; their performance must not be inferred from
the lazy lookup measurements.

Eligibility is deliberately narrow: a single main, non-temporary table; a bare
integer literal or parameter bound; no transaction, competing access path,
ordering, limit, distinct, join, expression projection, or SELECT INTO.
Cost ceilings, lifetime ceilings, query-memory limits, and profiling retain the
ordinary executor. The existing capability preflight still applies, including
authorized read-only sessions. Plan reuse checks table/type/environment identity
and timezone/collation context, with a 64-entry bound.

Construction still validates parameters and permissions immediately. Reading and
cost start on first pull. The cursor retains its original snapshot across later
writes, uses the existing local/remote pin hooks, checks storage poison/closure
and page-service generation before reading, and releases its retained state on
exhaustion, close, or error. Each charge updates the session's live cumulative
cost. Public row values and metadata remain caller-owned copies; text and text
arrays retain their collation labels. General transactional and resource-limited
cursors keep their previous lifecycle and accounting.

Other reductions apply below the cursor:

- Fixed PAX slots decode directly with `unpack`; inline text/bytea/JSON avoid
  synthetic presence tags and the recursive decoder. Extent, boolean, and UTF-8
  validation remain in place. Overflow/compressed forms retain their codecs.
- Paged rows track loaded fields with a bitset; leaves and pools retain reusable
  cache keys and overflow-fetch methods, avoiding repeated setup and a second
  leaf-cache fetch during key lookup.
  Decoded-leaf eviction and generation invalidation remain bounded and unchanged.
- Integer bindings perform their exact range check directly, and already-decoded
  primitive storage values avoid redundant type dispatch. Container and nominal
  type conversion retain their existing paths.
- Privilege checks avoid sorting; watermark callbacks are reused bound methods;
  page-service generation uses an integer equality token that still observes a
  connection failure immediately. Unchanged single-file publication checks avoid
  constructing attachment inventories.

The same Ruby 4.1 YJIT diagnostic drops from about **449 to 201 allocations per
lookup**, including the unchanged answer checksum. Its median falls from 53.51
to 25.00 µs, but its extra timers/checks make it unsuitable as the canonical
latency figure. Separate allocation tracing and TracePoint run outside timing.
The allocation trace attributes **256 → 27 objects per lookup** to core Ruby
source; that subset excludes Ruby internal allocations and the unchanged
benchmark checksum. [Allocation evidence](../../bench/ruby-native/results/lookup-second-pass/final-allocations.json)
records the complete categories against the [baseline](../../bench/ruby-native/results/lookup-second-pass/before-allocations.json).
[Before](../../bench/ruby-native/results/lookup-second-pass/before-profile.json)
and [after](../../bench/ruby-native/results/lookup-second-pass/final-profile.json)
retain all samples, including slower batches rather than discarding them.

## Canonical file results

Mean microseconds per operation for the first full final-source run. Lookup uses
the million-row file and 50,000 warmup plus 50,000 measured queries. Cold handle
open uses 10 plus 200 iterations; durable insert/commit uses 10 plus 300.

| Runtime/core | PK lookup | Cold open + lookup | Insert + durable commit |
|---|---:|---:|---:|
| Ruby 4.0.7 interpreter | 36.57 | 26559.60 | 6999.44 |
| Ruby 4.0.7 YJIT | 17.09 | 4095.62 | 6005.16 |
| Ruby 4.1 preview interpreter | 37.18 | 25471.26 | 6713.41 |
| Ruby 4.1 preview YJIT | 18.87 | 4200.31 | 5112.11 |
| Ruby 4.1 preview ZJIT | 20.57 | 8184.77 | 5081.38 |
| Go | 2.74 | 488.25 | 2037.15 |
| Rust | 2.36 | 438.74 | 1768.03 |
| TypeScript | 7.65 | 1141.17 | 2001.77 |

Two additional complete lookup runs per Ruby JIT confirm the result:

| Runtime/core | Runs | Median run mean, µs | Range, µs |
|---|---:|---:|---:|
| Ruby 4.0.7 YJIT | 3 | 16.99 | 16.91–17.09 |
| Ruby 4.1 preview YJIT | 3 | 17.64 | 17.57–18.87 |
| Ruby 4.1 preview ZJIT | 3 | 18.88 | 17.57–20.57 |
| Go | 5 | 2.69 | 2.19–2.75 |

Ratios of these medians are **6.3×, 6.5×, and 7.0× Go**, respectively. Even the
largest Ruby JIT run mean divided by the smallest Go run mean is below 10×.
On the same 4.0 YJIT runtime, the first full run improves **3.17×** over the
previous follow-up's 54.12 µs; 4.1 YJIT improves **2.32×** over its 43.84 µs.
The repeat samples overlap enough that this does not establish a general runtime
winner. Both JITs remain actually enabled, with no interpreter fallback.

Cold-open latency is broadly unchanged. The initial durable-write samples are
slower than the historical report for both 4.0 modes and 4.1 YJIT. Adjacent 4.0
baseline/final controls give **6612.62 → 7306.83 µs** for the interpreter (+10.5%)
and **5390.81 → 5458.12 µs** for YJIT (+1.25%). The interpreter remains slower in
this check; the historical gaps alone would overstate the YJIT difference.
These are single paired runs, not enough to establish the cause of a regression
or claim a write-speedup. [Baseline control](../../bench/ruby-native/results/lookup-second-pass/commit-control-before.json)
and [final control](../../bench/ruby-native/results/lookup-second-pass/commit-control-after.json)
retain the exact source hashes and results.

Raw full reports: [4.0 and supported cores](../../bench/ruby-native/results/lookup-second-pass/final-file-4.0.7.json),
[4.1 modes](../../bench/ruby-native/results/lookup-second-pass/final-file-4.1-preview1.json).
Repeats: [4.0 #1](../../bench/ruby-native/results/lookup-second-pass/repeat-4.0.7-1.json),
[4.0 #2](../../bench/ruby-native/results/lookup-second-pass/repeat-4.0.7-2.json),
[4.1 #1](../../bench/ruby-native/results/lookup-second-pass/repeat-4.1-preview1-1.json),
[4.1 #2](../../bench/ruby-native/results/lookup-second-pass/repeat-4.1-preview1-2.json).

## Memory queries

Median microseconds per operation across five batches on the unchanged 1,000-row
plan, with 32 answer-checksum cases per query. Short canonical warmups remain
unchanged; these are not uniformly steady-state measurements for either JIT.

| Query | 4.0 YJIT | 4.1 YJIT | 4.1 ZJIT | Go | Rust | TypeScript |
|---|---:|---:|---:|---:|---:|---:|
| Scalar | 26.74 | 23.12 | 25.45 | 0.81 | 1.40 | 2.08 |
| PK hit | 4.11 | 4.08 | 3.89 | 1.47 | 2.25 | 4.92 |
| PK miss | 3.23 | 3.16 | 3.35 | 1.19 | 1.77 | 3.20 |
| Filtered scan | 944.44 | 929.63 | 1105.45 | 165.24 | 110.42 | 430.66 |
| Aggregate | 905.04 | 986.35 | 1086.95 | 64.98 | 56.18 | 199.86 |
| Group | 1899.11 | 2156.47 | 1971.05 | 292.00 | 177.71 | 487.14 |
| Sort and limit | 1807.56 | 1576.88 | 1628.46 | 186.30 | 134.33 | 388.45 |

Memory PK hits are now **2.6–2.8× Go** with a JIT; interpreter hits are 8.17 µs
on 4.0 and 7.82 µs on 4.1. The narrow cursor helps these reads substantially,
while scalar and aggregate queries remain much more than 10× Go. Full samples,
interpreter results, checksums and memory peaks are in the [4.0 comparison](../../bench/ruby-native/results/lookup2-4_0_7.json)
and [4.1 comparison](../../bench/ruby-native/results/lookup2-4_1_preview1.json).

## Validation

| Final-source audit | Result | Peak sampled worker RSS |
|---|---:|---:|
| Memory SQL/concurrency corpus, Ruby 4.1 ZJIT | 20,267 / 20,267 | 142.6 MiB |
| Disk SQL/concurrency corpus, Ruby 4.1 YJIT | 18,250 / 18,250 | 207.6 MiB |
| Host/storage suite, Ruby 4.0.7 | 40 test files pass | 216.9 MiB |

Both corpus audits cover 679 files, with zero failures, unsupported files,
timeouts, or memory-limit failures. Existing storage-mode rules skip two files
in memory and 77 on disk. The shared corpus exercises the general eager
`execute` path; the new `PointRows` path is exercised by targeted lazy-cursor host
tests and benchmark answer checks. These are complementary coverage, not a claim
that every corpus case ran through the specialized cursor.

[Memory audit](../../bench/ruby-native/results/lookup-second-pass/final-conformance-memory-4.1-zjit.json),
[disk audit](../../bench/ruby-native/results/lookup-second-pass/final-conformance-disk-4.1-yjit.json),
and [host evidence](../../bench/ruby-native/results/lookup-second-pass/final-host-4.0.7.json)
record the final source hash before and after execution. Host coverage includes
shared file images, byte encodings, wire fixtures, live owner/client operation,
recovery, bounds, and cursor ownership/guard/fallback assertions.
Physical cursor cases verify external, compressed external, and compressed inline
values; nullable fixed-width values and i64 boundaries; untouched large-value
costs; and corrupt selected values. Decode failures preserve partial and lifetime
costs, omit the unproduced-row charge, and release pins. Unselected corrupt bodies
remain unread. All workers stay below the 512 MiB RSS limit.

All **24 full-file, 10 repeat-lookup, four durable-control, and 56 memory-query
cells** complete with matching answer checksums and row counts across the
corresponding cores/runtimes. The [cross-report summary](../../bench/ruby-native/results/lookup-second-pass/final-summary.json)
checks source hashes, actual JIT modes, answers, and resource bounds. Canonical
timing remains separate from tests, allocation tracing, and call instrumentation.

## Reproduction and limits

Use the explicit tasks and [runtime commands](../../bench/ruby-native/RUNTIMES.md)
documented in the [previous follow-up](LOOKUP_FOLLOWUP.md#reproduction-and-provenance).
For this pass, file reports go under `bench/ruby-native/results/lookup-second-pass/`;
memory comparison phases are `lookup2-4_0_7` and `lookup2-4_1_preview1`.
The experiment remains absent from normal test, benchmark, and CI tasks.

Canonical file workers use private dataset copies, exclusive locking, normal
synchronous durability, and a 512 MiB sampled RSS limit. A cold open does not
flush the operating-system cache. Go still includes parameter generation in
query/commit timing; the other adapters prepare it immediately before timing.
This existing difference is unchanged. Source hashes distinguish preliminary
`candidate-a-*` diagnostics from final results. Reaching the lookup target does
not establish general SQL parity in performance or justify promoting the core.
