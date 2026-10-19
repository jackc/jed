# Query-stage profiling: TypeScript async runtime

> Archive: [original report, committed data and experiment artifacts](https://github.com/jackc/jed/tree/8b450c4f298390fff405d2b1e6f1df63de047819/bench/ts/results/2026-10-19-query-profile). Artifact paths and historical reproduction commands below refer to that snapshot; measured commit IDs are unchanged. Outputs explicitly described as local or ignored were not committed.

Measured 2026-10-19 against production engine commit `d4f74290`. Production execution,
scheduling, codecs and limits are unchanged. The new code is a reproducible profiling harness.

The bulk-result penalty is spread across result encoding, message delivery/dispatch, decoding
and cooperative scheduling. Per-row Promise/iterator bookkeeping is a comparatively small
part. Expensive computation returning one row has much less end-to-end overhead. The CPU
profiles also reveal substantial SQL expression work that must not be mistaken for transport.

## Uninstrumented end-to-end results

Each entry is the median of three process medians, each containing seven samples. Query
creation, complete typed drain with the existing fingerprint consumer, and cursor close are
included. Setup and warmup are excluded. Direct means the same current semantic core's internal
synchronous API, including retained continuations; it is not an older pre-continuation engine.

| Query | Host | Direct ms | Worker ms | Worker overhead |
|---|---|---:|---:|---:|
| 1,000,000-row decimal aggregate → 1 row | Node | 308.14 | 313.27 | +1.7% |
| 1,000,000-row decimal aggregate → 1 row | Chromium | 272.80 | 277.40 | +1.7% |
| 200,000 typed output rows | Node | 336.20 | 523.72 | +55.8% |
| 200,000 typed output rows | Chromium | 360.30 | 674.50 | +87.2% |

The bulk query returns an i64, decimal, bytea, two-element i64 array and NULL per row:

```sql
SELECT g::i64 AS id, g::decimal AS exact,
       decode('0001feff','hex') AS bytes,
       ARRAY[g::i64, g::i64 + 1] AS pair, NULL::text AS absent
FROM generate_series(1, 200000) AS g
ORDER BY id
```

Process medians vary: direct/worker bulk medians are 351.1/510.5, 336.2/523.7 and
332.2/543.0 ms in Node; 360.3/671.3, 375.1/684.3 and 354.9/674.5 ms in Chromium.
Aggregate direct/worker medians are 315.2/290.1, 258.8/321.4 and 308.1/313.3 ms in Node;
272.8/272.6, 258.6/277.4 and 282.1/281.3 ms in Chromium. The approximately 2% aggregate
summary is not a stable universal overhead constant or proof that asynchronous work is free.
Ratios of pooled mean times give aggregate overhead of 6.5% Node / 9.8% Chromium, making
the variability visible even though the process-median summary is only 1.7%.

## Additive bulk-query wall-time breakdown

These are **coarse-instrumented arithmetic means**, over 21 fingerprint samples per host.
Components partition each sample before averaging; they add to the mean total. They are not
independent medians and must not be substituted directly into the uninstrumented table above.

| Stage | Node ms | Chromium ms |
|---|---:|---:|
| Worker execution + encoding + runtime dispatch, excluding awaited yields | 368.94 | 413.32 |
| Worker cooperative scheduling waits | 14.74 | 62.27 |
| Message delivery/posting/dispatch residual, both directions | 92.32 | 115.94 |
| Caller: complete batch validation | 19.37 | 20.30 |
| Caller: construct decoded values and result arrays | 21.64 | 19.87 |
| Caller: iteration, fingerprinting, Promise/runtime bookkeeping and cleanup | 41.77 | 41.29 |
| **Total** | **558.80** | **672.99** |

This result is exactly **560 fetches**, transferring **14,582,242 binary bytes** for
200,000 rows. The expanded-value byte allowance fills before the 512-row limit. Average
physical packet size is about 25.4 KiB despite the 256 KiB admission limit: the latter accounts
for expanded values and overhead, not just the packed binary representation. There are 561
ordinary RPCs including query creation. No data-page I/O wait is recorded: this is an in-memory
workload. The driver makes 2,683 awaited execution yields during each bulk drain.

The initial query-creation interval averages 0.77 ms Node / 1.80 ms Chromium. First pull takes
63.38 / 62.00 ms, including work required before the first batch. The remaining drain takes
494.64 / 609.20 ms. Thus the large output penalty is mostly accumulated while producing and
delivering the complete result, rather than a single expensive startup call.

The delivery/dispatch residual is caller send-to-reply duration minus worker receipt-to-send
duration. It includes both message directions, posting, queue scheduling and pauses outside
the worker service interval. It is **not a pure transport or copying measurement**. For example,
one Node sample has a 0.034 ms median per-RPC residual but two roughly 11 ms outliers. The
transferable buffer avoids a structured clone of its bytes; constructing and validating values
still costs work. Shared memory cannot be credited with eliminating the entire residual.

## What is inside worker execution and encoding?

Independent 1 ms V8 CPU profiles cover five repeated queries per path, with plain engine
sources and no stage timers. Approximate milliseconds below are interval-weighted samples
normalized by five, not exact baseline wall-clock costs. Profiles contain idle, GC, profiler
control and unclassified samples; those are retained in the raw data. Do not add inclusive
function times or sampled CPU figures to the preceding wall-time table.

| Typed-output sampled work | Node ms/query | Chromium ms/query |
|---|---:|---:|
| Core query execution, direct / worker | 254 / 273 | 276 / 296 |
| Worker result admission/preflight + binary writing | 57 | 63 |
| GC on direct execution thread | 33 | 72 |
| GC on worker execution thread | 49 | 90 |
| Caller fingerprinting, direct / worker | 36 / 28 | 32 / 23 |

**A major hotspot is SQL's repeated hex decoding.** The constant expression
`decode('0001feff','hex')` is evaluated for every output row. In Node,
[`decodeText`](https://github.com/jackc/jed/blob/d4f742903acbce75e7235d92b6bf433c5c92891e/impl/ts/src/executor.ts#L1602) accounts for roughly 62–66 ms/query
inclusive: approximately 46–50 ms in native UTF-8 encoding and 15 ms in
[`decodeHexPairs`](https://github.com/jackc/jed/blob/d4f742903acbce75e7235d92b6bf433c5c92891e/impl/ts/src/value.ts#L401). Chromium's native encoding alone
accounts for about 80–88 ms/query. This is expression evaluation in both direct and worker
paths, not result serialization. The full `applyScalarFunc` branch has inlined descendants in
Chromium, so its larger inclusive time is not a separate function-dispatch cost.

Expression traversal also matters. Node `isPendingEval` (`eval.ts:429`) has about 29 ms/query
direct / 21 ms worker sampled self-time, while `evalExprMaybeSteps` (`eval.ts:604`) has about
20 / 23 ms self-time. Its approximately 194–197 ms inclusive figure includes casts, scalar
functions and their descendants; summing those figures would double count work.

**Encoding is material, but is not the entire worker overhead.** Node's
`measureValues` (`async/wire.ts:331`) contributes about 21 ms/query inclusive. Writer `append`
(`async/packed.ts:66`) contributes about 56 ms/query inclusive, including that measurement.
The exclusive encoder categories total about 57 ms Node / 63 ms Chromium. Admission checking,
recursive value traversal and writing all contribute. Chromium's optimized `grow` stack
apparently contains functions its source never calls, so its inclusive samples cannot reliably
be called buffer-allocation cost; the broader codec attribution is more trustworthy.

Allocation/GC is significant in both paths. It is already included in measured wall stages,
not another row to add. A single profiled process is insufficient to establish a stable GC
regression. Parsing/planning accounts for only a handful of samples and query creation is
small. Sorting is not a major sampled cost here; the generated IDs are already ordered, so
this does not generalize to arbitrary large sorts.

## Row iteration is a smaller target than the complete caller interval suggests

With the fingerprint consumer removed, every row is still decoded and drained. Coarse caller
remainder falls from 41.77 to **8.85 ms** in Node and from 41.29 to **9.64 ms** in Chromium.
That remainder includes the row API, Promise/microtask work, client accounting, loop and close;
it is not a pure Promise timer. This bounds the scale of that observed interval in these trials.

Uninstrumented count-only process-median summaries are 307.64 ms direct / 498.20 ms worker
in Node, and 336.20 / 631.50 ms in Chromium. They still show about 62% / 88% overhead.
Fingerprint-versus-count differences are sensitivity measurements, not exact kernel subtraction:
consumer order, JIT, scheduling and GC can move other parts of the query too. CPU samples
independently identify substantial fingerprint work. The absence of samples explicitly named
Promise does not establish zero Promise overhead.

A batch-consumption API could be useful, but eliminating only per-row awaits is unlikely to
recover hundreds of milliseconds from this workload. Avoiding decoded object construction
would be a different, larger API/representation change.

## Expensive query with one result

The million-row decimal aggregate returns a single 24-byte packet. Encoding, validation and
construction together are below 0.1 ms in the timed diagnostics; message delivery/dispatch is
about 0.5 ms Node / 1.3 ms Chromium. The client remainder is about 0.1 ms. Its 15,625 cooperative
yields and ordinary execution dominate.

Node CPU samples attribute approximately 267 ms/query direct / 261 ms worker to query execution,
plus 43 / 48 ms GC. Aggregate folding (`foldAcc`, `executor.ts:39692`) contributes roughly
83 / 77 ms inclusive, and generated-row materialization approximately 35 / 36 ms inclusive.
Decimal addition, casts, cost metering and execution-loop work are visible hotspots.

The coarse diagnostics report 40.13 ms Node / 90.55 ms Chromium of scheduling waits, but
**the Chromium aggregate is materially perturbed by instrumentation**. Those are not baseline
wait estimates. The CPU-profiled Chromium aggregate also has much more GC than its direct
profile; this single sample window cannot establish a normal asynchronous GC penalty.

## Instrumentation calibration and limits of precision

The following changes compare worker median-of-process-medians with the plain baseline. Mean
changes are also retained because stage tables use means. CPU mode has one process with five
queries per path, so it is a diagnostic rather than a fourth repeated benchmark arm.

| Host / query | Coarse median / mean change | Detailed median / mean change | CPU median / mean change |
|---|---:|---:|---:|
| Node aggregate | −1.4% / −2.6% | −1.7% / −1.5% | −0.3% / −1.2% |
| Node typed output | +5.3% / +5.9% | +15.3% / +12.3% | −6.4% / −5.5% |
| Chromium aggregate | +22.4% / +12.3% | +25.5% / +17.2% | +24.2% / +15.4% |
| Chromium typed output | +1.2% / −0.5% | +18.8% / +18.3% | −10.6% / −12.4% |

Detailed timers put clocks around 200,559 encoder append/admission attempts and 560 finalizations.
They report 77.75 ms Node / 127.13 ms Chromium encoder time, but visibly perturb throughput.
These numbers are retained as diagnostics and are not presented as true baseline encoding time.
Coarse mode has no per-row timers, but its many awaited-yield timers can still affect execution.
Negative calibration deltas do not mean profiling makes the engine inherently faster; process,
JIT, allocation and scheduling differences remain visible. No synthetic clock-cost correction is
applied and no mixed-mode table is claimed to exactly reconstruct uninstrumented execution.

CPU sampling has further limits. Inspector control work is substantial in some Node profiles;
Chromium has unclassified `(program)` intervals. Original and generated frames are preserved.
Local browser `MessageChannel` posts are classified as **scheduler**, not result transport:
about 13.5 ms/query for the aggregate and 2.7 ms/query for typed output in the sampled run.
The shared packed reader's functions cannot reliably split validation from construction using
stacks alone; batch timers provide that split. Optimized stacks can fold functions together.

## Implications for further optimization

1. **Optimize substantial codec work without enlarging batches.** Worker encoding plus caller
   decoding is roughly a hundred milliseconds in the bulk diagnostics. A better representation
   or less duplicate traversal can help, provided admission validation, lossless values and bounded
   decoding remain intact. The previous fused-codec prototype was already rejected by measurements;
   this profile does not turn that specific implementation into a win.
2. **Investigate the delivery pipeline separately from byte copying.** The sequential request/fetch
   pattern accumulates a material residual. Bounded overlap/prefetch or an optional shared-memory
   protocol might reduce some gaps, but neither can be assigned the whole 92–116 ms residual as a
   guaranteed saving. Keep the current batch size and test cancellation and main-thread latency.
3. **Treat scalar/continuation work as a separate absolute-throughput opportunity.** Repeated SQL
   UTF-8/hex decoding and expression dispatch are substantial in both APIs. A hex ASCII fast path
   could avoid the UTF-8 allocation on valid inputs, but malformed-Unicode errors, costs and all
   cores' behavior must remain correct. Constant-expression caching would additionally need careful
   cost, ownership and value-aliasing semantics. These do not specifically remove worker overhead.
4. **Do not prioritize row awaits alone or another larger-batch experiment.** The count-only
   caller remainder is about 9–10 ms; larger batches already have known latency tradeoffs. The
   scheduling policy remains restored to 256 checkpoints, with no new latency tradeoff accepted.

## Reproduction, evidence and verification

Host: Intel Core Ultra 9 285K, Linux x64, CPU affinity `0,2`; Node v26.10.0 / V8
14.6.202.34-node.34; installed headless Chromium 149.0.7827.55. Chromium runs on an ordinary
local origin without isolation headers (`crossOriginIsolated=false`, no SharedArrayBuffer),
with a foreground visible page, production minification and source maps. Runtime limits are
512 rows / 256 KiB per batch / 2 MiB held results. No AbortSignal is supplied. The experiments
are warm in-memory execution with 8 KiB pages, not cold disk or OPFS measurements.

The 18 timing runs rotate mode order across three fresh-process repeats: plain/coarse/detailed,
coarse/detailed/plain, then detailed/plain/coarse. Host order alternates by repeat. Each process
runs three full warmups and seven samples per direct/worker path for both fingerprint and count
consumers. Direct/worker order alternates within samples. Count trials follow fingerprint trials.
Two later CPU runs use five queries per path. All performance runs are serialized. CPU and
stage profiling are separate; responsiveness probes do not run inside either measurement.

```sh
JED_PROFILE_HOST=node JED_PROFILE_MODE=coarse \
JED_PROFILE_RESULTS=/tmp/jed-profile-node-coarse \
JED_PROFILE_SAMPLES=7 JED_ASYNC_HEAVY_WARMUPS=3 \
taskset -c 0,2 mise run bench:ts_async_profile

JED_PROFILE_HOST=browser JED_PROFILE_MODE=cpu \
JED_PROFILE_RESULTS=/tmp/jed-profile-browser-cpu JED_PROFILE_CPU_REPEATS=5 \
taskset -c 0,2 mise run bench:ts_async_profile

mise run bench:ts_async_profile --analyze /tmp/jed-profile-analysis \
  /tmp/jed-profile-node-coarse /tmp/jed-profile-browser-cpu
```

The [summary](https://github.com/jackc/jed/blob/8b450c4f298390fff405d2b1e6f1df63de047819/bench/ts/results/2026-10-19-query-profile/summary.json) contains all grouped timing statistics, process medians and CPU
bucket summaries. [analysis.json.gz](https://github.com/jackc/jed/blob/8b450c4f298390fff405d2b1e6f1df63de047819/bench/ts/results/2026-10-19-query-profile/analysis.json.gz) retains every sample's additive ledger,
CPU overviews and analysis methodology. [runs.json](https://github.com/jackc/jed/blob/8b450c4f298390fff405d2b1e6f1df63de047819/bench/ts/results/2026-10-19-query-profile/runs.json) records chronology. Raw results
and profiles are under `raw/<host>-<mode>-<repeat>/`; CPU runs include compressed source maps.
Full mapped CPU summaries are under `cpu/`. Decompress each `results.json.gz` to `results.json`
and browser `site/assets/*.map.gz` to `.map` to re-run the analyzer on an archived directory;
compiled JavaScript is not needed for offline source mapping. Original paths may no longer
exist, but archived hashes, maps and the measured commit identify their source.

[manifest.json](https://github.com/jackc/jed/blob/8b450c4f298390fff405d2b1e6f1df63de047819/bench/ts/results/2026-10-19-query-profile/manifest.json) records archive hashes, the measured source hashes and environment.
All 142 recorded engine/workload source files and 14 actual measurement-adapter/tool files are
fixed across the 20 runs. Offline analyzer/classification code and documentation evolved during
collection; full `toolSha256` maps are therefore not identical. The final classification corrects
local scheduler-message attribution. Instrumentation patches are under `instrumentation/`; plain
leaves original engine bytes unchanged and adds only an unused counter module. Browser generated
JavaScript hashes agree across repeats of each mode; source maps retain distinct snapshot paths.

All **1,048 measured drains** agree on row count, column names/types and logical cost. Of these,
544 fingerprint drains additionally verify values; 504 count-only trials deliberately omit value
fingerprinting. Setup/reference validation checks full fingerprints for both paths in every
process. The aggregate fingerprint is `79e4eed1d6ce1755` at cost 4,985,718; typed output is
`ee3bf2c6ac80465a` at cost 18,000,000. RPC IDs/order, stage counts, packet totals, manifests/patches
and additive means were independently audited. No pending RPCs or negative RPC residuals occurred.

Node and Chromium smoke runs passed; all three TypeScript type-check programs passed. Lint passed
with four pre-existing warnings (large executor file, unused resolver variable/import and a test
import-style warning). The profiler/counter/source-map helpers also passed small lifecycle and
accounting checks. No SQL, storage format, public API or production scheduling changes were made,
so shared semantic conformance was not re-run for this benchmark-only change.

These results cover two specific workloads on one machine, one Node version and Chromium. They
do not establish latency tails, a worst-case cancellation bound, peak heap use, cold-storage
costs, other browser engines or ARM performance. The profiles identify significant work and
useful optimization targets; they do not support nanosecond-exact attribution of uninstrumented
query execution or a predicted speedup from shared memory.
