# Compiled Ruby expressions and query plans

> Historical report: raw evidence and run-specific drivers are on the preserved
> experiment branch. See the [archive and retrieval instructions](../../bench/ruby-native/ARCHIVE.md).
> Reproduction commands below refer to that original checkout.

Measured 2026-10-19. The maintainer requested runtime compilation using Ruby's
ability to generate executable methods. This update keeps the Ruby core frozen
and experimental, with explicit opt-in tests and benchmarks. It adds no dependency
and changes no SQL or public host API.

## What the experiment establishes

Generating Ruby methods from resolved expressions removes repeated expression-tree
dispatch, operator selection, and type inspection inside row loops. Ruby's JIT can
then optimize a method specific to that expression. This is worthwhile for reused
expression-heavy queries. It is not evidence that every Ruby query can approach
Go or Rust: parsing, storage, grouping, sorting, allocation, result collection, and
the public API still contribute to execution time.

The initial YJIT proof measured 3.4–7.1× faster expression evaluation across integer
arithmetic, CASE, NULL folds, and a predicate. Source compilation cost 35–140 µs per
expression. Those are isolated kernels, not complete queries. Final whole-query
measurements and compilation/churn diagnostics are recorded below and in the
[raw results](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/compiled-followup`).

Ruby methods generated with `class_eval` are ordinary method instruction sequences;
the library evaluates trusted source once to define the method, never once per row.
The diagnostic uses `RubyVM::YJIT.insns_compiled` to check generated methods directly.
ZJIT's compiled-instruction-sequence log provides the corresponding evidence for
that runtime. Global JIT counters alone would not establish this. See Ruby's
[YJIT documentation](https://docs.ruby-lang.org/en/master/jit/yjit_md.html) and
[ZJIT documentation](https://docs.ruby-lang.org/en/master/jit/zjit_md.html).

## Whole-query results

Microseconds per complete prepared query; lower is better. Ruby before/after uses
stable **Ruby 4.0.7 + YJIT**. Supported-core columns are contemporary after-run
controls. All twelve workloads have identical validation answers and row counts
across all cores and both Ruby source versions.

| Query | Ruby before | Ruby after | Speedup | Go | Rust | TypeScript |
|---|---:|---:|---:|---:|---:|---:|
| Scalar | 5.05 | 4.66 | 1.08× | 0.84 | 1.42 | 2.36 |
| PK hit | 4.15 | 4.13 | 1.00× | 1.42 | 2.23 | 4.97 |
| PK miss | 3.47 | 3.35 | 1.04× | 1.04 | 1.78 | 3.13 |
| Filtered scan | 342.89 | 256.64 | 1.34× | 163.60 | 106.75 | 447.49 |
| Simple aggregate | 297.66 | 299.03 | 1.00× | 64.83 | 50.83 | 205.21 |
| Group | 687.01 | 685.85 | 1.00× | 253.83 | 170.79 | 547.75 |
| Sort/LIMIT | 942.21 | 908.80 | 1.04× | 209.72 | 130.33 | 491.74 |
| Arithmetic filter | 728.47 | 443.47 | **1.64×** | 209.69 | 149.56 | 484.71 |
| Arithmetic projection | 1390.41 | 816.33 | **1.70×** | 268.83 | 214.12 | 711.31 |
| CASE projection | 1654.19 | 819.75 | **2.02×** | 251.91 | 183.93 | 598.17 |
| Aggregate argument | 1019.92 | 721.77 | **1.41×** | 301.78 | 217.86 | 548.42 |
| Computed sort | 1410.92 | 1026.30 | **1.37×** | 345.13 | 207.52 | 431.86 |

The five added expression-heavy shapes are now **2.1–3.3× Go** and **3.0–4.9×
Rust**. This supports runtime compilation as a useful optimization; it does not
establish parity with those native cores. Simple aggregation, grouping, and
column-only sorting retain their existing execution costs and see little change.
The warmed PK path already uses a specialized cursor and is essentially unchanged.

Raw prepared comparisons:
[stable before](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/compiled-before-4_0_7.json`),
[stable after](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/compiled-after-4_0_7.json`),
[preview before](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/compiled-before-4_1_preview1.json`),
[preview after](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/compiled-after-4_1_preview1.json`).

## Interpreter, YJIT, and ZJIT

Each cell is **before → after**, in µs/query. These are matching per-runtime
controls; the preview runtime is Ruby 4.1.0-preview1.

| Query | 4.0 interpreter | 4.0 YJIT | 4.1 YJIT | 4.1 ZJIT |
|---|---:|---:|---:|---:|
| Arithmetic filter | 1765.55 → 1329.66 | 728.47 → 443.47 | 936.84 → 704.35 | 1422.67 → 828.31 |
| Arithmetic projection | 3468.70 → 2277.77 | 1390.41 → 816.33 | 1790.05 → 1191.47 | 2909.08 → 1384.24 |
| CASE projection | 4793.06 → 2206.17 | 1654.19 → 819.75 | 1962.57 → 1268.28 | 3088.48 → 1332.93 |
| Aggregate argument | 2311.88 → 1789.38 | 1019.92 → 721.77 | 1032.70 → 743.50 | 1686.76 → 1035.53 |
| Computed sort | 2319.91 → 1894.65 | 1410.92 → 1026.30 | 1503.02 → 1116.65 | 1707.32 → 1223.11 |

The optimization helps even without a JIT because Ruby's own bytecode executes
the generated operations directly. ZJIT benefits substantially, including a 2.32×
CASE improvement. Stable YJIT remains the fastest heavy-query configuration in
this run. Preview YJIT/ZJIT are close on cheap scalar queries: 4.50/4.45 µs after
compilation. Preview does not uniformly outperform stable Ruby.

## Ruby Rust binding and host functions

These are separate API measurements under stable YJIT, in µs/query. Ordinary
native `query(sql)` reuses its SQL; eager native uses `execute(sql)`. The Rust
binding includes parsing and Ruby result conversion on every call.

| Query | Native query before | Native query after | Native eager after | Rust binding after |
|---|---:|---:|---:|---:|
| Scalar | 5.68 | 5.19 | 5.75 | 8.19 |
| PK hit | 4.54 | 4.38 | 4.26 | 10.64 |
| Arithmetic filter | 764.81 | 490.64 | 464.48 | 364.59 |
| Arithmetic projection | 1482.40 | 969.73 | 795.23 | 703.38 |
| CASE projection | 1669.53 | 841.16 | **710.66** | 789.12 |
| Aggregate argument | 1110.21 | 781.29 | 806.47 | 271.33 |
| Computed sort | 1344.37 | 1083.68 | 1112.26 | 276.80 |
| Host scalar | 5.44 | 5.21 | 28.76 | 8.69 |
| 1,000 host row callbacks | 1364.46 | 1178.16 | **1016.04** | 1408.65 |
| 1,000 rows through host batches | 1746.78 | 1420.45 | 1243.85 | **882.23** |

Compiled surrounding predicates help the host-stream workloads; callback bodies
and the host boundary remain unchanged. The eager CASE query and individual host
callbacks beat the available Rust binding in this run. The binding still wins the
other heavy expression queries and cheap batched callbacks. Eager scalar host
calls retain the general executor to preserve callback-visible session behavior;
they remain much slower than the lightweight lazy API.

Preview comparisons are also retained in
[`summary.json`](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/compiled-followup/summary.json`).
For example, preview ZJIT eager host row/batch workloads measured 932.67/1113.22 µs,
while its lazy equivalents measured 1496.85/1717.77 µs. The choice of API and
surrounding executor still matters; generated expressions do not remove those
costs.

