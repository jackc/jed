# Multiway join resolution follow-up

This explicitly requested optimization follows the
[prepared join resolution experiment](JOIN_PLAN_FOLLOWUP.md). The Ruby core stays
experimental, frozen and opt-in. Normal tests, benchmarks, CI and the supported
cores' maintenance obligations do not include it.

## Change

The previous experiment removed repeated resolution from bounded two-table joins.
Its three-table control still performed 171 base expression compilations per
query, with further calls through the surrounding resolution modules. This change
extends the same complete-root reuse to bounded multiway joins.

The current implementation admits three through eight plain table inputs joined
by INNER or CROSS joins, including comma-only FROM lists. Existing two-table
admission remains available, including its LEFT join case. Multiway outer-join
fences, USING/NATURAL joins, mixed comma/explicit join forms, lateral sources,
views and CTEs retain ordinary resolution. These are optimization boundaries;
they do not remove supported SQL behavior.

The retained entry contains resolved complete expression roots and table-column
scopes. Each distinct AST and ordered scope is copied once, frozen, and assigned
a token within the existing key budget. Root keys identify both tokens and
whether the expression is a value or a filter. Deduplication makes room for
more physical prefix orders without increasing the retention limits. Contextual parameter checks replay at their original resolution point
against the current binding. No winning physical plan is retained for a later
execution. Rewriting, bound evaluation, estimates, access selection and physical
row operators continue to run for each SELECT.

Within one multiway planning invocation, alternative access states can reach the
same ordered prefix. A bounded local memo reuses that prefix's scopes, ready
predicates and index candidates. Its key includes the planning context identity,
ordered prefix, next input and fixed-edge status. Cardinality estimates, step
costs and each state's mutable access hashes remain separate. The local memo
ends with the SELECT invocation; it is not a persistent prepared-plan cache.

The current limits are 64 retained statement entries, 128 complete roots per
entry, 4,096 copied key units and 64 KiB of copied key strings per entry. Input
count is at most eight and total input-column count at most 64. Original syntax
is bounded by 192 nodes, depth 32 and 32 KiB of node strings; scope names are
bounded before copying. Local extension reuse admits at most 128 memo entries.
Publication, table identity and session/type context continue to govern
invalidation. Cursor frames remain private while the bounded retained cache can
be shared. These are cache bounds, not a whole-engine heap guarantee.

## Preflight discoveries

The benchmark preflight ran before full audits or formal timing. It exposed two
existing problems in the proposed computed-key query on the archived baseline:

- With an uncast parameter used both directly and in `$1 + 1`, native Ruby
  rejected the query with `42804: conflicting parameter types`, while the actual
  Rust Ruby binding accepted it.
- Adding consistent `$1::i32` casts removed that inference failure. Values,
  names and types then agreed, but every validation binding cost 6,024 in native
  Ruby and 2,010,015 through the Rust binding. The other eight benchmark shapes
  agreed on their complete validation signatures.

Both failed preflight reports are preserved. No performance result should treat
these disagreements as a permitted validation exception, and explicit casts do
not constitute a fix for the uncast inference defect.

The parameter resolver now unifies scalar integer constraints at their widest
binding width, as the supported cores already do, while preserving each
occurrence's local expression type. A wider binding is not narrowed merely to
compare it with a narrower column; actual casts and arithmetic result overflow
still validate their respective widths. Reused roots replay the same checks as
fresh resolution. The shared parameter corpus adds mixed-context, metadata,
indexed-miss, and overflow regressions, and host tests exercise warmed statements
with compilation enabled and disabled.

The pushdown classifier also had an extra identity-cast allowance absent from
the shared non-trapping cast catalog. Removing that allowance restores the same
residual placement and exact costs as Rust and Go. Four shared regression records
pin parameter, column and literal identity casts, plus an allowed widening cast.
This changes incorrect earlier costs; it is not counted as a performance win.

The [failed preflights](../../bench/ruby-native/results/multiway-plan-followup/preflight-provenance.json)
and [cast regression evidence](../../bench/ruby-native/results/multiway-plan-followup/cast-gate-provenance.json)
remain available. The timed computed-key workload uses `b.id = a.id % 10`, a
PK-bound outer input and two inner inputs bounded below ten. Its complete
outcomes and costs already agree on the archived baseline, final implementation,
and Rust binding, so before/final timing compares equivalent physical work.
The original failing queries remain covered separately by correctness checks.

