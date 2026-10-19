# Native Ruby versus the Ruby Rust binding

Measured 2026-10-19 against `e5dbbe514e41822793e92c996fffbba5e5419e96`.
This comparison changes no engine code or experimental-support policy.

**Historical baseline:** the subsequent [API follow-up](API_FOLLOWUP.md) implements
and remeasures ordinary statement reuse, host planning, and batch improvements.
Use that report for current timings; this report preserves the original evidence.

## Assessment

Native Ruby is a credible implementation for applications that reuse prepared
statements. It beats the current binding on cheap prepared memory queries and
is close on warm file lookups. The existing Rust binding wins the heavier query
shapes measured here, ordinary unprepared calls, and whole host-function queries.
Native batching currently costs more than individual native callbacks.

The earlier comparisons within 10× Go describe prepared execution. They do not
establish the same performance for `query(sql)` or `execute(sql)`. This direct
comparison exposes that distinction: Ruby's parser, binding and general query
setup still dominate ordinary calls.

Recommendation: retain experimental status for a focused readiness pass. The
performance is sufficient to consider a maintained pure Ruby option, but does
not justify replacing the Rust binding as the general-purpose Ruby backend.
Promotion should explicitly accept the maintenance cost and close the testing
and packaging gaps below. A supported Ruby implementation need not become a
fourth member of the Rust/Go/TypeScript differential set.

## Method

The same Ruby executable and JIT run both packages. The binding uses a newly
built **release** Rust extension through its existing Fiddle API. All lanes use
a long-lived connection/session and fully consume every result:

| Lane | API and work included |
|---|---|
| Binding | `Jed::Database#query(sql, *params)`; parses each call, collects Rust results, marshals and constructs Ruby values/rows |
| Native prepared | `Session#query_prepared(statement, *params)`; preparation outside timing, lazy rows fully drained |
| Native query | `Session#query(sql, *params)`; parses each call, lazy rows fully drained |
| Native eager | `Session#execute(sql, *params).rows.each`; parses and collects each call, iterates plain result arrays |

The binding has no prepared-statement or lazy-row API. These are comparisons of
the available APIs, not isolated language speed or FFI overhead. Even the eager
control is not identical in row-wrapper allocation. Native database convenience
methods create fresh sessions; these measurements use explicit reusable sessions.

The seven original memory queries use the same SQL, parameters and 1,000-row
dataset as `compare/plan.json`. Cheap queries have 300 warmups and five batches
of 2,000 calls; other queries have 100 warmups and five batches of 50 calls.
Times below are the median batch time per query. Preparation for the
native-prepared lane, database creation, answer hashing and callback counters
are outside timing; the other lanes include parsing on every call. GC is enabled.

File tests use private copies of the canonical million-row `large.jed` fixture,
read-only opens with **auto locking in both packages**, 50,000 warmup probes and
five batches of 5,000 probes. The SQL returns `id, customer_id, amount, note`.
Every probe hits a row. This is warm lookup evidence, not cold-open, write or
durability-throughput evidence; its parameter stream and locking differ from
the previous exclusive-host core comparison.

Host: Linux x86-64, Intel Core Ultra 9 285K. Workers ran sequentially, with no
other tests/builds from this task overlapping measurements. This remains a
shared-machine microbenchmark, not a workload-wide application prediction.

## Stable Ruby 4.0.7 + YJIT

Microseconds per complete query; lower is better.

| Query | Native prepared | Native query | Native eager | Rust binding |
|---|---:|---:|---:|---:|
| Scalar `abs($1::i64) + 7` | 4.98 | 95.21 | 98.75 | 7.76 |
| Memory PK hit | 3.94 | 186.38 | 85.06 | 10.15 |
| Memory PK miss | 3.30 | 184.81 | 83.23 | 9.24 |
| Filtered scan | 317.54 | 435.09 | 446.19 | 124.10 |
| Aggregate | 295.35 | 402.39 | 358.57 | 64.51 |
| Group | 657.93 | 753.10 | 747.90 | 226.52 |
| Sort/limit | 1,005.60 | 981.42 | 1,077.16 | 151.29 |
| Million-row file PK hit | 18.85 | 134.16 | 132.38 | 17.45 |

Prepared native memory PK hits are 2.6× faster than the current binding; the
scalar is 1.6× faster. The binding is 2.6–6.6× faster on the four heavier
prepared query shapes. Native ordinary scalar execution is about 12× slower
than the binding. The 186 µs native-query PK result is reproducible across this
run's five batches (185–191 µs), although Ruby 4.1 substantially improves it.

