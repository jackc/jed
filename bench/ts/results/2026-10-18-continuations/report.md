# TypeScript async boundary measurements

> Archive: [original report, committed data and experiment artifacts](https://github.com/jackc/jed/tree/f63cae226bc5e75ca353e9a568b0dfcf040dd514/bench/ts/results/2026-10-18-continuations). Artifact paths and historical reproduction commands below refer to that snapshot; measured commit IDs are unchanged. Outputs explicitly described as local or ignored were not committed.

Measured 2026-10-18T22:58:34.204Z, base revision 610021972dd715b6847e2a0fa22ab52036d2643e (modified working tree), v26.10.0, linux/x64, Intel(R) Core(TM) Ultra 9 285K.

The measurements include uncommitted working-tree changes; the revision identifies their base commit.

Run with `mise run bench:ts_async`. This standalone harness adds no dependencies. Settings are in results.json; JED_ASYNC_SAMPLES, JED_ASYNC_WARMUP, JED_ASYNC_BATCH_SAMPLES, JED_ASYNC_LARGE_SAMPLES, JED_ASYNC_STARTUP_SAMPLES, JED_ASYNC_ROWS and JED_ASYNC_HEARTBEAT_ROWS override them. JED_ASYNC_RESULTS chooses the output directory.

The direct path uses the synchronous internal TS core. The worker path uses the same production AsyncDatabase and RuntimeConnection proxies over a real Node execution worker; its transport wrapper only counts requests and returned rows. Startup uses the public createDatabase factory. Both use in-memory storage and persistent sessions. Samples alternate direct/worker order, follow 20 warmups, and check values, command tags and logical costs for equality outside the timed region. Cheap-query and large-result times include complete typed iteration and cursor close. Prepared-execute drains/discards results in the worker. Batch-100 is 100 autocommit SELECT statements in one request; its latency is for the whole batch.

## Caller-observed latency

All values are microseconds. Percentiles use nearest-rank selection; sample counts limit tail precision.

| Probe | Boundary | Samples | p50 µs | p95 µs | p99 µs |
|---|---|---:|---:|---:|---:|
| cheap-query | Direct core | 200 | 31.86 | 47.07 | 82.89 |
| cheap-query | Node worker | 200 | 84.69 | 115.21 | 206.83 |
| prepared-execute | Direct core | 200 | 11.63 | 19.92 | 27.67 |
| prepared-execute | Node worker | 200 | 27.69 | 42.72 | 58.70 |
| batch-100 | Direct core | 30 | 960.71 | 1711.57 | 1735.99 |
| batch-100 | Node worker | 30 | 1562.63 | 3075.85 | 5026.50 |
| large-typed-result | Direct core | 15 | 10602.35 | 13342.48 | 13342.48 |
| large-typed-result | Node worker | 15 | 36637.33 | 40200.47 | 40200.47 |
| create in-memory database | Direct core | 10 | 182.13 | 1942.59 | 1942.59 |
| create in-memory database | Node worker | 10 | 178078.66 | 193673.58 | 193673.58 |

## Bounded result delivery

The large result contains 4096 rows with exact integers, decimals, bytes, arrays and NULL. Transport limits are 128 rows and 262144 budget bytes per fetch, with 2097152 client result credits. A final empty fetch is expected when the result ends exactly at a row boundary.

| Probe | Requests | Fetch requests | Result batches | Rows delivered | Largest batch |
|---|---:|---:|---:|---:|---:|
| cheap-query | 400 | 200 | 200 | 200 | 1 |
| prepared-execute | 200 | 0 | 0 | 0 | 0 |
| batch-100 | 30 | 0 | 0 | 0 | 0 |
| large-typed-result | 510 | 495 | 495 | 61440 | 128 |

## Application event loop

A 2 ms interval runs while a warmed decimal aggregate over 250000 generated rows executes. The gap includes the first delayed callback after synchronous work returns. This observes responsiveness separately from operation completion latency.

| Boundary | SQL wall time ms | Heartbeats during SQL | Median timer gap ms | p95 timer gap ms | Largest timer gap ms |
|---|---:|---:|---:|---:|---:|
| Direct core | 93.17 | 0 | 2.16 | 95.36 | 95.36 |
| Node worker | 105.29 | 51 | 2.06 | 2.19 | 2.62 |

These are warm in-memory measurements on this host. They do not measure durable commits, cold file reads, browser OPFS, cancellation latency, parallel readers, encryption, or another architecture. Result batching bounds delivery. The aggregate consumes its full input before producing a result; cooperative worker turns are separate from result-batch delivery.

## Comparison with the initial worker runtime

The [earlier report](../2026-10-18-async/report.md) measured the worker boundary before page and loop continuations. Both runs used this host, the same workload settings and the same transport limits. They are separate runs over different source trees, rather than repeated trials of a controlled two-version experiment.

| Probe | Earlier direct p50 µs | Current direct p50 µs | Change | Earlier worker p50 µs | Current worker p50 µs | Change |
|---|---:|---:|---:|---:|---:|---:|
| cheap-query | 22.21 | 31.86 | +43% | 64.44 | 84.69 | +31% |
| prepared-execute | 5.61 | 11.63 | +107% | 24.11 | 27.69 | +15% |
| batch-100 | 547.46 | 960.71 | +75% | 1059.64 | 1562.63 | +47% |
| large-typed-result | 5260.19 | 10602.35 | +102% | 28509.95 | 36637.33 | +29% |

These are substantial regressions in execution overhead. Both direct and worker paths now drive the shared continuation traversal; the worker additionally yields at page and common-loop boundaries. The comparison does not isolate the cost of individual changes. Worker startup was similar (179.59 ms previously, 178.08 ms here). The result-batch and request counts were identical. The new implementation admits message-only cancellation during work that produces no result batches, covered by the runtime and Chromium tests; this benchmark does not measure its cancellation latency.