## Evidence and method

Archived starting point: commit
`a612935772c97075826bb98e34fa89d2ec9bce34`, library SHA-256
`730a4a03496f62feec03acb2355e9c5f2fc9dda69fa055e7d9595f7457bb5edf`.
Its private archive is `/tmp/jed-multiway-plan-before/impl/ruby-native`.

Final library SHA-256:
`40ceac58a86ce19bba2ed2497adfb26b5ca7eafcc31e39def71312c1a9d56382`.
This is a comparison of the combined final revision, including the two
correctness fixes. Every timed shape must agree on values, metadata and costs
before and after; the two defective original queries are regression cases, not
timed substitutes. Control fluctuations cannot be attributed solely to the cache.

New evidence belongs in
[`multiway-plan-followup`](../../bench/ruby-native/results/multiway-plan-followup/).
Earlier experiment evidence remains unchanged. The explicit
[nine-workload plan](../../bench/ruby-native/compare/multiway-plan.json) uses the
existing 1,000-row fixture. Scalar, PK, two-table indexed join and historical
three-table join retain their previous SQL and schedules. New shapes cover a
three-table miss, a three-table range, a computed key, a four-table join and an
unsupported multiway LEFT fence.

New equality shapes restrict every input by a primary-key bound; the computed
key uses the bounded ranges described above. The range explicitly bounds all
three inputs to at most 100 rows. The LEFT control keeps its third input's
restriction in ON so NULL extension remains observable.

All timed queries drain their complete results. Source comparisons use serial
before/final/final/before processes, with five samples per process. The reported
source value is the mean of its two process medians. Keep every control result
and every raw sample; separation between four process medians is not a
statistical significance test.

Interpreter and YJIT lanes use Ruby 4.0.7; ZJIT uses Ruby 4.1.0-preview1. Comparing
ZJIT with the other modes also changes Ruby version. The library does not enable
a JIT on the application's behalf.

Native timing lanes use `query_prepared`, ordinary repeated `query`, and ordinary
repeated `execute` in a persistent session. The actual Rust Ruby package uses
its public query API, including its normal parsing, FFI and result handling.
The separate supported-core adapters retain their existing collectors, including
Ruby `Rows#rows`. Their common validation covers rendered-value checksums and
row counts; exact costs and column metadata are independently checked through
the Ruby binding comparison. These are whole-query measurements, not isolated
language or FFI costs.

Untimed direct diagnostics compare archived and final sources across all four
Ruby execution APIs in memory and actual file storage. They cover changing
bindings, chosen multiway orders/accesses/steps, skewed statistics, index changes,
cost ceilings, partial-result/error prefixes and cache admission. A 96-statement
three-table churn probe checks the 64-entry limit and records weak-reference
observations after the owning thread has completed. Report those observations
without promising a particular VM collection schedule.

Every Ruby workload worker is bounded at 512 MiB sampled RSS and at most 2 GiB
address space; the focused and complete host suites use a stricter 1 GiB virtual limit.
Full-corpus audits use a ten-second per-file deadline and a 900-second
outer deadline. Other benchmark workers retain their existing 512 MiB RSS and
16 GiB address-space guard. The separate TypeScript disk regression checks use
64 GiB of virtual address space for their second V8 isolate, with the same
512 MiB RSS cap. A retained 16/64 GiB probe showed the owner-thread startup
reservation crossing 18 GiB; actual RSS stayed below 220 MiB. Correctness workers must finish before formal timing;
the final profiling phase follows all primary measurements.

## Correctness

The changed shared files contain 72 parameter records and 30 cast/pushdown
records. Both files pass on Rust, Go and TypeScript in memory and disk modes
(12 checks), and both regenerate byte-identically through `mise run corpus:check`
against the checkout's PostgreSQL 18.6 oracle. Five precise SQL-and-binding
ledger entries record PostgreSQL's first-use integer inference versus jed's
existing widest-integer binding rule. These differences were independently
probed for SQLSTATE `22003`; they are not Ruby failure exceptions.

