# Ruby compilation performance checkpoint

This checkpoint follows method sharing and broader literal LIKE compilation.
The core remains an explicitly selected, frozen experiment. This report does
not promote it or add it to normal tests, benchmarks, CI or feature maintenance.

## Scope and provenance

The before source is commit `fc12f42798420fabc91dd6ee1274b961647071f8`, library
SHA-256 `3e749562c6b764681d6917604cf02aa73b3194cfdbb8c5e756616588de91dc88`.
The final library SHA-256 is
`e430431f7e9264c3cc3b9a75436c46841004a707f9c9ec0b72744beeb4c27f88`.
It is committed in `e914ae1127533745f2c28c1cafbeb293ba14ed79`.
This measures the last two changes together; earlier reports retain the history
of the larger port and compilation experiment.
Measurements run on Linux x86-64 with an Intel Core Ultra 9 285K on
2026-10-20 UTC. This remains shared-machine microbenchmark evidence.

Method sharing and broader literal LIKE compilation are worthwhile, bounded
extensions. The broad checkpoint confirms substantial LIKE gains, but other
steady-state queries remain mixed. Nineteen of 21 prepared memory shapes and
all three canonical file operations fall within 10× of Go in these runs.
The indexed join and mixed LIKE fallback remain clear exceptions.

There is still useful work, but the next priority should be prepared join
planning reuse, followed by scalar kernels and comparisons inside file blocking
operators. Expanding generated row loops alone will not address the repeated
planning seen in the join profile. After those bounded targets, a support-readiness
effort would be more valuable than an open-ended push to compile every query
shape. Promotion remains a separate decision with the gaps listed below.

## Measurement boundaries

The 21-query memory checkpoint uses the existing 1,000-row fixture. It covers
scalar expressions, PK hits/misses, filters, aggregate/group/HAVING expressions,
sorts, arithmetic and CASE projection, many groups, DISTINCT aggregates, mixed
scalar types, an indexed join with a residual predicate, the six new LIKE
targets and both simple-prefix and mixed-pattern controls. The arithmetic
projection retains its earlier SQL and schedule so the previously observed
slowdown remains visible.
The [method-sharing report](METHOD_SHARING_FOLLOWUP.md) retains the isolated
YJIT direct-query arithmetic slowdown of +10.86%, with separated process
medians. A different result in this larger workload mix would not erase that
observation or establish its cause.

The seven host queries preserve scalar, buffered/streamed row and batch
callbacks and no-callback controls. Validation counters are outside timing.
The Ruby comparison fully drains prepared/query cursors externally and
iterates eager results. The actual Rust binding uses its available query API,
which parses each call and marshals eager Rust results into Ruby rows. It has
no prepared/lazy API; this comparison includes those product/API differences.
Native repeated SQL in a persistent session reuses its bounded caches.

Buffered native batch callbacks use chunks of 64 rows; the Rust binding batches
all 500 matching rows in this fixture. Both use chunks of 64 for the 1,000-row
streamed case. Exact output, cost and total callback-row checks preserve those
existing policies. Callback increments include surrounding engine work and
cannot isolate FFI overhead.

The independent Go/Rust/TypeScript memory comparison uses the existing prepared
adapters and their result-handling paths, including Ruby's `Rows#rows`. It checks
rendered-value checksums and row counts. Exact metadata and cost comparisons
come from the separate Ruby/Rust-binding series and shared conformance, not
from those checksum-only adapters. The two latency series must not be combined
as if their result-handling paths were identical.

Canonical file tests measure warm PK access, cold database opens and durable
insert commits on private disk-backed fixtures with exclusive locking. A
separate Ruby-binding file comparison uses the existing million-row fixture
and auto locking. A cold database open does not imply a cold OS page cache.

The blocking checkpoint uses a private 4,096-row file and two 1,024-row join
tables. Four queries cover computed grouping/HAVING/order, full sort, DISTINCT,
and a hash join with a residual predicate. Four APIs run at both 8 MiB and
64 KiB `work_mem`, with 40 warmup calls and five batches of three measured calls.
`max_query_memory_bytes` is unlimited to isolate `work_mem`. All timing finishes
before TracePoint probes verify actual scratch reads/writes under the lower
budget, no spill under the higher budget, and hash-join input consumption.
Both budgets must return identical values, metadata and costs.
Scratch-byte totals vary between processes; they establish actual spill I/O,
not a deterministic temporary-byte-volume contract.

