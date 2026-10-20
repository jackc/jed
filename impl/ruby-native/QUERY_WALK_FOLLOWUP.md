# Query setup follow-up

> Historical report: raw evidence and run-specific drivers are on the preserved
> experiment branch. See the [archive and retrieval instructions](../../bench/ruby-native/ARCHIVE.md).
> Reproduction commands below refer to that original checkout.

This explicitly requested follow-up to [physical join-plan reuse](PHYSICAL_PLAN_FOLLOWUP.md)
reduces repeated query setup. The Ruby core remains experimental, frozen and
opt-in; normal tests, benchmarks, CI and future feature updates do not include it.

## Implementation

Aggregate discovery, window discovery and opaque-definition validation now
recurse only into Hash and Array children. Scalar children previously entered a
recursive method only to return immediately. Aggregate discovery also avoids
allocating arrays of Hash values. Opaque-definition validation reuses its
read-only CTE scope until a node introduces bindings, preserving shadowing and
validation order without copying an empty scope at every syntax node.

These walks always read the current tree. Each keeps its original traversal
rules, including the different aggregate-helper Array semantics, subquery
boundaries, window-node identities, duplicates and caller-owned accumulators.
Opaque definitions still depend on the current catalog and are checked before
execution. No authorization or successful validation result is cached.

The existing prepared-join entries also retain aliased base-table source column
metadata. This avoids repeatedly constructing the same descriptor hashes during
predicate rewriting and planning. Reuse requires an admitted join frame, the
current table descriptor and the existing schema/type/environment context.
Views, CTEs, lateral sources, column alias lists, introspection and explicitly
routed database sources fall back to ordinary metadata construction. Alias
strings and retained descriptors are copied and frozen; execution rows and
bindings are not retained.

Each existing statement entry can retain at most eight source descriptors,
4,096 traversal units and 64 KiB of copied strings, independently of its
existing resolution-key allowance. Oversized or nonprimitive metadata falls
back. The enclosing cache still has a 64-statement bound. These are retained
metadata limits, not a whole-engine heap guarantee.

The broad-query checks also exposed a stack overflow on a legal, flat 240-item
`IN` list. Its resolved predicate is a deep OR tree even though the SQL itself
is shallow. Predicate selectivity and work estimation now use the same bounded
stack slices as the existing resolver/evaluator. Each slice resumes the ordinary
method chain with its current table and parameter context; the fix changes
neither predicate visitation nor deterministic cost. The shared `IN`/`NOT IN`
regressions cover the tail, misses and NULL input, with additional host checks
for prepared statements, both storage modes and recovery after an error.

## Measurement method

The baseline is commit `aa8783c99afd1816209f09f194745964d12fd5db`,
library SHA-256 `d21f4edda6dc0a007fce8499c2200c334e5e1e242f0d1397617910a21cda7396`,
archived at `/tmp/jed-query-shape-before/impl/ruby-native`.
Final library SHA-256:
`bb56f9a0b9592def93f4e0fb24f23098439b1d657e77f2d5fdbfdd14eeafc17d`.
Evidence is in
[`query-walk-followup`](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/query-walk-followup`).
Earlier reports and artifacts are unchanged.

The main comparison retains the exact existing nine-workload
[multiway plan](../../bench/ruby-native/compare/multiway-plan.json): its SQL,
1,000-row fixture, binding schedules, warmups, iterations and five samples.
Each interpreter/YJIT/ZJIT and prepared/query/eager cell uses serial
before/final/final/before processes. Tables use the mean of the two process
medians per source. Process ranges describe observed variation; they are not a
statistical significance test. Every slower result remains in the evidence.

Supplemental YJIT controls cover positive windows, aggregation, broad syntax
and genuinely fresh SQL. Prepared APIs cannot use the fresh-SQL protocol, so
that comparison covers repeated query and execute only. All timed results are
fully drained. Separate preflights compare exact values, metadata and costs
with the actual Rust Ruby package.

Interpreter and YJIT use Ruby 4.0.7. ZJIT uses Ruby 4.1.0-preview1, so comparisons
between JIT modes also change Ruby version. The library never enables a JIT.
Fresh supported-core and actual-binding comparisons characterize their
different collector/preparation paths; historical ratios are not controlled
source comparisons. Profiling runs follow primary timing. TracePoint numbers
are method-call counts, not sampled CPU percentages.

Correctness and measurement workers run serially under a 512 MiB sampled RSS
guard. Ruby workers have at most 2 GiB of address space; supported-core
benchmark workers retain their established virtual-address allowance.
Supported-core regression checks use a 16 GiB virtual reserve, except for the
TypeScript file check's established 64 GiB reserve. That worker peaked at
184.4 MiB RSS, below the same 512 MiB resident limit.

## Correctness

All 65 Ruby host-test files passed, as did focused checks under the interpreter,
YJIT and ZJIT. The four complete corpus lanes passed **77,154/77,154 records**:

| Storage / API | Runtime | Passed records |
| --- | --- | ---: |
| Memory / query cursor | ZJIT | 20,297 |
| Memory / execute collector | ZJIT | 20,297 |
| File / query cursor | YJIT | 18,280 |
| File / execute collector | YJIT | 18,280 |

There were no failures, unsupported records, timeouts, memory-limit exits or
additional skips. Existing authored storage-mode skips remain unchanged.
The largest corpus worker used 177.4 MiB sampled RSS; the host-test peak was
163.5 MiB.

The focused checks cover mutable syntax trees, traversal order and identity,
CTE shadowing, view validation, source metadata ownership and invalidation,
cache bounds, exact costs, lifetime limits and query-memory limits. They exercise
four query APIs with memory and file storage.

Both new flat-`IN` corpus records pass on Ruby under all three runtime modes
and both storage modes, and on Rust, Go and TypeScript in both storage modes.
The complete 26-record `IN` suite also passes the PostgreSQL oracle. A preserved
baseline probe reproduces `SystemStackError`; the final source returns the
expected row and cost. These checks all completed before formal timing started.

The three final direct reports each cover 104 workload/API/storage groups and
520 traced invocations. Warmed three-table calls reduce aggregate-tree visits
from 178 to 34 and window-tree visits from 137 to 29; eager opaque validation
falls from 191 to 37. Repeated source-column construction falls from six calls
to zero in the warmed memory three-table case. Its retained source metadata
uses three entries, 171 traversal units and 126 copied string bytes. These
counts demonstrate less setup work; they do not measure CPU-time shares.

## Results

This is a modest, uneven setup optimization. Across the 94 paired cells, 32
have both final process medians below both baseline medians, 12 have separated
slower medians, and the other 50 have overlapping ranges. Thirty-eight cells
are slower on their paired means. All samples and slower cells are retained in
[`tables.md`](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/query-walk-followup/tables.md`);
overlap is not proof of equivalence, and separation is not a significance test.