Two full Rust compiler attempts exceeded their 512 MiB and 1 GiB guards and
were stopped. The successful harness-only build linked the existing engine
artifact; an offline Cargo check confirmed that artifact was current. Attempts,
commands, artifact hashes, oracle output and the TypeScript guard diagnosis are
retained alongside the successful regression reports. No dependencies were
downloaded and no supported-core implementation changed.

The focused two-table and multiway host suites pass under the interpreter, YJIT
and ZJIT, with peaks of 51.3, 66.8 and 80.8 MiB. All 63 Ruby host test files
pass together under the interpreter (50.5 seconds, 288.9 MiB peak).

| Full corpus lane | Passed records | Failed | Authored skipped files | Peak file-worker RSS |
| --- | ---: | ---: | ---: | ---: |
| Memory, external `query`, ZJIT | 20,288 | 0 | 2 | 149.6 MiB |
| Disk, external `query`, YJIT | 18,271 | 0 | 77 | 169.4 MiB |
| Memory, `execute`, ZJIT | 20,288 | 0 | 2 | 131.3 MiB |
| Disk, `execute`, YJIT | 18,271 | 0 | 77 | 169.3 MiB |

All four lanes cover the 679-file corpus: 77,118 record executions in total.
Each memory lane sends 20,108 SQL calls through its selected API; each disk lane
sends 18,120. Query collection is external throughout. There are zero failures,
timeouts, memory stops, unsupported files, omitted failure details or API
substitutions. The only skips are the corpus's authored storage-mode exclusions.
Immediate compilation remains enabled, including positive text-snapshot and LIKE
kernel counts in every lane. All audits preserve the final source hash and
conformance harness SHA-256
`9935e8bfaf7d1780b050a77f652291d3e2c1c1ac74669ea35e29d1852ddb53ab`.
No failure allowlist or new skip was added.

The before/final binding preflight passes all nine workloads and 32 validation
bindings per workload across all eight source/API lanes, including the actual
Rust Ruby package. All six direct reports (two sources, three runtimes) have the
same complete semantic SHA-256:
`41829776ff2c3f33ff0e4e52d1eb53eda3c89230e92c46578c770161d428da25`.
Each covers 252 workload/API/binding observations and 25 skew/catalog observations
per storage mode, plus exact budget and partial-error checks. The private file
fixture is 180,224 bytes with SHA-256
`7f7e67c3543d67b3e19b2d7d7e83b87122203e9378b97a30183425df8f82b856`.

The three-table fixture retains 31 roots, 15 distinct ASTs and 12 ordered scopes;
the four-table fixture retains 53 roots, 20 ASTs and 24 scopes. The latter reaches
4,077 of the 4,096 key units, so reuse is partial: its warmed diagnostic still
performs 119 base expression compilations. No cache limit was increased to hide
that fallback. The largest invocation-local extension memo has 28 entries,
below its 128-entry cap. Retained root keys and interned AST/scope snapshots are
recursively frozen; resolved Expressions remain available for kernel attachment.

In every final runtime, a 96-statement churn run retains exactly 64 entries.
Each entry holds 31 roots, 1,586 key units and 416 key bytes. After the owning
thread finishes, three GC observations report zero live entries. These are
observed lifetime checks, not a promise about a VM's collection schedule.

The host tests independently compare cached and uncached values, metadata,
errors, partial results, costs and chosen physical plans. Changing index
availability positively changes a physical winner; hot, rare, missing and NULL
bindings agree with the uncached planner at each catalog state. This fixture
does not demonstrate a winner flip from bindings alone. Eight-input coverage
checks admission; the complete execution diagnostics use three and four inputs.
An internal test supplies two different frontier states at the same real prefix
to prove local memo hits while retaining independent estimates and mutable
access hashes. That test does not establish a memo hit in every timed workload.

## Measurements

All 45 admitted target/runtime/API cells have both final process medians below
both baseline medians. Speedup ranges across the five targets and three APIs are
4.0-none: 1.273–2.009×; 4.0-yjit: 1.195–1.698×; 4.1-zjit: 1.109–1.639×. These are paired process observations,
not confidence intervals or a statistical significance test.