YJIT before/final controls use source ABBA order; the reported statistic is the
mean of two process medians per source. Memory, host and blocking process medians
come from five batched samples. Canonical file tests instead retain their
per-operation p50 statistic; their ABBA comparison averages two process p50s.
Final interpreter and preview ZJIT runs are separate runtime characterizations,
not before/after estimates. GC remains
enabled. Workers are serialized; this task runs no tests or builds concurrently
with measurements. Ruby engine workers retain 512 MiB RSS and 2 GiB address-space
guards; cross-core orchestration permits the larger address-space reservations
needed by the other runtimes. Recorded bounds and RSS accompany every worker.
The largest successful native Ruby worker peaks at 325.85 MiB sampled RSS;
all measured workers stay below the 512 MiB limit. This is measured process
usage under the guard, not a whole-engine heap guarantee.

These are single-caller measurements, not concurrent-throughput comparisons.
The Rust binding can release the GVL during native engine work; pure Ruby engine
execution remains Ruby VM work. Favorable single-query timings do not settle
that separate product tradeoff.

## Before and after these two changes

The broad YJIT memory repeats improve the six new LIKE targets by 2.48–6.29×
across the three APIs. Other query shapes have mixed changes. In particular,
the arithmetic projection retains a separated slowdown in both cursor APIs:

| Arithmetic projection, µs/query | Before | Final | Elapsed change |
| --- | ---: | ---: | ---: |
| Prepared cursor | 791.94 | 816.57 | +3.11% |
| Ordinary query cursor | 782.10 | 800.97 | +2.41% |
| Eager execution | 792.81 | 771.71 | −2.66% |

Other separated memory slowdowns include prepared CASE projection (+2.92%)
and ordinary scalar (+3.78%), filtered scan (+2.53%), many-groups (+2.18%) and
mixed scalar (+1.83%). "Separated" means that both final process medians are
on the same side of both before medians; two repeats are not a confidence
interval. In the mixed-scalar case the nearest medians differ by only 0.34 ns.
The full paired values and raw samples remain in the evidence.

The host-control changes range from −7.56% to +13.27%. Separated slowdowns
include prepared scalar (+3.76%), buffered no-callback (+2.37%), buffered row
callbacks (+2.20%), and streamed no-callback (+2.27%); ordinary streamed row
callbacks (+4.04%); and eager buffered no-callback (+8.11%), streamed no-callback
(+13.27%), streamed row callbacks (+7.20%) and streamed batch callbacks (+3.98%).
The largest shift has substantial between-process spread: eager streamed
no-callback final medians are 668.94 and 797.80 µs, versus 653.86 and 641.10 µs
before. Ordinary streamed no-callback improves 7.56%. These observations do not
establish a universal steady-query benefit from method sharing.

Canonical file results use means of two process p50s:

| File operation, µs/op | Before | Final | Elapsed change |
| --- | ---: | ---: | ---: |
| Warm million-row PK lookup | 15.12 | 14.32 | −5.25% |
| Fresh small-file open/query | 3,708.06 | 3,626.88 | −2.19% |
| Durable single-row insert/commit | 4,957.20 | 4,945.10 | −0.24% |

The first two have separated process p50s. Durable-commit observations overlap;
the final p50s are 4.58 and 5.31 ms. Neither the source changes nor these
measurements establish a durable-write improvement.

The file blocking quartet is similarly mixed: its 32 API/budget cells range
from −5.05% to +3.07% elapsed. Prepared cursor results below are milliseconds
per complete query; these are Ruby-only before/final comparisons.

