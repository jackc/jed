# Profile-guided TS delivery experiments

Measured 2026-10-19 against `74ed390506e5443fa6793328194bd85ae441f204`.
**Retained: direct ASCII scanning for successful SQL hex decoding.** The codec and idle-worker
dispatch prototypes are reverted; their patches and measurements remain as experiment artifacts.
Production transport, batch limits and 256-checkpoint task yields are unchanged.

## What was tried

The [preceding profile](../2026-10-19-query-profile/report.md) identified repeated SQL hex
conversion, codec work and delivery scheduling as targets. These experiments change them separately:

| Variant | Change |
| --- | --- |
| baseline | Unchanged production engine at the commit above. |
| hex | Scan successful ASCII hex directly, avoiding the intermediate `TextEncoder` allocation. Malformed input delegates to the original byte decoder. |
| codec | Cache one byte view per packed reader, instead of creating a temporary view before each bytea/UUID copy. Complete preflight and independent output buffers remain. |
| dispatch | Start an idle worker's admitted request in its message callback, avoiding a second task hop. Preserve queued turns, active continuations and cooperative yields. Includes a wakeup for requests arriving while a writer parks. |
| combined | All three changes. |

These are new narrow prototypes. They do not repeat the earlier rejected fused codec, remove
validation, enlarge batches, prefetch results or relax cancellation scheduling.

## Repeated production-boundary measurements

Each entry is the median of three fresh-process medians, each containing seven timed samples
after three warmups. Both boundaries execute the same query, check complete typed fingerprints,
column names/types and SQL cost, and include cursor creation, drain and close. The direct API
is the current internal synchronous core, including its retained-continuation machinery.

| Query | Host | Baseline direct ms | Baseline worker ms | Hex direct ms | Hex worker ms | Worker time change |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| 200,000 typed rows | Node | 339.23 | 539.18 | 255.40 | 505.20 | −6.3% |
| 200,000 typed rows | Chromium | 384.30 | 666.40 | 217.70 | 461.30 | −30.8% |
| 1,000,000-row decimal aggregate → 1 row | Node | 305.80 | 315.49 | 300.51 | 308.06 | −2.4% |
| 1,000,000-row decimal aggregate → 1 row | Chromium | 270.00 | 347.80 | 274.20 | 348.80 | +0.3% |

The aggregate does not call hex decoding. Its variation is a control, not a claimed optimization.
The typed query calls `decode('0001feff','hex')` once per row and returns an i64, decimal, bytea,
two-element i64 array and NULL. Exact SQL and setup remain in every raw result. Cost remains
18,000,000; the aggregate cost remains 4,985,718.

**Faster queries do not mean the async penalty is solved.** Hex makes the direct path faster too:

| Typed-output comparison | Node | Chromium |
| --- | ---: | ---: |
| Baseline worker/direct overhead | +58.9% | +73.4% |
| Hex worker/direct overhead | +97.8% | +111.9% |
| Baseline worker minus direct | 199.95 ms | 282.10 ms |
| Hex worker minus direct | 249.80 ms | 243.60 ms |

The absolute worker query is faster on both hosts, but this is chiefly an expression/allocation
optimization. It does not establish a general transport improvement; the Node async-minus-direct
gap actually grows. These end-to-end samples cannot attribute that difference to one codec,
GC or scheduling stage. The earlier CPU profile motivates the removed UTF-8 work, but its sampled
milliseconds must not be subtracted from these new wall-clock observations.

Use the new paired baselines for comparisons. Historical profiling runs used benchmark worker
adapters, whereas this experiment's throughput uses the actual production Node/browser entry
points. Warmup, code layout, GC and process variation also affect results. In particular, the
browser aggregate overhead here is about 27–29%, not the earlier profile's approximately 2%
median summary. Neither is a universal overhead constant.

## Why the other prototypes were reverted

The initial single-process pilots used two warmups, three timed samples and one separate
responsiveness observation per boundary. They are screening evidence, excluded from the repeated
summary above:

| Typed worker ms | Baseline | Hex | Codec | Dispatch | Combined |
| --- | ---: | ---: | ---: | ---: | ---: |
| Node pilot | 539.45 | 504.80 | 532.37 | 530.79 | 496.79 |
| Chromium pilot | 671.60 | 466.50 | 687.20 | 715.20 | 476.30 |

Repeated combined-versus-hex measurements give only a small additional throughput difference:

| Typed worker | Hex process medians ms | Combined process medians ms | Median improvement |
| --- | --- | --- | ---: |
| Node | 505.20, 509.17, 490.13 | 499.08, 484.45, 508.22 | 1.2% |
| Chromium | 468.50, 461.00, 461.30 | 451.00, 464.20, 456.20 | 1.1% |

The process ranges overlap. Node's median largest timer gap also rises from 7.70 ms with hex to
12.59 ms combined; its worst observation rises from 13.73 to 18.08 ms. This is insufficient
evidence of a worthwhile improvement under the responsiveness constraint. Both additional
changes were reverted. Their standalone pilots do not prove an inherent regression, and the
combined experiment cannot assign its small difference to either change individually.
Pooled means strengthen the decision: Node hex/combined are effectively identical at
493.566/493.575 ms; Chromium is 460.74/458.26 ms, only 0.5% apart. The unchanged aggregate's
pooled worker means also vary by about 4–5% between baseline and hex, underscoring the noise.