The measurements used an Intel Core Ultra 9 285K host. The fresh comparison used
Go 1.27.1, Rust 1.99.0 release builds and Node 26.10.0. All tables report
microseconds per completely consumed query unless a different unit is stated.
`prepared`, `query` and `eager` in the source tables mean `query_prepared`,
ordinary repeated `query`, and ordinary repeated `execute`, respectively.

YJIT prepared source ABBA (µs; mean of two process medians per source)

| Workload | Before | Final | Before/final | Before process range | Final process range |
|---|---|---|---|---|---|
| scalar | 4.816 | 4.955 | 0.972× | 4.752–4.880 | 4.812–5.098 |
| pk_hit | 3.968 | 4.101 | 0.968× | 3.910–4.025 | 3.975–4.227 |
| indexed_join | 365.283 | 369.324 | 0.989× | 365.043–365.522 | 361.712–376.936 |
| three_table_join | 1130.673 | 742.453 | 1.523× | 1130.163–1131.183 | 728.961–755.944 |
| three_table_join_miss | 1097.255 | 704.990 | 1.556× | 1096.713–1097.796 | 692.665–717.314 |
| three_table_join_range | 1825.699 | 1431.284 | 1.276× | 1823.352–1828.046 | 1407.313–1455.254 |
| three_table_computed_key | 1162.530 | 695.557 | 1.671× | 1158.846–1166.213 | 683.867–707.247 |
| four_table_join | 2640.139 | 2205.861 | 1.197× | 2614.330–2665.948 | 2182.787–2228.935 |
| three_table_left_fence | 684.976 | 702.876 | 0.975× | 681.909–688.044 | 695.956–709.797 |

Historical three-table source ABBA

| Runtime | API | Before µs | Final µs | Speedup | Before range | Final range |
|---|---|---|---|---|---|---|
| 4.0-none | prepared | 2058.051 | 1233.586 | 1.668× | 2022.006–2094.095 | 1226.863–1240.308 |
| 4.0-none | query | 2059.719 | 1217.735 | 1.691× | 1986.939–2132.499 | 1197.771–1237.700 |
| 4.0-none | eager | 1959.201 | 1164.196 | 1.683× | 1952.703–1965.700 | 1130.537–1197.854 |
| 4.0-yjit | prepared | 1130.673 | 742.453 | 1.523× | 1130.163–1131.183 | 728.961–755.944 |
| 4.0-yjit | query | 1129.594 | 725.013 | 1.558× | 1097.623–1161.564 | 716.007–734.019 |
| 4.0-yjit | eager | 1124.948 | 735.741 | 1.529× | 1124.251–1125.645 | 714.902–756.580 |
| 4.1-zjit | prepared | 1039.583 | 740.198 | 1.404× | 1021.536–1057.630 | 736.085–744.310 |
| 4.1-zjit | query | 1025.614 | 739.536 | 1.387× | 1018.387–1032.841 | 733.393–745.680 |
| 4.1-zjit | eager | 980.956 | 714.350 | 1.373× | 971.463–990.448 | 684.821–743.879 |

Fresh cross-core characterization (different collector/prepared paths; do not compare ratios to historical runs)

| Workload | Ruby µs | Go µs | Rust µs | TS µs | Ruby/Go | Ruby/Rust | Ruby/TS |
|---|---|---|---|---|---|---|---|
| scalar | 4.963 | 0.834 | 1.417 | 2.340 | 5.947× | 3.502× | 2.120× |
| pk_hit | 4.069 | 1.675 | 2.203 | 4.148 | 2.429× | 1.847× | 0.981× |
| indexed_join | 354.986 | 56.245 | 2.568 | 118.177 | 6.311× | 138.216× | 3.004× |
| three_table_join | 703.758 | 3.307 | 3.240 | 13.231 | 212.815× | 217.206× | 53.190× |
| three_table_join_miss | 674.544 | 1.314 | 1.534 | 3.741 | 513.332× | 439.614× | 180.292× |
| three_table_join_range | 1406.032 | 83.683 | 62.336 | 233.362 | 16.802× | 22.556× | 6.025× |
| three_table_computed_key | 720.593 | 6.297 | 6.207 | 16.573 | 114.441× | 116.084× | 43.479× |
| four_table_join | 2101.201 | 4.027 | 4.075 | 12.294 | 521.765× | 515.600× | 170.909× |
| three_table_left_fence | 677.714 | 3.717 | 3.171 | 11.227 | 182.346× | 213.733× | 60.366× |

