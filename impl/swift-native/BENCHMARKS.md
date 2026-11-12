# Native Swift benchmark results

The structural follow-up improves several important paths substantially, but
Swift still trails Rust and Go. On the **22 selected canonical workloads**, its
geometric-mean elapsed-time ratios are **5.4065× Rust, 5.1083× Go and 1.0423×
TypeScript**. Swift has lower medians on **0/22** workloads against Rust,
**0/22** against Go, and **13/22** against TypeScript. Each workload receives
equal weight. These are diagnostic results, not a new result for the full
143-workload suite.

Current production source: local-only
`fb34b0e8cb3d82287fbd3401e3426f04b3b86c7a`. All three final comparisons pass their
complete membership, result-signature and raw-sample checks. The [verification
report](VERIFICATION.md#structural-follow-up-acceptance) separately records 543
native tests, all 749 frozen SQL files and the refreshed resource/storage/process
gates. Swift remains a frozen, opt-in experiment; Apple platforms are unverified.

## Second follow-up (2026-11-09)

A later pass removed per-row and per-statement copy costs found by profiling;
see [the design record](../../spec/design/swift-native-performance.md#second-follow-up-copy-and-dispatch-costs-2026-11-09).
One diagnostic round of the same 22 workloads gives Swift/Rust geometric means
of 2.63× on the twelve scan and aggregate workloads (4.96× before), 7.81× on
the eight per-statement workloads (9.00× before) and 3.85× overall (5.75×
before). [Per-workload results](BENCHMARK_FOLLOWUP_20261109.csv). It ran on a
tmpfs data directory with one round, so it supplements the tables below and
does not replace them.

## Fifth follow-up (2026-11-10)

A cached direct key read and cached write access inventories; see
[the design record](../../spec/design/swift-native-performance.md#fifth-follow-up-direct-key-reads-and-cached-access-inventories-2026-11-10).
On the same 22 workloads Swift/Rust geometric means are 0.74× on the scan and
aggregate workloads (0.80× for the fourth follow-up's source in the same run),
2.58× on the eight per-statement workloads (3.76×) and 1.28× overall (1.53×).
A point lookup is 1.19× Rust (was 4.2×).
[Per-workload results](BENCHMARK_FOLLOWUP_20261110C.csv), same method.

## Fourth follow-up (2026-11-10)

A further pass on scans, grouping and lookup setup; see
[the design record](../../spec/design/swift-native-performance.md#fourth-follow-up-scans-grouping-and-lookup-setup-2026-11-10).
On the same 22 workloads Swift/Rust geometric means are 0.79× on the twelve
scan and aggregate workloads (2.57× for the third follow-up's source in the
same run), 3.69× on the eight per-statement workloads (4.01×) and 1.52×
overall (2.97×). [Per-workload results](BENCHMARK_FOLLOWUP_20261110B.csv),
best of two alternating rounds per Swift build on a tmpfs data directory.

## Third follow-up (2026-11-10)

A further pass removed per-statement and bulk-write costs; see
[the design record](../../spec/design/swift-native-performance.md#third-follow-up-per-statement-and-bulk-write-costs-2026-11-10).
On the same 22 workloads Swift/Rust geometric means are 3.86× on the eight
per-statement workloads (7.44× for the second follow-up's source in the same
run), 2.56× on the twelve scan and aggregate workloads (2.62×) and 2.92×
overall (3.78×). [Per-workload results](BENCHMARK_FOLLOWUP_20261110.csv), best
of two alternating rounds per Swift build on a tmpfs data directory. Like the
second follow-up it supplements the tables below.

## Current comparison with Rust, Go and TypeScript

Each cell is the median of three process means for one authored operation.
Lower is faster. Rollback operations time their authored batch, not one row.
All 22 selected workloads appear below; the [complete CSV](BENCHMARK_STRUCTURAL_PEERS.csv)
also retains all three round means, ranges and exact peer ratios.

| Workload | Unit | Swift | Rust | Go | TypeScript |
|---|---|---:|---:|---:|---:|
| `point_lookup_pk` | µs | 23.655 | 2.378 | 2.123 | 15.842 |
| `point_lookup_pk_miss` | µs | 18.787 | 1.895 | 1.256 | 8.821 |
| `full_scan_agg` | ms | 80.439 | 20.531 | 13.284 | 130.581 |
| `full_scan_minmax` | ms | 104.030 | 33.728 | 22.518 | 185.077 |
| `full_scan_agg_filter` | ms | 204.400 | 55.629 | 31.835 | 263.106 |
| `group_by_agg` | ms | 2888.364 | 634.601 | 989.636 | 2003.651 |
| `group_by_agg_high_card` | ms | 4072.654 | 850.935 | 1986.483 | 8674.512 |
| `varscan_text_eq` | ms | 13.943 | 2.315 | 2.392 | 16.325 |
| `varscan_text_isnull` | ms | 11.803 | 1.560 | 1.659 | 13.320 |
| `varscan_bytea_eq` | ms | 15.147 | 2.249 | 2.171 | 17.548 |
| `varscan_array_count` | ms | 12.027 | 1.694 | 1.560 | 8.600 |
| `varscan_array_contains` | ms | 18.986 | 2.889 | 3.144 | 19.144 |
| `varscan_text_max` | ms | 17.915 | 5.978 | 6.272 | 64.236 |
| `varscan_project_needle` | ms | 37.984 | 8.045 | 7.201 | 35.365 |
| `insert_rollback` | ms | 49.075 | 2.900 | 4.672 | 17.108 |
| `delete_rollback_serial` | ms | 63.622 | 2.673 | 3.978 | 18.674 |
| `delete_rollback_random` | ms | 75.525 | 14.374 | 15.715 | 73.179 |
| `delete_range_rollback` | ms | 139.029 | 7.818 | 12.779 | 39.431 |
| `update_untouched_large_rollback` | µs | 94.256 | 19.007 | 21.075 | 121.791 |
| `insert_commit_durable` | ms | 2.447 | 2.088 | 2.363 | 2.740 |
| `update_commit_durable` | ms | 2.705 | 2.115 | 2.673 | 3.070 |
| `cold_open_pk_small` | ms | 1.132 | 0.460 | 0.443 | 1.548 |

The results depend strongly on the path. Fixed aggregate scans beat TypeScript;
point lookups and several rollback writes remain slower. Durable insert/update
are close to Go, with ranking changes between rounds. Array containment's Swift
median is only 0.82% below TypeScript's and one round is slower. These are
descriptive median wins, not significance claims. Go rollback insertion ranges
from 3.694 to 6.944 ms across rounds, so its median is especially variable.

The original complete 143-workload comparison below measured **9.1990× Rust,
7.5125× Go and 2.1292× TypeScript** for the earlier `3db8fc2` actor. Comparing
those numbers directly with this selected 22-workload average would mix
membership and measurement conditions. The paired measurements below establish
the before/after gains instead.

## Controlled comparison with the original Swift actor

The retained original actor and final actor ran the same eight authored
workloads, alternating order over three rounds on CPU 1. Both use the same data,
warmup, iteration counts and timing adapter. Speedup is original/final; above
one means faster. [Full measurements and ranges](BENCHMARK_STRUCTURAL_SWIFT.csv).

| Workload | Unit | Original Swift | Final Swift | Speedup |
|---|---|---:|---:|---:|
| `point_lookup_pk` | µs | 45.183 | 23.436 | 1.93× |
| `point_lookup_pk_miss` | µs | 36.619 | 18.813 | 1.95× |
| `full_scan_agg` | ms | 217.569 | 79.491 | 2.74× |
| `varscan_array_count` | ms | 138.134 | 11.963 | 11.55× |
| `varscan_array_contains` | ms | 238.171 | 18.938 | 12.58× |
| `varscan_text_isnull` | ms | 83.048 | 11.813 | 7.03× |
| `varscan_text_max` | ms | 124.752 | 17.587 | 7.09× |
| `insert_commit_durable` | ms | 2.562 | 2.169 | 1.18× |

All eight final medians are lower. Primary-key lookup improves 1.93×, the fixed
aggregate 2.74×, array COUNT 11.55× and array containment 12.58×.
Durable insertion's 1.18× median gain is less conclusive:
the original 2.389–2.650 ms and final 1.998–2.400 ms round ranges overlap.

These measure the combined final changes. They do not assign the total gain to
individual edits. Earlier controlled stages are preserved in
`results/architecture-20261106/value-layout-comparison` and
`packed-foundation-comparison`: selective scalar indirection alone improved the
fixed aggregate 1.44×; the next packed-frame stage improved several scans but
regressed that aggregate relative to the value-only stage. The final resolved
aggregate kernels and compact operand vector address that intermediate cost.
No intermediate regression or slower final measurement is discarded.

## Aligned comparison with experimental Ruby

Ruby 4.0.7 runs with YJIT enabled and ZJIT disabled, using the captured native
Ruby core, automatic locking, synchronous commits, the same retained v50
database images and unchanged authored warmup/iterations. The tooling includes
parameter generation in Ruby's query/write timers to match Swift; the exact
tooling patch is archived and the Ruby engine is unchanged. These three
workloads do not establish a whole-core Ruby ranking.

| Workload | Unit | Swift | Ruby/YJIT | Ruby time / Swift time |
|---|---|---:|---:|---:|
| `point_lookup_pk` | µs | 23.620 | 16.757 | 0.71× |
| `full_scan_agg` | ms | 81.617 | 1943.232 | 23.81× |
| `insert_commit_durable` | ms | 2.315 | 6.269 | 2.71× |

Ruby wins primary-key lookup: 16.757 µs versus Swift's 23.620 µs (Swift takes
1.41× as long). Swift wins the full aggregate by 23.81× and durable insertion
by 2.71×. The latter Ruby round means range from 4.949 to 6.399 ms; the Swift
range is 2.276–2.416 ms. A blanket claim that this Swift core is slower than
the experimental Ruby core is not supported by these matched measurements.

The [Ruby/Swift CSV](BENCHMARK_STRUCTURAL_RUBY.csv) retains every round and range.
Historical Ruby reports use different data, storage/locking settings and timing
statistics and are not substituted for this comparison. Ruby's approved
runtime-compiled query kernels also perform different internal work from an
interpreted executor; these measurements compare implementations, not an
inherent language speed ordering.

The runner loads Ruby's compilation modules normally and preserves its default
256 MiB `work_mem` and unlimited cost/query-memory settings. The frozen Ruby
[aggregate compiler](../ruby-native/lib/jed_native/compiled_plans.rb) declines
file-backed execution. This query nevertheless qualifies for its existing
[file streaming aggregate fold](../ruby-native/lib/jed_native/spill_select.rb),
which evaluates operands and folds them per row. These are source-level path
differences; they do not constitute a measured breakdown of Ruby's elapsed time.

## What was fundamentally wrong, and what changed

The original actor already used Swift 6.4.2 release optimization (`-O`) and
whole-module optimization, with matching runtime libraries and no sanitizer,
testability or library-evolution flags. The environment audit found no current
cgroup CPU/memory cap or throttling. Historical clock/thermal conditions were
not captured. The evidence points to extra engine work, rather than a debug
build explaining the original gap.

- **Value representation:** enum-wide indirection boxed scalar values and types.
  Primitive cases now stay inline; recursive/large cases remain indirect.
- **Scan execution and ownership:** cursors avoid copying the retained tree per
  row, page counting uses bounded binary searches, and a reusable packed row
  decodes only touched cells. Admitted nullness/equality/array predicates inspect
  validated encoded spans and gather decoded survivor values as needed.
- **Aggregate execution:** the plan resolves aggregate dispatch once, and
  unfiltered packed folds use compact operand vectors instead of repeated
  name/set dispatch and temporary one-value arrays.
- **Writer representation:** immutable encoded dirty records replace repeated
  PAX packing, padding, checksumming and reparsing on every edit. Persistence
  still emits the canonical page bytes.
- **Prepared execution:** read bundles own their snapshot map and ordered pins;
  exact cached plans retain sequence/subquery requirements, and generic cache
  hits reuse the already admitted parameter vector. Scalar execution input is
  constructed only when its selected path needs it.

These are general representation, ownership and execution changes; they do not
match benchmark SQL. The specified cost, memory admission, error, snapshot,
file-format and durability contracts are preserved. Packed lanes honor lazy
decoding for unreached or nullness-only values, rather than eagerly decoding
unused bodies. No dependency,
unsafe-memory shortcut or fixture weakening was introduced. The [design and
remaining structural differences](../../spec/design/swift-native-performance.md)
record the reasoning and boundaries. Remaining candidates include leaf-scoped
ownership, direct null bitmap access, typed grouping keys/accumulators and
separating immutable plan metadata from execution bindings. Their contribution
to the final gap has not been measured.

## Follow-up controls, provenance and limits

All final measurements use the same Linux host described in the historical
report below, CPU 1, sequential actor processes, retained canonical v50 datasets
and fresh private ext4 copies. No build, correctness run or profiler overlaps
timing. Small source/document reads continue; this is not an isolated laboratory
host. The governor is unchanged (`powersave`); pinning does not fix frequency or
temperature. Three-round ranges are not confidence intervals.

One existing adapter asymmetry remains explicit: Swift and Go time parameter
generation for query/durable operations; retained Rust and TypeScript actors
exclude it. Draining and checksum work are timed. This matters for cheap
parameterized operations but cannot explain parameter-free scan gaps. The
supplemental Ruby timer is explicitly aligned with Swift's.

The 22-workload matrix omits 121 workloads; the paired eight omit 135; the Ruby
three omit 140. All selected authored fields are unchanged. The matrix retains
the exact original Rust/Go actors and Node/TypeScript tree; their hashes are
recorded in the historical provenance below. Final Swift SHA-256:
`5bc33bffebd2064a393e0f8a8acecd5c3792d4ef8affeeccb64fb2f6575d4c6d`.
The same final Swift executable is used in all three comparisons; the paired
original is `e33fd8a85db9193a84e9d6a0ae7a2531bd69bb2297dabda48dbc019133325d1f`.

Evidence under `results/architecture-20261106/`:

- `final-peer-matrix-retry`: 22 workloads × four cores × three rounds;
  immutable actors, input/corpus hashes, source capture, logs and raw results.
- `final-swift-paired`: eight workloads × two Swift actors × three rounds;
  alternating order, raw samples and exact original-result validation.
- `final-ruby-supplement-retry`: three workloads × Swift/Ruby × three rounds;
  captured Ruby source/runtime identities, activation facts and timer patch.
- `final-report`: independently revalidated inputs, the three CSVs copied
  exactly into this repository, `summary.json`, `summarize-task.rb` and
  `evidence-sha256.json` (SHA-256 `9fd2aa16e2281fbd3d040c9cf0e62eab26d3d38c3abc3c387d1cba156ecd0e9e`).

The strict summary verifies complete membership, checksums/row counts, exact
authored inputs, raw Swift/Ruby sample statistics and actual archived actor
bytes. These ignored local artifacts supplement the tracked report and CSVs.
The initial `final-peer-matrix` attempt failed on an unqualified Ruby constant
before any actor started. Its report/log remains preserved; the corrected
tooling uses a new output directory and unchanged engine binaries.
The initial `final-ruby-supplement` also remains as a failed attempt: its Swift
process passed, then the Ruby worker failed to parse before executing any Ruby
query. The corrected runner passes syntax checks; the retry changes no engine
source. Only complete successful runs enter the comparison tables.

Commands used (fresh, nonexistent output directories are required):

```sh
taskset -c 1 mise -C /tmp/jed-swift-architecture run matrix /home/jack/dev/jed/impl/swift-native/results/architecture-20261106/final-peer-matrix-retry
mise -C /tmp/jed-swift-architecture run compare /home/jack/dev/jed/impl/swift-native/results/architecture-20261106/final-swift-paired /home/jack/dev/jed/impl/swift-native/results/architecture-20261106/final-peer-matrix-retry/actors/jed-bench
mise -C /tmp/jed-swift-ruby-supplement-20261106 run compare -- /home/jack/dev/jed/impl/swift-native/results/architecture-20261106/final-ruby-supplement-retry /home/jack/dev/jed/impl/swift-native/results/architecture-20261106/final-peer-matrix-retry/actors/jed-bench
mise -C /tmp/jed-swift-architecture run summarize -- /home/jack/dev/jed/impl/swift-native/results/architecture-20261106/final-peer-matrix-retry /home/jack/dev/jed/impl/swift-native/results/architecture-20261106/final-swift-paired /home/jack/dev/jed/impl/swift-native/results/architecture-20261106/final-ruby-supplement-retry /home/jack/dev/jed/impl/swift-native/results/architecture-20261106/final-report
```

The diagnostic task bodies are archived with their run artifacts. The ordinary
full-suite task remains `mise run swift-native:bench`; this follow-up does not
claim a second complete 143-workload run.

## Historical complete 143-workload comparison (`3db8fc2`)

The remainder of this document preserves the original optimization report and
its original actors. It does not describe the later `fb34b0e` implementation.

The complete Linux comparison passed **143 workloads × four cores × three rounds in each run**, with zero result-validation failures. The profile-guided Swift change improves `full_scan_agg` from **1119.919 to 217.729 ms/query (5.1436×)** and `full_scan_minmax` from **1227.483 to 284.925 ms/query (4.3081×)**. Across all 143 workloads, the equal-weight geometric-mean Swift speedup is **1.0325×**; the median workload speedup is **1.0071×**.

The optimized Swift/peer geometric-mean time ratios are **9.1990× Rust, 7.5125× Go, and 2.1292× TypeScript**; greater than one means Swift took longer. This is a measured improvement in a narrow execution path, not performance parity. Three rounds on one host do not establish statistical significance or cross-platform performance.

The measured corpus has **100 faster and 43 slower Swift medians**, with no unchanged medians. Every slower result remains in the report artifacts. This document records the performance evidence; it does not promote Swift to a supported core or select production delivery. [README.md](README.md) and [VERIFICATION.md](VERIFICATION.md) record the experiment's lifecycle and final acceptance status.

* [All 143 workloads](BENCHMARK_WORKLOADS.csv): exact copy of the strict comparison CSV, including all three round means and medians for all four cores, result signatures, and peer ratios.
* [All categories](BENCHMARK_CATEGORIES.csv): exact copy, including geometric means, medians and peer drift.
* [All 43 slower medians](BENCHMARK_REGRESSIONS.csv): every original workload column plus percentage/absolute ranks, median increases in ns and ms, per-core percentage changes, and Swift round-range overlap. Rows sort by descending percentage increase, then descending absolute increase, then name; `regression_rank_absolute` gives the alternate ordering.

Evidence directories below are relative to `impl/swift-native/`. They are ignored local artifacts retained on durable ext4 storage; the linked report and CSV files are the repository deliverables. Other repository paths are explicitly identified. The matching correctness evidence is documented separately in [VERIFICATION.md](VERIFICATION.md) and [RESOURCE_ACCEPTANCE.md](Tools/RESOURCE_ACCEPTANCE.md).

### Complete results and controls

Speedup is before/after (>1 faster). Peer ratios are Swift/peer (>1 slower). Every category uses an equal-weight geometric mean of dimensionless workload ratios, not a total elapsed-time ratio.

| Peer | Swift/peer before, gmean | Swift/peer after, gmean | Peer before/after speedup, gmean |
| --- | --- | --- | --- |
| rust | 9.5012× | 9.1990× | 0.9996× |
| go | 7.7748× | 7.5125× | 0.9976× |
| ts | 2.2143× | 2.1292× | 0.9928× |

All operation categories:

| Category | Workloads | Swift speedup, gmean | Swift speedup, median | Swift/Rust after | Swift/Go after | Swift/TS after |
| --- | --- | --- | --- | --- | --- | --- |
| `cold_open` | 2 | 0.9865× | 0.9865× | 2.4240× | 1.9138× | 0.8815× |
| `concurrent_read` | 4 | 0.9997× | 1.0004× | 14.2096× | 8.7189× | 2.8961× |
| `query` | 114 | 1.0408× | 1.0102× | 9.8704× | 7.9890× | 2.0727× |
| `write_durable` | 6 | 0.9812× | 1.0138× | 1.4085× | 1.2655× | 1.0309× |
| `write_rollback` | 17 | 1.0088× | 1.0001× | 11.7471× | 10.5766× | 3.3995× |

All dataset categories:

| Category | Workloads | Swift speedup, gmean | Swift speedup, median | Swift/Rust after | Swift/Go after | Swift/TS after |
| --- | --- | --- | --- | --- | --- | --- |
| `gin` | 7 | 1.0103× | 1.0024× | 3.0331× | 3.2953× | 0.7108× |
| `large` | 33 | 1.0970× | 1.0043× | 7.4892× | 7.0430× | 1.8907× |
| `scratch` | 89 | 1.0133× | 1.0081× | 11.1030× | 8.6293× | 2.6212× |
| `small` | 12 | 1.0234× | 1.0138× | 8.7863× | 6.0662× | 1.5051× |
| `text` | 2 | 0.9971× | 0.9971× | 4.0526× | 2.9473× | 0.5420× |

Selected aggregate workloads and controls, with median changes for the unchanged peers:

| Workload | Before ms/op | After ms/op | Swift speedup | Swift change | Rust change | Go change | TS change |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `full_scan_agg` | 1119.919 | 217.729 | 5.1436× | -80.56% | +1.12% | -0.17% | +0.85% |
| `full_scan_minmax` | 1227.483 | 284.925 | 4.3081× | -76.79% | -0.41% | -0.41% | +1.92% |
| `full_scan_agg_filter` | 1001.085 | 953.953 | 1.0494× | -4.71% | +1.11% | +1.25% | +1.41% |
| `group_by_agg` | 3949.389 | 3995.001 | 0.9886× | +1.15% | +1.75% | +1.77% | -1.19% |
| `group_by_agg_high_card` | 5315.371 | 5205.152 | 1.0212× | -2.07% | -0.17% | -1.35% | -2.19% |

The two unfiltered fixed-cell aggregate workloads enter the new packed fold. The filtered and grouped controls retain their existing drivers. Their measured movement is reported as observed; it is not attributed solely to the new fold. The slice-aware decoder change is broader than that fold, and host/runtime variation also remains possible. The three before/after means for `full_scan_agg`, in ms, are `[1138.711, 1113.199, 1119.919]` and `[211.433, 218.586, 217.729]`; for `full_scan_minmax`, `[1208.692, 1267.756, 1227.483]` and `[284.925, 291.767, 280.872]`.

The unchanged peer engines' whole-suite geometric-mean before/after ratios are Rust **0.9996×**, Go **0.9976×**, and TS **0.9928×**. Individual peer lanes vary more than these summaries. No peer normalization, outlier removal, discarded round, or retiming is used in this report.

### Regressions and limits of attribution

Largest five Swift median increases by percentage:

| Workload | Before ms/op | After ms/op | Increase ms/op | Swift change | Rust change | Go change | TS change |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `order_by_pk_limit` | 0.155 | 0.250 | 0.095 | +61.39% | -0.02% | +45.46% | +1.11% |
| `update_commit_durable` | 2.408 | 2.805 | 0.397 | +16.49% | +5.23% | +12.11% | +21.56% |
| `gin_decimal_contains` | 3.628 | 4.178 | 0.550 | +15.17% | -0.66% | -0.54% | +3.70% |
| `gin_jsonb_path_match` | 7.302 | 8.151 | 0.849 | +11.63% | -0.60% | +4.95% | +3.75% |
| `insert_commit_durable_text` | 2.056 | 2.260 | 0.204 | +9.91% | -4.08% | +1.43% | +16.36% |

Largest five increases by absolute time per authored operation:

| Workload | Before ms/op | After ms/op | Increase ms/op | Swift change | Rust change | Go change | TS change |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `where_pushdown_join_residual` | 7708.348 | 7835.477 | 127.129 | +1.65% | +0.05% | -1.90% | -1.75% |
| `group_by_agg` | 3949.389 | 3995.001 | 45.611 | +1.15% | +1.75% | +1.77% | -1.19% |
| `on_pushdown_join_residual` | 8625.228 | 8664.964 | 39.736 | +0.46% | +1.67% | +2.29% | +0.49% |
| `similar_to_filter_scan` | 7862.579 | 7895.127 | 32.548 | +0.41% | +0.28% | +0.95% | -2.14% |
| `cte_materialized` | 2722.505 | 2738.574 | 16.069 | +0.59% | -1.03% | +0.72% | +0.59% |

Absolute increases are useful for prioritizing individual workloads, but are not summed across unlike operation units. In particular, rollback rows measure their entire authored batch, and concurrent rows measure throughput-derived ns/query.

Forty of the 43 slower workloads have overlapping before/after ranges of their three round means. The three without overlap are `secondary_pointset_delete`, `update_untouched_large_rollback`, `gin_jsonb_path_match`. Range overlap is descriptive, not a significance test; non-overlap also does not establish a causal engine regression with only three rounds. `order_by_pk_limit` is particularly variable: its before means are `[414833, 144468, 154839]` ns and after means `[249894, 408241, 245167]` ns, while Go's median also rises 45.46%. The durable update's peers also slow, by Rust 5.23%, Go 12.11%, and TS 21.56%. These observations qualify attribution without hiding the measured Swift increases.

### Runs and source identity

| Item | Baseline | Optimized full run |
|---|---|---|
| Evidence directory | `results/benchmark-baseline-20261106` | `results/benchmark-after-20261106` |
| Source checkpoint | `6d160e505da7a92269c0a14e38b90c5351a57119` | Actual capture HEAD `7c4803e9243c7be093dab8f28553d733fbbd1cba` |
| Engine optimization | None | `3db8fc2d2653475abc5e9d43049254ccd1e92387` |
| Difference after engine commit | Not applicable | `7c4803e9` changes only `impl/swift-native/PROGRESS.md` |
| Workload membership | 143 selected, zero omitted | Same 143 selected and completed, zero omitted, diagnostic=false |
| Core-round membership | 12/12, successful | 12/12, successful |
| Logical results | All four cores and all three rounds agree | All four cores and all rounds agree within and across both runs |
| Swift executable SHA-256 | `44a127af9e7891bb245e846fa38edd3cb1a186c3be6c90267365407ff39d3aa7` | `e33fd8a85db9193a84e9d6a0ae7a2531bd69bb2297dabda48dbc019133325d1f` |

These are local source checkpoints, not a statement that changes were pushed. The experiment's frozen shared-input revision is separately `0b76dd6e9c2675b552eb8af9e820065351ffa4b3`, distinct from the measured baseline engine checkpoint.

Both runs were freshly built by the task, not run with `--no-build`. Both source-status records contain only unrelated untracked `tasks/`. Each run retains build commands/logs, observed source files and hashes, revision/status/diff, immutable native actors, and a copied TypeScript/Node runtime tree. A source snapshot alone does not prove which source compiled an arbitrary existing binary, especially with `--no-build`; build logs and immutable actor hashes are separate provenance evidence. Both successful harness reports and the completed strict comparison establish the full membership and logical agreement above.

The source-hash comparison differs only in five optimized Swift production files: `ColumnarExecution.swift`, `GroupedSelectExecution.swift`, `PackedPage.swift`, `TreeCursor.swift`, and `ValueCodec.swift`. Git source comparison also shows no supported-peer engine changes. Rust actor SHA-256 remains `d7825b1b99db4f105dd0193a0b7f7faba8a16de118ec03dc14e91aa713d76db0`; the TS runner-tree identity remains `8b1e8554e0c6ffe1582c50c4711bb4cb7b0a6db80427e4fd86f547a02071aacc`, with Node `ab9c8eecf9f82d6693cdc3accced17034065c8d96213b0aa76a7e803d20ae1da`.

Go executable SHA-256 changes from `005a0ef15038188dff10f7ce7d698003e354f04719e2ed85e1db57338be806fd` to `bdf1f2b9561495a120bdc65f7301ca5420f83a668a4838ac236296853bf00144`. The recorded `go version -m` audit identifies only `vcs.revision` and `vcs.time` changes from the baseline checkpoint to the docs checkpoint; modules, toolchain, flags and `CGO_ENABLED=0` match. Both report `vcs.modified=true` with the unrelated untracked `tasks/`. Thus the Go actors are not byte-identical even though the peer engine source is unchanged.

Raw workload start timestamps span **2026-11-06 13:09:38–17:18:27 UTC** before and **18:00:23–22:06:14 UTC** after. These are the earliest and latest workload starts in retained JSONL, not claimed total-run durations; builds and the final workload extend outside those intervals.

### Host and toolchains

| Field | Recorded value |
|---|---|
| CPU / architecture | Intel Core Ultra 9 285K, x86_64 |
| Topology | 24 online logical CPUs, one socket, 24 cores/socket, one thread/core, one NUMA node |
| Cache | L2 40 MiB; L3 36 MiB, as reported by `lscpu` |
| OS | Linux `7.0.0-38-generic`, Ubuntu kernel, x86_64 |
| Data filesystem | ext4 on `/dev/mapper/ubuntu--vg-ubuntu--lv`, mounted at `/` |
| Swift | 6.4.2 release; target `x86_64-unknown-linux-gnu` |
| Rust | rustc 1.99.0, `b940084d7` dated 2026-09-28 |
| Go | go1.27.1, linux/amd64; build sets `CGO_ENABLED=0` |
| Node / TS execution | Node v26.10.0; native TypeScript stripping, copied runtime/import tree |
| Driver | Ruby 4.0.7, PRISM, x86_64-linux |
| Selected runtime override variables | Baseline metadata records `{}` |

Sources: baseline `metadata.json` and `build-*.log.command.json`. Swift uses a release SwiftPM native build with isolated cache/config/scratch paths; Rust uses `cargo build --release --locked --offline`; Go uses its normal optimized `go build` with offline module lookup. The task captures the Node executable rather than relying on a later executable at the same path.

The data and scratch files stay on the recorded ext4 filesystem, not `/tmp` tmpfs. CPU affinity, governor, thermal state, installed RAM, and physical drive model were not captured in this metadata and are not claimed as controlled. No CPU pinning appears in the recorded commands. Before/after toolchain commands, uname, selected environment, dataset identities and workload inventory match. `lscpu` differs only in its instantaneous `CPU(s) scaling MHz` reading: 53% before and 64% after. That is an observation at capture, not a fixed-clock control or proof of equal runtime frequency. Filesystem used/free-block counts differ, while the device, mount and ext4 type agree. Three rounds on one Linux host do not establish cross-platform performance or statistical significance.

### Commands and complete inventory

Baseline command, completed successfully:

```sh
mise run swift-native:bench --output /home/jack/dev/jed/impl/swift-native/results/benchmark-baseline-20261106
```

Matched full after command, completed successfully:

```sh
mise run swift-native:bench --output /home/jack/dev/jed/impl/swift-native/results/benchmark-after-20261106
```

Both use task defaults: all 143 workloads, `swift,rust,go,ts`, three rounds, fresh release builds, and `bench/data/swift-native`. No workload filter or alternate durability override is part of the full comparison. Core processes run sequentially, with order rotated: Swift/Rust/Go/TS, then Rust/Go/TS/Swift, then Go/TS/Swift/Rust. The process/build deadline is 21,600 seconds; a timeout or incomplete result remains a failure. Profiling, other benchmark jobs, builds and heavy verification were kept idle while timing. Small read-only documentation work continued; this was not an isolated laboratory host.

The pinned corpus contains 114 query, 17 rollback-write, six durable-write, four concurrent-reader, and two cold-open workloads. Dataset assignments are 89 scratch, 33 large, 12 small, seven GIN, and two text. The 143-row CSV preserves every authored workload; no subset substitutes for it. PostgreSQL, SQLite, wrappers, browsers, WASI, and the separate larger-than-work-memory spill corpus are outside this four-core run and receive no performance claim here.

| Pinned input | SHA-256 |
|---|---|
| `benchmarks.toml` | `a7be0f426d9d4432b238eca05b73d6a29b4a114b9c8aaca6c85f71cb94fe3f96` |
| `datasets.toml` | `b9bfb4cb1f27ae1757268a94434e96df67626bc6b39f005c3626ab3a6e289133` |
| `spill.toml` | `53c7d9034296e5c6cb03c61f30323f859f034a0ceb8645a49032f00790f084fb` |

| Initial native dataset | Bytes | SHA-256 |
|---|---:|---|
| `small.jed` | 1,572,864 | `b03f463e6108f9bd4157e85484bfbe543aea8ac48e2c9454dd55a87d63e4d440` |
| `large.jed` | 110,624,768 | `cb6af3e8ace33e49dfe61b29f2fe9a95890121a61e2bfb8f3f51cc5159f1cdc2` |
| `gin.jed` | 17,301,504 | `227e5e5cde77e30a6a4d7b19035c512bfdc1d0f7277f19990051323da95cc9ac` |
| `text.jed` | 46,661,632 | `82dc8efde2a3161ec1a0acb8605be4c874b4e8b83eefe929e920ddf9066c3d08` |

The shared generator produced these files locally; their sidecars contain the `datasets.toml` fingerprint. Each core-round gets fresh verified copies beside the canonical source data. Scratch setup SQL runs outside timing on fresh native files. Temporary run copies are removed after each core process; immutable source images, raw outputs and provenance remain retained.

### What the timers measure

`spec/design/benchmarks.md` §§3–8 and the unchanged shared records define SQL, setup, seed, parameters, warmups, iterations, and durability. Warmup and measured parameters share one continuous SplitMix64 stream. Queries include prepared execution, full result draining and checksum folding. A later adapter audit found that query/durable parameter generation is inside Swift and Go timers but outside Rust and TypeScript timers (and the original Ruby adapter). The retained actors are unchanged; this asymmetry qualifies small parameterized comparisons but cannot explain parameter-free aggregate gaps. Ordinary setup and preparation happen outside that per-query timed span.

Rollback ns/op includes BEGIN, the complete authored batch of writes, and ROLLBACK; it is not automatically ns per modified row or per individual statement. Durable-write ns/op covers one real autocommit and its durability work. Default synchronous mode stays on; only the authored `insert_commit_relaxed` and `update_commit_relaxed` lanes use relaxed mode. Their post-run counts include warmup writes as required.

Cold-open ns/op includes open, prepare, and one full drain; close is outside the timed span. It does not imply an OS page-cache drop. Concurrent-reader `total_ns` is timed-phase wall time including the canonical concurrency-hook boundary; `ns_per_op` is wall time divided by total measured queries, a throughput measure, not an individual reader's latency. Swift/Go/Rust use their native concurrent drivers; the shared synchronous TS driver runs reader blocks sequentially. Block checksums fold in reader-index order. Host-function workloads use the authored row/batch kernel and ordinary public extension path.

Raw timing fields are integer nanoseconds. Displayed milliseconds are ns/1,000,000 and microseconds are ns/1,000; CSV retains exact ns. Schema-2 `ns_per_op` is integer `total_ns / iterations`. Within-run percentiles use the shared lower-sample index `floor((n-1)*p/100)`. Swift retains all per-iteration `samples_ns`; peers retain their canonical aggregate JSONL and percentile fields, not a newly invented per-iteration sample stream.

Every result must have exact lane membership, SQL/setup/parameter contract, dataset fingerprint, warmup/iterations/readers, row counts and 64-bit FNV checksum. The checksum uses canonical cell rendering and shared delimiters; writes also validate final row counts. Validation failures prohibit publishing an overall successful comparison.

### Summary formulas and validation

For stage `s`, core `c`, workload `w`, and round `r`:

```text
t[s,c,w,r] = reported ns_per_op = floor(total_ns / iterations)
M[s,c,w]   = median(t[s,c,w,1], t[s,c,w,2], t[s,c,w,3])
Swift speedup[w] = M[before,swift,w] / M[after,swift,w]       (>1 is faster after)
Swift change %[w] = 100 * (M[after,swift,w] / M[before,swift,w] - 1)
Swift/peer[s,p,w] = M[s,swift,w] / M[s,p,w]                 (>1 is slower than peer)
Peer speedup[p,w] = M[before,p,w] / M[after,p,w]
Geomean(R,W) = exp(sum(log(R[w]) for w in W) / count(W))
```

The central value is the median of three round means, not a pooled median of all query samples. Whole-suite and category geometric means give one equal weight to each workload's dimensionless ratio. Category membership and median ratios are also reported. Timings across unlike operation units are not summed, and long-running workloads receive no implicit extra weight.

The strict comparison completed successfully with:

```sh
mise run swift-native:bench-report \
  --before /home/jack/dev/jed/impl/swift-native/results/benchmark-baseline-20261106 \
  --after /home/jack/dev/jed/impl/swift-native/results/benchmark-after-20261106 \
  --output /home/jack/dev/jed/impl/swift-native/results/benchmark-comparison-20261106
```

Each full harness report contains 12 completed core-round processes, 143 workloads each, `success=true`, and an empty failures list. The comparison revalidates exact workload membership, normalized SQL/setup/parameters/seed, timing settings and units, per-round result structure, row counts, checksums, dataset identities and shared signatures within and across both runs. It relies on the successful harness reports for actor immutability, raw-sample validation and dataset-copy checks; it does not reread every large actor, image or sample array. No validation error or partial run is turned into a successful comparison.

`results/benchmark-comparison-20261106/` retains `comparison.json`, `workloads.csv`, `categories.csv`, and the helper's concise `report.md`. The original run directories retain every `round-*/results.jsonl`, Swift `samples.jsonl`, command/build/run logs, input/provenance files, source snapshots and immutable actors. Source capture and binary identity remain distinct evidence, as described above. The CSVs preserve all round-level means rather than treating the displayed median as the only observation.

An [independent read-only audit](results/benchmark-independent-audit-20261106/REVIEW.md) recomputed all 3,432 raw result rows, 143 logical signatures, 1,144 medians, 11 category groups and 43 regression rows, and rehashed the captured sources and actors. It found no report discrepancy or concrete engine defect requiring a correction. The retained audit includes its script, machine-readable results and logs; its [manifest](results/benchmark-independent-audit-20261106/manifest.json) has SHA-256 `3ddd870dd7cc4ff4a00b4d74ade963a612a414b21ab9dbbc2ea912be52e83c70`.

### Profile-guided change and earlier diagnostic

Commit `3db8fc2d2653475abc5e9d43049254ccd1e92387` adds a retained packed-leaf position to the tree cursor and uses it for the existing eligible, unfiltered, ungrouped whole-table fixed-cell aggregate lane. It decodes touched cells in their original order, folds the original aggregate states, and preserves cost, cancellation and memory-account schedules. Filtered, grouped and variable-width aggregation stays on its existing driver. Slice-aware value decoding also removes avoidable intermediate byte-array copies. No file format, result semantics, shared workload, or durability change is part of the optimization.

The completed baseline selected `full_scan_agg` for investigation. The corrected before stack capture has 91 main-thread observations at the 20-second duration limit; all include `GroupedSelectExecution`, with leaf `swift_release` 29, `swift_retain` 25, and slow decrement four. The after capture has 19 observations and exits naturally with status 0 after the same canonical three warmups plus 20 queries; 17 include `foldPackedWholeTable`. One after sample is in opening the tree skeleton, and one is a single CRC32 frame with unavailable parent context.

Both capture configs use the same sampler backend, 20-second maximum, 100 ms requested interval, 64 frames and 32 threads. These are debugger-stop observations, not CPU percentages or valid benchmark timings. Unequal termination and phase coverage prohibit direct frequency-percentage attribution. The first six-sample attempt failed on sampler event sequencing; it remains preserved at `results/profile-full-scan-before` and is not substituted for the complete capture. Exact tooling, symbol/module identities, cleanup records and compact stack analyses are retained under `results/profile-full-scan-before-second/tooling` and `results/profile-full-scan-after/tooling`; both child/debugger pairs were verified gone.

The before profile uses the baseline actor `44a127af9e7891bb245e846fa38edd3cb1a186c3be6c90267365407ff39d3aa7`. The after profile uses the earlier diagnostic actor `046a44b5c9481d55d6bdcf4aaf585b97a05cba37b6c42fbc35f53b9d4ec6a0ad`, distinct from the freshly built full-after actor `e33fd8a8…`. Their tooling archive manifests are `8f700635f12eee8de4b5da3f046db75ff3857fc6549e4351990ebb4d645accd7` before and `aefaeb1157162f5e9325fc99465abcd74ad7ef6be7e274a7a97c35d0a0cac67b` after; the after archive links the exact before tooling.

A separate **single-round, two-workload preliminary diagnostic** at `results/benchmark-packed-diagnostic-20261106`, not the final report headline or a substitute for the full run, recorded `full_scan_agg` 229.996 ms against the baseline three-round median 1119.919 ms; the filtered control recorded 978.662 ms against 1001.085 ms. Checksums and row counts agreed. These numbers mix a single new round with a baseline median and were only initial evidence for proceeding to complete validation and the full matched run. The complete matched results above supersede that preliminary timing comparison.

### Retained evidence hashes

SHA-256 values identify the small reports and CSV artifacts. Paths beginning `benchmark-` in this table are beneath `results/`; `BENCHMARK_*.csv` paths are beside this report. The comparison JSON additionally records the hashes of every input result JSONL and small evidence file it read. Raw run directories and their actor/source manifests preserve the larger evidence; these small hashes are not a substitute for those identities.

| Artifact | SHA-256 |
| --- | --- |
| `benchmark-baseline-20261106/metadata.json` | `a102a94f211eab214cd7c9b1a02f94d61ea716c829601d9eb60ef093a3d37d20` |
| `benchmark-baseline-20261106/report.json` | `a8306f2b072053560433af2c946a2f3ab0f815a3c18181e3b699992e4b240bfb` |
| `benchmark-after-20261106/metadata.json` | `41e06b1b9117cdba82ab5dd32df963e4a3b68bbcb13bc181dd4a3dd89d2f8e24` |
| `benchmark-after-20261106/report.json` | `22cef3aab0d414078af28455949e12c2599c1d20242444e51d0a47ea193f0f87` |
| `benchmark-comparison-20261106/comparison.json` | `26ed9526d68fb3d42e9dc046271a1729968447589866191e5c40048b44f2a406` |
| `benchmark-comparison-20261106/workloads.csv` | `e5f2073fdc58550f6c5b03edff64c544671887daa79e37bb99d200189a68ec73` |
| `benchmark-comparison-20261106/categories.csv` | `33faf40f720f74bf822c680bde4da4a8631b0fb29b3a72e8b533e3e490798f1d` |
| `benchmark-comparison-20261106/report.md` | `7dcbc41b0dccc4506f4b7e4c46c73a1bb752311fae3a195aadb87fe2b50c6d4a` |
| `BENCHMARK_WORKLOADS.csv` | `e5f2073fdc58550f6c5b03edff64c544671887daa79e37bb99d200189a68ec73` |
| `BENCHMARK_CATEGORIES.csv` | `33faf40f720f74bf822c680bde4da4a8631b0fb29b3a72e8b533e3e490798f1d` |
| `BENCHMARK_REGRESSIONS.csv` | `b9e9da97b4bfe5a4a82b868c0945009801654d3f6029bde393f98a3beeaa838a` |

Earlier diagnostic and failed-profile artifacts are retained separately. They are not counted among the two successful full benchmark runs. Correctness failures and subsequent repairs remain in the verification/progress history rather than being relabeled as successful optimization evidence.
