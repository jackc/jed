# TypeScript elapsed scheduling and result-codec experiments

The selected change uses a **1 ms elapsed scheduling slice**, checked at existing 256-unit work checkpoints, with the existing packed codec. Transport limits remain 512 rows / 256 KiB per batch and 2 MiB client result credit. It retains most of the 4 ms prototype's aggregate improvement while avoiding its larger observed Node bulk-result slowdown. The fused codec prototype was reverted because its isolated decode improvement did not establish a consistent application-level gain.

Compared with the baseline process medians, the later 1 ms trials reduce aggregate worker time from 328 to 240 ms in Chromium and 314 to 284 ms in Node. Typed-output time is approximately flat in Chromium (671 to 662 ms). Node typed-output time rises 3.7% (524 to 544 ms), alongside a 3.5% direct-reference rise; its worker/direct overhead is essentially unchanged, 61.0% to 61.2%. This is a scheduling improvement for the measured compute-heavy query, not a general reduction in typed-result conversion cost.

This trades some message-only cancellation responsiveness for aggregate throughput. In ordinary Chromium, typed-output abort-to-rejection rises from a 13.1 ms median of process medians to 21.1 ms, with observed maxima of 33.9 and 58.4 ms. All measured cancellations complete with `57014` and leave the session reusable. The 1 ms slice is not a cancellation bound.

## Baseline and controlled design

The baseline is `2a8bdfcb264d1ef388296ee8554b485bee3e7d26`. It already includes the retained-continuation TS core, transferable typed result batches and the 512-row transport default. These are comparisons against that current core, not against the earlier synchronous executor before continuations. Historical reports with 128-row fetches are not the controlled baseline for this experiment.

All variants use the same limits: 512 rows per fetch, 256 KiB logical bytes per batch, and 2 MiB client result credit. The byte limit can stop this typed workload before the row limit. Workloads, SQL, data sizes and consumer fingerprint work are unchanged.

| Variant | Engine scheduling | Result codec |
| --- | --- | --- |
| Baseline | Task yield at each 256-unit cooperation checkpoint | Baseline packed codec |
| Scheduler | Check elapsed time at the same 256-unit checkpoints; yield when a 4 ms slice expires | Baseline packed codec |
| Combined | Same 4 ms scheduling prototype | Fused codec prototype |
| 1 ms (selected) | Same sparse elapsed-time check with a 1 ms slice | Baseline packed codec |

The scheduling clock affects physical task placement only. The synchronous driver remains synchronous, and SQL host time and deterministic SQL cost do not depend on it. Actual asynchronous I/O is awaited immediately; the elapsed deadline remains across awaited promises so hot resolved providers cannot suppress task yields indefinitely. A scheduled task starts a new slice. Sorting, parsing and long scalar kernels can overrun a slice, so neither 1 ms nor 4 ms is a cancellation or responsiveness guarantee.

There are 18 controlled process runs: three variants × two hosts × three repeats. Each run covers both queries below, with three warmups, seven latency samples per boundary and three separate responsiveness observations per boundary. Direct/worker sample order alternates; process variant order rotates. Six further process runs evaluate 1 ms after the initial 18-run matrix. They are later tuning trials, not a fourth variant interleaved into that matrix. The chronological run order and SHA-256 of each raw result are retained in [summary.json](summary.json). No test or benchmark load ran concurrently with these serialized measurements.

| Query | Work and output | Final SQL cost | Ordered typed fingerprint |
| --- | --- | ---: | --- |
| `decimal-aggregate` | Exact-decimal sum over 1,000,000 generated integers; one result row | 4,985,718 | `79e4eed1d6ce1755` |
| `typed-output` | Generate, order and fully consume 200,000 integer/decimal/bytea/array/NULL rows | 18,000,000 | `ee3bf2c6ac80465a` |

Every run checks row count, column names/types, rows affected, typed fingerprint and final cost against its direct counterpart. The fingerprint is noncryptographic. The typed query includes `ORDER BY id`: this is ordered generation plus delivery, not an isolated transport measurement. Setup, startup and comparisons are excluded from latency; query creation, full typed drain, the same fingerprint and close are included. Timers/frame observers run only in separate responsiveness observations.

Host: Intel Core Ultra 9 285K, Linux x64, CPU affinity `0,2`. Node v26.10.0 uses V8 `14.6.202.34-node.34`. Browser runs use Chromium `149.0.7827.55`, V8 `14.9.207.21`, a minified production build, a real execution worker and a visible 1280×800 headless page. All measured browser pages are non-isolated and have no SharedArrayBuffer. Both hosts use in-memory databases and 8 KiB pages. Latency queries provide no AbortSignal.