Actual Rust Ruby binding comparison (external full drain; native/wrapper elapsed)

| Workload | Wrapper µs | Prepared µs | Query µs | Eager µs | Prepared/wrapper | Query/wrapper | Eager/wrapper |
|---|---|---|---|---|---|---|---|
| scalar | 8.104 | 4.984 | 5.496 | 5.580 | 0.615× | 0.678× | 0.689× |
| pk_hit | 10.584 | 4.109 | 4.666 | 4.073 | 0.388× | 0.441× | 0.385× |
| indexed_join | 37.687 | 376.640 | 383.930 | 356.838 | 9.994× | 10.187× | 9.468× |
| three_table_join | 137.555 | 747.821 | 760.677 | 746.681 | 5.437× | 5.530× | 5.428× |
| three_table_join_miss | 169.858 | 728.854 | 724.019 | 708.329 | 4.291× | 4.263× | 4.170× |
| three_table_join_range | 235.769 | 1464.111 | 1464.551 | 1445.618 | 6.210× | 6.212× | 6.131× |
| three_table_computed_key | 129.445 | 702.318 | 717.421 | 715.943 | 5.426× | 5.542× | 5.531× |
| four_table_join | 419.906 | 2233.808 | 2225.951 | 2238.257 | 5.320× | 5.301× | 5.330× |
| three_table_left_fence | 50.626 | 711.268 | 713.635 | 708.707 | 14.049× | 14.096× | 13.999× |

Every slower control, including overlapping process ranges

| Runtime | API | Workload | Elapsed change | Before process range µs | Final process range µs | Separated slower? |
|---|---|---|---|---|---|---|
| 4.0-none | prepared | scalar | +1.332% | 12.006–12.907 | 12.572–12.673 | no |
| 4.0-none | prepared | pk_hit | +0.433% | 8.163–8.739 | 8.427–8.548 | no |
| 4.0-none | prepared | indexed_join | +1.556% | 614.359–615.683 | 613.028–636.155 | no |
| 4.0-none | prepared | three_table_left_fence | +3.253% | 1245.537–1277.505 | 1299.546–1305.576 | yes |
| 4.0-none | query | scalar | +1.825% | 13.420–14.003 | 13.452–14.472 | no |
| 4.0-none | query | indexed_join | +2.224% | 589.871–618.988 | 598.145–637.597 | no |
| 4.0-none | query | three_table_left_fence | +0.712% | 1270.354–1289.391 | 1264.801–1313.164 | no |
| 4.0-none | eager | pk_hit | +3.013% | 9.327–9.413 | 9.514–9.791 | yes |
| 4.0-none | eager | indexed_join | +1.952% | 561.253–563.502 | 557.620–589.089 | no |
| 4.0-none | eager | three_table_left_fence | +3.265% | 1182.626–1192.242 | 1205.792–1246.606 | yes |
| 4.0-yjit | prepared | scalar | +2.884% | 4.752–4.880 | 4.812–5.098 | no |
| 4.0-yjit | prepared | pk_hit | +3.359% | 3.910–4.025 | 3.975–4.227 | no |
| 4.0-yjit | prepared | indexed_join | +1.106% | 365.043–365.522 | 361.712–376.936 | no |
| 4.0-yjit | prepared | three_table_left_fence | +2.613% | 681.909–688.044 | 695.956–709.797 | yes |
| 4.0-yjit | query | pk_hit | +1.074% | 4.348–4.535 | 4.370–4.609 | no |
| 4.0-yjit | eager | scalar | +0.491% | 5.361–5.434 | 5.423–5.425 | no |
| 4.0-yjit | eager | pk_hit | +0.345% | 3.923–3.929 | 3.937–3.942 | yes |
| 4.0-yjit | eager | three_table_left_fence | +3.456% | 675.506–680.533 | 688.113–714.797 | yes |
| 4.1-zjit | prepared | scalar | +3.937% | 5.314–5.637 | 5.579–5.803 | no |
| 4.1-zjit | prepared | pk_hit | +2.628% | 3.940–4.102 | 4.086–4.168 | no |
| 4.1-zjit | prepared | indexed_join | +3.598% | 341.747–355.631 | 350.811–371.657 | no |
| 4.1-zjit | prepared | three_table_left_fence | +1.319% | 600.661–653.291 | 619.626–650.869 | no |
| 4.1-zjit | query | scalar | +2.161% | 5.679–6.037 | 5.772–6.197 | no |
| 4.1-zjit | query | pk_hit | +4.362% | 4.316–4.493 | 4.427–4.766 | no |
| 4.1-zjit | query | three_table_left_fence | +0.863% | 652.258–678.698 | 668.659–673.781 | no |
| 4.1-zjit | eager | scalar | +3.665% | 5.315–5.507 | 5.462–5.755 | no |
| 4.1-zjit | eager | pk_hit | +0.255% | 3.719–4.007 | 3.774–3.972 | no |
| 4.1-zjit | eager | indexed_join | +0.419% | 333.082–338.782 | 327.605–347.073 | no |
| 4.1-zjit | eager | three_table_left_fence | +3.771% | 607.270–608.004 | 577.364–683.736 | no |