## Compilation cost, JIT evidence, and memory

Final direct diagnostics compare the unchanged evaluator with generated methods
from the same source. Nanoseconds per expression:

| Expression | YJIT evaluator | YJIT generated | ZJIT evaluator | ZJIT generated |
|---|---:|---:|---:|---:|
| Integer scalar | 377 | 102 | 359 | 111 |
| Row arithmetic | 674 | 201 | 684 | 196 |
| CASE | 638 | 132 | 904 | 149 |
| NULL folds | 755 | 141 | 1418 | 141 |
| Predicate | 438 | 62 | 607 | 82 |

Every measured generated method has direct JIT evidence, not just a rise in
global counters. The ZJIT log is checked per workload before the later churn
probe. Values, NULL/error cases, and every tested cost-prefix failure are checked
again after warmup and timing, exercising the warmed generated code.

Repeated source-compilation medians were **20–93 µs** across runtimes. First
generation measurements ranged from **0.10 to 2.35 ms**, and first invocation from
**2 to 95 µs**: runtime warmup/GC can make first-use latency substantially larger
than warmed emission. This favors repeated plans and per-row expressions;
compilation can take many cheap scalar executions to amortize.

The churn probe creates 256 methods, calls each 200 times, retains only 64, then
releases them. All three runtimes returned to **zero live kernel instances**.
JITs retained code/class metadata: YJIT still showed 25 anonymous kernel classes
and a 2.95 MB code region; ZJIT showed 266 classes, a 3.56 MB code region, and
6.56 MB total JIT memory. These counters are process-wide and include surrounding
engine/diagnostic code. Peak diagnostic RSS was **57.4 MiB**. Evicting a plan does
not imply immediate JIT code reclamation or falling RSS. Ruby exposes runtime
memory controls such as `--yjit-mem-size` and `--zjit-mem-size`; the library does
not change them.

