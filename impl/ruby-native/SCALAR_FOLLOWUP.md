# Ruby scalar-query optimization pass

This explicitly requested continuation removes the general cursor setup cost
from eligible scalar queries and reduces evaluator dispatch and allocations.
The original scalar workload is now **4.61–5.27 µs across the JIT modes**, or
**5.9–6.7× Go**, down from roughly 26–31× in the previous pass. The expanded
conditional workload improves **10–12× under YJIT**. Some decimal, float, and
text shapes still exceed 10× Go; the eager API receives smaller improvements.
The [previous pass](QUERY_FOLLOWUP.md) identified cheap scalar queries as the
largest remaining gap. This experiment remains frozen and opt-in: normal tests,
benchmarks, CI, and future feature updates still exclude it. No dependency or
supported-core promotion is part of this change.

## Implementation and contracts

`ScalarRows` retains the normal Ruby `Rows` API, snapshot pin, service-generation
guard, captured settings and grants, and live session lifetime accounting. It
uses a private expression context without a relational snapshot clone or Fiber.
Binding and metadata remain available at construction; expression resolution
and execution remain lazy. Cached plans contain compiled expressions, never
query answers. Every execution evaluates its actual parameters and copies host
values on output.

The lane admits fromless numeric/boolean/text expressions and immutable builtins,
including CASE/BETWEEN without WHERE. Transactions, finite query-memory budgets,
host/stateful functions, regexes, containers, and more involved query shapes
retain ordinary execution. Conditional expressions with WHERE also retain that
path because it filters before compiling projections. Both original projection
charging orders are preserved, including failure prefixes and zero-work budget
checkpoints. Nested CASE parameter sites are visited when validating a reused
plan. The ordinary compiler and evaluator still own SQL semantics.

Canonical integer casts, unary operators, `abs`, `coalesce`, `nullif`, `greatest`,
and `least` bypass inapplicable extension dispatch. Integer folds preserve lazy
coalesce, left-to-right eager evaluation of all other arguments, NULL behavior,
range checks, and charges. Nominal types, mixed numeric types, deep expressions,
and window expressions retain their existing paths. Explicit keyword forwarding
removes temporary hashes from six function wrappers and five cast wrappers while
preserving their normal dispatch and current argument values.

The new lazy-API checks exposed adjacent correctness bugs. An unread ordinary
cursor now checks the live session lifetime counter at first pull, and generic
deferred projection publishes its row-produced unit to that counter before the
next checkpoint. Parameter-free array metadata now retains the constructor's
source type through explicit casts and nested arrays. LIKE ESCAPE reports its
signature error separately from operand-type errors. These changes fix ten
pre-existing lazy expression-corpus failures, independently reproduced at the
baseline revision.

## Measurement contract

Baseline commit: `28b12a8acca84c51d1fb4c3b56c58d9c8fae5f07`.
Baseline Ruby library SHA-256:
`a4e732343b161afcab199bf9b5a4bde5927c657e0721caa21726bb99a14ef0a7`.
Final Ruby library SHA-256:
`7d1197be345d44a7c389a66ec0a18d038282d26d59fad97ab08f8bdd6f8a7a28`.
Timing workers load immutable source copies and record their hash before and
after execution. The supported cores and adapters are unchanged.

The existing seven-query memory plan remains the default. An explicit
`RUBY_NATIVE_COMPARE_PLAN` option selects the new ten-query
[scalar plan](../../bench/ruby-native/compare/scalar-plan.json), which covers
integer arithmetic, conditional expressions, NULL-aware folds, decimal/float
arithmetic, text functions, mixed projections, constants, and filtered scalars.
Each workload uses 300 warmups, five batches of 2,000 prepared queries, and 32
answer-validation cases. Tables report median microseconds per query. These
short warmups do not establish uniform steady-state compilation across shapes.

The host is an Intel Core Ultra 9 285K on x86-64 Linux/ext4. Runtimes are Ruby
4.0.7 interpreter/YJIT and the existing local 4.1.0-preview1 build with
interpreter/YJIT/ZJIT. Runtime probes require the requested JIT to be active.
Comparison cores use Go 1.27.1, Rust 1.99.0, and Node 26.10.0. All timing runs
are sequential, without concurrent tests or builds from this task. This is a
shared host; small differences should not be treated as established gains.

The query/execute diagnostic finishes all timing before TracePoint or ObjectSpace
tracing. Traced time is never reported. Its allocation counts and API timings are
diagnostic evidence, separate from canonical cross-core comparison.

## Expanded scalar results

Median microseconds per prepared query, before → after:

