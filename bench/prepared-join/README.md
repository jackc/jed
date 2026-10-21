# Prepared JOIN investigation (2026-10-20)

The Go and TypeScript anomaly was unnecessary custom planning, not extra rows scanned.
The fix gates a contradiction comparison on **both sources being plan-time literals before
reading either value**. It does not change the eight-entry memo, the selected plan, or execution.

Baseline: fetched master `7d179be9674fb3ddcc1e64519b978488c65df471`.
That master does not contain the frozen Ruby experiment. `plan.json` and the three supported-core
adapters were recovered from local commit `3a8d4e6d`; the adapters here build against this checkout's
cores. The historical numbers in the request are not current-master measurements. No Ruby core,
FFI lane, or experimental core is built or changed here.

## Cause and evidence

`estimatorBoundsContradict` in Go/TS eagerly read both operands. An INL candidate can carry a
runtime sibling column together with `$1` as equality sources. The runtime column has no
plan-time value, so the comparison cannot prove anything; reading `$1` nevertheless marks the
whole probe sensitive. Rust's sequential `?` returns on the unavailable first source in these
shapes. The Go/TS fix checks both shapes, independent of source order. It retains parameter reads
when both sources can be literals, all NULL checks, statistics reads, and cache invalidation.

For the original post-warmup five cycles, both Go and TS reported:

| Target | Executions | Before memo hits / misses | After plan hits / replans |
|---|---:|---:|---:|
| indexed_join | 250 | 0 / 250 | 250 / 0 |
| join_text_predicate | 100 | 0 / 100 | 100 / 0 |
| join_text_projection | 100 | 0 / 100 | 100 / 0 |
| join_integer_outputs | 100 | 0 / 100 | 100 / 0 |

All entries were admitted. Before, those four alone were sensitive and filled the memo to eight;
after, they retain one value-blind plan and an empty custom memo. Other original controls were
already value-blind. TS directly wraps `planQuery` outside timing; Go uses private cache inspection
and count-mode coverage, avoiding permanent production instrumentation.

Separate target-only CPU profiles (startup/setup included, not used as latency samples):

| Cumulative category | Go sampled CPU seconds | TS sampled seconds |
|---|---:|---:|
| Total | 36.27 | 29.82 |
| planQuery | 22.80 | 23.45 |
| resolve (included in planning) | 1.73 | 1.14 |
| Background GC / V8 GC | 6.98 | 2.17 |
| Go Rows.Next / TS buildScanRowsSteps | 1.27 | 0.17 |

These categories overlap and Go background GC runs concurrently. TS buildScanRows is cursor setup,
not its complete drain. Go's materializeRelRows accounted for 0.90 seconds: execution was a small
fraction of the anomaly, so no routing/materialization rewrite was justified. EXPLAIN ANALYZE
confirmed point-bound scans and unchanged actual root rows/cost; there was no hidden full scan.

## Measurements

Intel Core Ultra 9 285K, Linux, Go 1.27.1, Node 26.10.0, release Rust. Builds, diagnostics,
allocation sampling and CPU profiling run outside latency samples. Core workers are serialized.
Every query is prepared once and fully drained; Go and Rust explicitly check drain errors.

The original schedule is retained unchanged in `plan.json` (100 JOIN warmups, cycles of 50 or 20
bindings). The main extended comparison uses 20 warmup cycles, five samples of 20 schedule cycles,
and three fresh processes per core/version. A follow-up alternates before/after fresh processes
three times with 200 cycles per sample for targets and cheap JOIN controls (4,000–10,000 executions
per sample); those longer results replace their shorter estimates below. Each cell is the median
of the three process medians. No slow samples were discarded. Scalar/range/residual/five-table rows
use the main extended run. Allocations are a separate post-timing Go MemStats pass.

