# Physical join-plan reuse follow-up

> Historical report: raw evidence and run-specific drivers are on the preserved
> experiment branch. See the [archive and retrieval instructions](../../bench/ruby-native/ARCHIVE.md).
> Reproduction commands below refer to that original checkout.

This explicitly requested optimization follows the
[multiway resolution experiment](MULTIWAY_PLAN_FOLLOWUP.md). The Ruby core remains
experimental, frozen and opt-in. Normal tests, benchmarks, CI and future feature
updates do not include it.

## Motivation and implementation

The preceding source comparison improved every measured multiway target, but
still repeated substantial physical planning. Its final three-table profile
made 162 base predicate-estimation calls per query; the four-table profile made
437. Four-table resolution also remained partial within the existing retained-key
budget. These observations motivate investigating reuse of a completed physical
plan separately from resolved expression reuse.

The implementation retains at most one physical key, plan and validation record
inside each existing statement-resolution entry. Planning runs normally on the
first call. A successful, unmetered plan is retained only if candidate selection
did not read a parameter's actual value. Later calls with the same structural
key reuse that plan after replaying its contextual parameter checks. Queries
whose estimates inspect values still plan afresh.

Checking whether a binding is NULL is distinguished from reading its value.
Every invocation containing a NULL binding bypasses physical reuse, preserving
the retained non-NULL plan for a later call. Transactions, temporary tables and
block-bound cursors also bypass it. Existing statement, table-descriptor and
context checks still govern the enclosing resolution entry; replacing those
facts invalidates the physical record with that entry. These are internal
choices, not new host API settings.

Physical selection remains limited to the admitted INNER/CROSS paths. The
broader resolution cache's two-table LEFT support does not imply physical LEFT
plan reuse.

Plan-owned arrays, hashes and strings are copied and frozen. Existing expression
objects and table descriptors are borrowed, preserving their identity and
existing compiled state. Execution rows are not captured. Unsupported snapshot
objects, including collation metadata outside the accepted primitive shape,
cause fresh planning. Duplicate pure, unmetered validations of normalized
integer/boolean bindings to integer/boolean targets can be removed at
publication while preserving each check's first occurrence and error order.
Text and other potentially observable validation paths keep their full order.

The existing limits remain 64 statement entries, 128 resolved roots per entry,
4,096 structural key units and 64 KiB of copied key strings. Each physical
snapshot adds limits of 16,384 traversal units, 128 KiB of copied strings and
4,096 observed validations before deduplication. Oversized observations fall
back to ordinary planning. These are bounded retained structures, not a
whole-engine heap guarantee.

## Evidence and method

Archived starting point: commit
`b5262268371ff5e7d93c2b61e04bcfb304665cf4`, library SHA-256
`40ceac58a86ce19bba2ed2497adfb26b5ca7eafcc31e39def71312c1a9d56382`.
The complete Ruby port was archived from that commit at
`/tmp/jed-physical-plan-before/impl/ruby-native` after fetching and checking the
pushed baseline. Final library SHA-256:
`d21f4edda6dc0a007fce8499c2200c334e5e1e242f0d1397617910a21cda7396`.