Across the final benchmark workers, native Ruby peaked at **295.2 MiB** during
the million-row file control. The largest worker including supported cores was
**342.1 MiB** (TypeScript baseline), below the 512 MiB bound.

## File control

The million-row warm file PK workload uses the existing fixture, 50,000 warmups,
and five batches of 5,000 calls. The historical before report has the exact
pre-compilation source, fixture, SQL, schedule, answers, and costs.

| API | Before µs | After µs |
|---|---:|---:|
| Native prepared | 18.68 | 18.88 |
| Native ordinary query | 18.97 | 19.24 |
| Native eager | 18.27 | 18.59 |
| Rust binding | 17.61 | 18.15 |

The small movement is shared by the binding control; this gives no meaningful
evidence of a file-PK regression. Compilation does not accelerate this existing
specialized point path. These are warm reads, not cold-open or write measurements.

## Fresh SQL and cold costs

Unique SQL comments force parse/plan cache misses. The initial stable-YJIT
ordinary-query control measured scalar 90.82 → 95.30 µs, PK 85.48 → 92.91 µs,
mixed scalar 293.53 → 308.41 µs, join 659.86 → 763.27 µs, and arithmetic projection
1911.75 → 2153.31 µs. Rust-binding controls also slowed roughly 4–8%; eager native
join/projection were essentially flat or faster. These initial results are
retained, rather than attributing all changes to compilation.

The two larger lazy slowdowns prompted a focused alternating-source repeat with
the same full iteration counts and a fresh process per lane:

| Fresh query | Before A | After B | After B | Before A |
|---|---:|---:|---:|---:|
| Join | 630.17 | 692.44 | 631.88 | 687.06 |
| Arithmetic projection | 1753.29 | 1750.80 | 1646.57 | 1876.20 |