Diagnostic allocations and exact base compiler calls (separate profile runs)

| Workload | Before allocations | Final allocations | Before Engine#compile | Final Engine#compile | Before PreparedJoins#compile | Final PreparedJoins#compile |
|---|---|---|---|---|---|---|
| scalar | 39.000 | 39.000 | 0 | 0 | 0 | 0 |
| pk_hit | 31.000 | 31.000 | 0 | 0 | 0 | 0 |
| indexed_join | 2418.620 | 2419.580 | 0 | 0 | 13.0 | 13.0 |
| three_table_join | 10962.500 | 5045.200 | 171.0 | 0 | 201.0 | 9.0 |
| three_table_join_miss | 10739.500 | 4822.200 | 171.0 | 0 | 201.0 | 9.0 |
| three_table_join_range | 19912.900 | 13995.700 | 171.0 | 0 | 201.0 | 9.0 |
| three_table_computed_key | 11403.300 | 4547.100 | 221.0 | 0 | 230.0 | 9.0 |
| four_table_join | 26492.200 | 16543.750 | 420.0 | 119.0 | 486.0 | 139.0 |
| three_table_left_fence | 6401.900 | 6402.900 | 110.0 | 110.0 | 124.0 | 124.0 |

Remaining final planner/evaluator calls per query (counts, not sampled CPU percentages)

| Method | Three-table hit | Three-table range | Computed key | Four-table |
|---|---|---|---|---|
| JedNative::AccessPlanning#plan_constant | 280.0 | 301.0 | 206.0 | 752.0 |
| JedNative::AccessPlanning#expression_work | 231.0 | 231.0 | 182.0 | 625.0 |
| JedNative::AccessPlanning#estimate_predicate | 162.0 | 162.0 | 110.0 | 437.0 |
| JedNative::IndexScans#conjunctions | 181.0 | 373.0 | 146.0 | 499.0 |
| JedNative::JoinPushdown#pushdown_column_nodes | 141.0 | 141.0 | 203.0 | 414.0 |
| JedNative::NWayJoins#nway_extensions | 9.0 | 9.0 | 9.0 | 28.0 |
| JedNative::IndexNestedLoops#join_index_plan | 9.0 | 9.0 | 9.0 | 28.0 |
| JedNative::Engine#evaluate | 1.0 | 17.0 | 1.0 | 1.0 |
| JedNative::FastEvaluation#evaluate | 28.0 | 460.0 | 99.0 | 39.0 |


There are 29 slower control cells. Six have both final process medians above
both baseline medians: interpreter prepared LEFT, eager PK and eager LEFT;
YJIT prepared LEFT, eager PK and eager LEFT. Their observed increases range
from 0.345% to 3.456%. Every other slower control, including all ZJIT controls,
has overlapping process ranges. All raw samples and slower observations are
retained; the target gains do not establish a uniform speedup.