| Query | Ruby 4.0 YJIT | Ruby 4.1 YJIT | Ruby 4.1 ZJIT |
|---|---:|---:|---:|
| Integer abs + addition | 23.42 → 5.19 | 19.51 → 4.61 | 21.61 → 5.27 |
| Integer arithmetic | 19.61 → 5.79 | 15.47 → 5.22 | 18.02 → 5.98 |
| CASE / BETWEEN | 69.53 → 5.57 | 51.68 → 4.95 | 53.13 → 6.05 |
| NULL-aware numeric folds | 30.54 → 8.17 | 24.77 → 7.38 | 28.50 → 9.49 |
| Decimal arithmetic | 33.99 → 11.68 | 23.85 → 10.45 | 26.15 → 11.36 |
| Floating-point arithmetic | 30.89 → 10.48 | 23.26 → 9.41 | 27.32 → 11.22 |
| Text functions | 30.52 → 10.94 | 22.69 → 9.74 | 26.45 → 11.40 |
| Mixed projection | 25.98 → 7.23 | 20.25 → 6.28 | 25.59 → 7.70 |
| Constant | 17.72 → 4.24 | 13.42 → 3.66 | 16.43 → 4.04 |
| Filtered scalar | 18.89 → 5.43 | 15.05 → 4.77 | 18.60 → 5.60 |

Final comparison, in the same units:

| Query | 4.0 YJIT | 4.1 YJIT | 4.1 ZJIT | Go | Rust | TypeScript |
|---|---:|---:|---:|---:|---:|---:|
| Integer abs + addition | 5.19 | 4.61 | 5.27 | 0.79 | 1.42 | 2.42 |
| Integer arithmetic | 5.79 | 5.22 | 5.98 | 0.75 | 1.45 | 2.32 |
| CASE / BETWEEN | 5.57 | 4.95 | 6.05 | 0.80 | 1.20 | 1.85 |
| NULL-aware numeric folds | 8.17 | 7.38 | 9.49 | 0.91 | 1.30 | 2.04 |
| Decimal arithmetic | 11.68 | 10.45 | 11.36 | 1.09 | 1.35 | 2.29 |
| Floating-point arithmetic | 10.48 | 9.41 | 11.22 | 0.86 | 1.21 | 1.91 |
| Text functions | 10.94 | 9.74 | 11.40 | 1.01 | 1.25 | 2.31 |
| Mixed projection | 7.23 | 6.28 | 7.70 | 0.87 | 1.24 | 1.57 |
| Constant | 4.24 | 3.66 | 4.04 | 0.57 | 1.12 | 0.91 |
| Filtered scalar | 5.43 | 4.77 | 5.60 | 0.70 | 1.16 | 1.39 |

Ruby 4.1 YJIT is within 10× Go on nine of the ten expanded shapes; floating-point
arithmetic is 10.9×. Ruby 4.0 YJIT has three shapes above 10×, peaking at 12.2×;
ZJIT peaks at 13.1×. Both JITs matter substantially. For the original abs query,
the 4.0 interpreter improves 58.77 → 14.42 µs and the 4.1 interpreter improves
53.62 → 13.13 µs, still about 2.5–2.8× slower than their JIT lanes. The complete
interpreter samples are retained alongside all JIT measurements.

Artifacts: [4.0 baseline](../../bench/ruby-native/results/scalar-before-4_0_7.json),
[4.1 baseline](../../bench/ruby-native/results/scalar-before-4_1_preview1.json),
[4.0 final and supported cores](../../bench/ruby-native/results/scalar-after-4_0_7.json),
[4.1 final modes](../../bench/ruby-native/results/scalar-after-4_1_preview1.json).

## Lazy and eager API diagnostics

Ruby 4.1 YJIT, before → after. Allocation counts are rounded to whole objects;
all twenty first-answer digests match across before/after and query/execute.

| Query | Lazy µs | Lazy allocations | Eager µs | Eager allocations |
|---|---:|---:|---:|---:|
| Integer abs + addition | 20.43 → 4.57 | 147 → 35 | 10.29 → 7.98 | 140 → 120 |
| Integer arithmetic | 17.90 → 5.09 | 125 → 34 | 9.26 → 8.93 | 130 → 130 |
| CASE / BETWEEN | 54.91 → 4.84 | 484 → 34 | 42.32 → 43.23 | 489 → 489 |
| NULL-aware numeric folds | 24.50 → 7.22 | 197 → 50 | 19.42 → 13.65 | 228 → 180 |
| Decimal arithmetic | 23.80 → 10.22 | 197 → 66 | 17.05 → 15.18 | 210 → 170 |
| Floating-point arithmetic | 23.11 → 9.27 | 201 → 64 | 15.60 → 13.41 | 198 → 154 |
| Text functions | 22.98 → 9.64 | 193 → 73 | 15.51 → 14.00 | 185 → 159 |
| Mixed projection | 20.33 → 6.29 | 172 → 46 | 14.07 → 10.77 | 179 → 149 |
| Constant | 13.65 → 3.58 | 124 → 33 | 6.57 → 6.06 | 105 → 105 |
| Filtered scalar | 15.12 → 4.69 | 125 → 35 | 9.30 → 8.55 | 134 → 134 |