## Controlled worker latency

Values are medians of the three process medians, in milliseconds. Changes compare worker elapsed time with the same host/query's baseline worker. Positive changes are slower. They are not confidence intervals or tail estimates.

| Host | Query | Baseline worker | 4 ms worker | Change | Combined worker | Change |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| browser | decimal-aggregate | 328.30 | 232.90 | -29.1% | 232.00 | -29.3% |
| browser | typed-output | 671.30 | 681.40 | +1.5% | 651.00 | -3.0% |
| node | decimal-aggregate | 314.00 | 280.38 | -10.7% | 284.91 | -9.3% |
| node | typed-output | 524.36 | 552.21 | +5.3% | 571.86 | +9.1% |

The 4 ms aggregate change is substantial in the browser (−29.1%) and smaller in Node (−10.7%). It does not generalize to typed delivery: browser bulk time rises 1.5%, while Node bulk time rises 5.3%. The combined prototype improves browser bulk time by 3.0% but increases Node bulk time by 9.1%; this does not support adopting the codec change.

The direct reference is retained for every variant because separate process/isolate JIT, allocation and GC behavior can move both paths. Worker time below a direct median in some aggregate runs is an observation, not evidence that asynchronous dispatch intrinsically speeds up execution. Even with unchanged synchronous semantics, direct aggregate process medians vary; small differences should not be treated as a portable property.

| Host | Query | Variant | Direct median ms | Direct process medians ms | Worker process medians ms | Worker/direct overhead |
| --- | --- | --- | ---: | --- | --- | ---: |
| browser | decimal-aggregate | baseline | 273.60 | 270.90 / 284.70 / 273.60 | 343.40 / 269.00 / 328.30 | +20.0% |
| browser | decimal-aggregate | scheduler | 265.40 | 265.40 / 240.20 / 276.80 | 232.90 / 244.00 / 230.00 | -12.2% |
| browser | decimal-aggregate | combined | 263.90 | 262.90 / 263.90 / 268.30 | 228.90 / 232.00 / 238.90 | -12.1% |
| browser | typed-output | baseline | 367.10 | 373.20 / 364.70 / 367.10 | 660.90 / 672.90 / 671.30 | +82.9% |
| browser | typed-output | scheduler | 366.30 | 375.10 / 364.40 / 366.30 | 655.80 / 686.20 / 681.40 | +86.0% |
| browser | typed-output | combined | 371.10 | 379.70 / 367.00 / 371.10 | 644.00 / 652.90 / 651.00 | +75.4% |
| node | decimal-aggregate | baseline | 316.21 | 301.89 / 316.21 / 323.02 | 296.22 / 323.00 / 314.00 | -0.7% |
| node | decimal-aggregate | scheduler | 293.10 | 296.98 / 293.10 / 290.68 | 304.29 / 280.38 / 274.72 | -4.3% |
| node | decimal-aggregate | combined | 288.00 | 287.01 / 288.00 / 322.24 | 284.91 / 277.83 / 289.25 | -1.1% |
| node | typed-output | baseline | 325.76 | 325.76 / 328.92 / 325.48 | 534.04 / 524.36 / 517.15 | +61.0% |
| node | typed-output | scheduler | 329.18 | 329.18 / 354.29 / 320.99 | 537.88 / 552.21 / 571.17 | +67.8% |
| node | typed-output | combined | 339.31 | 334.93 / 348.61 / 339.31 | 557.05 / 580.23 / 571.86 | +68.5% |

## Later 1 ms tuning trials and selected behavior

These six runs use the same limits, workloads, three warmups, seven latency samples and three separate responsiveness observations, with three process repeats per host. They ran after the initial matrix. Comparisons below reuse the earlier baseline, so temporal/process variation is a limitation; do not count them as a randomized fourth arm.

| Host | Query | Baseline direct / worker ms | 1 ms direct / worker ms | Worker change | 1 ms direct process medians ms | 1 ms worker process medians ms |
| --- | --- | ---: | ---: | ---: | --- | --- |
| browser | decimal-aggregate | 273.60 / 328.30 | 281.50 / 240.10 | -26.9% | 286.20 / 271.70 / 281.50 | 240.10 / 250.70 / 230.40 |
| browser | typed-output | 367.10 / 671.30 | 369.10 / 662.40 | -1.3% | 369.20 / 358.60 / 369.10 | 652.10 / 669.50 / 662.40 |
| node | decimal-aggregate | 316.21 / 314.00 | 314.92 / 283.91 | -9.6% | 313.42 / 314.92 / 320.57 | 275.49 / 307.01 / 283.91 |
| node | typed-output | 325.76 / 524.36 | 337.22 / 543.75 | +3.7% | 337.22 / 337.48 / 330.97 | 540.65 / 543.75 / 568.24 |

