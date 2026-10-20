# Go static kernels: G1 outcome, 2026-10-19

**Retain the opt-in research implementation; do not adopt it as the production
path yet.** Small static kernels improve the intended scans, but the repeated
control measurements do not establish the proposed regression budget. G2
composition and G3 loop replacement remain unimplemented. Rust and TypeScript
code are unchanged.

## Decision evidence

The final five-process matrix meets the two-target gain criterion on memory:
integer arithmetic projection improves **12.9%** and arithmetic-based filtering
**13.5%**, both faster in **5/5** process pairs. Warm-file arithmetic improves
**10.5%**; warm-file filtering improves **9.8%**, just below the suggested 10%
threshold. Against the same experimental build's ordinary control, the memory
gains are 12.3% and 17.2%. This is useful evidence of a dispatch benefit, not a
claim of a general Go speedup.

The controls prevent adoption. The bounded recheck still shows warm-file
columnar aggregation **16.9% slower**, ad-hoc **6.2% slower**, first-call **7.7%
slower**, scalar **3.8% slower**, and a point miss **5.8% slower** than clean.
Several controls have substantial dispersion and even reverse direction between
runs. The untagged build also moves with the host noise; we cannot use that to
explain away all experimental regressions. No samples were discarded.

The first implementation added a non-inlined fallback wrapper per projected
row. The retained implementation replaces that with inlined selectors, keeping
the original evaluator call directly in the ordinary branch. Unsupported
memory scans improved from +5.3% in the initial matrix to +0.2% in the final
matrix. The changed implementation received a fresh correctness pass and timing
matrix; eligibility was not narrowed after looking at results.

The remaining work should investigate cheap-query preparation and experimental
object/code layout before widening the pattern set. There is no evidence here
that adding CASE closure graphs or replacing the scan loop would fix unrelated
columnar/cheap-control regressions. Default builds continue to use the ordinary
evaluator. No architecture, SQL/API, planner, memory-account schedule, cost, or
file-format decision has been changed.

## Method and reproducibility

- Clean source: `dfb0699f18e7c5bb120bc0f885f257791b19e2b9`, fetched and verified
  from origin before work. Only the identical benchmark source was copied into
  its archived Go tree. The runner now pins this revision by default;
  `JED_SPECIALIZATION_BASELINE` can explicitly select another revision.
- Go **1.27.1**, linux/amd64, **Intel Core Ultra 9 285K**, 24 logical CPUs as
  reported by the benchmark suffix. No GOFLAGS, GOMAXPROCS, GOGC or GOMEMLIMIT
  override; default compiler optimization, no PGO, affinity or host isolation.
- Memory plus file create/close/reopen, warm-page read measurements; 4,096 rows
  with deterministic small integer values. Smaller scans use LIMIT 16.
  File setup uses SkipFsync; no write/durability or cold-I/O claim is made.
- All binaries build before timing. Sequential workers, alternating order,
  five fresh process sets at **100 ms per lane**. The initial pass had clean,
  ordinary and selective variants. The final pass adds the untagged disabled
  build. Both full matrices are retained.
- Only suspicious controls were rerun: three fresh process sets, **250 ms per
  lane**, using the exact final binaries. One extra 100 ms arithmetic run
  supplied a diagnostic CPU/allocation profile; its times are not decision data.
  There were no long soak runs.
- Prepared steady state warms both bindings; each timed operation fully drains
  the cursor and checks result checksum plus exact cost. The final report checks
  **both parameter bindings** across every variant/process. First-call churn
  includes prepare plus first execution; ad-hoc includes normal parse/plan.
  Pure parse/compile time and JIT-style warmup are not separately estimated.
- Final engine and timed-test sources match the SHA-256 inventory in
  [`final/environment.json`](final/environment.json). The diagnostic-only weak
  ownership test and runner/report conveniences were added after timing; they
  are excluded from the measured binaries. The initial inventory describes the
  earlier wrapper implementation.

Commands: see the [experiment README](../../README.md). Exact benchmark argv
are preserved in the environment manifest and
[`recheck/commands.json`](recheck/commands.json). `mise run
experiment:go-specialization report` checks and summarizes a run. The initial
`point_miss` accidentally used `id=$1+10000`, which planned a scan; regard that
initial lane as a scan-miss control. The final/recheck lane uses `id=$1` with
missing keys 10050/10051, costs two units, and is a real point miss.

## Final full matrix

Median complete-query time; faster-pair counts are out of five. All raw samples
are in [`final/`](final/), including per-query bytes, allocations, checksums and
costs. Negative percentages mean faster.