The five main join targets—indexed, three-table hit/miss/computed-key and
four-table—improve 1.026–1.044× through the interpreter's prepared API.
YJIT eager execution improves 1.036–1.089× on those targets; ZJIT eager improves
1.063–1.120×. Each of those observations has separated process medians.
YJIT prepared results are mostly near-neutral or overlapping:

| Workload | Before µs | Final µs | Speedup |
| --- | ---: | ---: | ---: |
| Scalar | 4.829 | 5.080 | 0.951× |
| Primary key | 3.956 | 4.123 | 0.959× |
| Indexed join | 158.068 | 157.223 | 1.005× |
| Three-table hit | 231.214 | 231.280 | 1.000× |
| Three-table miss | 206.303 | 202.110 | 1.021× |
| Three-table range | 1443.150 | 1499.134 | 0.963× |
| Three-table computed key | 260.727 | 257.040 | 1.014× |
| Four-table join | 330.952 | 330.391 | 1.002× |
| Three-table LEFT | 711.801 | 747.814 | 0.952× |

The separated slower YJIT prepared cells are scalar (+5.188%), PK (+4.224%),
range (+3.879%) and LEFT (+5.059%). Eager range is also slower (+2.077%).
Interpreter prepared LEFT is +1.843%; ZJIT prepared PK is +4.946%.
The largest original regression is ZJIT prepared three-table hit:
227.452→286.436 µs, or +25.932%. Its final processes have early spikes, but
even their last two samples remain slower; those tails do not replace the
original medians. A separate warmup investigation is recorded below.

Supplemental controls also expose costs. Query windows (+3.377%), query
aggregation (+2.893%), prepared aggregation (+0.974%) and eager broad projection
(+4.406%) have separated slower medians. Eager windows improve 1.014×.
Fresh scalar SQL improves 1.052× through eager execution; the fresh-query scalar
and both fresh-join API comparisons are slower by 1.987–3.018%, with overlapping
process ranges.

The eager profiles show a more consistent allocation reduction:

| Workload | Before objects/query | Final objects/query | Reduction |
| --- | ---: | ---: | ---: |
| Indexed join | 844.5 | 648.5 | 23.2% |
| Three-table hit | 1297.0 | 999.0 | 23.0% |
| Three-table miss | 1074.0 | 776.0 | 27.7% |
| Three-table computed key | 1309.0 | 993.0 | 24.1% |
| Four-table join | 1708.0 | 1304.0 | 23.7% |

Scalar and PK allocations remain 39 and 31 objects/query. Range and LEFT
allocations fall 2.1% and 3.4%, respectively. The new stack guard adds 162
predicate-estimation and 231 expression-work wrapper calls to range planning,
and 61/93 to LEFT. These counts locate additional correctness work; they do not
assign a share of elapsed regressions to it. Warmed prepared scalar/PK paths
exercise neither the new walks nor source metadata reuse, so their slowdowns
cannot be attributed directly to descriptor construction. Eager PK substitutes
one new aggregate-helper call; eager scalar's profiled method counts are unchanged.

### Fresh cross-core and binding comparisons

