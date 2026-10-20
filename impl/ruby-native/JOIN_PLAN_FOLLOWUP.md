# Prepared join resolution follow-up

This explicitly requested optimization follows the [performance checkpoint](PERFORMANCE_CHECKPOINT.md).
The Ruby core remains experimental, frozen and opt-in. No supported core, default
task, SQL behavior or public API changes in this slice.

## Change

The checkpoint's indexed join returned at most one row but resolved expressions
repeatedly during pushdown, physical planning and execution. The new
[`prepared_joins.rb`](lib/jed_native/prepared_joins.rb) retains complete resolved
expression roots and table-column scopes alongside a prepared statement. Repeated
ordinary SQL within a persistent session benefits through the existing
parsed-statement cache too. Database convenience calls create fresh sessions and
resolve joins anew.

Access inventories, cardinality estimates, parameter bounds and the winning
physical plan are computed for each execution. File spill preflight and
materialization can reuse the same physical plan within that one SELECT invocation.
No physical plan is retained for the next parameter binding. Pushdown rewriting
also runs each time; some unsupported expressions have parameter-sensitive safety
checks, so persistently caching arbitrary rewrites would be incorrect.

Admission covers bounded two-base-table INNER, CROSS and LEFT joins over plain
integer, boolean and text columns, with simple scalar roots. Calls, subqueries,
nominal/open types, decimal/float operations, LIKE, grouping, windows, DISTINCT,
EXPLAIN and more complicated joins retain their previous path. This is an
optimization boundary, not a SQL coverage restriction.

Only successfully resolved, unmetered complete roots are retained. Value and
filter roots use separate keys because their AND short-circuit rules differ.
Contextual parameter casts replay in their original resolution order and at the
original compile call, against the current binding. Child expressions are not
independently shared across parents that could mutate their contextual types.
Existing scalar compilation can attach its kernels to these stable resolved roots;
this change does not add a new emitter or evaluate SQL text as Ruby.

Each session/cursor cache holds at most 64 entries; each entry admits at most 128
roots, 4,096 copied key units and 64 KiB of copied key strings. Original syntax is
limited to 192 nodes, depth 32 and 32 KiB of node strings. Table-column metadata is
limited to 64 columns across both inputs; copied scope names are bounded as well.
Context changes or table descriptor replacement invalidate reuse conservatively.
Publication prunes stale entries. Cursors share the bounded cache but keep their
resolution frames and per-execution plans private, preserving suspended snapshots.
These bounds limit this cache; they are not a whole-engine heap guarantee.

## Evidence

Baseline: commit `ef79bb7442588ec87bb67fd454c3f0f2376806dc`, library SHA-256
`e430431f7e9264c3cc3b9a75436c46841004a707f9c9ec0b72744beeb4c27f88`.
Final library SHA-256:
`730a4a03496f62feec03acb2355e9c5f2fc9dda69fa055e7d9595f7457bb5edf`.

Results and runnable evidence are retained in
[`join-plan-followup`](../../bench/ruby-native/results/join-plan-followup/).
The explicit [nine-workload plan](../../bench/ruby-native/compare/join-plan.json)
uses the unchanged 1,000-row fixture and historical indexed-join SQL/parameter
schedule, plus hit/miss, range, computed-key, LEFT, bucket and three-table fallback
shapes. Scalar and primary-key queries are controls. Every timed query consumes
its complete result.

Interpreter and YJIT use Ruby 4.0.7; ZJIT uses Ruby 4.1.0-preview1. Source comparisons
use serial before/final/final/before processes, five samples per process, and
report the mean of the two process medians for each source. These observations
are not confidence intervals. Comparing JIT modes also changes Ruby version for
ZJIT. The library never enables a JIT itself.

The native API lanes are `query_prepared`, ordinary repeated `query`, and ordinary
repeated `execute`. The separate cross-core adapters measure prepared execution
with their existing result collectors. The actual Ruby Rust binding comparison
uses its public ordinary query API, which has no prepared-statement API; it
includes parsing, FFI conversion and result handling. These are whole-query
comparisons, not isolated language or call-boundary costs.

The focused host test retains prepared statements while changing parameter
values, catalog publications and column types, pending writes, savepoints, statistics,
indexes and session context. It checks all four execution APIs in memory and
file storage, compares exact budget/error prefixes with this cache disabled,
interleaves suspended cursors, and tests eviction and object reachability.
Untimed diagnostics additionally compare archived and final sources, including
actual physical winners and externally drained cursor values, metadata and costs.
The shared conformance harness is unchanged.

## Correctness

The final frozen library passed all four full-corpus lanes and all 62 host test
files, including the new [`prepared_join_plan_test.rb`](test/prepared_join_plan_test.rb).

| Storage/API | Runtime | Passing records | Failures |
| --- | --- | ---: | ---: |
| Memory query, external drain | 4.1.0-preview1 ZJIT | 20,267 | 0 |
| Memory execute | 4.1.0-preview1 ZJIT | 20,267 | 0 |
| File query, external drain | 4.0.7 YJIT | 18,250 | 0 |
| File execute | 4.0.7 YJIT | 18,250 | 0 |