The fresh cross-core results retain a large multiway gap: the three-table hit
is 212.8× Go and the four-table hit is 521.8× Go. Scalar, PK and indexed
two-table workloads remain within 10× Go. These current ratios are not a
before/after comparison with historical cross-core runs. The controlled Ruby
source quartets above establish the effect of this revision.

Against the actual Rust Ruby package, prepared Ruby is faster for scalar and
PK queries, but slower for every join shape here. Ordinary repeated query and
execute comparisons are also shown so preparation is not hidden in the
comparison. In particular the three-table ordinary query is 5.53× the wrapper,
and the four-table ordinary query is 5.30×. These API measurements include
normal binding, parsing and collection work; they do not isolate FFI overhead.

Profiles use the existing eager `execute_prepared`/`Result#rows` collector,
separately from the externally drained source timings. They run last; their
call counts are not sampled CPU percentages. Three-table allocations fall
54.0%, computed-key allocations 60.1%, and four-table allocations 37.6%.
Base `Engine#compile` calls fall from 171 to zero for the three-table hit,
from 221 to zero for the computed key, and from 420 to 119 for four tables.
Root-cache lookup wrappers still run, as shown separately in the table.

The final three-/four-table profiles still perform 9/28 extension requests and
9/28 inner extension resolutions. Thus these particular measured searches
have no local-memo hits: their gains come from persistent resolved-root reuse,
not the invocation-local memo. The latter's reuse remains covered by the
internal alternative-state test. Parameter reads, estimates, candidate work
and index selection still run on every execution. Four-table metadata-budget
misses also retain substantial resolution work; this is partial reuse, not
complete physical-plan compilation.

## Remaining candidates

The four-table budget fallback gives a smaller next target than physical-plan
caching: store each complete column descriptor once, then represent an ordered
scope as descriptor tokens. This could remove repeated metadata across distinct
scope orders within the same 4,096-unit budget. It must preserve every metadata
field, column multiplicity and order, structural lookup of mutable caller data,
owned frozen snapshots and atomic publication after successful unmetered
resolution. A temporary token vector also adds lookup allocation, so smaller
join controls must be measured. Neither full four-table admission nor an elapsed
time gain is assumed in advance.

Physical-plan reuse is the larger opportunity for the remaining multiway gap.
The supported cores distinguish
parameter value reads from NULL assumptions and validate relation dependencies;
Ruby would need that machinery before retaining a winning plan across bindings.
Only structural templates could be shared, with execution state, bound ranges,
rows and cursor ownership remaining separate. Sensitive plans would continue
fresh selection. This report does not claim a measured gain from either future
change.

## Reproduction and retained evidence

Run from the repository root with the archived baseline present and the final
source hash substituted below. The measurement runner calls the explicit mise
tasks and refuses existing output. Preserve an existing evidence directory
before deliberately rerunning; do not overwrite retained attempts or samples.