| File query | `work_mem` | Before, ms | Final, ms |
| --- | ---: | ---: | ---: |
| Computed grouping, HAVING and ordering | 8 MiB | 39.44 | 38.96 |
| Computed full sort | 8 MiB | 24.66 | 25.26 |
| DISTINCT and sort | 8 MiB | 36.98 | 36.33 |
| Hash join with residual predicate | 8 MiB | 33.13 | 34.14 |
| Computed grouping, HAVING and ordering | 64 KiB | 91.33 | 89.86 |
| Computed full sort | 64 KiB | 78.50 | 78.68 |
| DISTINCT and sort | 64 KiB | 144.18 | 143.69 |
| Hash join with residual predicate | 64 KiB | 61.08 | 62.84 |

Three blocking slowdowns have separated process medians: prepared full sort
at 8 MiB (+2.43%), prepared hash join at 64 KiB (+2.89%), and eager full sort
at 64 KiB (+1.74%). Most spilling cells overlap. There is no broad blocking
speedup from these two changes. Every low-budget cell writes and reads scratch
files, every high-budget cell avoids spill, and both budgets preserve exact
results and costs. The cross-core file series below covers PK/open/commit;
this checkpoint does not add cross-core spill latency measurements.

## Fresh supported-core comparisons

These memory numbers use each core's existing prepared adapter and complete
result collector, as described above. They are fresh single-process medians
of five samples, in µs per query.

| Query | Ruby YJIT | Go | Rust | TypeScript | Ruby / Go |
| --- | ---: | ---: | ---: | ---: | ---: |
| Scalar | 4.99 | 0.79 | 1.41 | 2.05 | 6.29× |
| PK hit | 4.11 | 1.60 | 2.22 | 3.60 | 2.57× |
| PK miss | 3.44 | 1.29 | 1.35 | 2.31 | 2.66× |
| Filtered scan | 261.11 | 166.84 | 111.04 | 286.12 | 1.57× |
| Aggregate | 272.92 | 65.06 | 53.20 | 161.39 | 4.19× |
| Grouped HAVING | 790.01 | 284.44 | 210.86 | 526.03 | 2.78× |
| Sort/LIMIT | 387.37 | 215.66 | 135.98 | 296.37 | 1.80× |
| Arithmetic projection | 835.25 | 270.93 | 223.95 | 565.23 | 3.08× |
| CASE projection | 832.85 | 241.33 | 191.73 | 505.96 | 3.45× |
| 100 groups | 1,667.26 | 410.00 | 265.19 | 539.22 | 4.07× |
| DISTINCT aggregate | 695.59 | 270.88 | 177.96 | 461.60 | 2.57× |
| Mixed scalar | 16.10 | 2.61 | 2.07 | 5.70 | 6.17× |
| Indexed join | 681.81 | 54.54 | 2.59 | 119.71 | 12.50× |
| Underscore-prefix LIKE | 791.09 | 148.32 | 142.04 | 274.32 | 5.33× |
| Fixed-width LIKE | 861.18 | 145.44 | 137.21 | 227.48 | 5.92× |
| Interior `%` LIKE | 390.12 | 170.48 | 148.71 | 292.09 | 2.29× |
| Interior `%` NOT LIKE | 437.62 | 138.60 | 139.86 | 241.97 | 3.16× |
| Wildcard projection | 798.71 | 406.57 | 331.34 | 734.90 | 1.96× |
| Wildcard grouping | 840.63 | 483.27 | 332.47 | 624.47 | 1.74× |
| Simple-prefix LIKE control | 318.40 | 128.69 | 141.76 | 270.89 | 2.47× |
| Mixed LIKE fallback | 2,289.40 | 164.89 | 147.37 | 286.03 | 13.88× |

Nineteen of these 21 shapes are within 10× of Go, spanning about 1.57–6.29×.
The exceptions are the indexed join (12.50×) and mixed LIKE fallback (13.88×).
The prepared Rust adapter is particularly strong on that indexed join: 2.59 µs
versus Ruby's 681.81 µs, about 263.21×. The actual Ruby Rust binding includes
parsing and marshaling and measures that query differently, as shown below.
These are workload observations, not a whole-engine speed ratio.

The separate canonical file comparison keeps exclusive locking and the original
per-operation schedules. Values below are p50 µs per operation.