Relative to the 4 ms candidate, 1 ms keeps a similar aggregate result and has less Node bulk slowdown. The selected production change therefore retains the original codec and uses 1 ms. The observed Node bulk +3.7% must remain visible, but it is accompanied by a +3.5% direct change: worker/direct overhead moves only 0.28 percentage points, from 60.97% to 61.25%. This does not demonstrate an intrinsic 3.7% scheduling regression, nor prove that it is absent on other workloads.

The direct reference here is the same current semantic core, including continuations and immediate expression paths. Negative worker/direct aggregate overhead does not mean async execution is inherently faster; separate isolates, JIT and allocation/GC behavior remain part of the comparison.

## Main-thread responsiveness in the controlled runs

Each entry is the median / worst of nine observations' largest overlapping gaps. The 5 ms timer includes the first delayed callback after work finishes. Browser animation frames and long tasks are recorded separately from latency, with delayed observer entries flushed. All 54 browser worker observations in the initial matrix and all 18 in the later 1 ms trials report zero main-thread long tasks. The 1 ms rows below are the later trials, not additional samples pooled into the initial variants. This measures application scheduling while a worker runs; it does not measure when the worker receives or acts on cancellation.

| Host | Query | Variant | Timer largest gap, median / worst ms | Frame largest gap, median / worst ms |
| --- | --- | --- | ---: | ---: |
| browser | decimal-aggregate | baseline | 9.70 / 12.90 | 19.80 / 21.70 |
| browser | decimal-aggregate | scheduler | 7.20 / 12.50 | 18.40 / 22.30 |
| browser | decimal-aggregate | combined | 8.10 / 10.90 | 19.80 / 27.00 |
| browser | typed-output | baseline | 10.30 / 17.10 | 19.60 / 22.00 |
| browser | typed-output | scheduler | 10.10 / 14.10 | 20.00 / 25.10 |
| browser | typed-output | combined | 10.90 / 13.40 | 20.00 / 20.90 |
| node | decimal-aggregate | baseline | 7.29 / 12.94 | not observed |
| node | decimal-aggregate | scheduler | 7.71 / 10.97 | not observed |
| node | decimal-aggregate | combined | 7.68 / 7.99 | not observed |
| node | typed-output | baseline | 6.87 / 14.63 | not observed |
| node | typed-output | scheduler | 6.94 / 11.78 | not observed |
| node | typed-output | combined | 7.56 / 12.91 | not observed |
| browser | decimal-aggregate | 1 ms (later) | 6.80 / 13.30 | 18.60 / 19.80 |
| browser | typed-output | 1 ms (later) | 8.60 / 10.40 | 20.00 / 22.10 |
| node | decimal-aggregate | 1 ms (later) | 7.30 / 13.73 | not observed |
| node | typed-output | 1 ms (later) | 7.01 / 11.30 | not observed |

Headless frame gaps show scheduling availability, not a full application's input latency. Browser idle largest timer gaps across these runs have a median of about 5.30 ms and worst of 5.40 ms; Node's corresponding values are about 6.04 ms and 6.58 ms. These idle figures summarize the initial matrix; each raw run also retains its own before/after idle observations.

## Additional-query coverage

Four later all-case runs compare baseline and selected 1 ms behavior once per host. They retain the same three warmups/seven samples/three responsiveness observations, but there is only **one process pair per host**. They are coverage checks, excluded from all three-process medians above. The two additional workloads are a 100,000-row indexed self-join with grouped aggregation and an 800,000-row bounded top-k query returning 100 rows.

| Host | Query | Baseline direct ms | Baseline worker ms | 1 ms direct ms | 1 ms worker ms |
| --- | --- | ---: | ---: | ---: | ---: |
| browser | resident-join-group | 309.30 | 329.40 | 312.10 | 324.40 |
| browser | sort-top100 | 339.10 | 351.90 | 323.10 | 322.50 |
| node | resident-join-group | 462.61 | 476.93 | 387.82 | 396.51 |
| node | sort-top100 | 484.18 | 652.00 | 578.88 | 615.54 |

Both extra queries have lower observed worker medians in each pair. Direct-reference variation is substantial, especially in Node, so these single pairs do not establish the size or portability of a gain. Values, types, row fingerprints and final cost agree. All four cases from each extra run remain in its raw JSON, including the repeated aggregate and typed-output cases.

## Isolated codec probe — separate evidence

