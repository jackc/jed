# JOIN compilation admission

> Historical report: raw evidence and run-specific drivers are on the preserved
> experiment branch. See the [archive and retrieval instructions](../../bench/ruby-native/ARCHIVE.md).
> Reproduction commands below refer to that original checkout.

This explicitly requested follow-up reduces setup overhead identified in the
[JOIN-predicate pass](JOIN_PREDICATE_FOLLOWUP.md). The Ruby core remains
experimental, frozen and opt-in. Normal tests, benchmarks, CI and future
feature updates do not include it.

## Implementation

The existing general-executor small-input threshold increases from eight to
sixteen. The heuristic uses the outer Array's length for indexed joins, or
the product of two exact Array lengths for nonindexed joins. The indexed
quantity is not a count of all matching candidates. Unknown containers and
lateral sources retain existing admission without additional length reads.

For known input sizes below 128, a plain integer comparison of two immediate
literal, column or parameter leaves retains the ordinary evaluator before
source emission. Larger and unknown inputs keep the existing compiler policy.
The rule does not recursively classify expressions or cache an admission
decision on a mutable expression. The existing second-use, source-and-pool
cache and compiler limits remain in place.

The executor creates the independent ON-program pairs only after a kernel
is actually returned. If every ON tree declines compilation, it uses the
original evaluator loop without allocating those pairs.

Immediate-compilation diagnostics bypass both size heuristics; global and
JOIN-specific disable controls still take precedence. The spill executor has
no new input-size observation and retains its existing admission policy.

No SQL behavior, cost model, host API, dependency or supported-core changes.

## Evidence and method