| File operation | Ruby YJIT | Go | Rust | TypeScript |
| --- | ---: | ---: | ---: | ---: |
| Million-row PK lookup | 14.36 | 2.50 | 2.31 | 6.53 |
| Fresh small-file open/query | 3,592.80 | 469.27 | 401.68 | 1,117.81 |
| Durable insert/commit | 3,748.16 | 1,494.61 | 1,444.63 | 1,645.79 |

Ruby is 5.73× Go for the million-row PK lookup, 7.66× for opening the small file,
and 2.51× for durable insertion in this sample. Durability timings vary with
the filesystem and run; the earlier paired Ruby controls remain the source
comparison, rather than these independently timed core observations.

The initial Rust adapter compilation exceeded the query-sized 512 MiB limit,
before any Rust query timing. Its preparation failure and the other three
successful lanes are retained. The unchanged Rust adapter was then prebuilt
offline with a separate 1 GiB build limit and measured in a Rust-only retry
under the original 512 MiB worker limit. No core timing was discarded.

## Runtime observations

The interpreter and YJIT use Ruby 4.0.7; ZJIT uses 4.1.0-preview1. YJIT below
is the mean of the two final control medians, while the other runtimes have
one process each. These observations include runtime-version differences;
they do not isolate the JIT implementation alone.

| Ordinary memory query, µs | Interpreter | YJIT | ZJIT |
| --- | ---: | ---: | ---: |
| Scalar | 14.09 | 5.49 | 6.11 |
| PK hit | 9.99 | 4.60 | 4.34 |
| Arithmetic projection | 2,097.48 | 800.97 | 961.76 |
| 100 groups | 2,893.55 | 1,594.82 | 1,680.89 |
| Indexed join | 1,238.79 | 668.02 | 583.50 |
| Underscore-prefix LIKE | 1,009.13 | 780.75 | 762.05 |
| Interior `%` LIKE | 763.95 | 383.56 | 374.83 |

| Canonical file operation, p50 µs | Interpreter | YJIT | ZJIT |
| --- | ---: | ---: | ---: |
| Million-row PK lookup | 32.53 | 14.32 | 16.46 |
| Fresh small-file open/query | 26,200.30 | 3,626.88 | 7,017.18 |
| Durable insert/commit | 6,552.58 | 4,945.10 | 5,803.43 |

Both JITs make the pure Ruby engine substantially more viable. YJIT leads on
most prepared/query memory shapes and on opening files. ZJIT is faster on
2 of 21 prepared, 7 of 21 ordinary-query and 14 of 21 eager memory shapes in
these observations; the preferred runtime varies by API. Every interpreter
and ZJIT blocking/spill case also passed. The raw evidence retains all runtime,
API and budget cells rather than selecting one runtime per query.

## Actual Ruby Rust binding

These fresh comparisons use Ruby 4.0.7 YJIT for both packages, with the API
and drain differences described above. Values are µs per complete query.

| Memory query | Rust binding | Ruby prepared | Ruby query | Ruby eager |
| --- | ---: | ---: | ---: | ---: |
| Scalar | 8.09 | 4.97 | 5.75 | 5.60 |
| PK hit | 10.53 | 4.16 | 4.70 | 4.18 |
| Filtered scan | 138.69 | 260.05 | 259.43 | 366.44 |
| Aggregate | 63.70 | 271.48 | 274.69 | 252.49 |
| Arithmetic projection | 724.14 | 823.48 | 828.04 | 862.93 |
| CASE projection | 815.57 | 814.38 | 825.25 | 691.04 |
| 100 groups | 403.24 | 1,610.50 | 1,652.81 | 1,291.63 |
| DISTINCT aggregate | 218.82 | 690.99 | 694.11 | 646.59 |
| Mixed scalar | 16.20 | 16.16 | 17.00 | 18.23 |
| Indexed join | 36.84 | 681.75 | 687.30 | 657.40 |
| Interior `%` LIKE | 178.37 | 386.80 | 396.33 | 353.00 |
| Wildcard projection | 937.00 | 768.99 | 780.13 | 684.74 |
| Mixed LIKE fallback | 176.19 | 2,218.07 | 2,169.79 | 2,173.83 |

For the ordinary query API, the Ruby core is faster on scalar, PK hit/miss and
wildcard projection in this sample. Rust retains substantial advantages on
aggregation and the indexed join. The latter is about 18.66× slower in Ruby;
the uncompiled mixed LIKE fallback is about 12.32× slower. A small result set
therefore does not guarantee a cheap Ruby query.