The codec probe excludes SQL and worker messaging. It uses 200,000 prebuilt representative typed rows and the same 512-row/256 KiB batching constraints. This produces 560 batches, 14,582,242 physical packet bytes and 146,320,008 logical value bytes. Exact packet bytes, reconstructed values, per-row accounting and budget equality are checked before warmup. Both variants use the same byte-identical dependencies and dependency instances. Three warmups and seven samples alternate variant order; natural GC remains inside samples, with one explicit full GC before warmup only.

| Operation | Baseline median ms (range) | Prototype median ms (range) | Change |
| --- | ---: | ---: | ---: |
| decode | 37.54 (36.82–37.89) | 29.01 (28.76–29.10) | -22.7% |
| encode | 58.01 (43.56–87.20) | 70.98 (44.28–93.17) | +22.4% |

Decoder fusion reduces isolated decoding from 37.54 to 29.01 ms. Encoding has broad, overlapping sample ranges and a worse observed median, 58.01 to 70.98 ms. Those components cannot be added to predict a full query: SQL, messaging, consumer work, allocation lifetimes and GC differ. The isolated decode result does not establish an application-level win.

Decoder-only end-to-end pilots were effectively flat for typed output: browser baseline 667.9 ms versus decoder 670.5 ms; Node baseline 519.13 ms versus decoder 519.01 ms. These are single-process pilots, not part of the 18-run controlled matrix. Codec-only and early scheduler/combined pilots also remain excluded from its medians. Their role was to choose experiments, not to strengthen the final sample count.

## Cancellation measurements

Cancellation was measured in a separate rotated 18-process comparison: baseline, 4 ms and selected 1 ms × two hosts × three repeats. Each case has three full-query warmups, two cancellation warmups and seven measured cancellations per process, giving 21 measured cancellations per case/variant/host. The [cancellation summary](cancellation-summary.json) retains all raw latencies, process medians, chronology and result hashes; per-process records are under `cancellation/`.

The benchmark-only adapter marks the first real task yield inside active query execution. The caller requests abort 20 ms after receiving that marker, measures rejection on the caller's clock before explicit close/follow-up, and checks the same session's reuse afterward. Ordinary Chromium tests message-only cancellation; Node tests shared atomic cancellation. No cross-clock subtraction is used. The marker selects a particular execution phase, rather than canceling before execution starts. It adds a message and bookkeeping, so this is an instrumented cancellation probe, separate from production throughput timing.

The table gives the median of three process medians / largest observed latency across 21 samples, in milliseconds. These small samples do not establish percentile or worst-case bounds.

| Host | Query | Baseline abort-to-rejection ms | 4 ms abort-to-rejection ms | 1 ms abort-to-rejection ms |
| --- | --- | ---: | ---: | ---: |
| browser | decimal-aggregate | 0.60 / 13.50 | 8.10 / 13.30 | 2.00 / 16.50 |
| browser | typed-output | 13.10 / 33.90 | 22.00 / 84.20 | 21.10 / 58.40 |
| node | decimal-aggregate | 0.59 / 9.47 | 0.51 / 5.98 | 0.52 / 12.53 |
| node | typed-output | 0.62 / 6.08 | 1.85 / 12.69 | 0.58 / 13.72 |

All 252 measured operations reject with SQLSTATE `57014`, with no completion-before-abort or watchdog expiry, and all follow-up queries verify session reuse. The selected 1 ms slice improves the browser aggregate cancellation median relative to 4 ms, but remains slower than baseline. Browser typed-output cancellation also worsens versus baseline: 13.1 to 21.1 ms by process-median summary, and 33.9 to 58.4 ms for the largest observed delay. The typed process medians vary substantially: baseline 13.1 / 24.4 / 0.7 ms, selected 19.6 / 21.1 / 31.1 ms. Node's atomic cancellation medians remain below 1 ms with the selected slice, but its observed maxima also increase.

Caller marker-to-abort delay is recorded separately: group medians range from about 20.1 to 20.2 ms and the largest individual delay is 27.6 ms. Submit-to-marker and worker-local cancel-receipt-to-error timings are retained in raw records. None is subtracted across clocks or presented as transport-only delay. The throughput gain therefore has a measured cancellation tradeoff, especially in the message-only browser. Parsing, sorting, long scalar kernels, host scheduling and message delivery can exceed the slice; these observations cannot bound arbitrary cancellation latency.

## Reproduction and retained artifacts