The separate supported-core adapter measures Ruby YJIT at 4.614 µs for scalar
and 3.879 µs for PK: 5.747× and 2.462× Go, or 3.250× and 1.749× Rust.
Indexed join is 2.812× Go; the other join shapes still range from 17.123× to
206.904× Go. Substantial execution/setup differences remain despite the
allocation reduction. Full per-core values are in the evidence tables.

The actual Ruby Rust-wrapper comparison has different public API costs.
Native prepared elapsed time is 0.617× the wrapper for scalar, 0.377× for PK
and 0.754× for the four-table join. It remains slower for indexed join
(4.002×), three-table hit (1.650×), miss (1.173×), range (6.216×),
computed key (1.960×) and LEFT (14.437×). These are fresh package-path
comparisons, not controlled measures of the source change.

### Warmup investigation

The adaptive follow-up ran after the original 33-command queue, using eight
new uninstrumented ZJIT prepared processes and four separate counter replays.
All nine jobs retain their original order, fixture, SQL, parameter formulas,
validation and batch sizes. One ABBA quartet repeats the original plan exactly;
the other changes only the three-table hit's warmup from 100 to 1,000 calls.
Both still use five samples of 20 target queries.

| Three-table prepared comparison | Before µs | Final µs | Elapsed change |
| --- | ---: | ---: | ---: |
| Original, 100 warmups | 227.452 | 286.436 | +25.932% |
| Separate repeat, 100 warmups | 236.788 | 208.019 | −12.150% |
| Separate repeat, 1,000 warmups | 229.322 | 213.889 | −6.729% |

Each quartet has separated process medians, but the direction changes between
repeats. The follow-up did not reproduce the original regression, and longer
warmup is not demonstrated as its cause. These additional observations remain
separate from the original 94 cells; none replaces a primary sample. This
query's ZJIT result is too variable here to claim a dependable elapsed gain.

The four instrumented replays use `--zjit-stats-quiet` and collect counters
outside each workload loop. Each records four additional compiled instruction
sequences during the target warmup and none during its five subsequent batches.
GC activity varies across the replays. Statistics collection and its extra
execution frames may themselves affect runtime behavior, so these observations
do not identify the cause of the uninstrumented timing difference.
The [separate evidence](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/query-walk-followup/warmup-diagnostic`)
verifies every source/runtime/helper pin, complete result/cost signature,
schedule and serial completion interval. All 12 child processes succeeded;
their peak was 88.8 MiB RSS.

### Remaining candidates

The final profiles show temporary arrays in expression collection and index-key
comparison: three-/four-table queries allocate 55/77 arrays in
`expression_nodes` and 36/52 in `same_key_expression?`. Accumulator-based walks
and direct field comparisons are candidates for a further measured change.
Value-sensitive planning and unsupported LEFT joins remain larger, separate
opportunities. No further optimization is included here.

All primary and adaptive workers completed within their resource guards.
Primary Ruby comparison/binding workers peaked at 87.4 MiB RSS; the supported
TypeScript comparison peaked at 176.1 MiB. These are in-memory timing fixtures;
file storage is covered by correctness and direct diagnostics, not a new
file-backed elapsed-time comparison.

## Reproduction

The evidence directory contains `check.rb` for focused, host and full-corpus
checks, and `run.rb` for measurements. These drivers call the explicit
`mise run ruby-native:*` tasks, run workers serially and refuse to overwrite
existing output. Preserve earlier output before intentionally rerunning.

After correctness passes and the library and measured helpers are frozen,
invoke `run.rb --phase PHASE --final-source-sha256 FINAL_HASH` in this order:
`pins`, `verify`, `diagnostics`, `controls`, `supplemental`, `cross`, `binding`,
`profiles`. Run the orchestration directly or with at least 16 GiB of outer
address space so supported-core workers can obtain their required virtual
reserve; each workload still has its own RSS guard. The correctness regression
driver's TypeScript file lane needs the separately documented 64 GiB reserve.

After the primary `profiles` phase exits, reproduce the separate adaptive queue
with `warmup-diagnostic/run.rb --phase pins`, then
`warmup-diagnostic/run.rb --phase all`. This runs the eight new timing processes
and four statistics replays serially, with their own guards. The final verifier
requires both the primary and adaptive evidence.

After all workers exit, `summarize.rb` checks source and helper hashes, runtime
pins, exact validation signatures, unchanged schedules, raw-sample arithmetic
and serialized completion intervals. It derives `summary.json` and `tables.md`,
records lossless JSON compaction and writes an artifact manifest. No timing
workload is run by the summarizer.

The final verifier passed all 33 primary and 12 adaptive command records.
The manifest covers 136 files totaling 19,494,502 bytes; its SHA-256 is
`f732d3726ed2218a03e5e39ef7ab7adbb6a0a69a612f5f84034f5d2dba6ec4ed`.
The serialization ledger checks 94 compacted JSON files. The two original
flat-`IN` JSON captures and all nested adaptive evidence retain their original
bytes.