## Ruby 4.1.0-preview1: both JITs

Microseconds per query. The native columns use prepared execution except where
explicitly labeled eager.

| Query | Native YJIT | Binding YJIT | Native ZJIT | Binding ZJIT |
|---|---:|---:|---:|---:|
| Prepared scalar | 5.19 | 8.05 | 6.45 | 7.76 |
| Prepared memory PK hit | 3.88 | 10.35 | 4.04 | 10.11 |
| Prepared filtered scan | 471.66 | 128.65 | 691.90 | 127.42 |
| Prepared aggregate | 284.24 | 63.68 | 495.22 | 59.52 |
| Prepared group | 648.03 | 224.82 | 859.16 | 215.62 |
| Prepared sort/limit | 916.15 | 157.13 | 1,107.20 | 149.64 |
| Prepared file PK hit | 17.05 | 17.89 | 20.60 | 17.62 |
| Unprepared eager scalar | 74.20 | 8.05 | 83.14 | 7.76 |
| Unprepared eager memory PK hit | 66.73 | 10.35 | 77.15 | 10.11 |
| Unprepared eager file PK hit | 114.36 | 17.89 | 130.56 | 17.62 |

Neither JIT changes the conclusion. The binding's engine executes in Rust, so
its JIT differences primarily affect the Ruby boundary and host kernels.
Preview results do not establish a supported runtime minimum or replace stable
Ruby measurements. Shapes and warmup history matter: these workers register host
functions and mix more query types than the earlier core-only comparisons.

## Host functions

Both packages run identical Ruby arithmetic:
`(value * 2_654_435_761).remainder(1_000_003)`. Validation confirms one scalar
callback per returned row and actual batched callbacks. Counters exist only in
the separate validation database, not timed kernels.

Stable Ruby 4.0.7 YJIT, milliseconds per whole query:

| Query | Native prepared | Native eager | Rust binding |
|---|---:|---:|---:|
| Secondary predicate, 500 rows, no host function | 1.201 | 1.241 | 0.479 |
| Secondary predicate, 500 individual callbacks | 1.437 | 1.449 | 0.751 |
| Secondary predicate, batched callbacks | 1.645 | 1.689 | 0.530 |
| PK range, 1,000 rows, no host function | 1.006 | 1.124 | 0.724 |
| PK range, 1,000 individual callbacks | 1.499 | 1.543 | 1.313 |
| PK range, batched callbacks | 2.099 | 1.996 | 0.897 |

For the secondary predicate the binding batches all 500 rows once, while native
Ruby makes eight calls (seven batches of 64 and one of 52). For the PK range
both make 16 calls (15 batches of 64 and one of 40). The first pair includes
different plan/batching choices; neither pair isolates FFI alone.

Incremental cost per output row on the 1,000-row PK-range query, subtracting
each package's own no-host baseline, in nanoseconds:

| Runtime | Native individual | Binding individual | Native batch | Binding batch |
|---|---:|---:|---:|---:|
| Ruby 4.0.7 YJIT | 493 | 589 | 1,093 | 174 |
| Ruby 4.1 preview YJIT | 482 | 623 | 964 | 94 |
| Ruby 4.1 preview ZJIT | 385 | 567 | 964 | 141 |

The individual native callback increment is smaller, but its surrounding query
execution costs more. Therefore the full native query still loses. Batching
amortizes the binding's upcalls effectively; native batch construction, argument
copying and outcome replay currently add more work than they save. These are
end-to-end incremental costs, including evaluation, metering and validation,
not isolated callback-body timings.

A single host call per scalar query also bypasses the new native scalar fast
path: native prepared takes **37.55 / 23.97 / 28.12 µs** on 4.0 YJIT / 4.1 YJIT /
4.1 ZJIT, versus **8.41 / 8.69 / 8.25 µs** through the binding. Host setup and
plan reuse are a clear remaining opportunity.

There are API differences beyond performance. Native registration supports
component identity/version and more scalar types, while the current binding
lacks identity/version registration and restricts callback types. The binding
releases the GVL during Rust execution, then reacquires it for Ruby callbacks;
pure Ruby engine execution remains Ruby VM work. These tests measure one
thread, not concurrent throughput.

## Before promotion