| Host/query | Clean µs | Disabled µs | Ordinary µs | Kernels µs | vs clean | Faster pairs |
|---|---:|---:|---:|---:|---:|---:|
| memory/arithmetic | 588.51 | 545.79 | 584.09 | 512.48 | -12.9% | 5 |
| memory/filter | 536.54 | 533.10 | 560.18 | 463.95 | -13.5% | 5 |
| memory/finite | 621.42 | 587.32 | 559.62 | 567.09 | -8.7% | 4 |
| memory/small | 3.18 | 3.14 | 3.33 | 3.05 | -4.1% | 5 |
| memory/offset | 10.46 | 10.18 | 10.35 | 8.80 | -15.9% | 5 |
| memory/point_hit | 1.36 | 1.36 | 1.38 | 1.38 | +1.2% | 3 |
| memory/point_miss | 0.96 | 0.95 | 0.95 | 0.97 | +0.7% | 3 |
| memory/scalar | 0.60 | 0.59 | 0.61 | 0.61 | +0.9% | 3 |
| memory/unsupported | 441.56 | 451.14 | 454.70 | 442.50 | +0.2% | 1 |
| memory/columnar | 65.49 | 71.40 | 74.20 | 75.56 | +15.4% | 2 |
| memory/case | 502.20 | 520.93 | 519.31 | 501.05 | -0.2% | 2 |
| memory/adhoc | 10.06 | 10.02 | 10.56 | 10.27 | +2.1% | 1 |
| memory/first_call | 10.13 | 10.41 | 11.02 | 10.47 | +3.3% | 1 |
| file/arithmetic | 505.58 | 504.56 | 507.66 | 452.25 | -10.5% | 5 |
| file/filter | 483.34 | 476.25 | 506.56 | 436.14 | -9.8% | 5 |
| file/finite | 548.29 | 538.97 | 565.89 | 511.97 | -6.6% | 4 |
| file/small | 3.07 | 3.06 | 3.22 | 2.90 | -5.3% | 5 |
| file/offset | 9.51 | 10.04 | 10.12 | 8.33 | -12.4% | 5 |
| file/point_hit | 1.35 | 1.33 | 1.33 | 1.32 | -1.9% | 3 |
| file/point_miss | 0.95 | 0.95 | 0.93 | 0.97 | +2.5% | 0 |
| file/scalar | 0.58 | 0.59 | 0.62 | 0.61 | +4.1% | 1 |
| file/unsupported | 432.31 | 432.49 | 432.90 | 442.54 | +2.4% | 3 |
| file/columnar | 64.00 | 71.13 | 73.91 | 67.42 | +5.4% | 2 |
| file/case | 502.75 | 494.91 | 482.93 | 470.62 | -6.4% | 5 |
| file/adhoc | 10.21 | 10.07 | 10.45 | 10.37 | +1.6% | 2 |
| file/first_call | 10.29 | 10.02 | 10.28 | 10.32 | +0.2% | 3 |

For the targets, clean → kernel min–max ranges (µs) were:

- memory/arithmetic: 557.6–601.3 → 439.7–529.1.
- memory/filter: 529.7–557.9 → 416.0–486.3.
- file/arithmetic: 469.4–568.9 → 445.2–459.9.
- file/filter: 461.1–524.3 → 395.8–450.9.

The large control ranges matter: final memory columnar was 50.1–78.0 µs clean
versus 59.3–83.0 µs kernels; file columnar was 60.9–68.5 versus 50.0–74.4 µs.
See [`final/summary.json`](final/summary.json) for every range, including the
ordinary and disabled controls.

## Short control recheck

These are additional samples, not replacements. Faster-pair counts are out of
three. Every reported checksum/cost still agrees across variants.

| Host/query | Clean µs | Disabled µs | Ordinary µs | Kernels µs | vs clean | Faster pairs |
|---|---:|---:|---:|---:|---:|---:|
| memory/point_miss | 1.02 | 0.99 | 1.01 | 0.99 | -3.4% | 3 |
| memory/scalar | 0.60 | 0.61 | 0.63 | 0.63 | +4.2% | 1 |
| memory/columnar | 66.91 | 73.26 | 71.14 | 62.88 | -6.0% | 2 |
| memory/adhoc | 9.86 | 9.77 | 10.25 | 10.32 | +4.7% | 1 |
| memory/first_call | 10.21 | 10.14 | 10.12 | 10.44 | +2.3% | 0 |
| file/point_miss | 0.92 | 0.90 | 0.94 | 0.97 | +5.8% | 0 |
| file/scalar | 0.58 | 0.57 | 0.62 | 0.60 | +3.8% | 0 |
| file/columnar | 57.61 | 56.35 | 66.11 | 67.34 | +16.9% | 0 |
| file/adhoc | 9.70 | 9.42 | 10.08 | 10.31 | +6.2% | 0 |
| file/first_call | 9.96 | 9.90 | 10.54 | 10.72 | +7.7% | 1 |