New evidence belongs in
[`physical-plan-followup`](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/physical-plan-followup`).
Earlier experiment reports and artifacts remain unchanged. This comparison uses
the exact existing [nine-workload plan](../../bench/ruby-native/compare/multiway-plan.json),
including its SQL, 1,000-row fixture, parameter schedules, warmups, iterations and
five samples per process. It covers scalar and PK controls, the two-table indexed
join, three-table hit/miss/range/computed-key shapes, a four-table join and the
unsupported multiway LEFT fence. The bounded predicates keep these small
fixtures from becoming a multiplicative join experiment.

All timed queries drain their complete results. Source comparisons run serial
before/final/final/before processes, using the mean of the two process medians
for each source. Every raw sample and every slower control remains in the
evidence. Separation between four process medians is an observation, not a
statistical significance test.

Interpreter and YJIT runs use Ruby 4.0.7; ZJIT uses Ruby 4.1.0-preview1. Comparing
ZJIT with the other modes also changes Ruby version. The library does not enable
a JIT on the application's behalf.

The native API lanes use `query_prepared`, repeated `query`, and repeated
`execute` on a persistent session. The actual Rust Ruby package uses its public
query API, including its normal parsing, FFI and result handling. The separate
supported-core adapters retain their existing collectors, including Ruby
`Rows#rows`. Their common validation covers rendered-value checksums and row
counts; the binding comparison separately checks exact values, column metadata
and costs. Fresh cross-core ratios characterize these paths and must not be
compared with historical ratios as though they were a controlled source change.

The new untimed direct diagnostic retains the memory/file, four-API, varying
binding, skewed-statistics, index-change and exact cost/error-prefix cases from
the previous experiment. Its control disables only physical reuse, leaving
resolved-root reuse enabled on both sides. Winning plans are observed at the
planner wrapper's return so a cache hit can still be compared with fresh
planning; calls to the original planners are counted separately. Cache entry
identity and NULL/following-non-NULL observations remain explicit. Retention is
observed after the owning thread and database have completed, without promising
a particular VM collection schedule.

The five physical targets are the two-table indexed join and the three-table
hit, miss, computed-key and four-table joins. Each warmed non-NULL direct case
must retain a physical plan and avoid the original planner, while its disabled
control must still call that planner. The range query remains value-sensitive;
scalar, PK and multiway LEFT cases remain controls. A NULL binding must preserve
the retained plan and match ordinary planning, including cases pruned before
the planner runs.

The new analyzed-skew test exposed an earlier statistics-context bug: a second
`estimate_scan_candidate` definition had replaced the wrapper that supplies
table statistics. Related join-local and index-join estimates also need that
table context. The seven new shared records exercise common, rare and absent
bindings against analyzed facts. The archived Ruby source fails three of those
records, including charging 96 instead of the specified 92 for the common
value. Those failures are retained as evidence of the corrected bug.

The new expectations pass Rust, Go and TypeScript in memory and file modes.
PostgreSQL verifies the three result sets; jed's own planner and cost contract
determines the EXPLAIN and work-cost expectations. Those checks retain their
original `2f8c` Ruby supervisor provenance; the verifier confirms that all
consumed supported-engine sources, binaries, corpus and oracle helpers remain
unchanged. A separate final-source Ruby run passes all 34 regression records.
Workloads and budget-prefix
digests must remain equal across the
archived and final sources. Analyzed-skew observations have a separate digest:
their complete before/final data is retained, each source must match its own
physical-disabled control, and any corrected plan/cost difference must be
identified explicitly rather than counted as an optimization gain.

Ruby workload workers are bounded at 512 MiB sampled RSS and at most 2 GiB
address space. Go, Rust and TypeScript benchmark workers retain their existing
512 MiB RSS and 16 GiB address-space guards. All correctness workers must finish
before formal timing. The before/final profiling phase runs after all primary
measurements, and its traces are diagnostic counts rather than sampled CPU-time
percentages.

The separate TypeScript disk regression check uses its established 64 GiB
virtual-address allowance, with the same 512 MiB RSS guard; it peaked at
285,966,336 resident bytes. This allowance is not used by the timed comparison.

## Correctness and direct evidence

All 64 host-test files and the focused join/physical-plan tests passed on the
final source. Focused runs covered interpreter, YJIT and ZJIT. Four complete
corpus audits passed 77,146/77,146 records: 20,295 in memory and 18,278 on disk,
through each of the query and execute APIs. Query results were collected
externally and every audit used one API uniformly. No record-level failure
allowance, unsupported file or extra skip was introduced.

The six direct reports preserve exact workload values, metadata, costs, error
prefixes and winning plans across both sources and all runtimes. They prove 720
warmed non-NULL physical hits across the five targets, four APIs, two storage
modes and three runtimes, with original planner calls eliminated and positive
calls in the physical-disabled controls. Another 120 NULL observations preserve
plan identity and match fresh planning or upstream pruning. Range and LEFT
fallbacks retain their original planning behavior.

Each admitted integer target now retains one contextual replay check. The
indexed, three-table hit/miss, computed-key and four-table snapshots respectively
use 757, 1,388, 1,388, 1,242 and 1,914 traversal units, with 99, 110, 110, 110 and
141 copied string bytes. The cache churn creates 96 statement variants, retains
64 physical records at its bound, then observes zero live entries after each of
three completed-thread full collections in every runtime.

The analyzed-skew correction affects exactly nine of 25 observations per storage
mode, identically in all runtimes. Hot-value cost changes from 579 or 594 to 101
with the corrected selected plan; one absent-value observation changes only
EXPLAIN. Every result row, column name and type remains unchanged; all five
pre-ANALYZE observations and the other unchanged cases remain exact. These cost
changes belong to the correctness fix and are excluded from performance claims.

Final-source corpus workers peaked at 181,813,248 bytes (173.39 MiB); the
64-file host suite peaked at 164,241,408 bytes (156.63 MiB). The main Ruby
comparison/binding workers peaked at 89,407,488 bytes (85.27 MiB), and the six
untimed direct workers at 97,607,680 bytes (93.09 MiB). All final-source
correctness and formal measurement guard reports show normal completion below
their configured limits. The TypeScript comparison
worker peaked at 202,735,616 bytes (193.34 MiB), also below its RSS limit.

## Measurements and remaining work

The five retained-plan targets improved across all 45 runtime/API cells, with
both final process medians below both baseline medians in every cell. Target
speedups range from 2.319–7.554× in the interpreter, 2.288–6.803× with YJIT and
2.309–6.691× with ZJIT. The YJIT prepared three-table query improves 3.069× and
the four-table query 6.321× against the immediately preceding Ruby source.

The fallbacks have a measurable cost. Twenty-five control cells are slower on
the paired process means; eleven have separated slower process medians. The
largest is the ZJIT query range shape at +7.543%. The interpreter prepared range
is +6.340%, YJIT prepared range +3.638%, and YJIT eager LEFT +4.063%. Every slower
control, including overlapping observations, is shown below. These results do
not establish zero overhead on unsupported or value-sensitive shapes.

The fresh Rust-binding comparison puts the four-table query at 334.103 µs for
native prepared versus 407.601 µs for the wrapper, an 18.0% elapsed reduction.
Scalar and PK queries are also faster through the native Ruby API. Other joins
still vary greatly: native prepared is 1.720× slower for three-table, 1.226× for
the miss, 1.979× for the computed key, 4.392× for indexed, 6.415× for range and
14.335× for LEFT. These public-package paths have different preparation and FFI
costs; beating the wrapper is not beating the Rust core. The fresh adapter table
still shows large multiway gaps to Go, Rust and TypeScript. Both fresh tables
are single-process characterizations, separate from the controlled Ruby ABBA.

### YJIT prepared source ABBA (µs; mean of two process medians per source)

| Workload | Before | Final | Before/final | Before process range | Final process range |
|---|---|---|---|---|---|
| scalar | 4.861 | 4.954 | 0.981× | 4.781–4.941 | 4.852–5.056 |
| pk_hit | 4.034 | 4.037 | 0.999× | 3.973–4.096 | 3.982–4.092 |
| indexed_join | 366.132 | 160.005 | 2.288× | 360.523–371.741 | 157.900–162.110 |
| three_table_join | 730.284 | 237.950 | 3.069× | 715.199–745.369 | 234.229–241.671 |
| three_table_join_miss | 695.250 | 208.874 | 3.329× | 677.323–713.177 | 204.526–213.222 |
| three_table_join_range | 1445.722 | 1498.318 | 0.965× | 1424.417–1467.028 | 1485.422–1511.215 |
| three_table_computed_key | 694.170 | 263.923 | 2.630× | 684.206–704.134 | 260.601–267.244 |
| four_table_join | 2166.631 | 342.791 | 6.321× | 2122.079–2211.183 | 339.809–345.772 |
| three_table_left_fence | 698.597 | 723.416 | 0.966× | 680.685–716.509 | 708.465–738.366 |

### Historical three-table source ABBA

| Runtime | API | Before µs | Final µs | Speedup | Before range | Final range |
|---|---|---|---|---|---|---|
| 4.0-none | prepared | 1186.395 | 369.078 | 3.214× | 1178.347–1194.444 | 362.852–375.304 |
| 4.0-none | query | 1230.646 | 381.651 | 3.225× | 1217.355–1243.936 | 373.763–389.539 |
| 4.0-none | eager | 1209.254 | 356.625 | 3.391× | 1200.021–1218.487 | 349.984–363.267 |
| 4.0-yjit | prepared | 730.284 | 237.950 | 3.069× | 715.199–745.369 | 234.229–241.671 |
| 4.0-yjit | query | 757.718 | 239.056 | 3.170× | 744.760–770.676 | 230.963–247.149 |
| 4.0-yjit | eager | 750.609 | 233.337 | 3.217× | 723.835–777.384 | 225.063–241.610 |
| 4.1-zjit | prepared | 732.297 | 234.833 | 3.118× | 721.482–743.112 | 227.474–242.192 |
| 4.1-zjit | query | 724.362 | 237.721 | 3.047× | 714.630–734.095 | 229.656–245.787 |
| 4.1-zjit | eager | 726.955 | 250.252 | 2.905× | 723.407–730.503 | 234.925–265.578 |

### Fresh cross-core characterization (different collector/prepared paths; do not compare ratios to historical runs)

| Workload | Ruby µs | Go µs | Rust µs | TS µs | Ruby/Go | Ruby/Rust | Ruby/TS |
|---|---|---|---|---|---|---|---|
| scalar | 5.023 | 0.836 | 1.398 | 2.188 | 6.010× | 3.593× | 2.295× |
| pk_hit | 4.039 | 1.728 | 2.204 | 3.829 | 2.337× | 1.832× | 1.055× |
| indexed_join | 159.187 | 58.024 | 2.491 | 126.870 | 2.743× | 63.904× | 1.255× |
| three_table_join | 240.831 | 4.107 | 3.071 | 13.204 | 58.635× | 78.425× | 18.239× |
| three_table_join_miss | 209.192 | 1.155 | 1.464 | 3.874 | 181.103× | 142.872× | 53.992× |
| three_table_join_range | 1467.620 | 80.570 | 59.517 | 248.374 | 18.215× | 24.659× | 5.909× |
| three_table_computed_key | 278.416 | 6.433 | 5.790 | 17.013 | 43.278× | 48.082× | 16.365× |
| four_table_join | 338.623 | 4.101 | 3.949 | 12.176 | 82.568× | 85.748× | 27.811× |
| three_table_left_fence | 702.164 | 3.208 | 3.068 | 11.388 | 218.896× | 228.863× | 61.660× |

### Actual Rust Ruby binding comparison (external full drain; native/wrapper elapsed)

| Workload | Wrapper µs | Prepared µs | Query µs | Eager µs | Prepared/wrapper | Query/wrapper | Eager/wrapper |
|---|---|---|---|---|---|---|---|
| scalar | 7.694 | 4.790 | 5.395 | 5.294 | 0.623× | 0.701× | 0.688× |
| pk_hit | 10.281 | 3.944 | 4.477 | 3.915 | 0.384× | 0.435× | 0.381× |
| indexed_join | 35.469 | 155.786 | 157.885 | 150.782 | 4.392× | 4.451× | 4.251× |
| three_table_join | 134.823 | 231.916 | 230.005 | 221.540 | 1.720× | 1.706× | 1.643× |
| three_table_join_miss | 166.542 | 204.206 | 202.386 | 200.814 | 1.226× | 1.215× | 1.206× |
| three_table_join_range | 227.940 | 1462.349 | 1452.245 | 1436.205 | 6.415× | 6.371× | 6.301× |
| three_table_computed_key | 130.035 | 257.346 | 258.343 | 249.062 | 1.979× | 1.987× | 1.915× |
| four_table_join | 407.601 | 334.103 | 334.089 | 325.749 | 0.820× | 0.820× | 0.799× |
| three_table_left_fence | 48.490 | 695.081 | 697.470 | 702.890 | 14.335× | 14.384× | 14.496× |

### Every slower control, including overlapping process ranges

| Runtime | API | Workload | Elapsed change | Before process range µs | Final process range µs | Separated slower? |
|---|---|---|---|---|---|---|
| 4.0-none | prepared | pk_hit | +2.363% | 8.102–8.107 | 8.166–8.427 | yes |
| 4.0-none | prepared | three_table_join_range | +6.340% | 2678.460–2696.682 | 2841.482–2874.454 | yes |
| 4.0-none | prepared | three_table_left_fence | +3.268% | 1253.243–1262.506 | 1263.176–1334.794 | yes |
| 4.0-none | query | scalar | +0.628% | 13.386–14.067 | 13.701–13.924 | no |
| 4.0-none | query | pk_hit | +0.850% | 9.489–10.095 | 9.718–10.032 | no |
| 4.0-none | query | three_table_join_range | +1.702% | 2671.765–2846.318 | 2732.396–2879.592 | no |
| 4.0-none | query | three_table_left_fence | +2.221% | 1269.593–1295.145 | 1275.383–1346.320 | no |
| 4.0-none | eager | three_table_join_range | +2.024% | 2710.114–2726.508 | 2759.779–2786.864 | yes |
| 4.0-yjit | prepared | scalar | +1.912% | 4.781–4.941 | 4.852–5.056 | no |
| 4.0-yjit | prepared | pk_hit | +0.067% | 3.973–4.096 | 3.982–4.092 | no |
| 4.0-yjit | prepared | three_table_join_range | +3.638% | 1424.417–1467.028 | 1485.422–1511.215 | yes |
| 4.0-yjit | prepared | three_table_left_fence | +3.553% | 680.685–716.509 | 708.465–738.366 | no |
| 4.0-yjit | query | three_table_join_range | +0.604% | 1452.991–1475.570 | 1440.464–1505.791 | no |
| 4.0-yjit | eager | scalar | +2.370% | 5.325–5.620 | 5.485–5.719 | no |
| 4.0-yjit | eager | pk_hit | +1.762% | 3.896–4.186 | 4.028–4.196 | no |
| 4.0-yjit | eager | three_table_join_range | +2.904% | 1408.687–1468.312 | 1431.546–1529.014 | no |
| 4.0-yjit | eager | three_table_left_fence | +4.063% | 697.589–705.143 | 722.351–737.368 | yes |
| 4.1-zjit | prepared | pk_hit | +1.307% | 4.019–4.074 | 3.956–4.242 | no |
| 4.1-zjit | prepared | three_table_join_range | +7.076% | 1500.257–1540.090 | 1617.288–1638.184 | yes |
| 4.1-zjit | prepared | three_table_left_fence | +1.237% | 650.280–660.504 | 652.935–674.064 | no |
| 4.1-zjit | query | three_table_join_range | +7.543% | 1432.726–1511.755 | 1582.334–1584.246 | yes |
| 4.1-zjit | query | three_table_left_fence | +3.561% | 655.281–657.393 | 651.801–707.615 | no |
| 4.1-zjit | eager | scalar | +4.476% | 5.437–5.688 | 5.702–5.921 | yes |
| 4.1-zjit | eager | three_table_join_range | +4.672% | 1457.631–1476.866 | 1522.404–1549.186 | yes |
| 4.1-zjit | eager | three_table_left_fence | +3.453% | 641.732–646.799 | 657.170–675.859 | yes |

### Diagnostic allocations and exact base compiler calls (separate profile runs)

| Workload | Before allocations | Final allocations | Before Engine#compile | Final Engine#compile | Before PreparedJoins#compile | Final PreparedJoins#compile |
|---|---|---|---|---|---|---|
| scalar | 39.000 | 39.000 | 0 | 0 | 0 | 0 |
| pk_hit | 31.000 | 31.000 | 0 | 0 | 0 | 0 |
| indexed_join | 2419.660 | 844.500 | 0 | 0 | 13.0 | 7.0 |
| three_table_join | 5045.300 | 1297.000 | 0 | 0 | 9.0 | 9.0 |
| three_table_join_miss | 4822.300 | 1074.000 | 0 | 0 | 9.0 | 9.0 |
| three_table_join_range | 13995.800 | 14259.700 | 0 | 0 | 9.0 | 9.0 |
| three_table_computed_key | 4547.200 | 1309.000 | 0 | 0 | 9.0 | 9.0 |
| four_table_join | 16543.900 | 1708.000 | 119.0 | 0 | 139.0 | 12.0 |
| three_table_left_fence | 6403.000 | 6485.900 | 110.0 | 110.0 | 124.0 | 124.0 |

### Remaining final planner/evaluator calls per query (counts, not sampled CPU percentages)

| Method | Three-table hit | Three-table range | Computed key | Four-table |
|---|---|---|---|---|
| JedNative::OpaqueDefinitions#validate_opaque_query_tree | 191.0 | 191.0 | 205.0 | 256.0 |
| JedNative::AggregateExecution#local_aggregate? | 178.0 | 178.0 | 206.0 | 252.0 |
| JedNative::WindowExecution#window_nodes | 137.0 | 137.0 | 151.0 | 185.0 |
| JedNative::FastEvaluation#cast_value | 23.0 | 146.0 | 11.0 | 29.0 |
| JedNative::Session#statement_writes? | 32.0 | 32.0 | 34.0 | 43.0 |
| JedNative::AccessPlanning#metadata_expression? | 36.0 | 36.0 | 46.0 | 54.0 |
| JedNative::JoinPlanner#two_base_join_plan | 0 | 0 | 0 | 0 |
| JedNative::NWayJoins#nway_join_plan | 0 | 1.0 | 0 | 0 |
| JedNative::AccessPlanning#plan_constant | 0 | 249.0 | 0 | 0 |
| JedNative::AccessPlanning#expression_work | 0 | 231.0 | 0 | 0 |
| JedNative::AccessPlanning#estimate_predicate | 0 | 162.0 | 0 | 0 |
| JedNative::IndexScans#conjunctions | 19.0 | 442.0 | 15.0 | 27.0 |
| JedNative::JoinPushdown#pushdown_column_nodes | 12.0 | 141.0 | 22.0 | 18.0 |
| JedNative::NWayJoins#nway_extensions | 0 | 9.0 | 0 | 0 |
| JedNative::IndexNestedLoops#join_index_plan | 0 | 9.0 | 0 | 0 |
| JedNative::Engine#evaluate | 1.0 | 17.0 | 1.0 | 1.0 |
| JedNative::FastEvaluation#evaluate | 28.0 | 460.0 | 99.0 | 39.0 |


### What remains

Original predicate-estimation calls fall from 63, 162, 162, 110 and 437 per query
to zero on indexed, three-table hit, miss, computed-key and four-table targets.
Four-table base expression compilation also falls from 119 calls to zero,
without raising the earlier retained-resolution limits. Allocation reductions
are substantial: three-table 5,045.3→1,297.0 objects/query and four-table
16,543.9→1,708.0. Range remains on fresh planning and increases from 13,995.8 to
14,259.7 allocated objects/query; LEFT increases from 6,403.0 to 6,485.9.

The next evidence-backed candidate is bounded reuse of static query-shape and
validation metadata, with the same schema and context invalidation discipline.
Three-table/four-table still make 191/256 opaque-tree validation calls,
178/252 aggregate-classification calls and 137/185 window-tree calls per query.
The allocation snapshots also show 48/64 hashes from query-column metadata.
These are repeated-call and allocation counts, not sampled CPU-time percentages;
a follow-up must measure the benefit rather than assume those counts translate
directly to time. Value-sensitive planning and LEFT joins remain distinct
opportunities. No further implementation is included in this change.

The experiment remains opt-in and frozen. These gains do not constitute a
promotion to a supported core or a promise to update it with future features.


## Reproduction

The runner is explicit and refuses existing output files. Preserve previous
attempts before an intentional rerun. Run orchestration directly or with an
outer address-space allowance of at least 16 GiB; a 2 GiB outer hard limit would
prevent the supported-core workers from obtaining their required virtual space.

After correctness is green and the final library and measured helpers are
frozen, capture `source-pins.json` with
`bench/ruby-native/results/physical-plan-followup/pin_sources.rb`. Then invoke
`run.rb --phase PHASE --final-source-sha256 FINAL_HASH` in this order:

1. `verify`
2. `diagnostics`
3. `controls` (all three runtimes, each with a complete ABBA quartet)
4. `cross`
5. `binding`
6. `profiles`

The runner calls the existing explicit `mise run ruby-native:*` tasks and records
every command, selected source, resource environment and completion interval.
Only after all workers have exited, run the new evidence verifier to derive
`summary.json`, the lossless JSON serialization ledger and artifact manifest.
The final verifier and an independent raw-data audit checked all 81 paired
cells, 1,620 raw control samples, six direct reports and 24 serialized commands.
The recursive manifest covers 111 files totaling 27,368,884 bytes; its SHA-256 is
`b4d40cc5314b3a8a078253a2293f338a9a56435587859b3f1d585a61d2901a03`.
The 82-entry serialization ledger verifies lossless top-level JSON compaction.
Nested prototype and superseded correctness evidence retains its original bytes;
the provisional af697 core can be reconstructed from the retained baseline patch.
These preliminary and superseded reports are not substituted for final-source
verification or included as final ABBA samples.