Join ranges overlap and source averages differ by only 0.5%; the projection
slowdown does not reproduce. These runs support substantial workload/JIT/host
variation, not a repeatable cold regression. They also establish **no cold-query
speedup** from the new compiler. First-use plan admission keeps unique scalar/read
SQL on the evaluator, while parsing and planning still dominate these controls.
Both initial and alternating runs, with wrapper controls, are retained in
[`summary.json`](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/compiled-followup/summary.json`).

## Implementation

- A handwritten emitter produces bounded Ruby methods for resolved plain integer
  and boolean expressions: arithmetic, comparisons, casts, unary and NULL
  predicates, AND/OR, searched CASE, and integer `abs`, `coalesce`, `nullif`,
  `greatest`, and `least`.
- Existing scalar, prepared, streaming, and sorted read plans own predicate,
  fused-projection, and computed-sort-key kernels. First execution uses the
  evaluator; reuse can trigger compilation. Read plans now also retain safe
  CASE/BETWEEN trees and their nested contextual parameter sites.
- Aggregate argument vectors can use a separate 64-entry LRU cache in both the
  general and spill executors. Exact emitted source plus its immutable scalar
  pool is the key. Resolution and bounded emission still happen per query; an
  identical program reuses its Ruby method. Grouping substitutions, aggregate
  folding, filters, ordering, and spill algorithms remain the existing engine.
- Unsupported types and expressions use the existing evaluator. A fused
  projection can delegate an unsupported complete root in its original position.
  Host calls, collatable/open/nominal types, subqueries, and window expressions are
  not specialized. General scalar conditional projections retain their existing
  checkpoint schedule. Joins and most relational planning remain interpreted.
- SQL text, names, literal values, and diagnostic strings never enter generated
  source. Only compiler-owned templates/operators and validated numeric ordinals
  do. Values live in a frozen pool. Generated anonymous classes capture no engine,
  session, row, or bindings and are collectible with their plans.
- Each method preserves per-node preorder charges, NULL short circuits, eager
  argument ordering, integer overflow, SQL division/remainder, and error messages.
  Execution supplies the current engine, row, and parameters. There is no retry
  through the evaluator after a compiled execution fails.

The existing resolved-plan caches remain capped at 64 entries each. Each generated
method is limited to 192 nodes, depth 32, 256 pool entries, and 32 KiB source; each
resolved plan retains at most three methods and 64 KiB source. The aggregate cache
also has at most 64 entries. These are compilation/source bounds, not a whole-VM
heap guarantee; Ruby controls JIT code reclamation. See the
[design contract](../../spec/design/ruby-runtime-compilation.md).

## Measurement method and reproduction

The unchanged source immediately before this experiment has SHA-256
`d3db0d7784b23d9d9aa6da1f3f45045b298eef7279dc7e7987b81052ee2032fc`.
It includes the preceding ordinary API/host optimizations and is preserved as a
[deterministic source archive](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/compiled-followup/before-source.tar.gz`),
with a checked restore recorded in `before-source.json`. It is a working-tree
baseline, not a new commit. The final implementation source is identified in every
measurement and verification report; source hashes are checked before and after.

The expanded [comparison plan](../../bench/ruby-native/compare/compiled-plan.json)
keeps the original seven queries and adds arithmetic filtering/projection, CASE,
computed aggregate arguments, and computed sorting over the same 1,000-row fixture.
All lanes validate checksums and consumed row counts outside timing. Ruby uses
`Session#query_prepared` and drains results. Go, Rust, and TypeScript use their
existing prepared adapters. Five samples retain the plan's declared warmups and
iteration counts; tables report the median time per complete query. GC stays on.

The separate binding harness compares the available native prepared, ordinary
query, and eager APIs with the Ruby Rust binding under the same Ruby executable
and JIT. The binding reparses and returns materialized Ruby results; it has no
prepared/lazy API, so those measurements are API comparisons, not isolated FFI
costs. Fresh-SQL controls append a unique comment to defeat the parsed-SQL cache.
Host controls retain identical Ruby callbacks and validate counts/costs outside
timing. Direct generator diagnostics separately measure emission, first execution,
steady evaluation, allocations, JIT evidence, and generated-method churn.

