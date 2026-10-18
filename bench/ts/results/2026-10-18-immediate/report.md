# Immediate-expression prototype

> Archive: [original report, committed data and experiment artifacts](https://github.com/jackc/jed/tree/d20bc1bb09199e35c1aa5f78e8d8a882c7558508/bench/ts/results/2026-10-18-immediate). Artifact paths and historical reproduction commands below refer to that snapshot; measured commit IDs are unchanged. Outputs explicitly described as local or ignored were not committed.

This compares the retained-generator implementation at `2ba44d3d60dfb65d14376024b59baf630940f867`
with the immediate-expression and synchronous-driver prototype in this change. The prototype
returns ordinary values for common resident expressions, retaining continuations for pending
operands. It also avoids redundant synchronous cursor adapters and per-segment driver setup.
Eight per-column/key-slot checkpoints were removed; logical page traversal, row/operator
progress, async I/O, and the scheduling quantum remain intact.

## Method

Both variants use the unchanged `bench/ts/src/bench-async.ts` harness, Node v26.10.0 on
Linux/x64, and this host's Intel Core Ultra 9 285K. The baseline source was extracted with
`git archive`; the prototype used the working tree. Both use the same installed dependencies.
No correctness suites or other agent benchmarks ran concurrently with these measurements.

There are three independent process runs per variant, in order before/after, after/before,
before/after. Both caller and execution worker inherit CPU affinity `0,2` via `taskset`, limiting
migration between this host's different core classes. This is a controlled relative comparison,
not an estimate of normal scheduler behavior. Unrestricted preliminary runs varied noticeably.

Each probe has 200 warmups (`JED_ASYNC_WARMUP=200`), rather than the earlier reports' 20. Other
settings are unchanged: 200 cheap/prepared samples, 30 batch samples, 15 large-result samples,
4,096 result rows, 128 rows per transport batch, and a 250,000-row heartbeat workload. The harness
alternates direct/worker sample order and verifies values, command tags, and logical costs outside
the timed region. Prepared execution drains/discards its result; batch-100 contains 100 autocommit
SELECT statements. Large-result timing includes complete typed iteration and close.

Reproduce a run on the selected revision with:

```sh
JED_ASYNC_WARMUP=200 JED_ASYNC_RESULTS=/tmp/jed-immediate-result \
  taskset -c 0,2 mise run bench:ts_async
```

Raw samples, configuration, host details, startup and heartbeat observations are retained in
[before-1.json](https://github.com/jackc/jed/blob/d20bc1bb09199e35c1aa5f78e8d8a882c7558508/bench/ts/results/2026-10-18-immediate/before-1.json), [before-2.json](https://github.com/jackc/jed/blob/d20bc1bb09199e35c1aa5f78e8d8a882c7558508/bench/ts/results/2026-10-18-immediate/before-2.json), [before-3.json](https://github.com/jackc/jed/blob/d20bc1bb09199e35c1aa5f78e8d8a882c7558508/bench/ts/results/2026-10-18-immediate/before-3.json),
[after-1.json](https://github.com/jackc/jed/blob/d20bc1bb09199e35c1aa5f78e8d8a882c7558508/bench/ts/results/2026-10-18-immediate/after-1.json), [after-2.json](https://github.com/jackc/jed/blob/d20bc1bb09199e35c1aa5f78e8d8a882c7558508/bench/ts/results/2026-10-18-immediate/after-2.json), and [after-3.json](https://github.com/jackc/jed/blob/d20bc1bb09199e35c1aa5f78e8d8a882c7558508/bench/ts/results/2026-10-18-immediate/after-3.json).
The prototype records the base revision plus `workingTree: "modified"`; its source is the code
committed alongside this report. Absolute numbers should not be compared directly with earlier
reports, which used different warmup and CPU-affinity settings.

## Results

Values below are the median of the three run medians, in microseconds. Negative change is lower
latency; these small sample sets do not establish a confidence interval.

| Probe | Direct before | Direct after | Change | Worker before | Worker after | Change |
|---|---:|---:|---:|---:|---:|---:|
| cheap-query | 49.07 | 46.48 | -5.3% | 124.99 | 123.37 | -1.3% |
| prepared-execute | 10.23 | 11.24 | +9.9% | 25.15 | 25.80 | +2.6% |
| batch-100 | 729.38 | 696.98 | -4.4% | 985.07 | 974.36 | -1.1% |
| large-typed-result | 10,706.27 | 5,657.77 | -47.2% | 31,845.65 | 26,376.11 | -17.2% |

The large-result gain appears in all three runs: direct medians span 10.64–10.85 ms before and
5.05–5.91 ms after; worker medians span 31.54–32.07 ms before and 26.15–26.39 ms after. Request,
fetch, batch and row counts are identical: the change reduces engine execution overhead without
changing transport batching. Wire encoding, result copying and worker request overhead remain.

Small calls do not show a broad improvement. In particular, prepared execution is about 1.01 µs
slower in the direct core. The batch prototype also has wider run-to-run variation. This is evidence
for the resident-expression approach, not a claim that all workloads are faster or that the full
continuation overhead has been recovered.

The worker caller's largest heartbeat gaps were 4.87–7.79 ms before and 4.74–5.61 ms after.
Heartbeat measurements observe application responsiveness; they do not measure cancellation
latency. These warm in-memory results do not cover cold I/O, durable writes, browser OPFS,
encryption, other CPU architectures, or long synchronous scalar kernels.

## Correctness and remaining scope

The same semantic dispatch and scalar kernels serve immediate and pending operands. Focused tests
cover evaluation order, exactly-once host calls across suspension/failure, strict NULL and boolean
short-circuiting, cost ceilings, 4,096 pending arguments under the existing stack budget, nested
planning/cancellation scopes, and early cursor close without evaluating or charging another row.
Resident aggregate, join and recursive-query tests cancel before any result is emitted.

Validation passed: 1,114 TS unit tests, 677 memory and 596 disk conformance files, 9 Chromium
runtime/OPFS tests, 111 website browser tests, Node/browser/worker type checks, and configured
lint/format checks. Existing lint warnings are unchanged.

Uncommon expression forms still use generators. Operator traversal, storage cursors and async
result delivery retain continuations. The prototype adds no prefetch, whole-database preload,
SQL replay, new dependency, or public API. Further work should measure those remaining costs
separately from transport and task scheduling before extending the optimization.