| Host query | Rust binding | Ruby prepared | Ruby query | Ruby eager |
| --- | ---: | ---: | ---: | ---: |
| Scalar callback | 8.41 | 4.94 | 5.17 | 27.32 |
| Buffered, no callback | 476.27 | 612.13 | 595.34 | 539.95 |
| Buffered, row callbacks | 779.21 | 739.84 | 715.55 | 659.94 |
| Buffered, batch callbacks | 534.64 | 878.48 | 845.93 | 830.28 |
| Streamed, no callback | 744.53 | 791.17 | 760.26 | 675.90 |
| Streamed, row callbacks | 1,358.67 | 1,228.46 | 1,081.07 | 1,088.25 |
| Streamed, batch callbacks | 854.48 | 1,475.13 | 1,365.28 | 1,292.61 |

Native cursor scalar callbacks and row callbacks remain competitive, but
batching and eager scalar callbacks currently favor the binding. Relative to
each implementation's no-callback control,
ordinary Ruby row callbacks add about 120.21 µs buffered and 320.81 µs streamed,
versus 302.94 and 614.14 µs for the binding. These are differences between whole
query medians, including callback result handling and engine work. They do
not measure an isolated call boundary. Buffered batching also uses different
chunk sizes, as recorded above; streamed batching uses the same 64-row policy.

The actual file API comparison, with auto locking, measures million-row PK
lookups at 18.16 µs through the Rust binding, 19.74 µs native prepared,
20.70 µs native query and 18.41 µs native eager. Ordinary native query is
1.14× the binding's elapsed time here. These are batch medians under a different
locking/API setup from the canonical exclusive-locking file p50 series.

The first file-binding attempt failed before query timing because the sandbox
denied the page-service Unix socket. Its report and phase journal remain in
the evidence. A separate, file-only retry with the needed local permissions
passed all four APIs; memory/host measurements were not repeated or replaced.

## Diagnostic profile and remaining candidates

The final diagnostic uses prepared eager execution on the same memory fixture.
Its allocation samples precede TracePoint and ObjectSpace tracing, and its
latencies are separate from the primary comparison series. Selected allocation
counts per complete query are:

| Query | Allocated objects/query |
| --- | ---: |
| Scalar | 39 |
| PK hit | 31 |
| Arithmetic projection, 800 output rows | 2,085.04 |
| 100 groups | 7,561 |
| Indexed join, at most one output row | 6,051.70 |
| Mixed LIKE fallback | 30,829 |

The indexed join executes two primary-key scans and one join-index lookup,
but also calls `Engine#compile` 114 times, `compile_filter` 22 times and
`scan_access_candidates` six times per query. Many compilation wrapper modules
each see 126 calls; those are layered calls, not independent compilation jobs
to sum together. Allocation sites concentrate in expression typing/resolution
and planner traversal. This is strong evidence to investigate repeated planning
and resolution before specializing the residual predicate. Call counts are
not sampled CPU percentages and do not establish a predicted speedup.

The 100-group query also records 300 fast-evaluator calls and 200
`compute_aggregate` calls per execution; the DISTINCT aggregate records 2,002
fast-evaluator calls. These suggest grouped-output/HAVING and DISTINCT-input
work remains. Aggregate method calls can retrieve cached results and must not
be read as proof that inputs were refolded that many times.

The next bounded target should reuse safe prepared-join planning metadata,
preserving parameter handling, invalidation and statement-local execution
state. It must retain exact values, costs and error timing across schema,
statistics and host-context changes. Three further compiler candidates remain:

1. **Scalar expressions and typed comparisons inside file blocking operators.**
   [spill_select.rb](lib/jed_native/spill_select.rb) still interprets WHERE,
   computed grouping keys, rewritten HAVING, projections and ordering keys.
   Reuse scalar kernels at those existing call sites while keeping spool
   iteration, memory reservations, fold/replay order and cleanup intact.
   The spill sort comparator also remains generic. Existing compiled ordering
   expects `[row, keys, ordinal]` entries, whereas this path uses flat spill
   rows; a specialization needs its own explicit layout and stable-tie contract.