Memory columnar reverses from +15.4% to −6.0%; file columnar stays slower and
reaches +16.9%. This host cannot establish a precise columnar effect from these
short samples. The combination of reproducible slower controls, direction
changes, and ad-hoc allocation overhead fails the proposed adoption gate.

## Allocation, ownership, and bounds

Steady-state arithmetic remains **8,210 allocations/query, about 526 KB/query**;
filtering remains **6,468 allocations/query, about 470 KB/query**. G1 removes
some dispatch work, not the source/output row allocations. The short allocation
profile attributes roughly 63% of sampled allocation bytes to packed-leaf row
reconstruction and another 24% directly to the streaming cursor. That profile
includes benchmark setup and is diagnostic, not a precise allocation census.
The CPU sample is only 180 ms; it suggests row decode/allocation is still
important and does not justify another large kernel expansion.

Ad-hoc scans add **two allocations and about 433 bytes/query** (116 → 118
allocations); first-call scans likewise add two (119 → 121). That includes the
200-byte root descriptor and 216-byte artifact with allocator rounding. This
is a real preparation cost even when elapsed-time differences are noisy.

The fixed grammar has seven source nodes/depth three at most, seven inline
descriptors, at most four inline primitive constants, no pool/register file,
no allocated scratch and no row-proportional added buffer. At most eight roots
are retained per admitted plan (seven projections and one filter). Payload is
at most 1,816 bytes plus the plan's pointer and allocator overhead. Existing
resolved trees and cache entries remain separately owned; the change adds no
source-tree copies or process cache. The structural test cross-checks these
bounds against the shared TOML limits and asserts zero successful per-row
kernel allocations.

Escape diagnostics show input expression trees, parameter-type slices and the
binding helper's closure do not escape through preparation. The immutable root
and plan descriptors escape as expected. The small selectors inline; the typed
load/arithmetic bodies do not all inline. The diagnostic GC test confirms that
a replaced artifact remains reachable while a cursor pins it and becomes
unreachable after its last owner is released, while the statement still owns its
new artifact. Shared prepared-statement race tests use separate sessions and
per-call bindings. No sync.Pool or global artifact retention is introduced.

## Correctness evidence and limits

- Full Go shared corpus, **ordinary / forced / selective**, each:
  **678 memory passes, 0 failures, 2 skips**; **597 disk passes, 0 failures,
  83 skips**. These are the harness's existing capability/storage-mode skips;
  the new expression fixture runs in both modes.
- New shared SQL fixture regenerates byte-identically from this checkout's
  PostgreSQL oracle. It covers nullable arithmetic/comparisons, strict RHS
  errors, width overflow, division/remainder, boolean/lazy negative controls,
  integer-result noninteger inputs, and LIMIT/OFFSET. The existing deliberate
  OFFSET skipped-projection behavior is checked privately, not asserted as PG
  behavior.
- Two Go NoREC seeds: **148 scenario files, 1,620 queries**, all pass. The added
  relation compares admitted roots to complete CASE-wrapped fallbacks.
- Default Go unit suite, experimental unit suite, and focused **forced -race**
  cache/custom-plan/cursor/cancellation/memory/depth tests pass.
- Forced resource corpus with a finite query-memory account and 256-byte
  work_mem passes in both hosts. Actual file-page faults and cancellation at
  the second read preserve read-offset traces, output prefixes and accrued cost;
  LIMIT 0 reads no pages.
- Diagnostic named-target invocation counts: arithmetic **4**, filter **4**,
  NULL test **4**, OFFSET projection **1**; LIMIT 0 and CASE fallback **0**.
  Coverage is not a fallback-only corpus pass.
- Exhaustive small-expression statement/lifetime prefixes compare full errors,
  values, costs, lifetime totals and first-ceiling state, including a nonzero
  starting lifetime total and tied limits. Cache, DDL, binding isolation,
  concurrent sessions, interleaved cursors and bounded rejection reuse pass.
- `mise run verify`: all spec/codegen checks pass. Logs are in [`checks/`](checks/).

This is **not a full repository CI claim**. Rust/TS builds and their corpus runs
were not performed in this checkout; their implementations were left unchanged.
The corpus additions are language-neutral and PG-checked. No new production
behavior is enabled, and full repository CI remains a prerequisite for adoption.

Remaining research gaps include isolated preparation time, broader environment /
attachment / transactional artifact-invalidation introspection beyond existing
cache tests, larger-scale retained-heap/churn measurements, and explaining the
control regressions on a less contended host. G2 forms, integer casts and G3
fusion remain negative controls/deferred work. Those gaps and the failed
regression gate are recorded rather than hidden by more repetitions.