| Workload | Rust before → after µs | Go before → after µs | TS before → after µs | Go bytes/op before → after |
|---|---:|---:|---:|---:|
| scalar | 1.14 → 1.15 | 0.69 → 0.71 | 3.60 → 3.87 | 1384 → 1384 |
| pk_hit | 1.59 → 1.60 | 1.58 → 1.53 | 6.67 → 6.76 | 2136 → 2136 |
| indexed_join | 2.46 → 2.49 | 51.50 → 2.89 | 78.05 → 10.81 | 93918 → 3170 |
| three_table_join | 3.06 → 3.11 | 3.50 → 3.57 | 13.32 → 13.57 | 4096 → 4096 |
| three_table_join_miss | 1.51 → 1.54 | 1.18 → 1.13 | 5.56 → 5.39 | 1512 → 1512 |
| three_table_join_range | 60.21 → 59.39 | 73.54 → 76.60 | 253.22 → 245.89 | 117766 → 117764 |
| three_table_computed_key | 5.77 → 5.95 | 7.39 → 7.21 | 19.39 → 18.74 | 12232 → 12232 |
| four_table_join | 3.85 → 3.99 | 4.65 → 4.56 | 16.19 → 15.03 | 5536 → 5536 |
| three_table_left_fence | 3.05 → 3.12 | 3.80 → 3.57 | 13.18 → 13.58 | 4094 → 4094 |
| join_residual | 772.76 → 772.56 | 361.51 → 353.98 | 1792.69 → 1792.68 | 969927 → 969925 |
| join_text_predicate | 2.43 → 2.52 | 68.46 → 2.82 | 98.04 → 11.24 | 113546 → 2976 |
| join_text_projection | 2.37 → 2.44 | 79.13 → 2.77 | 114.37 → 9.79 | 142511 → 2992 |
| join_integer_outputs | 2.53 → 2.62 | 105.51 → 2.70 | 152.39 → 11.19 | 182723 → 3336 |
| five_table_bounded | 17.90 → 18.03 | 22.66 → 22.24 | 68.82 → 68.18 | 36188 → 36188 |

TS allocation estimates below use V8 sampling (256-byte sampling interval, including objects
collected by minor/major GC), after 1,000 additional warm executions. These are allocation volume,
not retained heap, and include the diagnostic caller. They are approximate; do not compare their
absolute values with Go's MemStats as equivalent runtime overhead. Rust allocations were not measured.
| Workload | TS sampled bytes/op before → after |
|---|---:|
| scalar | 16135 → 16137 |
| pk_hit | 23086 → 23144 |
| indexed_join | 401094 → 37640 |
| three_table_join | 48108 → 48168 |
| three_table_join_miss | 23188 → 23615 |
| three_table_join_range | 921850 → 922595 |
| three_table_computed_key | 70415 → 70442 |
| four_table_join | 57603 → 58151 |
| three_table_left_fence | 47487 → 47620 |
| join_residual | 5095197 → 5095869 |
| join_text_predicate | 489148 → 38267 |
| join_text_projection | 615258 → 37843 |
| join_integer_outputs | 784478 → 39366 |
| five_table_bounded | 250867 → 251751 |

The Go target allocation counts fell from approximately 622/857/930/1,103 to 47/45/47/43 per
execution, respectively. In the focused allocation passes, GC cycles fell from 407 to 12 for
10,000 indexed JOIN executions and from 205/248/336 to 5/5/5 for 4,000 executions of the other
targets. These counts are runtime/heap dependent, not engine cost units.

## Controls, correctness, and limits

- All original checksums and row counts agree. Exact out-of-timing diagnostics comprise 168
  records per core: 14 SQLs × six bindings (0, 17, 51, 999, absent 1000, NULL) × result/verbose
  EXPLAIN ANALYZE. Values, types, complete plans/estimates, inclusive node costs, and actual root
  rows/cost agree across all six before/after core runs. Rust is a comparator, not an oracle.
- Untimed TS storage wrappers count 500 scanned source rows over 250 indexed JOIN executions,
  and 200 over 100 executions for each other target, both before and after. These count records
  returned by base scans before pushed filtering, not output rows. There is no scan amplification.
- Go count-mode coverage over setup, warmups, and the 14-workload diagnostic records `planQuery`
  965 → 15 calls and `resolve` 14,874 → 274. Only one initial probe per value-blind statement
  remains. TS directly counts zero post-warmup `planQuery` calls for every original workload.
- An additional 50 never-before-bound values per target produce 50 replans before and zero after
  in the TS counter control; both cores' reuse tests compare 24 distinct, previously unseen
  bindings with independently fresh planning, then check the NULL fallback. The unseen-binding
  control is a counter/correctness check, not a separate latency sample.
- Repeated-binding controls use 1, 8, 9, 20, and 50 distinct values. For indexed JOIN, Go's
  un-analyzed eight-binding control is 2.45 → 2.54 µs; nine bindings are 51.25 → 2.68 µs. TS:
  15.33 → 14.41 µs and 100.02 → 13.18 µs. The threshold supports memo churn as the cause.
