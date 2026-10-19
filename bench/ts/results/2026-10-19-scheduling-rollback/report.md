# Restore checkpoint scheduling after cancellation regression

> Archive: [original report, committed data and experiment artifacts](https://github.com/jackc/jed/tree/752cd4b922673af9a107ea4b65a8ee3a6a24fd7b/bench/ts/results/2026-10-19-scheduling-rollback). Artifact paths and historical reproduction commands below refer to that snapshot; measured commit IDs are unchanged. Outputs explicitly described as local or ignored were not committed.

Production restores the task yield at every 256 cooperation visits from
`2a8bdfcb264d1ef388296ee8554b485bee3e7d26`. The elapsed-time scheduler introduced in
`14745cbd` is rejected: its aggregate throughput improvement came with slower message-only
browser cancellation. A fresh experiment with a shorter **0.125 ms** slice did not remove
that regression, so neither the 1 ms nor the 0.125 ms policy remains enabled.

For the 200,000-row typed workload, the fresh browser comparison records a median of process
medians of **0.7 ms** from abort to rejection for the restored scheduler, **15.0 ms** for
1 ms scheduling, and **16.6 ms** for 0.125 ms scheduling. The largest observed delays are
32.1, 61.8 and 54.0 ms respectively. These are small-sample observations, not latency bounds.
All measured operations cancel correctly and leave the session reusable; the regression is
in responsiveness.

The packed result codec, 512-row / 256 KiB batch limits and 2 MiB held-result allowance remain
unchanged. This rollback adds no shared-memory transport, new result API, dependency or SQL
behavior. The [earlier report](../2026-10-19-scheduling/report.md) retains the throughput and
codec experiments as historical evidence; its initial selection of 1 ms is superseded here.

## Fresh cancellation comparison

Nine serialized process runs compare three variants, with three repeats each, in ordinary
Chromium without SharedArrayBuffer. The process order rotates:

1. baseline, current, eighth;
2. eighth, baseline, current;
3. current, eighth, baseline.

`baseline` is the original 256-visit scheduler; `current` is the then-current 1 ms scheduler;
`eighth` is its 0.125 ms experimental variant. The names in raw records describe the experiment,
not the production policy after this rollback. The [summary](https://github.com/jackc/jed/blob/752cd4b922673af9a107ea4b65a8ee3a6a24fd7b/bench/ts/results/2026-10-19-scheduling-rollback/summary.json), [manifest](https://github.com/jackc/jed/blob/752cd4b922673af9a107ea4b65a8ee3a6a24fd7b/bench/ts/results/2026-10-19-scheduling-rollback/manifest.json)
and `raw/{browser-baseline,browser-current,browser-eighth}-{1,2,3}.json` retain chronology,
individual samples and source/bundle hashes.

Each process runs the same two queries as the earlier cancellation probe: exact-decimal
aggregation over 1,000,000 generated integers, and complete ordered generation/delivery of
200,000 mixed typed rows. Each case has three complete-query warmups and two cancellation
warmups before seven measured cancellations. Complete warmups validate row count, column
names/types, typed fingerprint and deterministic cost. There are **21 measured cancellations
per case and variant**, 126 in total.

The benchmark-only adapter posts an active marker at the first real task yield inside the
query/fetch continuation. The caller requests abort 20 ms after receiving that marker.
Abort-to-rejection uses only the caller's clock and ends before explicit cursor close and
the follow-up query. Submit-to-marker, marker-to-abort and worker-local receipt-to-error
timings are retained separately; no cross-clock subtraction is performed. Instrumentation
adds one marker message and bookkeeping. This samples a particular active execution phase,
not cancellation before a query starts or every possible point in its execution.

Host: Intel Core Ultra 9 285K, Linux x64, CPU affinity `0,2`, Chromium `149.0.7827.55`
with V8 `14.9.207.21`. The minified production build runs in a real worker with a visible
1280×800 headless page. All pages report `crossOriginIsolated=false` and no SharedArrayBuffer.
Databases are warm and in-memory with 8 KiB pages. Transport limits are fixed across variants.
These runs measure cancellation only; they add no new throughput comparison.

## Results and decision

The median column is the **median of three process medians**, not the median of all 21 pooled
samples. The maximum is the largest observation across those 21 samples. All values are ms.

| Query | Variant | Process medians | Median | Observed maximum |
| --- | --- | --- | ---: | ---: |
| decimal-aggregate | baseline: 256 visits | 4.5 / 0.5 / 1.0 | 1.0 | 8.3 |
| decimal-aggregate | current: 1 ms | 2.0 / 1.4 / 0.7 | 1.4 | 10.9 |
| decimal-aggregate | eighth: 0.125 ms | 0.7 / 0.6 / 8.8 | 0.7 | 19.6 |
| typed-output | baseline: 256 visits | 0.9 / 0.7 / 0.4 | 0.7 | 32.1 |
| typed-output | current: 1 ms | 15.0 / 15.0 / 24.0 | 15.0 | 61.8 |
| typed-output | eighth: 0.125 ms | 16.6 / 14.4 / 22.0 | 16.6 | 54.0 |

All 126 measured operations reject with SQLSTATE `57014`; none completes before abort or
expires its watchdog. Every follow-up query verifies reuse of the same session and worker.
Correct cancellation cleanup therefore holds in this probe for all three policies.

The shorter slice does not recover baseline typed-result cancellation. Its aggregate median
is lower, but one process has an 8.8 ms median and its largest delay is also worse. The data
does not support treating a smaller elapsed slice as a reliable fix. The original scheduler
is restored rather than preserving the throughput improvement at this cancellation cost.

The fresh baseline typed median differs from the earlier cancellation matrix's 13.1 ms.
That variation is why these runs retain their own fresh baseline and are not pooled with the
earlier experiment. The regression is present against this contemporaneous baseline in all
three typed-result process pairs, but its magnitude is not a portable constant. Seven samples
per process do not justify percentile or worst-case claims.

This experiment does **not** establish message-queue delay as the cause. The measured interval
includes worker progress, host scheduling, cancellation handling and reply delivery. Further
instrumentation would be needed to separate them. Restoring the earlier policy also does not
make 256 visits a wall-clock bound: parsing, sorting and long scalar kernels remain synchronous,
and the baseline itself has an observed 32.1 ms typed-result delay.

## Shared memory and faster result delivery: available options

The following is platform research and an implementation assessment as of 2026-10-19, not a
new performance experiment or an implemented transport change.

Jed already transfers each packed result's dedicated `ArrayBuffer` through `postMessage`.
`RuntimeServer.finish` supplies the transfer list, and the
[Node adapter](../../../../impl/ts/src/async/node-worker.ts) and
[browser adapter](../../../../impl/ts/src/browser/worker.ts) forward it. Transferring an
ArrayBuffer moves its backing memory without a cross-thread copy; the enclosing message still
uses structured cloning. Shared memory would therefore optimize a path that already has
zero-copy payload transfer. [Browser transfer documentation](https://developer.mozilla.org/en-US/docs/Web/API/Web_Workers_API/Transferable_objects),
[Node worker documentation](https://nodejs.org/api/worker_threads.html#portpostmessagevalue-transferlist).

Copies and conversion still occur around that transfer. `PackedRowsWriter.append` measures
and encodes values; growth can copy the packet, and `finish` normally trims it into an exact
buffer. `decodePackedRows` validates the complete packet, then constructs owned strings,
Decimal objects, arrays and byte buffers. `AsyncRows.pull` requests another batch only after
the current batch is consumed, and `next` returns a Promise for each row. These paths are in
[packed.ts](../../../../impl/ts/src/async/packed.ts) and
[client.ts](../../../../impl/ts/src/async/client.ts). Merely enabling SharedArrayBuffer would
not remove this work.

Current shared memory is a separate cancellation mechanism: `RuntimeConnection.call`
allocates a four-byte shared flag when an AbortSignal is supplied and shared cancellation
is enabled; the worker polls it with `Atomics.load`. Node enables this capability. The
[browser connection](../../../../impl/ts/src/browser/async.ts) enables it only when
`crossOriginIsolated` and SharedArrayBuffer are available. No result rows currently travel
through shared memory.

| Capability | Availability and deployment requirements |
| --- | --- |
| SharedArrayBuffer | Supported by current major browsers; browser use requires a secure context and cross-origin isolation. Node worker threads support shared buffers without browser isolation headers. |
| `Atomics.waitAsync` | Nonblocking synchronization, usable on the browser main thread. Full support: Chrome 90+, Firefox 145+, Safari 16.4+, Node 16+. Baseline since November 2025. |
| `scheduler.yield()` | Supported in Chrome 129+ and Firefox 142+, including workers; Safari is currently unsupported. |
| `scheduler.postTask()` | Supported in Chrome 94+ and Firefox 142+; Safari is currently unsupported. |

Sources: [SharedArrayBuffer API](https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/SharedArrayBuffer),
[waitAsync API](https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/Atomics/waitAsync),
[current Atomics compatibility data](https://raw.githubusercontent.com/mdn/browser-compat-data/main/javascript/builtins/Atomics.json),
[current Scheduler compatibility data](https://raw.githubusercontent.com/mdn/browser-compat-data/main/api/Scheduler.json).

The usual browser deployment sets `Cross-Origin-Opener-Policy: same-origin` and
`Cross-Origin-Embedder-Policy: require-corp`. Cross-origin resources must satisfy CORS/CORP;
opener/popup relationships can change. Embedded applications need cooperation from the parent
frame chain and permission to use `cross-origin-isolated`. COEP `credentialless` offers an
alternative with different resource/cookie behavior where supported. This is a host deployment
choice, so a general embedding library still needs its ordinary-browser fallback.
[Isolation requirements](https://developer.mozilla.org/en-US/docs/Web/API/Window/crossOriginIsolated),
[embedding guidance](https://web.dev/articles/coop-coep),
[COEP behavior](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Cross-Origin-Embedder-Policy).

The concrete opportunities below are engineering hypotheses requiring measurement:

- **Reusable shared slots or a bounded ring.** Encode directly into shared slots and publish
  them with atomic sequence numbers. This could reduce packet allocation/trim and per-batch
  messaging. `waitAsync` permits waiting without blocking the event loop. It does not remove
  value encoding, validation, reconstruction or per-row Promises.
- **Reusable transferred buffers.** Return consumed packet buffers for reuse. This could
  reduce allocation without requiring cross-origin isolation, though messages, safe packet
  boundaries and recycling still cost work.
- **A batch-oriented result API.** Expose already-decoded batches so a consumer can iterate
  synchronously within each batch. This targets per-row Promise/await overhead in all hosts
  and is independent of shared memory.
- **Column-oriented or borrowed typed views.** These could avoid some object reconstruction
  when consumers accept a different representation. Decimal/nested values and mutable bytes
  still require explicit types and ownership. Views into reusable slots need a release/lifetime
  contract; the current owned-value API cannot silently acquire that restriction.

Shared transport must preserve the existing logical byte credits, ordered delivery, cancellation
and close cleanup. Published data must remain stable through validation and decoding; producers
must not overwrite a slot still in use. Pipelining would need bounded credits and an explicit
decision about evaluating rows ahead of caller demand. Blocking `Atomics.wait` inside the
execution worker would also block its control-message and I/O event loop. None of these changes
has been implemented or shown to improve end-to-end performance here.

Scheduler APIs are a separate option. Jed already uses MessageChannel tasks in browsers and
`setImmediate` in Node. `scheduler.yield` provides prioritized continuations, but it requires
a fallback and offers no documented guarantee of faster transport or cancellation. It should
be tested independently rather than assumed to fix the measured regression.
[Scheduler documentation](https://developer.mozilla.org/en-US/docs/Web/API/Scheduler/yield).

## Reproduction and verification

Use the existing cancellation task from this checkout, pointing `JED_PROBE_ROOT` at an isolated
source checkout or reconstructed source snapshot. The restored baseline is `2a8bdfcb`; the
1 ms variant is preserved by the [earlier patch](https://github.com/jackc/jed/blob/752cd4b922673af9a107ea4b65a8ee3a6a24fd7b/bench/ts/results/2026-10-19-scheduling/experiments/short.patch).
The [0.125 ms patch](https://github.com/jackc/jed/blob/752cd4b922673af9a107ea4b65a8ee3a6a24fd7b/bench/ts/results/2026-10-19-scheduling-rollback/eighth.patch) applies to the 1 ms source at `14745cbd`, not directly to
the restored baseline. The [manifest](https://github.com/jackc/jed/blob/752cd4b922673af9a107ea4b65a8ee3a6a24fd7b/bench/ts/results/2026-10-19-scheduling-rollback/manifest.json) identifies each source tree and artifact.
Run the same harness and fixed limits for every variant, with no concurrent performance load.

```sh
JED_ASYNC_HEAVY_CASES=decimal-aggregate,typed-output \
JED_ASYNC_CANCEL_HOST=browser \
JED_ASYNC_CANCEL_RESULTS=/tmp/cancellation-repeat \
JED_PROBE_ROOT=/path/to/source-checkout \
taskset -c 0,2 mise run bench:ts_async_cancel
```

Use a fresh output directory for each process and reproduce the rotating order above. Defaults
are three complete-query warmups, two cancellation warmups, seven measured samples and a 20 ms
delay from the active marker. Record different CPUs or browser versions when reproducing on
another host. Source and bundle hashes, raw samples and timing stages are retained per run;
do not substitute the historical timings for a freshly measured baseline.

Verification of the restored production tree passes:

- TypeScript unit tests: 1,135 passed.
- Chromium runtime tests: 9 passed.
- All three TypeScript programs typecheck.
- Lint: 337 files, zero errors and four existing warnings.
- TypeScript conformance: memory 677 passed / 2 skipped; disk 596 passed / 83 skipped; zero failures.
- `git diff --check` passes.

Every production TypeScript source and test file, plus `CLAUDE.md` and `AGENTS.md`, is
byte-identical to `2a8bdfcb`. Benchmark tools and retained experimental evidence remain available.
The rollback changes the scheduling implementation back to the verified earlier policy; it
introduces no new transport implementation.

The result applies to warm in-memory queries in one headless Chromium environment. It does not
measure Node cancellation anew, isolated-browser cancellation, shared-memory throughput, OPFS,
cold storage, other browser engines, mobile devices or concurrent consumers. It supports the
rollback decision without claiming that any scheduler has a universal cancellation bound.