2. **Join ON and residual predicates at candidate boundaries.**
   [engine.rb](lib/jed_native/engine.rb) and the spill join paths can reuse
   supported scalar kernels without replacing candidate enumeration or index
   key encoding. Outer unmatched rows, NULL, duplicate matches, short-circuit
   order, exact costs and deferred errors need dedicated boundary verification.
3. **Memory grouped output.** HAVING, projections and sorting after aggregation
   still have interpreted scaffolding. This needs explicit group-key and cached
   aggregate slots, preserving GROUPING masks and evaluation order. Its value
   depends on group cardinality; ten output groups offer little per-row work.

File scan pipelines and aggregate arguments already compile. Full compiled
aggregate reductions and general relation loops deliberately reject file
storage in [compiled_plans.rb](lib/jed_native/compiled_plans.rb). Removing those
guards would apply memory-array execution assumptions to interleaved spool and
spill operations. Extending those complete loops requires a separate design.

The indexed join returns at most one row, with costs of 8 or 10 units, despite
its much higher latency than the simple PK lookup. Faster residual predicates
alone would not remove its repeated planning work. The mixed LIKE fallback
also remains expensive, but supporting retrying wildcard combinations needs
its own bounded algorithm and exact cost/error tests; it should not be folded
casually into the straight-line literal matcher.

## Correctness and support status

The measured library is exactly the source audited in the
[broader LIKE report](LITERAL_LIKE_FOLLOWUP.md): 20,267 passing records in each
memory lane and 18,250 in each disk lane, through execute and externally drained
query APIs, plus all 61 host test files. All four lanes visit all 679 corpus
files. Existing authored mode exclusions remain 2 memory files and 77 disk
files; no new exclusion or failure allowance was introduced.
All four audits and all 61 host test files passed without test failures.

These results establish compatibility with the tested specification snapshot.
Promotion still requires an explicit maintenance decision, supported Ruby
versions/platforms, packaging and public value conventions, and integration
with the supported-core assurance paths: deterministic storage simulation and
generated NoREC/TLP. The experiment has no native gemspec yet. Stable 4.0.7
YJIT and 4.1-preview ZJIT measurements are runtime evidence, not a shipping
platform matrix. This checkpoint does not change the frozen-experiment policy.

## Reproduction

The evidence directory is
[performance-checkpoint](../../bench/ruby-native/results/performance-checkpoint/).
It retains the explicit commands, selected plans, runtime identities, source
pins, raw samples, bounds, complete validation signatures and derived summaries.

Restore the before source from `fc12f427` into
`/tmp/jed-method-sharing-before/impl/ruby-native`. The driver accepts
`--baseline-source PATH`; the source-pin helper uses the recorded default path.
Preserve the existing JSON evidence outside this directory before a new run;
both the pin helper and driver refuse to overwrite evidence by default. Capture
a fresh `source-pins.json` with `pin_sources.rb` before measurements. Invoke
`run.rb --phase PHASE --final-source-sha256 HASH` with the final library hash
above. Run `verify`, `memory-controls`, `host-controls`, `file-controls`,
`blocking-controls`, `characterize`, `binding`, `cross`, and `profile` serially,
with the profile last. Child workers use the explicit opt-in mise tasks.
Use prepared canonical fixtures with matching corpus fingerprints; file
workspaces must reside on an actual disk filesystem.
The outer driver needs at least 16 GiB address space (or no address-space cap);
the individual Ruby engine workers still enforce their 2 GiB cap.

If the same preparation restrictions occur, preserve their reports and use
`retry_binding_file.rb` or `retry_cross_file.rb` with the final source hash,
before the final profile. The first needs local page-service socket permission;
the second separates the Rust build budget from measured worker limits.

Run `summarize.rb` after all phases have completed. It verifies recorded values,
metadata, costs, callback schedules, source/runtime identity, file checksums, actual spill,
resource bounds and nonoverlapping phase journals. It references the unchanged
full-corpus/host audit from the broader LIKE commit, checks all those source and
test pins again, and writes a lossless JSON ledger and complete artifact manifest.