The largest gain is in lazy `query_prepared`, the API used by the comparison
adapters. Eager `execute_prepared` benefits from the evaluator and forwarding
changes, but retains its general execution machinery. Eager conditional
resolution is still expensive and did not improve. Moving that path into cached
execution needs a separate treatment of its compile/error and memory-accounting
order; this change does not silently reroute it through a different contract.

The [baseline diagnostic](../../bench/ruby-native/results/scalar-followup/before-profile.json)
and [final diagnostic](../../bench/ruby-native/results/scalar-followup/final-profile.json)
retain every sample, method count, and allocation site. Intermediate A/B reports
isolate the lightweight numeric cursor, conditional/text extension, and keyword
forwarding before the final integer folds. An allocation-only forwarding probe
records six function wrappers dropping from 6 → 0 objects on Ruby 4.0 and
12 → 0 on 4.1; five cast wrappers drop from 5 → 0 and 10 → 0 respectively.

## Regression controls

The unchanged original benchmark plan was rerun as a regression control. Median
microseconds per query:

| Query | 4.0 YJIT | 4.1 YJIT | 4.1 ZJIT | Go | Rust | TypeScript |
|---|---:|---:|---:|---:|---:|---:|
| Scalar | 5.17 | 4.79 | 5.56 | 0.78 | 1.37 | 2.08 |
| PK hit | 3.99 | 4.00 | 4.45 | 1.43 | 2.22 | 4.57 |
| PK miss | 3.56 | 3.33 | 3.53 | 1.05 | 1.78 | 3.02 |
| Filtered scan | 330.14 | 301.85 | 528.79 | 169.68 | 107.96 | 303.30 |
| Aggregate | 301.28 | 293.12 | 561.00 | 64.86 | 62.05 | 166.44 |
| Group | 680.94 | 671.43 | 875.43 | 256.76 | 166.76 | 431.04 |
| Sort and limit | 912.76 | 951.83 | 1116.63 | 204.80 | 129.32 | 319.72 |

All seven original shapes are within 10× Go in each tested JIT mode; the largest
ratio is ZJIT aggregate at 8.65×. These controls include small regressions as well
as improvements outside scalar queries. For example, 4.0 YJIT sort-and-limit is
912.76 µs versus 867.20 in the previous pass; 4.1 ZJIT aggregate is 561.00 versus
499.77. This pass does not establish a general relational-query speedup.
[4.0 controls](../../bench/ruby-native/results/scalar-regression-4_0_7.json) and
[4.1 controls](../../bench/ruby-native/results/scalar-regression-4_1_preview1.json)
retain their samples and answer checks.

A fresh sequential ZJIT baseline/final pair measured aggregate at
490.18 → 538.04 µs (+9.8%) under the original short warmup. Its later batches
were substantially faster, so a separate, explicitly recorded 500-warmup,
300-iteration aggregate-only diagnostic measured 434.09 → 451.98 µs (+4.1%).
This does not eliminate the regression; it shows the short-run gap includes
warmup sensitivity. A temporary helper-extraction candidate measured 534.12 µs
under the original plan and did not materially improve it, so that candidate
was not adopted. These controls remain separate from the original tables.
[Fresh baseline](../../bench/ruby-native/results/scalar-regression-control-before-1.json),
[fresh final](../../bench/ruby-native/results/scalar-regression-control-after-1.json),
[helper experiment](../../bench/ruby-native/results/scalar-regression-control-split.json),
[longer-warmup baseline](../../bench/ruby-native/results/scalar-zjit-warm-before.json),
[longer-warmup final](../../bench/ruby-native/results/scalar-zjit-warm-after.json).

The million-row file lookup control keeps the original 50,000 warmup and 50,000
measured operations, exclusive file ownership, and normal durability. Mean
microseconds per operation:

| Runtime/core | File PK lookup |
|---|---:|
| Ruby 4.0 YJIT | 16.41 |
| Ruby 4.1 YJIT | 16.71 |
| Ruby 4.1 ZJIT | 17.74 |
| Go | 2.84 |
| Rust | 2.37 |
| TypeScript | 7.53 |