- The same controls run after ANALYZE and after skewing 90% of buckets to 1, creating a bucket
  index, then ANALYZE. Indexed JOIN with nine bindings on skewed data is Go 81.27 → 2.74 µs,
  TS 108.25 → 12.85 µs. Reverse-key joins improve (skewed: Go 65.13 → 2.45 µs,
  TS 75.97 → 7.75 µs). Point misses improve too. All four target families retain the improvement.
- **Slower observations remain:** initial short Go computed-key results were 5.96 → 7.02 µs;
  the longer alternating runs were 7.39 → 7.21 µs. The additional skewed `key_range` control
  remains slower in TS's longer follow-up: median 557.88 → 629.44 µs (+12.8%). Its three process
  medians span 548.16–565.45 before and 506.63–638.46 after. The skew-equality follow-up is
  469.30 → 447.68 µs. This range control warrants separate JIT/execution investigation; these
  measurements do not establish a uniform control-suite improvement. All observations remain
  in the raw output; no favorable-run selection was used.
- The remaining TS target execution is roughly four times Rust, and multiway controls still show
  a gap. No secondary execution optimization was attempted. Measurements use the direct in-memory
  core; they do not claim worker-RPC, disk-I/O, browser, or whole-engine memory improvements.

The regression checks pin cache reuse, NULL fallback, analyzed PK JOINs, runtime sources in
both comparison orders, and continued sensitivity/contradiction detection for two bound literals.
Existing suites cover DDL/DML/ANALYZE invalidation, session/temp/attachment isolation, errors,
resource accounting, cancellation, and retained async continuations. No public API, SQL behavior,
spec rule, file format, dependency, or website surface changes.

Verification: full Go unit suite; TS unit/integration suite (1,156 tests); all 25 cross-core
conformance walks (including async codec, page-service, spill and query-memory variants, with
identical query-memory peaks); Go cache/custom-plan regressions under the race detector;
TypeScript Node/browser/worker type checks; repository formatting.
No corpus expectations changed, so no new PostgreSQL oracle probe was required.

## Reproduce

All commands run from the checkout root; output defaults to `/tmp/jed-prepared-join/current`.
Run one benchmark at a time, after other tests/build work has completed. Build time is excluded.

```sh
# Original schedule, then exact values/types/plans/cost diagnostics.
mise run bench:prepared_join
JOIN_OUTPUT=/tmp/jed-prepared-join/exact mise run bench:prepared_join --diagnostics
# Longer samples without enlarging the 20/50-binding schedule.
JOIN_CYCLES=200 JOIN_WARM_CYCLES=20 mise run bench:prepared_join
# Untimed CPU profiles; never use these samples as latency evidence.
JOIN_OUTPUT=/tmp/jed-prepared-join/profile mise run bench:prepared_join --profile
# Generate extra controls (select with JOIN_PLAN), then run untimed diagnostics and tests.
mise run bench:prepared_join_controls
mise run bench:prepared_join_counters
mise run bench:prepared_join_diagnostics
JOIN_NO_ALLOC=1 mise run bench:prepared_join_diagnostics
mise run bench:prepared_join_checks
```

`mise run bench:prepared_join_controls` generates the repeated/overflow/miss/reversed/range/analyzed/skewed plans into
an external directory; set `JOIN_PLAN` to any generated JSON. `JOIN_GO_LANE` and `JOIN_TS_LANE`
can point to baseline binaries/source adapters built from the baseline checkout. Rust is unchanged.
`JOIN_DIAGNOSTIC_SOURCE` selects a baseline copy of the TS diagnostic driver. For a clean fresh
reproduction, copy only this tooling into a separate checkout at the baseline revision and build
there; never silently benchmark a different core revision.

Raw evidence for this run is outside git under `/tmp/jed-prepared-join`: `baseline`,
`before/after-long-{1,2,3}`, `before/after-focused-{1,2,3}`, `before/after-{controls,analyzed,skewed}`,
`before/after-skew-followup-{1,2,3}`, exact transcripts, coverage, profiles, and verification logs.
The before binary and an isolated baseline TS source copy were retained there for alternating runs.
The table and findings above are the retained compact record; `/tmp` is not a durable archive.
