# TypeScript query-stage profiling

Run `mise run bench:ts_async_profile` with the existing Node/Vite/Playwright installation.
This does not install dependencies or change production engine files. It prepares an isolated
copy of the TS core, records original/instrumented source hashes and an exact patch, and runs
the production connection, server, codec and execution driver through benchmark-only adapters.

```sh
JED_PROFILE_HOST=browser JED_PROFILE_MODE=coarse \
JED_PROFILE_RESULTS=/tmp/jed-profile-browser \
mise run bench:ts_async_profile
```

Use a fresh output directory. Modes are `plain`, `coarse`, `detailed`, and `cpu`; hosts are
`node` and `browser`. The defaults are three full warmups, seven samples per path and consumer,
and the million-row decimal aggregate plus 200,000 typed output rows. Shared
`JED_ASYNC_HEAVY_CASES` and workload-size variables select the existing heavy workloads.
`JED_PROFILE_SAMPLES`, `JED_PROFILE_CPU_REPEATS` (default three), and `JED_PROFILE_COUNT=0`
control the profiling repetitions and count-only sensitivity trials. Runtime limits remain
512 rows, 256 KiB per batch, and 2 MiB held results. No abort signal is supplied.

The browser uses an ordinary origin without cross-origin-isolation headers, a foreground
headless Chromium page, production minification and source maps. Node uses `setImmediate`
and the browser uses the production-style reusable `MessageChannel` task scheduler. Setup,
full-result validation, warmups, profiling controls and telemetry retrieval are outside each
timed query. Query creation, first pull, the rest of the typed drain and cursor close are
inside. Every measured drain checks row count, column metadata and logical cost; fingerprint
trials additionally check the same typed fingerprint as the direct reference. Count-only
trials still decode and consume every row. Their difference measures consumer sensitivity,
not an exact isolated fingerprint cost.

## Timing boundaries

Coarse mode times batch decoding and genuine awaited execution yields, with no clocks per
row or continuation segment. RPC durations run from immediately before client `postMessage`
to reply-handler entry, and from worker request-handler entry to immediately before reply
`postMessage`. Telemetry is retrieved after draining; ordinary replies carry no extra fields.
The drain is sequential and has no prefetch, so these intervals can form an additive ledger:

- Worker service contains execution, encoding, runtime dispatch and cooperative waits.
- Caller RPC duration minus worker service contains both directions of message delivery,
  posting and dispatch. It is **not** a pure byte-copy or IPC cost measurement.
- Decode validation checks the envelope and all values before construction allocates them.
- The remaining caller interval contains iteration, promises/microtasks, client bookkeeping,
  consumption and cleanup. It is **not** a pure promise-overhead measurement.

Durations are subtracted, never timestamps from different threads. Execution scheduling wait
starts after enqueue returns and ends when the existing await resumes. It includes task and
microtask scheduling delay; it excludes initial server dispatch and synchronous enqueue work.
The I/O wait includes the provider invocation. These workloads are in-memory, so no asynchronous
page-I/O interval is expected.

Detailed mode additionally times each encoder append, its nested admission measurement, and
batch finalization. Subtract measurement from append before adding finalization to avoid
counting nested time twice. This mode intentionally adds clocks per row and can perturb code
optimization or GC. Compare plain, coarse and detailed runs in separate fresh processes before
interpreting its absolute times. Do not subtract a synthetic clock microbenchmark as a correction.
GC occurring within any interval is already included in it.

## CPU samples and analysis

CPU mode profiles the plain engine in both isolates, separately from stage timers. It stores
raw compressed V8 profiles, sampled at a requested 1 ms interval. Node uses one inspector
session per isolate. Browser workers use CDP target sessions; main and worker profiles are
captured independently. CPU windows include small control/setup intervals around repeated
queries. Idle, GC and unclassified samples remain visible.

`cpu-summary.mjs` partitions interval-weighted sampled self-time once per sample. Inclusive
hotspots include descendants and must not be summed. Source mapping retains generated frames
and original source coordinates; inferred stage buckets remain best effort, particularly for
inlined/minified functions. The decoder's shared reading functions cannot reliably distinguish
validation and construction from CPU stacks alone; use coarse stage timers for that split.

Use repeated processes, alternate direct/worker order, serialize performance runs, record CPU
affinity and retain raw data. Means of additive sample partitions sum to the mean total;
independent stage medians need not sum. Preserve small negative residuals caused by clock
quantization instead of clipping them upward. These warm, in-memory measurements say nothing
about cold storage, OPFS durability, other browser engines, peak memory or latency tails.

Analyze completed output directories with:

```sh
mise run bench:ts_async_profile --analyze /tmp/jed-profile-analysis \
  /tmp/jed-profile-node /tmp/jed-profile-browser
```

The analyzer validates RPC identity/order, result/cost equality, exact packet totals, stage
counts and additive means. It writes `analysis.json` with per-sample ledgers and separate
process/pooled summaries, plus compressed full CPU summaries. Add `--wall-only` to skip CPU
mapping. Keep the input browser `site/` source maps until CPU analysis has completed.