The baseline is pushed commit
`7b854f1f37551b47a06c595d446a0176080fb64b`, library SHA-256
`f980101e855c807c25c6a7cdb5e55bd0c5b53a1ee5e3f01f918aff843dbcf13f`,
archived at `/tmp/jed-join-admission-before/impl/ruby-native`.
New evidence belongs in
[`join-admission-followup`](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/join-admission-followup`).
Earlier source reports and measurements remain unchanged.

Final library SHA-256:
`5feb7b064f414131a1c5f5d4aba8a64be8facc0885e3b4e5299ba0b5527ef29c`.

The existing ten-job
[JOIN-predicate plan](../../bench/ruby-native/compare/join-predicate-plan.json)
is unchanged: 1,000 fixture rows, the same SQL and parameter cycles, warmups,
iterations and five samples per process. Each runtime runs serial
before/final/final/before processes across prepared cursors, ordinary query
cursors and eager execute. All results are drained. Paired times average
two process medians per source; raw samples, process ranges and all slower
cells remain available. Range separation is descriptive, not a statistical
significance test.

Interpreter and YJIT use Ruby 4.0.7; ZJIT uses Ruby 4.1.0-preview1. Comparing
the JITs also changes Ruby version. The library does not enable a JIT.
Fresh supported-core and actual Ruby Rust-binding observations use different
public APIs and collection paths and are separate from source-change effects.
Exact wrapper preflights precede timing. Profiles run last using reused
`execute_prepared` statements; the timed eager lane repeats `execute(SQL)`.

Workers run serially under a 512 MiB sampled RSS guard. Ruby measurement,
host and corpus workers have 2 GiB of address space; formal focused checks
have 1 GiB. Supported-core comparisons retain their established 16 GiB
virtual reserve. These process guardrails are not a whole-engine memory
guarantee.

## Correctness

All **68 host-test files** passed. The focused suite passed under the
interpreter, YJIT and ZJIT. It covers the 15/16 and 127/128 boundaries,
all integer comparison operators and widths, leaf/compound/text predicates,
current type-registry and expression mutations, warmed kernels, unknown row
sources, immediate/disable controls and pre-emission rejection.

Mixed independent ON trees exercise both compiled/interpreted orders,
all-declined and empty vectors. TracePoint confirms that all-declined roots
create no ON-program vector. Statement/lifetime prefixes, contextual cast
errors and deferred column errors match the existing evaluator. The retained
tests also cover snapshots, outer joins, spilling, cache bounds and ownership.

The largest focused worker used 67.0 MiB sampled RSS; the complete host suite
peaked at 197.1 MiB.

All four complete corpus lanes passed **77,154/77,154 records**:

| Storage / API | Runtime | Passed records |
| --- | --- | ---: |
| Memory / query cursor | ZJIT | 20,297 |
| Memory / execute collector | ZJIT | 20,297 |
| File / query cursor | YJIT | 18,280 |
| File / execute collector | YJIT | 18,280 |

There are no failures, unsupported records, timeouts, memory-limit exits or
additional skips. Existing authored storage-mode skips remain unchanged.
The largest corpus worker used 179.9 MiB sampled RSS. Corpus lanes force
immediate compiler admission; the focused and direct diagnostics separately
exercise the new normal-admission decisions. All correctness workers exited
before measurement work began.

Direct diagnostics cover 80 workload/API/storage groups, 32 public-query
boundary groups and 56 helper-admission cases. They compare complete values,
types, errors, costs, cost prefixes, physical plans and EXPLAIN, while tracing
source emission and calls to kernels actually returned by the JOIN helper.
Cache probes retain the existing 64-entry bound and observe kernel lifetimes;
the focused suite separately verifies release of execution objects.

The memory boundary queries exercise known products of 96 and 128. The same
file queries reach the spill executor, where the observed input size is
unknown; they positively verify spill calls, scratch writes and retained
kernel execution. The logical product alone does not establish which
admission path a query takes.

The first diagnostic smoke incorrectly expected the file product-96 query
to decline compilation. Bounded memory/file probes established the different
executor paths. The corrected smoke passed before source/helper pins and
formal measurements. Its original failure, script and probes are preserved
under `smoke-fixture/`; the correction changed no core or correctness tests.

## Results

**Decision: retain this candidate for its interpreter/YJIT computed-key
benefit and lower setup allocation.** It is not a universal speedup. All six
computed-key comparisons on those runtimes have both final process medians
below both original medians. ZJIT has an eager-execution regression:

| Runtime | Computed-key prepared before → final (µs) | Prepared elapsed change | Query elapsed change | Execute elapsed change |
| --- | ---: | ---: | ---: | ---: |
| Interpreter | 411.454 → 400.680 | −2.62% | −8.07% | −6.26% |
| YJIT | 262.908 → 251.032 | −4.52% | −6.37% | −4.13% |
| ZJIT | 242.327 → 242.890 | +0.23% | −3.69% | +13.36% |

The ZJIT query improvement has separated process medians; prepared overlaps.
Its eager computed-key result is **230.925 → 261.782 µs, +13.363%**, with
both final medians above both original medians. No replacement run removes
that result.

Range queries are mixed. Interpreter prepared is 1.117% slower with overlapping
process ranges; query and execute are 3.879% and 2.282% faster with separated
medians. YJIT prepared/execute are 0.548%/0.743% slower with overlapping ranges,
while query is 2.592% faster with separated medians. ZJIT prepared/query/execute
change by +3.534%, +2.042% and −2.831%, all with overlapping ranges.

The arithmetic-residual profile retains kernel preparation and allocation
counts; the general sixteen-input guard still declines tiny executions.
Interpreter prepared is 2.146% slower with overlapping ranges; query/execute
are 4.044%/2.779% faster with separated medians. YJIT
prepared/execute are 0.434%/1.515% slower with overlapping ranges, while query
is **2.925% slower with separated medians**. ZJIT is 0.500–2.660% faster across
the three APIs, all with overlapping ranges. This pass does not establish
an additional residual-query speedup.

The complete [90-cell tables](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/join-admission-followup/tables.md`)
retain every observation: 23 cells have separated faster medians, 12 have
separated slower medians and 55 overlap. Forty-two paired means are slower.
These counts include controls outside the changed setup and do not estimate
the fraction of queries improved by this change.

The largest measured slowdown is ZJIT ordinary-query three-table equality:
**210.531 → 242.377 µs, +15.127%**, with separated medians. ZJIT prepared
four-table equality is also **13.519% slower**. Neither query reaches the
JOIN compiler in the retained YJIT profiles. Their call/allocation observations
do not establish a cause for these ZJIT timings; this profile does not measure
ZJIT generated code or CPU time. The table also retains smaller scalar, PK,
miss and LEFT regressions.

The profiles expose the intended tradeoff: avoid preparing kernels when a
small amount of ordinary evaluation is cheaper.

| Workload | Objects/query before → final | Compiler preparations/query before → final | Fast evaluator calls/query before → final |
| --- | ---: | ---: | ---: |
| Range | 10,651.05 → 10,559.90 | 1.333 → 0 | 358 → 460 |
| Computed key | 999 → 887 | 1 → 0 | 49 → 99 |
| Arithmetic residual | 15,784.2 → 15,784.2 | 1 → 1 | 272 → 272 |

Range saves 91.15 objects/query (0.856%); computed key saves 112 (11.211%).
The other seven workloads and arithmetic residual retain their complete
profile call maps and allocation counts. These are diagnostic call/allocation
counts, not CPU-time percentages. The timed eager
lane repeats `execute(SQL)`; these profiles use `execute_prepared`, so they
cannot alone explain an API-specific timing change.