1. **Improve ordinary query setup and host batching**, then repeat this direct
   comparison. Bounded SQL-text plan reuse and host-call planning are candidates;
   any cache must preserve parameter typing, schema/environment invalidation,
   captured session context and the resource guarantees.
2. **Integrate the missing assurance paths.** Current source already passes the
   full memory/disk corpus and host suites, but Ruby has no deterministic
   simulation runner or generated NoREC/TLP integration. Static conformance is
   not the entire supported-core test policy.
3. **Choose the supported package/API and runtime/platform matrix.** There is no
   native gemspec yet. Native decimal/date/time values differ from the binding's
   `BigDecimal`/`Date`/`Time` conventions. Evidence currently covers Linux x86-64
   and the installed Ruby runtimes, not a broad shipping matrix.
4. **Accept ongoing ownership.** Promotion means maintaining roughly 40,600
   library lines across 151 files as features change, and running the appropriate
   checks in normal CI. The current frozen-experiment exception would need an
   explicit policy revision.

See [the latest conformance audit](SCALAR_FOLLOWUP.md#correctness-and-resource-checks),
[the reach-core testing requirement](../../spec/design/cores.md#42-a-reach-core-goal-2-sequential-triggered-by-contract-maturity),
[simulation runners](../../scripts/sim.rb), [generated-query runners](../../scripts/norec_gen.rb),
and [the experimental policy](../../spec/design/ruby-native-experiment.md).

## Verification and reproduction

All 168 memory timing cells and 12 file timing cells agree across API lanes on
values, column names/types, deterministic costs and consumed row counts. Scalar
and batched host kernels return identical values. Every requested JIT was
confirmed active. Each worker was capped at 512 MiB RSS, 2 GiB address space and
300 seconds; the largest sampled worker was 371.7 MiB. These are benchmark
worker limits, not whole-engine memory guarantees.

File-workload worker peaks were about 295–372 MiB for native Ruby and 91–93 MiB
for the binding. These include the runtime, JIT, setup/validation and engine
caches; they are process high-water marks, not isolated steady-state database
heap measurements.

The freshly built binding passes **89 seam tests / 3,093 assertions**. Its first
sandboxed run could not bind local Unix sockets; the permitted rerun passed.
The first file timing attempt hit a benchmark helper's missing return value;
no timings from that attempt are used. The corrected helper's file-only branch
does not change the already-recorded memory measurements or either engine.

The native library hash is unchanged from the scalar pass:
`7d1197be345d44a7c389a66ec0a18d038282d26d59fad97ab08f8bdd6f8a7a28`.
Artifacts include native/binding/Rust source hashes, release-library hash,
runtime executable hash, worker hash, actual JIT state, parameters, answers,
costs, callback sizes, all five samples and peak RSS.

Results: [4.0 YJIT memory](../../bench/ruby-native/results/binding-comparison/memory-4.0-yjit.json),
[4.1 YJIT memory](../../bench/ruby-native/results/binding-comparison/memory-4.1-yjit.json),
[4.1 ZJIT memory](../../bench/ruby-native/results/binding-comparison/memory-4.1-zjit.json),
[4.0 YJIT file](../../bench/ruby-native/results/binding-comparison/file-4.0-yjit.json),
[4.1 YJIT file](../../bench/ruby-native/results/binding-comparison/file-4.1-yjit.json),
[4.1 ZJIT file](../../bench/ruby-native/results/binding-comparison/file-4.1-zjit.json).
The [cross-report audit](../../bench/ruby-native/results/binding-comparison/summary.json)
checks 180 timing cells and 3,324 validation queries.

```sh
mise run ruby:build
mise run ruby-native:binding-compare -- --output /tmp/ruby-binding-memory.json
mise run ruby-native:binding-compare -- \
  --file .dev/ruby-native-bench-data/large.jed \
  --output /tmp/ruby-binding-file.json

# Add --verify-only to check answers without timing.
# Preview runtime, with either --jit yjit or --jit zjit:
mise run ruby-native:binding-compare -- \
  --ruby .dev/ruby-native-runtimes/ruby-4.1.0-preview1/bin/ruby --jit zjit \
  --output /tmp/ruby-binding-zjit.json
```

File measurements require the existing canonical dataset and permission to bind
local Unix sockets. The new task has no dependencies from normal tests,
benchmarks or CI. No engine, dependency or support-policy changes are included.