The [manifest](manifest.json) identifies the frozen baseline/prototypes and their source hashes; the result summaries retain raw-result hashes. [summary.json](summary.json) contains the 18-run matrix and six later 1 ms runs with their chronology and process medians. Raw host runs are under `raw/`, named `{browser,node}-{baseline,scheduler,combined,short}-{1,2,3}.json`; `-extra` runs are the single-pair all-case coverage checks. Browser raw data retains both bundle hashes and benchmark source hashes. Pilot names end in `-pilot` and are excluded from the repeated-result summary. The isolated codec results and exact variant/dependency hashes are in [codec-probe.json](codec-probe.json).

The [1 ms patch](experiments/short.patch) and [4 ms patch](experiments/scheduler.patch) reconstruct the scheduler variants relative to baseline `2a8bdfcb264d1ef388296ee8554b485bee3e7d26`. Apply the [fused codec patch](experiments/fused-codec.patch) alongside the 4 ms patch to reconstruct the combined variant. These are experiment artifacts for isolated baseline checkouts, not changes to apply to the selected production checkout. The manifest identifies the frozen sources used for the repeated comparisons; earlier 4 ms pilots predate the internal execution-clock test seam and must not be assumed to match that full source hash.

To repeat the selected implementation, use this change's checkout or reconstruct the selected source from the baseline and retained patch. Choose a fresh output directory for every process run and run the existing tasks serially:

```sh
JED_ASYNC_HEAVY_CASES=decimal-aggregate,typed-output \
JED_ASYNC_HEAVY_BATCH_ROWS=512 JED_ASYNC_HEAVY_BATCH_BYTES=262144 \
JED_ASYNC_RESULTS=/tmp/scheduling-node-repeat \
taskset -c 0,2 mise run bench:ts_async_heavy

JED_ASYNC_HEAVY_CASES=decimal-aggregate,typed-output \
JED_ASYNC_HEAVY_BATCH_ROWS=512 JED_ASYNC_HEAVY_BATCH_BYTES=262144 \
JED_ASYNC_BROWSER_RESULTS=/tmp/scheduling-browser-repeat \
JED_ASYNC_BROWSER_ISOLATED=0 \
taskset -c 0,2 mise run bench:ts_async_browser
```

Defaults are three warmups, seven latency samples and three separate responsiveness observations. Repeat each host three times, retaining the raw samples and host metadata. Reconstruct baseline and rejected prototypes using the retained identifiers/patches and preserve the recorded rotating process order. Do not compare a newly measured selected build only against these historical baseline numbers and call that a controlled rerun. The byte/result-credit limits remain unchanged. Removing the case selector exercises all four heavy workloads.

Cancellation uses `mise run bench:ts_async_cancel`, with `JED_ASYNC_HEAVY_CASES=decimal-aggregate,typed-output`, `JED_ASYNC_CANCEL_HOST=node` or `browser`, `JED_ASYNC_CANCEL_RESULTS` selecting a fresh output directory and `JED_PROBE_ROOT` pointing at each reconstructed source checkout. Its defaults reproduce three full-query warmups, two cancellation warmups, seven samples and a 20 ms marker delay. Run each host/variant three times in the recorded rotating order. Keep the same benchmark adapters and workload definitions across reconstructed sources.

The isolated codec probe has [durable reproduction instructions](experiments/codec-reproduction.md), exact source patches, hashes, the original script and a configurable script. It uses shared byte-identical dependencies and checks source snapshots against hashes. It did not run concurrently with host measurements; its timings are not substituted for end-to-end throughput.

## Verification and limits

Verification on the selected production tree passes:

- TypeScript unit tests: 1,143 passed, zero failed.
- Chromium runtime tests: 9 passed.
- TypeScript conformance: memory 677 passed / 2 skipped; disk 596 passed / 83 skipped; zero failures.
- All three TypeScript programs typecheck. Lint checks 337 files with zero errors and four existing warnings.
- Probe JavaScript syntax checks pass for 11 files; the Ruby task syntax check passes.

Every engine-source hash matches the frozen selected 1 ms variant in the manifest. The production packed codec and its tests remain at the baseline implementation. The surrounding commit identifies the final code, tests and report; the source hashes identify the measured engine independently of that commit.

Focused scheduler checks cover sparse clock reads, budget reset, hot async providers, unchanged logical cost, immediate I/O awaits, deterministic message cancellation, transaction rollback, cursor/pin cleanup and planning-scope restoration. Tests that require an exact yield point inject a clock or zero slice rather than depending on machine speed.

This report covers warm in-memory workloads on one x64 machine in Node and headless Chromium. OPFS durability, cold storage, other browser engines, ARM, arbitrary scalar kernels, concurrent consumer load and peak memory remain unmeasured here. Seven samples and three process repeats do not establish tail guarantees. The selected 1 ms budget is a physical scheduling policy, not a maximum cancellation latency or a claim of universal throughput improvement.