```sh
multiway_source_sha='40ceac58a86ce19bba2ed2497adfb26b5ca7eafcc31e39def71312c1a9d56382'
multiway_harness_sha='9935e8bfaf7d1780b050a77f652291d3e2c1c1ac74669ea35e29d1852ddb53ab'
export MISE_STATE_DIR=/tmp/jed-ruby-native-mise-state
export MISE_CACHE_DIR=/tmp/jed-ruby-native-mise-cache
export MISE_TRUSTED_CONFIG_PATHS="$PWD"
export JED_NATIVE_MEMORY_BYTES=536870912
export JED_NATIVE_ADDRESS_SPACE_BYTES=1073741824
unset RUBYOPT RUBY_YJIT_ENABLE RUBY_ZJIT_ENABLE
multiway_results=bench/ruby-native/results/multiway-plan-followup
multiway_tests='require_relative "impl/ruby-native/test/prepared_join_plan_test"; require_relative "impl/ruby-native/test/prepared_multiway_plan_test"'

# First establish exact before/final workload agreement.
ruby bench/ruby-native/results/multiway-plan-followup/run.rb \
  --phase verify --final-source-sha256 "$multiway_source_sha"

# Run the focused and complete host tests under the recorded resource guard.
JED_NATIVE_TIMEOUT=180 JED_NATIVE_RUN_REPORT="$multiway_results/focused-interpreter-bounded.json" \
  mise run ruby-native:bounded -- ruby --disable-yjit --disable-zjit -e "$multiway_tests"
JED_NATIVE_TIMEOUT=180 JED_NATIVE_RUN_REPORT="$multiway_results/focused-yjit-bounded.json" \
  mise run ruby-native:bounded -- ruby --yjit -e "$multiway_tests"
JED_NATIVE_TIMEOUT=180 JED_NATIVE_RUN_REPORT="$multiway_results/focused-zjit-bounded.json" \
  mise run ruby-native:bounded -- .dev/ruby-native-runtimes/ruby-4.1.0-preview1/bin/ruby --zjit -e "$multiway_tests"
JED_NATIVE_TIMEOUT=180 JED_NATIVE_RUN_REPORT="$multiway_results/host-tests-bounded.json" \
  mise run ruby-native:test

# Each call runs memory then disk; run them sequentially.
ruby bench/ruby-native/results/multiway-plan-followup/run_audits.rb \
  --source-sha256 "$multiway_source_sha" \
  --harness-sha256 "$multiway_harness_sha" --api query
ruby bench/ruby-native/results/multiway-plan-followup/run_audits.rb \
  --source-sha256 "$multiway_source_sha" \
  --harness-sha256 "$multiway_harness_sha" --api execute

# Freeze measured source/plan/runtime hashes before formal timing.
ruby bench/ruby-native/results/multiway-plan-followup/pin_sources.rb
ruby bench/ruby-native/results/multiway-plan-followup/run.rb \
  --phase diagnostics --final-source-sha256 "$multiway_source_sha"
ruby bench/ruby-native/results/multiway-plan-followup/run.rb \
  --phase controls --final-source-sha256 "$multiway_source_sha"
ruby bench/ruby-native/results/multiway-plan-followup/run.rb \
  --phase cross --final-source-sha256 "$multiway_source_sha"
ruby bench/ruby-native/results/multiway-plan-followup/run.rb \
  --phase binding --final-source-sha256 "$multiway_source_sha"
ruby bench/ruby-native/results/multiway-plan-followup/run.rb \
  --phase profiles --final-source-sha256 "$multiway_source_sha"

# Derive and verify only after every workload/profile worker has exited.
ruby bench/ruby-native/results/multiway-plan-followup/summarize.rb
```

Do not place the orchestration driver under an inherited address-space limit
below 16 GiB: Go/Rust comparison workers cannot raise a lower hard limit. Ruby
workers still receive their separate 2 GiB limit. The final verifier checks
source/runtime/harness pins, exact outcomes, schedules, resource guards and
serialized command journals before producing the summary and manifest. JSON
compaction preserves complete parsed values and records both file and semantic
hashes; it does not discard raw samples or validation data.

The final verifier accepts all 24 serialized measurement/diagnostic commands,
the four completed corpus journals and the shared regression/oracle evidence.
See the [summary](../../bench/ruby-native/results/multiway-plan-followup/summary.json),
[source provenance](../../bench/ruby-native/results/multiway-plan-followup/source.json),
[serialization ledger](../../bench/ruby-native/results/multiway-plan-followup/serialization.json)
and [manifest](../../bench/ruby-native/results/multiway-plan-followup/manifest.json).
The manifest covers 123 files totaling 19,450,704 bytes, excluding the manifest
itself; 103 JSON files have lossless serialization records. Manifest SHA-256:
`d3b410e1322959e21f4f410ff4ddb3844be28a316b0e0ddc3b1702b51e3100ce`.

The final verifier also requires the preserved preflight, cast, supported-core
and oracle investigation artifacts. The timing commands alone do not recreate
that history. The explicit `run_regressions.rb` can rerun the 12 supported-core
cases with existing harness binaries; its driver needs at least 64 GiB virtual
address space for the Node disk worker, while retaining the 512 MiB RSS guard.
Do not erase earlier attempts when reproducing a successful check.