Each lane accounts for all 679 corpus files. Only corpus-authored storage-mode
exclusions apply: two files in memory and 77 in file mode. There are no failure
allowlists, mixed-API routing, unsupported files, timeouts or memory-limit failures.
The focused test also passed in the 4.0.7 interpreter, 4.0.7 YJIT and preview ZJIT.
The all-host run used the interpreter.

All six archived/final direct diagnostic reports also agree on their complete
semantic fingerprint and file bytes. They exercise every join target's cache in
both storage modes, retain the ordinary paths for the three controls, and match
uncached physical-plan choices. Across all three runtime modes, a 96-statement
churn probe reaches the 64-entry limit and observes zero surviving entries after
the owning thread finishes and GC runs. This is a recorded reachability result
for these runtimes, not a promise about when another Ruby VM will collect objects.

Every Ruby workload worker is guarded at 512 MiB sampled RSS and 2 GiB address
space. Corpus files have a ten-second deadline inside a 900-second outer audit.
The largest corpus worker used 172.04 MiB; the all-host suite peaked at 184.46 MiB.
Tests completed before formal timing began. Journals independently establish the
corpus/diagnostic/timing order; the bounded host report has no wall-clock fields.

## Measurements

Prepared YJIT source comparison, microseconds per complete query:

| Shape | Before | Final | Before/final |
| --- | ---: | ---: | ---: |
| Scalar control | 4.77 | 4.94 | 0.97× |
| PK control | 3.89 | 3.97 | 0.98× |
| Indexed join | 625.38 | 365.18 | 1.71× |
| Indexed join miss | 605.36 | 347.17 | 1.74× |
| Indexed join range | 4440.18 | 4136.59 | 1.07× |
| Computed-key join | 926.72 | 630.74 | 1.47× |
| LEFT join residual | 267.08 | 174.92 | 1.53× |
| Bucket join | 920.09 | 686.21 | 1.34× |
| Three-table fallback control | 1100.17 | 1134.42 | 0.97× |

The historical indexed join across runtimes and public API lanes:

| Runtime | API | Before µs | Final µs | Speedup |
| --- | --- | ---: | ---: | ---: |
| 4.0.7 interpreter | query_prepared | 1205.34 | 607.35 | 1.98× |
| 4.0.7 interpreter | query | 1180.84 | 615.40 | 1.92× |
| 4.0.7 interpreter | execute | 1146.93 | 566.18 | 2.03× |
| 4.0.7 YJIT | query_prepared | 625.38 | 365.18 | 1.71× |
| 4.0.7 YJIT | query | 653.78 | 369.91 | 1.77× |
| 4.0.7 YJIT | execute | 632.66 | 343.34 | 1.84× |
| 4.1-preview ZJIT | query_prepared | 567.33 | 350.09 | 1.62× |
| 4.1-preview ZJIT | query | 594.87 | 367.69 | 1.62× |
| 4.1-preview ZJIT | execute | 550.62 | 345.60 | 1.59× |

Across the six join targets and three APIs, interpreter gains are 1.05–2.10×,
YJIT gains 1.06–1.85×, and ZJIT gains 1.03–1.69×. Both final process medians are
below both baseline medians in 52 of the 54 target/runtime/API cells. The two
overlapping cells are YJIT range/query and ZJIT range/execute. “Separated” here
describes these four observations; it is not a statistical significance test.

Controls are mixed and must remain part of the tradeoff. Across all 27 control
cells, elapsed change ranges from −3.07% to +4.50%. Separated slowdowns are:
interpreter scalar/query +3.27% and three-table/query +3.49%; YJIT scalar/prepared
+3.60%, PK/prepared +2.01% and three-table/prepared +3.11%; ZJIT scalar/prepared
+4.50%, PK/prepared +3.15% and scalar/execute +2.38%. The new resolution wrappers
add dispatch checks to ordinary resolution too. The measurements establish the
tradeoff, without isolating how much each check contributes. All raw medians,
including overlapping and faster control observations, remain in the summary.

## Fresh supported-core comparison

These are separate fresh-process measurements, with five samples per core.
Times are microseconds per complete query. Values and row counts agree across
all four adapters; exact costs and column metadata are checked separately through
the actual Ruby Rust binding.

| Shape | Ruby YJIT | Go | Rust | TypeScript | Ruby/Go |
| --- | ---: | ---: | ---: | ---: | ---: |
| Scalar control | 4.91 | 0.99 | 1.41 | 2.34 | 4.99× |
| PK control | 4.03 | 1.67 | 2.15 | 4.31 | 2.41× |
| Indexed join | 364.23 | 52.48 | 2.44 | 126.12 | 6.94× |
| Indexed join miss | 347.48 | 48.65 | 1.43 | 75.68 | 7.14× |
| Indexed join range | 4043.44 | 504.97 | 331.53 | 1306.39 | 8.01× |
| Computed-key join | 623.24 | 327.68 | 323.55 | 829.39 | 1.90× |
| LEFT join residual | 174.76 | 2.23 | 2.23 | 6.83 | 78.47× |
| Bucket join | 691.32 | 286.59 | 324.39 | 736.66 | 2.41× |
| Three-table fallback control | 1118.37 | 6.19 | 3.05 | 11.97 | 180.75× |

