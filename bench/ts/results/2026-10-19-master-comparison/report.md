# Rebased TS runtime versus master — 2026-10-19

> Archive: [original report and compact provenance](https://github.com/jackc/jed/tree/cb23e92204eb93b38869adb2088c9c4b95ca2b1d/bench/ts/results/2026-10-19-master-comparison). Measured commit IDs are unchanged; the bulk raw runs described below remain local, ignored files and were never committed.

The production worker path takes 17–36% longer than master's direct core for these three expensive,
small-result Node workloads, and 18–36% longer in Chromium. The 200,000-row typed drain is 64% slower
on Node and 19% slower in Chromium. This is the full branch-versus-original-core comparison;
earlier reports mostly compared the worker with the already-modified branch core.

The typed workload includes repeated SQL hex decoding, which this branch accelerates. Worker
delivery still adds 86.8% (Node) / 89.5% (Chromium) over the branch's own direct core. The total
master comparison therefore does **not** imply that typed delivery overhead itself is only 19%.
The direct-core columns also reveal execution regressions: resident join/group is 30% slower on
Node and 24% slower in Chromium before introducing the worker boundary.

## Revisions and method

- Master: `0b1869c8e97051e7e4c88853058cf11eb942181b`.
- Rebased engine: `f70719f03f1a6589d148481f120a0000c5bcc61a`. The rebase was clean; the subsequent comparison
  harness/report changes do not alter this measured engine.
- Intel Core Ultra 9 285K, Linux 7.0.0-38-generic, x64; both hosts pinned to CPUs 0,2.
- Node 26.10.0 / V8 14.6.202.34-node.34. Headless Chromium 149.0.7827.55 / V8 14.9.207.21;
  production Vite bundles, ordinary origin, no COOP/COEP or SharedArrayBuffer in the browser.
- Three independent processes per variant and host; variant order rotates through ABC/BCA/CAB.
  Each heavy case has three warmups, seven timed complete drains and three separate responsiveness
  probes. Each cheap case has three warmup batches and 21 timed batches of 100 sequential calls.
  Headline latency is the median of the three process medians (21 heavy latency samples).
- Frozen engine sources come from git archives, using the same existing installed dependencies.
  Each process loads only its selected revision and boundary. Direct cheap calls never await
  per operation. Browser source hashes are recorded before build; source and bundle hashes are
  checked before/after measurement. All variants agree on typed fingerprints, row counts,
  column metadata, SQL/setup, parameters, and deterministic cost. Prepared validation checks 42→43.
- In-memory databases, 8 KiB pages, production worker limits (512 rows / 256 KiB per batch,
  2 MiB result credit), complete typed drains. No explicit AbortSignal in timed queries.
  Startup, setup/preparation, result comparisons and responsiveness monitoring are untimed.
- The first Node master run overlapped a 3.7-second harness typecheck. The entire initial Node
  ABC block was excluded and repeated after the matrix. The retained chronological Node block
  order is BCA/CAB/ABC; browser order is ABC/BCA/CAB. Excluded runs remain local only.

## Measurements

Median of independent process medians. Rotating order; complete typed drains. Responsiveness measured separately. All variants agree on values, types, costs and configuration.

node:

| Workload | Master direct | Branch direct | Branch worker | Direct/master | Worker/master |
|---|---:|---:|---:|---:|---:|
| decimal-aggregate (ms) | 283.86 | 313.92 | 332.86 | +10.6% | +17.3% |
| resident-join-group (ms) | 351.37 | 454.94 | 476.75 | +29.5% | +35.7% |
| sort-top100 (ms) | 416.45 | 499.23 | 561.91 | +19.9% | +34.9% |
| typed-output (ms) | 280.15 | 245.96 | 459.44 | -12.2% | +64.0% |
| cheap-query (us) | 9.76 | 11.65 | 48.41 | +19.4% | +396.2% |
| prepared-execute (us) | 2.06 | 7.85 | 21.10 | +281.2% | +924.1% |

| Heavy workload | Master timer gap | Worker timer gap | Worker frame gap | Worker long tasks |
|---|---:|---:|---:|---:|
| decimal-aggregate | 315.99 ms | 12.75 ms | — | — |
| resident-join-group | 390.95 ms | 10.60 ms | — | — |
| sort-top100 | 411.58 ms | 12.40 ms | — | — |
| typed-output | 301.50 ms | 14.88 ms | — | — |

browser:

| Workload | Master direct | Branch direct | Branch worker | Direct/master | Worker/master |
|---|---:|---:|---:|---:|---:|
| decimal-aggregate (ms) | 233.40 | 226.30 | 317.30 | -3.0% | +35.9% |
| resident-join-group (ms) | 272.80 | 337.00 | 358.00 | +23.5% | +31.2% |
| sort-top100 (ms) | 283.10 | 314.90 | 335.10 | +11.2% | +18.4% |
| typed-output (ms) | 338.90 | 212.90 | 403.50 | -37.2% | +19.1% |
| cheap-query (us) | 6.00 | 11.00 | 58.00 | +83.3% | +866.7% |
| prepared-execute (us) | 2.00 | 4.00 | 22.00 | +100.0% | +1000.0% |

| Heavy workload | Master timer gap | Worker timer gap | Worker frame gap | Worker long tasks |
|---|---:|---:|---:|---:|
| decimal-aggregate | 310.70 ms | 12.40 ms | 23.90 ms | 0 |
| resident-join-group | 294.90 ms | 14.30 ms | 23.20 ms | 0 |
| sort-top100 | 372.70 ms | 12.00 ms | 23.90 ms | 0 |
| typed-output | 412.40 ms | 11.30 ms | 21.30 ms | 0 |

The timer/frame columns report the worst observed gap across nine separate probes per case.
All 36 browser worker probes observed zero main-thread long tasks. A worker isolates synchronous
sorting and scalar execution from the application thread; it does not make those kernels
interruptible. This measurement does not establish cancellation latency.

## Variation and interpretation

Heavy process medians in milliseconds (each entry lists all three independent processes):

| Host / workload | Master direct | Branch direct | Branch worker |
|---|---:|---:|---:|
| node / decimal-aggregate | 283.9, 287.6, 280.6 | 313.9, 308.3, 359.2 | 324.8, 332.9, 345.9 |
| node / resident-join-group | 360.4, 351.4, 349.5 | 393.0, 454.9, 487.4 | 476.7, 468.8, 491.8 |
| node / sort-top100 | 416.5, 415.3, 455.9 | 454.8, 573.3, 499.2 | 561.9, 562.4, 530.7 |
| node / typed-output | 280.1, 288.7, 280.2 | 246.0, 245.0, 254.2 | 430.1, 459.4, 459.9 |
| browser / decimal-aggregate | 227.2, 255.1, 233.4 | 214.3, 226.3, 236.5 | 279.9, 317.3, 339.1 |
| browser / resident-join-group | 272.8, 280.3, 261.9 | 318.1, 337.0, 340.5 | 335.7, 363.5, 358.0 |
| browser / sort-top100 | 257.5, 343.2, 283.1 | 296.2, 354.7, 314.9 | 335.1, 375.9, 327.6 |
| browser / typed-output | 313.6, 355.9, 338.9 | 212.9, 210.7, 234.2 | 378.1, 403.5, 415.1 |

These are measurements on one machine, with visible process-to-process variation; differences of
a few percent should not be generalized. Cheap-call percentages look large because the original
calls take only a few microseconds; use their absolute times when assessing application impact.
The branch's direct path remains internal and synchronous, so its timings isolate core changes;
they are not an alternative public async API.

The browser baseline here is master's direct semantic core. Master's older browser client already
used a worker, but returned fully rendered string results with a different API. This is not a
before/after benchmark of that old rendered-string client. Browser frame comparisons demonstrate
isolation relative to executing the original core on the main thread. File I/O, cold startup,
mobile devices, other browser engines, concurrent clients and encryption are outside this run.

WebCrypto remains follow-on work. Async traversal/retained continuations provide a foundation,
but the encrypted-page codec, async open/recovery/maintenance/commit paths and nonce protocol are
not implemented by this branch. These numbers do not measure encrypted SQL or predict its gains.

## Reproduce and verification

`JED_COMPARE_MASTER=0b1869c8e97051e7e4c88853058cf11eb942181b JED_COMPARE_BRANCH=f70719f03f1a6589d148481f120a0000c5bcc61a JED_COMPARE_CPUS=0,2 mise run bench:ts_compare_matrix`

See [the harness](../../compare/README.md) for controls and [compact provenance](https://github.com/jackc/jed/blob/cb23e92204eb93b38869adb2088c9c4b95ca2b1d/bench/ts/results/2026-10-19-master-comparison/provenance.json)
for exact result/source hashes. Raw gzip results, snapshots, bundles and detailed summaries remain
in ignored `bench/results/ts-master-comparison-final-20261019/`; no new raw benchmark outputs are committed. A reproduction should run with
other tests/builds stopped and does not need the one-off contaminated-block replacement.

The rebased engine passed all 18 `mise run ci` steps: generated-data verification, formatting,
linting, TS/web checks, cross-core conformance, unit/CLI/Ruby/migration tests, process locking,
simulation regressions and sweeps, both browser suites, and corpus sweeps/reduction checks.
The added harness passed formatting/linting, strict scoped Node/browser typechecks and all six
reduced revision/mode smoke runs. The full matrix's value/type/cost validation also passed.
