# TypeScript large typed-result investigation

> Archive: [original report, committed data and experiment artifacts](https://github.com/jackc/jed/tree/b4fb408c6e173e8f092e0e21d9ef2c7364fb6d18/bench/ts/results/2026-10-18-result-batching). Artifact paths and historical reproduction commands below refer to that snapshot; measured commit IDs are unchanged. Outputs explicitly described as local or ignored were not committed.

Increasing the default row cap from 128 to 512, within the existing **256 KiB batch / 2 MiB
held-result limits**, reduces the 200,000-row query's worker time by **8.1% in Chromium** and
**21.4% in Node** in this controlled comparison. It improves throughput but leaves substantial
overhead: typed values still have to be encoded, transferred, validated and reconstructed.
No SQL evaluator, codec, cancellation quantum, wire version or byte allowance changes.

## Why this workload is expensive

The result contains 200,000 rows with integer, decimal, bytea, array and NULL columns. Including
the two integer array elements, that is 1.4 million value nodes. Packed transport writes about
14.6 MB; conservative logical value accounting is about 146.3 MB across the complete result,
consumed in bounded batches. These totals are not peak heap measurements.

The old 128-row cap requires 1,563 fetch round trips. Its typical batch occupies only about
93 KiB of the permitted 256 KiB logical allowance. The new 512-row cap lets this row shape
reach the byte limit, about 357–361 rows per batch and 560 batches in the isolated codec probe.
No next batch is fetched speculatively. Client per-row promises remain; the worker drives
retained generators within each fetch, rather than awaiting a promise per emitted row.

A Node CPU profile of twelve worker queries attributed about 53 ms/query to packed writing
and 39 ms/query to decoding. These inclusive CPU samples are approximate, contain normal
allocation effects, and must not be added to their child-function timings or substituted for
browser wall-clock measurements. They establish significant per-row work beyond dispatch.

## Controlled default comparison

Before is engine commit `da455ecd24ede5c66c8f80bbbaa689c221cc8e3b`. After differs only in the
production `maxBatchRows` default, 128 → 512. Both variants use the same benchmark harness,
including this change's optional batch-limit overrides; the controlled runs supply neither
override. All source checks, actual limits and browser build hashes are retained in raw JSON.

Each host/variant has three independent process runs, three warmups, seven alternating
direct/worker latency samples and three separate responsiveness observations. Only the
`typed-output` case runs, with exactly the previous report's SQL and 200,000-row dataset.
Timings include query, complete typed drain/fingerprint and close. Row values/fingerprints,
metadata and final SQL cost (18,000,000) agree throughout. Startup and setup are excluded.

Hardware is the same Intel Core Ultra 9 285K, Linux x64, affinity `0,2`; Node v26.10.0 and
headless Chromium 149.0.7827.55. Chromium uses the production minified bundles, a visible page,
no isolation headers and no shared memory. No measured query supplies an AbortSignal.
No other performance workload runs concurrently.

Chronological order: browser-before-1, browser-after-1, node-after-1, node-before-1,
node-before-2, node-after-2, browser-after-2, browser-before-2, browser-before-3,
browser-after-3, node-after-3, node-before-3. Preliminary profiling, batch sweeps and codec
prototypes are excluded from the controlled tables. Compare these contemporaneous variants;
do not treat the previous report's 807 ms browser timing as this experiment's baseline.

The table uses medians of process medians; individual medians expose variation.

| Host / variant | Direct ms | Worker ms | Worker overhead | Worker process medians ms |
|---|---:|---:|---:|---|
| browser-before | 369.50 | 735.30 | +99.0% | 726.90 / 735.30 / 773.90 |
| browser-after | 375.90 | 675.60 | +79.7% | 659.00 / 675.60 / 683.60 |
| node-before | 337.35 | 643.30 | +90.7% | 617.43 / 643.30 / 666.60 |
| node-after | 333.25 | 505.68 | +51.7% | 496.73 / 521.53 / 505.68 |

The unchanged direct engine varies between runs. These are warm in-memory observations on
one machine, not confidence intervals, tail guarantees, OPFS/durable-I/O results or evidence
for other browser engines and ARM.

## Responsiveness and batching tradeoffs

Across the eighteen browser worker observations in the controlled comparison, no main-thread
long tasks were recorded. The median of each observation's largest frame gap changes from
19.9 to 20.5 ms; the worst changes from 21.1 to 26.2 ms. Direct runs have one long task each
and no animation callbacks during execution. Node's median largest worker timer gap changes
from 7.15 to 7.94 ms; the worst changes from 11.77 to 17.08 ms. The caller remains responsive,
but these observations do not claim identical scheduling or measure worker cancellation delay.

A larger fetch evaluates more rows before returning the first one. That can delay first-row
delivery, expose a later SQL error earlier in iteration, or discard more evaluated work on
early close. It can also make ordinary queued statements wait longer for that fetch. Existing
cooperative cancellation points remain active. Set `runtime.maxBatchRows` to 128 or 1 when
a smaller evaluation prefix matters. Actual resident bytes may rise toward the existing cap;
unchanged allowances do not imply identical heap use.

## Optional tuning for bulk consumption

Three additional browser runs use the same after build and measurement settings, with
`maxBatchRows: 1024` and `maxBatchBytes: 1048576`. They run after the controlled comparison
and verification, so they are a tuning follow-up rather than another interleaved causal test.

Direct median: 384.80 ms; worker median:
**613.30 ms** (process medians
613.30 / 614.60 / 612.30 ms).
Largest worker frame-gap median/worst: 19.60 /
23.10 ms; 0 observed long tasks.

```ts
const db = await createDatabase({
  runtime: { maxBatchRows: 1024, maxBatchBytes: 1024 * 1024 },
});
```

The total held-result allowance stays 2 MiB, so this configuration admits only two full batch
reservations concurrently instead of eight. Larger individual batches also increase decode
bursts and evaluation ahead of consumption. It remains opt-in; the default byte limit stays
256 KiB. It does not eliminate per-value conversion costs.

## Codec experiments

An isolated Node probe constructs the same representative 200,000 typed rows before timing,
then measures packing and decoding independently with three warmups and seven rotated samples.
Packet bytes and decoded values are checked; no SQL or worker messaging is involved.

| Variant | Encode median ms | Decode median ms |
|---|---:|---:|
| Existing codec | 59.09 | 36.49 |
| Reuse the writer's row budget | 76.44 | — |
| Skip repeated accounting during construction | — | 36.62 |
| Copy byte values with ArrayBuffer.slice | — | 139.97 |
| All three changes | 73.75 | 136.36 |

Encoding is noisy (baseline 49.52–71.50 ms, budget-reuse 50.09–92.20 ms), so it establishes
no reliable improvement. The byte-copy regression is clear (baseline 35.89–37.24 ms versus
131.02–143.85 ms). All codec changes were reverted. Raw data and the exact rejected prototype
patch are retained; these isolated Node measurements are not browser measurements. A temporary
browser consumer bypassing per-row awaits also failed to show a gain in its exploratory run;
it is insufficient evidence to justify adding a batch-consumption API.

## Reproduction and verification

Use `mise run bench:ts_async_heavy` or `mise run bench:ts_async_browser` with
`JED_ASYNC_HEAVY_CASES=typed-output`. Defaults are three warmups, seven latency samples and
three responsiveness samples. `JED_ASYNC_HEAVY_BATCH_ROWS=128` reproduces the old limit on the
new engine; the production difference is only that default. `JED_ASYNC_HEAVY_BATCH_ROWS=1024`
and `JED_ASYNC_HEAVY_BATCH_BYTES=1048576` select the bulk setting. Fix CPU affinity and keep
other benchmarks idle. Per-run JSON and `summary.json` preserve the data behind these tables.

Verification: 71 async host/transport tests pass, including the shared concurrency schedules,
exact early-close prefixes for the default and explicit limits, and message-only cancellation
inside a larger fetch with cleanup before queued work. All nine real Chromium runtime/OPFS
checks pass. Node, browser and worker TypeScript checks and lint pass; lint retains four
existing warnings. All fifteen measurement runs validate identical typed results and costs.