No change was made to the packed format or validation policy. Larger codec improvements remain
unproven; the previous fused-writer experiment already failed to establish an end-to-end win.

## Responsiveness and cancellation

Responsiveness is measured separately from throughput: nine observations per query/host/variant.
The following are the median of each observation's largest frame/timer gap, followed by the
largest observed gap. They are small-sample observations, not latency bounds.

| Typed worker | Baseline median / worst ms | Hex median / worst ms |
| --- | ---: | ---: |
| Chromium animation frame gap | 21.80 / 25.20 | 18.90 / 21.20 |
| Chromium 5 ms timer gap | 9.30 / 14.90 | 8.30 / 9.80 |
| Node 5 ms timer gap | 7.40 / 12.98 | 7.70 / 13.73 |

All 18 baseline and all 18 hex browser-worker observations across the two queries record zero
main-thread long tasks. The unchanged aggregate has ordinary frame-timing variation: median
largest gap 19.80 → 20.80 ms, worst 21.60 → 25.10 ms.

Cancellation uses three fresh processes per host/variant, three complete-query warmups, two
abort warmups and seven measured aborts. The benchmark adapter marks the first real task yield
inside active execution; the caller aborts 20 ms after receiving it. Abort-to-rejection uses
one caller clock and excludes subsequent explicit close and reuse checks. This adapter adds
instrumentation, so these are separate observations from production throughput.

| Host / query | Baseline median / maximum ms | Hex median / maximum ms |
| --- | ---: | ---: |
| Node aggregate | 0.58 / 3.05 | 0.53 / 5.33 |
| Node typed output | 0.93 / 3.98 | 0.84 / 5.46 |
| Chromium aggregate | 0.50 / 8.20 | 0.50 / 20.30 |
| Chromium typed output | 5.10 / 52.90 | 5.50 / 46.50 |

Medians are medians of three process medians; maxima cover all 21 samples per cell. All 168
measured aborts returned `57014`, no watchdog fired, and every same-session follow-up succeeded.
The browser typed-output process medians vary widely: baseline 3.5, 5.1, 27.2 ms; hex 5.5, 5.0,
25.1 ms. There is no comparable large bulk-query regression to the rejected elapsed-scheduling
experiment. This does not prove identical cancellation latency: maximum aggregate cancellation
rose in the hex runs even though that query never executes the changed code. Both the control
variation and the fixed observation phase limit causal claims and worst-case guarantees.

## Correctness checks

The shared string-functions corpus adds empty/whitespace/mixed-case inputs, Unicode errors and
an exact per-row cost assertion. PostgreSQL regenerates it byte-identically; Go and Rust pass
the filtered file in both memory and disk modes. TS host tests compare arbitrary UTF-16 input
(including lone surrogates) with the original byte decoder and check independently owned output
buffers. No result caching, constant folding or cost-meter changes are involved.

Final verification passes: all three TS typecheck configurations; 1,137 TS unit tests;
677 memory and 596 disk TS corpus files (2/83 expected skips); all nine Chromium end-to-end
tests, including OPFS and message-only cancellation. The PostgreSQL oracle check and filtered
Go/Rust memory/disk checks above also pass. Repository lint passes with four existing warnings
(the oversized executor, two unused symbols, and a type-import style warning).
Detailed commands are in [verification.json](verification.json).

## Reproduction and scope

Node v26.10.0 / V8 14.6.202.34; headless Chromium 149.0.7827.55; Linux x64;
Intel Core Ultra 9 285K. Every benchmark process uses CPU affinity `0,2`. Runs are serial;
variant order reverses for the middle repetition and each process alternates direct/worker
sample order. No builds or other benchmark/test suites ran concurrently with the measured matrix.
The checkout's PostgreSQL supervisor was idle after the oracle check.

The browser is an ordinary origin without cross-origin isolation or shared memory. Node uses
atomic cancellation; browser cancellation is message-based. Storage is warm in-memory, 8 KiB
pages. Limits remain 512 rows / 256 KiB per batch, 2 MiB client result credit. Timed throughput
does not pass an AbortSignal; cancellation is a separate instrumented experiment.

[Reproduction instructions](experiments/reproduce.md), source manifests and patches identify
every variant. [Raw-result manifest](manifest.json) and [derived summary](summary.json) retain individual samples, validation,
environment, bundle hashes and timer/frame observations. Source hashes were checked before
and after every process, and the selected production source matches the frozen `hex` variant.
The revision field inside raw results describes the launching checkout; variant source hashes
identify the actual measured engine.

These measurements do not cover OPFS/durable reads, other browser engines, ARM, app rendering,
arbitrary bytea-heavy SQL or worst-case scalar/sort cancellation. Valid hex still allocates an
independently owned output for each evaluation and accrues the same deterministic SQL cost.
Malformed input may scan a valid prefix twice before returning the original error; the success
optimization deliberately preserves the existing UTF-8 error behavior.