All six formal direct diagnostic reports match complete semantic outcomes
across the two source revisions. The normal-admission boundaries skip source
emission as intended for known small inputs; larger and unknown spill inputs
retain actual generated-kernel execution. Across main, boundary and mutation traces,
JOIN-scoped source preparations fall from 212 to 52, with identical counts
across runtimes. The explicit file boundary cases record 24 spill-join calls
and 2,820 scratch appends per report on both revisions. Cache churn observes
zero live kernel weak references after its owning thread exits and garbage collection.
The focused suite separately confirms that a retained kernel owns no engine,
expression, row or bindings. Neither check claims immediate release of all
Ruby JIT code.

The fresh supported-core run measures computed key at **242.658 µs** for Ruby
YJIT, **6.300 µs** for Go, **5.811 µs** for Rust and **16.552 µs** for
TypeScript. Ruby still takes **38.516× Go** and **41.760× Rust** time on that
shape; the smaller setup cost does not close the broader multiway JOIN gap.
Arithmetic residual takes **1,781.009 µs** versus Go's **400.023 µs**,
Rust's **739.813 µs** and TypeScript's **1,564.693 µs**: **4.452× Go**
and **2.407× Rust**. Scalar and PK take 5.523× and 2.243× Go time.

In the separate actual Ruby-package run, native prepared computed key takes
**249.747 µs** versus the Rust binding's **132.112 µs**, or **1.890×** its
time. Native prepared residual takes **1,892.018 µs** versus **885.755 µs**,
or **2.136×**. Native prepared scalar and PK remain faster than the binding
in this run: **5.119 versus 8.117 µs**, and **4.057 versus 10.711 µs**.
These are separate API/collection-path observations, not controlled
source-change effects.

All 24 formal commands completed serially within their guards. Timing and
binding workers peaked at 91.2 MiB sampled RSS, direct diagnostics at 76.1 MiB
and profiles at 67.8 MiB. The largest supported-core worker was TypeScript
at 243.2 MiB. All workload processes exited before final evidence verification.

## Remaining opportunities

The next candidate is a narrow extension of prepared JOIN resolution reuse
to builtin integer `abs`. Its current syntax admission rejects call nodes,
so the arithmetic-residual statement still repeats resolution despite its
compiled evaluation kernel. The final profile still records 220
`Engine#compile` calls and 204 calls at each contextual-binary wrapper per
query. This is unmeasured future work; these counts are not CPU attribution.

Admission must distinguish builtin integer resolution from a same-name host
overload, reject aggregate/window modifiers, preserve parameter-cast replay
and existing cache budgets, and retain fresh parameter-sensitive physical
selection. The existing environment identity invalidates resolution entries
when host functions are registered. No resolved bound values or execution
owners should outlive their current execution.

Execution-local preparation of indexed-JOIN comparison structure remains
another candidate, as described in the preceding report. Neither follow-up
is implemented by this change.

## Reproduction

The evidence directory contains `check.rb` for correctness and `run.rb` for
measurements. Both use explicit `mise run ruby-native:*` tasks, run serially
and refuse to replace existing reports. Preserve earlier artifacts before an
intentional rerun. Reconstruct the baseline from commit
`7b854f1f37551b47a06c595d446a0176080fb64b`; the runner verifies its library hash.

Run `check.rb --phase PHASE --source-sha256 FINAL_HASH` for `focus`, `host`
and `corpus`. The outer correctness driver needs 2 GiB of address space and
local Unix-socket access for the existing page-service host tests.

After correctness passes and measured source/helpers are frozen, run
`run.rb --phase PHASE --final-source-sha256 FINAL_HASH` in this order:
`pins`, `verify`, `diagnostics`, `controls`, `cross`, `binding`, `profiles`.
Invoke drivers through the bounded mise wrapper, with at least 16 GiB of
outer address space for supported-core comparisons. Each workload retains
its own 512 MiB RSS guard.

After workers exit, `summarize.rb` verifies source/runtime/helper hashes,
unchanged schedules, exact validation signatures, raw-sample arithmetic and
serial command intervals. It produces `summary.json`, `tables.md` and
`manifest.json`, preserving parsed values when compacting JSON evidence.
The manifest covers 80 files totaling 17,598,433 bytes. Its SHA-256 is
`bc9e9c51a53ddb03d9a93e145115a08aa9d0aba37cbdee9b1ee9f059ad4caaf2`.