Ruby file latency is broadly unchanged from the prior 16.27/17.61/17.49 µs.
Go's fresh sample is slower than its previous 2.10 µs, so the current 5.8–6.2×
ratios also reflect that variation. Ruby remains below 10× even against the
earlier Go measurement. These are one complete run per mode, not repeated
proof of a lookup improvement. Cold-open and durable-write performance were
not remeasured in this scalar pass.
[4.0 file control](../../bench/ruby-native/results/scalar-followup/final-file-point-4.0.7.json),
[4.1 file controls](../../bench/ruby-native/results/scalar-followup/final-file-point-4.1-preview1.json).

## Correctness and resource checks

| Final-source audit | Result | Peak sampled worker RSS |
|---|---:|---:|
| Memory SQL/concurrency, Ruby 4.1 ZJIT | 20,267 / 20,267 | 167.2 MiB |
| Disk SQL/concurrency, Ruby 4.1 YJIT | 18,250 / 18,250 | 179.4 MiB |
| Host/storage, Ruby 4.0.7 | 41 test files pass | 113.2 MiB |
| Expanded scalar host cases, Ruby 4.1 preview | Pass | 47.0 MiB |
| Lazy expression/cast/type corpus, Ruby 4.0.7 | 4,497 / 4,497 | 35.6 MiB |

Each full corpus audit covers 679 files with zero failures, unsupported files,
timeouts, or memory-limit failures; existing storage rules skip two memory files
and 77 disk files. The lazy audits cover 137 files and actually exercise 540
scalar cursors, with ordinary cursor fallback for the other shapes. The baseline
lazy expression audit independently reproduces the ten metadata failures now
fixed. No shared expectations or supported-core behavior changed, so no new
PostgreSQL oracle run is needed.

Host tests compare scalar and ordinary cursors through the public API, including
error prefixes, integer bounds, decimal/float/NULL values, metadata and result
ownership, snapshot pins, file poisoning/generation changes, grants, captured
settings, shared lifetime counters, and reentrant host callbacks. Additional
integer-fold cases check callback order, late errors, width promotion, and finite
cost/lifetime thresholds. The full host suite preceded those final test-only
additions; the separately recorded scalar-host run covers the expanded file on
the same frozen library hash.

Evidence: [memory audit](../../bench/ruby-native/results/scalar-followup/final-conformance-memory-4.1-zjit.json),
[disk audit](../../bench/ruby-native/results/scalar-followup/final-conformance-disk-4.1-yjit.json),
[host suite](../../bench/ruby-native/results/scalar-followup/final-host-4.0.7.json),
[expanded scalar host suite](../../bench/ruby-native/results/scalar-followup/final-scalar-host.json),
[lazy expressions](../../bench/ruby-native/results/scalar-followup/metadata-fixed-lazy-expr.json),
[lazy casts](../../bench/ruby-native/results/scalar-followup/final-lazy-cast.json),
and [lazy types](../../bench/ruby-native/results/scalar-followup/final-lazy-types.json).

The [cross-report summary](../../bench/ruby-native/results/scalar-followup/final-summary.json)
verifies 216 memory cells, six file cells, 23 additional ZJIT control cells,
twenty diagnostic answers, source
hashes, requested JIT modes, conformance totals, and resource bounds. Every
before/after comparison uses identical workload checksums. Intermediate artifacts
retain their own source hashes and failures; successful final-source evidence is
identified separately.

## Reproduction

Use the opt-in [runtime instructions](../../bench/ruby-native/RUNTIMES.md).
For example:

```sh
RUBY_NATIVE_COMPARE_PLAN=bench/ruby-native/compare/scalar-plan.json \
  RUBY_NATIVE_PHASE=scalar-local mise run ruby-native:compare
RUBY_NATIVE_COMPARE_PLAN=bench/ruby-native/compare/scalar-plan.json \
  JED_NATIVE_TIMEOUT=180 JED_NATIVE_ADDRESS_SPACE_BYTES=2147483648 \
  mise run ruby-native:bounded -- ruby --yjit \
  bench/ruby-native/query_profile.rb --report /tmp/scalar-profile.json
JED_NATIVE_QUERY_API=query mise run ruby-native:conformance expr/
```

The conformance harness still uses eager execution by default. The explicit
query mode drains inside the error boundary so deferred errors are checked too,
and records the number of scalar cursors actually exercised. Expression, cast,
and type suites are used here; finite query-memory fixtures describe eager
collection thresholds and can legitimately need different lazy expectations.
Every workload worker retains a 512 MiB sampled RSS limit and a deadline.
These are guardrails, not a whole-engine heap guarantee.
