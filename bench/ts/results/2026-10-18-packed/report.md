# TypeScript packed result delivery

The 4,096-row typed result takes **29.97 ms before and 9.25 ms after**, a **69.1% latency reduction
(3.24× throughput for this sequential drain)**. The direct core stays around 5 ms. Small-call
results do not establish a broad improvement: cheap worker queries are about 3.76 µs slower,
prepared execution about 0.60 µs faster, and the 100-statement batch varies widely between runs.

## Change and motivation

CPU profiling of the previous implementation identified worker `postMessage` result serialization
as a larger cost than its value validation/copying routines. Results were recursive object graphs,
including version envelopes and separate byte arrays, that structured clone serialized and
reconstructed across the port. Each client row also went through validation, copying and a chain
of promises even when its batch was already resident.

The worker now preflights each row against the same logical byte allowance and writes it into an
owned binary batch. Node and browser adapters transfer that batch's `ArrayBuffer`. The client
validates the complete encoded and expanded batch before building independent typed values.
Resident iteration returns a resolved promise without scheduling another promise chain. Exact
bigints, decimal limbs, float bits, raw UTF-16 and recursive value metadata survive unchanged.
Request parameters retain their existing encoding. Runtime protocol 2 rejects older worker/client
bundles; the SQL API, costs, database format and default limits are unchanged.

No engine evaluator, execution-loop checkpoint or generator-driver changes are included in this
comparison. The remaining gap includes packing/decoding, async iteration, worker messages and
cooperative execution. This does not establish that all remaining overhead is fixed per call.

## Method

Before is frozen commit `4f7e376d422cbc7431f8d588f028bcce15ab733e`; after is this report's accompanying
working-tree implementation based on that commit. The raw after JSON therefore identifies the
base commit and marks the working tree modified. Three independent process runs per variant use
`v26.10.0`, linux/x64, Intel(R) Core(TM) Ultra 9 285K, with fixed CPU affinity
`taskset -c 0,2`. Retained runs in time order are before-2, before-3, after-3, after-1, before-1,
after-2. Preliminary profiling/prototype runs are excluded. All reported after runs include the
final decoder metadata validation change.

The existing `mise run bench:ts_async` harness uses 200 warmups per probe, 200 cheap/prepared
samples, 30 batch samples, 15 large-result samples, and 10 startup samples. The baseline runs
an archived copy of the same harness through `mise exec -- node`; its only harness difference is
the erased TypeScript annotation for counting batch rows. Within each probe, direct/worker
measurement order alternates, and values, checksums, command results and costs are compared
outside the timed interval. Large results include the complete typed iteration and close; the
batch is 100 autocommit SELECT statements. All queries use warm in-memory sessions.

The table reports the median of the three process medians, in microseconds. Individual medians
are shown to make process variation visible; these are not pooled percentiles or confidence
intervals. Raw samples, per-run p95/p99, startup, host metadata and timer measurements are in
`before-{1,2,3}.json` and `after-{1,2,3}.json` beside this report.
The direct engine source is identical in both variants; its timing changes illustrate process
variation and are not attributed to this optimization.

| Probe | API | Before µs | After µs | Change | Before run medians µs | After run medians µs |
|---|---|---:|---:|---:|---|---|
| cheap-query | Direct | 30.38 | 29.98 | -1.3% | 31.65 / 30.38 / 28.35 | 29.98 / 30.22 / 28.36 |
| cheap-query | Worker | 74.27 | 78.03 | +5.1% | 76.08 / 74.27 / 72.61 | 78.03 / 84.83 / 76.24 |
| prepared-execute | Direct | 9.82 | 8.73 | -11.1% | 10.53 / 8.64 / 9.82 | 8.73 / 10.04 / 8.28 |
| prepared-execute | Worker | 21.05 | 20.45 | -2.8% | 22.15 / 21.05 / 20.19 | 19.41 / 21.59 / 20.45 |
| batch-100 | Direct | 556.70 | 545.71 | -2.0% | 712.23 / 517.35 / 556.70 | 545.71 / 714.15 / 535.42 |
| batch-100 | Worker | 1062.85 | 1078.98 | +1.5% | 1313.88 / 770.69 / 1062.85 | 1078.98 / 1263.61 / 1040.99 |
| large-typed-result | Direct | 4829.73 | 5042.36 | +4.4% | 5309.28 / 4829.73 / 4484.05 | 4760.21 / 5140.49 / 5042.36 |
| large-typed-result | Worker | 29967.59 | 9247.28 | -69.1% | 29967.59 / 29098.52 / 30781.10 | 9394.49 / 9247.28 / 8372.52 |

Transport counts are identical in all six runs. A large query still takes 33 fetches, at most
128 rows per batch, with an empty final fetch at the row-count boundary. The 15 timed queries
produce 61,440 rows through 495 fetches. Byte accounting still uses the previous conservative
expanded-value charges, with a 256 KiB batch ceiling and 2 MiB client-held result allowance;
compact bytes do not silently admit more rows or relax result limits. Unit coverage exercises
byte-boundary pending rows as well as row-count boundaries.

Worker heartbeat maximum gaps across the three runs were
4.37 / 3.79 / 4.83 ms before and
4.01 / 4.91 / 4.69 ms after. This measures application timer
delivery while a worker executes a resident aggregate, not cancellation latency or decoder
responsiveness for every possible value. Cancellation checkpoints are unchanged and covered by
host tests, including message-only cancellation before a first row is emitted.

These results support packed delivery for this warm, typed result workload. They do not measure
cold reads, durable writes, browser throughput, ARM, very large scalar values, or peak heap usage.
The cheap-query increase is small but present in these runs; the batch spread makes small changes
there inconclusive. No claim of a universal speedup is warranted.

## Verification

All 1,130 TypeScript unit tests pass, including packed-value bounds/ownership and async
cancellation/ordering tests. Shared conformance passes in memory (677 cases) and on disk
(596 cases). Real Chromium checks pass for the runtime/OPFS host (9 tests) and website
(111 tests). Node/browser/worker type checks, website checks, changed-file formatting and
lint pass; lint retains four pre-existing warnings.
