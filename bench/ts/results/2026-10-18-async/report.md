# TypeScript async boundary measurements

> Archive: [original report, committed data and experiment artifacts](https://github.com/jackc/jed/tree/b4ebe9c45f1b295add890f0a7d9478c6ac586c46/bench/ts/results/2026-10-18-async). Artifact paths and historical reproduction commands below refer to that snapshot; measured commit IDs are unchanged. Outputs explicitly described as local or ignored were not committed.

Measured 2026-10-18T20:01:04.636Z, base revision c105eed383cae6ef98cfc8872b71aa3657f1c999 (modified working tree), v26.10.0, linux/x64, Intel(R) Core(TM) Ultra 9 285K.

The measurements include the uncommitted async implementation; the revision identifies its base commit.

Run with `mise run bench:ts_async`. This standalone harness adds no dependencies. Settings are in results.json; JED_ASYNC_SAMPLES, JED_ASYNC_WARMUP, JED_ASYNC_BATCH_SAMPLES, JED_ASYNC_LARGE_SAMPLES, JED_ASYNC_STARTUP_SAMPLES, JED_ASYNC_ROWS and JED_ASYNC_HEARTBEAT_ROWS override them. JED_ASYNC_RESULTS chooses the output directory.

The direct path uses the synchronous internal TS core. The worker path uses the same production AsyncDatabase and RuntimeConnection proxies over a real Node execution worker; its transport wrapper only counts requests and returned rows. Startup uses the public createDatabase factory. Both use in-memory storage and persistent sessions. Samples alternate direct/worker order, follow 20 warmups, and check values, command tags and logical costs for equality outside the timed region. Cheap-query and large-result times include complete typed iteration and cursor close. Prepared-execute drains/discards results in the worker. Batch-100 is 100 autocommit SELECT statements in one request; its latency is for the whole batch.

## Caller-observed latency

All values are microseconds. Percentiles use nearest-rank selection; sample counts limit tail precision.

| Probe | Boundary | Samples | p50 µs | p95 µs | p99 µs |
|---|---|---:|---:|---:|---:|
| cheap-query | Direct core | 200 | 22.21 | 71.62 | 75.30 |
| cheap-query | Node worker | 200 | 64.44 | 597.35 | 666.21 |
| prepared-execute | Direct core | 200 | 5.61 | 9.20 | 11.79 |
| prepared-execute | Node worker | 200 | 24.11 | 43.26 | 54.81 |
| batch-100 | Direct core | 30 | 547.46 | 602.46 | 636.04 |
| batch-100 | Node worker | 30 | 1059.64 | 1226.42 | 1254.23 |
| large-typed-result | Direct core | 15 | 5260.19 | 7395.32 | 7395.32 |
| large-typed-result | Node worker | 15 | 28509.95 | 31527.29 | 31527.29 |
| create in-memory database | Direct core | 10 | 198.66 | 1663.15 | 1663.15 |
| create in-memory database | Node worker | 10 | 179588.01 | 191740.78 | 191740.78 |

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
| Direct core | 88.10 | 0 | 2.16 | 90.34 | 90.34 |
| Node worker | 69.73 | 33 | 2.09 | 2.19 | 2.28 |

These are warm in-memory measurements on this host. They do not measure durable commits, cold file reads, browser OPFS, cancellation latency, parallel readers, encryption, or another architecture. Result batching bounds delivery; the aggregate still uses the core's existing blocking execution path.