The original indexed join is now 6.94× Go, compared with 12.50× in the preceding
checkpoint. Seven of these nine shapes are within 10× Go. The new LEFT and
three-table probes expose much larger remaining gaps: 78.47× and 180.75× Go.
Rust's prepared indexed join remains 149.24× faster than Ruby here; TypeScript
is 2.89× faster. This slice improves Ruby's setup work but does not close those
execution/setup differences across all engines or query shapes.

## Actual Ruby Rust binding

A separate fresh YJIT run compares the public Ruby APIs. The native column below
is ordinary repeated `query` in a persistent session, so both sides include
their normal public query setup and external result drain.

| Shape | Rust binding µs | Native query µs | Native/binding |
| --- | ---: | ---: | ---: |
| Scalar control | 8.08 | 5.31 | 0.66× |
| PK control | 10.57 | 4.44 | 0.42× |
| Indexed join | 39.30 | 359.01 | 9.14× |
| Indexed join miss | 42.32 | 338.20 | 7.99× |
| Indexed join range | 660.04 | 3969.79 | 6.01× |
| Computed-key join | 398.99 | 622.22 | 1.56× |
| LEFT join residual | 15.36 | 170.11 | 11.08× |
| Bucket join | 479.41 | 676.70 | 1.41× |
| Three-table fallback control | 157.71 | 1124.44 | 7.13× |

Native prepared and eager indexed joins measured 374.62 and 365.59 µs in this
same comparison, respectively, versus 39.30 µs through the Rust binding.
Scalar and PK queries continue to favor native Ruby; the binding wins all seven
join probes. Host-function behavior was not changed or rebenchmarked in this
slice; the [checkpoint](PERFORMANCE_CHECKPOINT.md) retains that comparison.

## Why it helps, and the next targets

The separate prepared-eager diagnostic records indexed-join allocations falling
from 6,051.70 to 2,418.58 per query, a 60.03% reduction. Warm `Engine#compile`
calls fall from 114 to zero, and ordinary `Engine#compile_filter` calls from 22
to one. The new tier performs 34 complete-root lookups. Layered compile-wrapper
counts are not independent compilations and must not be summed together.

Parameter-sensitive work remains: 63 `estimate_predicate`, six
`scan_access_candidates`, four `join_index_plan` and one `two_base_join_plan`
call per query. Both PK scans also remain. This confirms that the gain comes
from resolution reuse, rather than dropping scans or freezing a physical winner.
The file path's additional per-invocation planning reuse has correctness and
admission evidence here; this slice does not publish a file timing comparison.

The three-table fallback still executes 171 base compile calls and 201 calls to
many outer compile wrappers. Extending bounded resolution/structure reuse to
multiway joins is therefore a concrete next target, with a large remaining
whole-query gap. Within two-table planning, repeated index-candidate inventory
construction and structural validation remain candidates for reuse within one
execution. LEFT joins also need further setup investigation. Any extension must
retain binding-sensitive selection and exact cost/error ordering.

## Reproduction and retained evidence

The [summary](../../bench/ruby-native/results/join-plan-followup/summary.json)
contains all API/runtime cells, raw per-process medians, exact verification
signatures, allocation counts and diagnostic observations. The
[manifest](../../bench/ruby-native/results/join-plan-followup/manifest.json)
covers 66 evidence files. Its SHA-256 is
`98f6a51529579776ff6dbc269386909dc978b9e96836ea9f74ecaac83e6edb00`.
The serialization ledger verifies lossless compaction of 60 JSON reports;
no samples, failures or semantic fields were removed or rounded.

The explicit runner is `bench/ruby-native/results/join-plan-followup/run.rb`.
Select phases `verify`, `diagnostics`, `controls`, `cross`, `binding`, then
`profiles`, with `--final-source-sha256` set to the library hash above. It invokes
the opt-in mise tasks, verifies the archived baseline at
`/tmp/jed-join-plan-before/impl/ruby-native`, and refuses to overwrite existing
reports unless explicitly requested. The baseline can be restored from the
recorded Git commit. Runtime paths and all exact commands are retained in the
phase journals; separate runtime installation is documented in
[RUNTIMES.md](../../bench/ruby-native/RUNTIMES.md).

An outer orchestration guard needs at least 16 GiB address space for Go/Rust
comparison workers; each Ruby workload retains its own 2 GiB address-space and
512 MiB RSS bounds. All 24 official commands succeeded and ran serially, with
profiles last. The largest binding-comparison worker used 88.11 MiB and the
largest direct diagnostic worker 88.22 MiB. A separate initial baseline
diagnostic smoke passed before the recorded sequence; it provided no timing
samples and is not used in these comparisons.
