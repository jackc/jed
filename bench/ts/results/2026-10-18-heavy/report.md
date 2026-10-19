# TypeScript heavy queries in Chromium and Node

> Archive: [original report, committed data and experiment artifacts](https://github.com/jackc/jed/tree/37c8b9115850097fa4b1bd259936b197c6a296b3/bench/ts/results/2026-10-18-heavy). Artifact paths and historical reproduction commands below refer to that snapshot; measured commit IDs are unchanged. Outputs explicitly described as local or ignored were not committed.

These measurements compare the current synchronous internal API with the current public
worker API. Expensive queries returning few rows show substantially less relative overhead
than the earlier tiny-call benchmarks. Delivering 200,000 typed rows still adds considerable
time. In Chromium, worker execution keeps animation frames and application timers running
throughout both kinds of work, including on a page without shared memory.

Across the two browser modes, the compute-heavy cases add about 9–29% to median elapsed time.
The ordered 200,000-row result takes 2.0–2.2× the direct time. These workloads support worker
isolation for expensive execution while identifying substantial result delivery as further work.

## Workloads and measurement

The engine is unchanged from `7928ac81be3064e875eff39d0fef26f700d7be72`; this change adds
benchmarks and documentation. Both paths use that same semantic core, including its retained
continuations. This is the current worker-boundary overhead, not a comparison against the
old executor before resumability was added. Raw results identify the base commit and dirty
working tree; browser results additionally record source and production bundle hashes.

| Case | Work | Returned rows | SQL cost |
|---|---|---:|---:|
| decimal-aggregate | Exact-decimal sum over 1,000,000 generated integers | 1 | 4,985,718 |
| resident-join-group | Self-join of 100,000 resident table rows, grouped sum | 128 | 798,985 |
| sort-top100 | Order 800,000 generated rows by two integer keys, retain top 100 | 100 | 4,000,500 |
| typed-output | Generate, order and completely consume integer/decimal/bytea/array/NULL rows | 200,000 | 18,000,000 |

The join uses an indexed nested loop with a primary-key lookup. The top-N query uses a
bounded top-k sort. The typed-output query includes `ORDER BY id` to define fingerprint order:
it measures ordered generation plus delivery, not isolated transport. Exact SQL and setup are
in every raw JSON file. All databases are in memory with 8 KiB pages; table loading is untimed.

Host: Intel Core Ultra 9 285K, Linux x64, fixed CPU affinity `0,2`. Node is v26.10.0;
Chromium is 149.0.7827.55, V8 14.9.207.21, headless with a visible 1280×800 page.
The browser uses a minified production Vite build and the real module execution worker.
Each host has three independent process runs. Retained chronological order is
node-1, browser-1, isolated-1, isolated-2, node-2, browser-2, browser-3, isolated-3, node-3.
No other benchmark ran concurrently. Preliminary calibration and smoke runs are excluded.

Each case performs an untimed validation and three warmups, then seven latency samples per
boundary, alternating direct/worker order. Query creation, complete typed iteration, the same
lightweight fingerprint and cursor close are timed. Result comparisons occur afterward;
row count, typed fingerprint, column metadata, rows affected and logical cost agree across all
runs and hosts. The fingerprint is noncryptographic and does not retain result rows.

Tables report the median of three process medians. Overhead is the ratio of those medians
minus one; individual process medians expose variation. These are not confidence intervals
or tail guarantees. Worker startup, database/session creation and table setup are excluded.
Neither boundary receives an `AbortSignal`: isolation mode is recorded, but this experiment
does not measure atomic cancellation or cancellation delay.

## Ordinary Chromium, without shared memory

All three pages reported `crossOriginIsolated=false`, `SharedArrayBuffer=false`.

| Workload | Direct ms | Worker ms | Added ms | Overhead | Direct process medians ms | Worker process medians ms |
|---|---:|---:|---:|---:|---|---|
| decimal-aggregate | 280.60 | 320.20 | +39.60 | +14.1% | 281.70 / 262.40 / 280.60 | 337.00 / 299.90 / 320.20 |
| resident-join-group | 322.50 | 364.00 | +41.50 | +12.9% | 382.50 / 322.50 / 314.50 | 412.90 / 364.00 / 335.70 |
| sort-top100 | 340.10 | 372.00 | +31.90 | +9.4% | 489.00 / 340.10 / 279.20 | 500.40 / 355.70 / 372.00 |
| typed-output | 362.00 | 807.40 | +445.40 | +123.0% | 413.40 / 355.20 / 362.00 | 817.20 / 778.60 / 807.40 |

## Cross-origin isolated Chromium

The same production bundles run with COOP/COEP, reporting `crossOriginIsolated=true` and
`SharedArrayBuffer=true`. Variation between these and ordinary-browser timings must not be
attributed to shared memory: the measured queries do not provide a cancellation signal.

| Workload | Direct ms | Worker ms | Added ms | Overhead | Direct process medians ms | Worker process medians ms |
|---|---:|---:|---:|---:|---|---|
| decimal-aggregate | 262.82 | 339.61 | +76.79 | +29.2% | 282.52 / 262.82 / 250.38 | 318.80 / 358.05 / 339.61 |
| resident-join-group | 328.23 | 361.08 | +32.84 | +10.0% | 330.41 / 328.23 / 311.61 | 361.08 / 386.02 / 338.13 |
| sort-top100 | 324.66 | 381.09 | +56.42 | +17.4% | 324.66 / 330.21 / 299.06 | 380.30 / 403.81 / 381.09 |
| typed-output | 376.31 | 754.14 | +377.83 | +100.4% | 376.31 / 401.34 / 363.12 | 750.86 / 754.14 / 820.00 |

## Node

| Workload | Direct ms | Worker ms | Added ms | Overhead | Direct process medians ms | Worker process medians ms |
|---|---:|---:|---:|---:|---|---|
| decimal-aggregate | 334.81 | 323.67 | -11.15 | -3.3% | 367.04 / 334.81 / 264.45 | 326.97 / 323.67 / 305.27 |
| resident-join-group | 453.08 | 457.46 | +4.38 | +1.0% | 527.91 / 453.08 / 388.20 | 550.30 / 457.46 / 419.86 |
| sort-top100 | 468.39 | 523.30 | +54.91 | +11.7% | 612.36 / 468.39 / 414.43 | 711.39 / 523.30 / 481.79 |
| typed-output | 329.21 | 614.60 | +285.39 | +86.7% | 376.27 / 329.21 / 326.07 | 614.60 / 641.62 / 610.70 |

A negative difference for an individual workload is an observation of these runs, not evidence
that asynchronous dispatch inherently speeds up execution. Separate isolates, JIT/GC state
and process variation remain part of this public-boundary comparison.

## Main-thread responsiveness

Responsiveness uses three additional observations per case and boundary in each process,
separate from latency timing. A 5 ms timer is primed before work; the first callback after
completion is included. Chromium additionally records actual animation callback times and
overlapping long tasks, with an 80 ms observer drain afterward. Long-task durations are whole
overlapping tasks, not durations clipped to the SQL interval. Headless frames demonstrate
scheduling availability; they are not measurements of a full application's input latency.

The following are medians of the nine observations' largest overlapping timer/frame gaps.
The last column also exposes the worst worker observation, rather than hiding it in a median.

Across all 72 browser worker observations, no main-thread long tasks were recorded and animation
frames continued. All 72 direct observations recorded one long task and zero animation callbacks
during work. Non-isolated timer counts can include a boundary callback at the same rounded
timestamp as query start/end; the gap measurements capture the synchronous stall regardless.

Ordinary Chromium:

| Workload | Direct largest timer gap ms | Worker largest timer gap ms | Direct largest frame gap ms | Worker largest frame gap ms | Worst worker timer / frame gap ms |
|---|---:|---:|---:|---:|---:|
| decimal-aggregate | 275.90 | 8.10 | 278.80 | 18.90 | 10.00 / 20.30 |
| resident-join-group | 325.50 | 7.70 | 331.10 | 19.60 | 9.00 / 23.00 |
| sort-top100 | 348.00 | 8.60 | 350.60 | 21.00 | 10.90 / 24.00 |
| typed-output | 378.70 | 9.20 | 385.50 | 18.90 | 13.10 / 22.10 |

Isolated Chromium:

| Workload | Direct largest timer gap ms | Worker largest timer gap ms | Direct largest frame gap ms | Worker largest frame gap ms | Worst worker timer / frame gap ms |
|---|---:|---:|---:|---:|---:|
| decimal-aggregate | 274.40 | 8.34 | 280.79 | 20.04 | 11.08 / 23.21 |
| resident-join-group | 333.02 | 8.20 | 336.95 | 20.88 | 10.34 / 22.96 |
| sort-top100 | 336.24 | 9.83 | 348.10 | 20.48 | 11.78 / 21.96 |
| typed-output | 375.66 | 8.48 | 379.44 | 20.31 | 16.18 / 23.90 |

Idle observations before/after each suite establish the scheduling baseline:

| Host | Idle largest timer gap, median / worst ms | Idle largest frame gap, median / worst ms |
|---|---:|---:|
| Ordinary Chromium | 5.35 / 7.20 | 17.00 / 19.40 |
| Isolated Chromium | 5.26 / 6.01 | 16.97 / 17.84 |

Node responsiveness (median of nine observation maxima):

| Workload | Direct largest timer gap ms | Worker largest timer gap ms | Worst worker timer gap ms |
|---|---:|---:|---:|
| decimal-aggregate | 321.19 | 7.39 | 10.18 |
| resident-join-group | 430.36 | 7.63 | 12.37 |
| sort-top100 | 499.61 | 8.44 | 14.96 |
| typed-output | 341.60 | 8.52 | 9.99 |

Node's idle largest timer gap was 5.55 ms at the median,
6.49 ms at worst.

## Reproduction and limits

Run each command three times with distinct output directories, preserving the chronological
order above. Defaults select exactly the sizes and sample counts used in this report:

```sh
JED_ASYNC_RESULTS=/tmp/node-1 taskset -c 0,2 mise run bench:ts_async_heavy
JED_ASYNC_BROWSER_RESULTS=/tmp/browser-1 JED_ASYNC_BROWSER_ISOLATED=0 taskset -c 0,2 mise run bench:ts_async_browser
JED_ASYNC_BROWSER_RESULTS=/tmp/isolated-1 JED_ASYNC_BROWSER_ISOLATED=1 taskset -c 0,2 mise run bench:ts_async_browser
```

The browser task uses already-installed Vite, Playwright and Chromium. It installs/downloads
nothing. `JED_ASYNC_HEAVY_` settings override `WARMUPS`, `SAMPLES`, `RESPONSE_SAMPLES`,
`DECIMAL_ROWS`, `TABLE_ROWS`, `SORT_ROWS`, `TYPED_ROWS` and comma-separated `CASES`.
Node/browser tasks share the workload, drain and validation harness. Existing result bounds
remain 128 rows / 256 KiB per batch and 2 MiB held results.

Raw `node-{1,2,3}.json`, `browser-{1,2,3}.json` and `isolated-{1,2,3}.json` preserve every
latency sample, observed callback, SQL statement, result fingerprint, cost and environment.
`summary.json` records the aggregates used here. The benchmark bundles are not retained;
their hashes and sizes are metadata, not a measurement of the production package's total size.

This experiment covers warm in-memory execution on one x64 machine and one browser engine.
It does not measure OPFS, cold/durable I/O, Firefox/WebKit, ARM, peak memory, concurrent
consumers, expensive application rendering, cancellation delay or historical executor overhead.
Synchronous sort/scalar work inside the execution worker can delay cancellation while the
application's main thread remains responsive. Additional pause points should be judged by
cancellation needs, not inferred from application heartbeat measurements.

## Verification

All nine full runs validate identical typed results and deterministic SQL costs; browser source
and bundle hashes agree across all six browser runs. Small smoke runs pass in ordinary and
isolated Chromium. Node/browser benchmark TypeScript checks, Ruby/JavaScript syntax checks,
changed-file formatting and repository lint pass; lint retains four existing warnings.
Engine code is unchanged, so this change does not rerun or claim new core conformance coverage.