Workers run sequentially on the same Linux x86-64 Intel Core Ultra 9 285K host.
No tests, builds, or benchmarks from this task overlap timed work. Ruby workers
have a 512 MiB RSS ceiling and 2 GiB address-space limit; supported-core adapters
retain their larger virtual-address reservation allowance with the same RSS cap.
Every corpus file has a separate ten-second deadline. Results remain measurements
on a shared machine, and JIT history affects short queries.

```sh
RUBY_NATIVE_COMPARE_PLAN=bench/ruby-native/compare/compiled-plan.json \
  RUBY_NATIVE_PHASE=compiled-local RUBY_NATIVE_COMPARE_RUBY_ADDRESS_SPACE_BYTES=2147483648 \
  mise run ruby-native:compare

JED_NATIVE_TIMEOUT=300 JED_NATIVE_ADDRESS_SPACE_BYTES=2147483648 \
  mise run ruby-native:test

JED_NATIVE_COMPILATION=immediate JED_NATIVE_TIMEOUT=900 \
  JED_NATIVE_ADDRESS_SPACE_BYTES=2147483648 \
  mise run ruby-native:bounded -- \
  .dev/ruby-native-runtimes/ruby-4.1.0-preview1/bin/ruby --zjit \
  impl/ruby-native/bin/conformance.rb

JED_NATIVE_TIMEOUT=180 JED_NATIVE_ADDRESS_SPACE_BYTES=2147483648 \
  mise run ruby-native:bounded -- ruby --yjit \
  bench/ruby-native/compiled_diagnostics.rb --jit yjit \
  --churn 256 --churn-calls 200 --report /tmp/compiled-diagnostics.json
```

Runtime selection and ZJIT availability remain documented in
[RUNTIMES.md](../../bench/ruby-native/RUNTIMES.md). Normal repository tasks remain
unchanged; this experiment must still be selected explicitly.

## Verification

The measured implementation has source SHA-256
`e461f1f256a672948823623a88bc20fa949f594ac2708bda5225069de378d473`.

- All 47 experimental host/storage test files pass, including the direct emitter
  and warmed-plan tests; peak RSS was 116.6 MiB.
- Full memory corpus, Ruby 4.1 preview ZJIT, forced first-use compilation:
  **20,267/20,267** executions pass; **2,313** generated kernels; peak worker RSS
  **140.8 MiB**.
- Full disk corpus, Ruby 4.1 preview YJIT, exclusive locking, forced compilation:
  **18,250/18,250** executions pass; **2,277** generated kernels; peak worker RSS
  **185.2 MiB**.
- Lazy-query expression, cast, and parameter suites, preview YJIT with forced
  compilation: **3,374/3,374** pass; **488** generated kernels and 532 scalar
  cursor executions; peak worker RSS **68.7 MiB**.

Together these cover **41,891 conformance executions** on the final source.

Both full corpus runs have zero mismatches, unsupported harness files, timeouts,
or memory-limit hits. The existing corpus excludes two files from memory mode
and 77 from disk mode; these are preserved and counted in the reports.

Emitter tests compare values, SQLSTATE/message, exact statement/lifetime costs at
every tested limit prefix, lazy row faults, NULL truth tables, overflow, negative
division/remainder, branch laziness, source injection resistance, source/depth/node
limits, and collectibility. Integration tests cover all four APIs, warm reuse,
changing bindings, captured snapshots, schema replacement, privileges, host
fallback/error ordering, sort keys, memory budgets, file spills, aggregate filters
and grouping, folded subquery constants, and cache bounds. They caught and fixed
an identity-hash initialization bug that prevented lazy aggregate cache hits.

An independent code review found no remaining semantic/invalidation blockers.
No SQL contract or file format changed, so no new PostgreSQL oracle expectation
was introduced. Unsupported compiler shapes retain the complete existing SQL
implementation.
