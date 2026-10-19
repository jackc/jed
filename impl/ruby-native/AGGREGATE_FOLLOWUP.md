# Compiled primitive Ruby aggregates

The cursor failures reported below are historical. The
[cursor conformance correction](CURSOR_CONFORMANCE_FIX.md) fixes them and
supersedes the mixed-routing interpretation. Original measurements are retained.

Measured 2026-10-19, after the compiled ordering work at `8243d0bc`. This explicitly
requested extension keeps Ruby frozen, experimental, and opt-in. It changes no
dependency, public API, shared SQL contract, supported core, or normal task selection.

## Scope and semantic boundary

The memory executor can compile one resolved primitive aggregate into a Ruby
method. Supported reductions are COUNT, i16/i32 SUM widened to i64, integer and
boolean MIN/MAX, boolean AND/OR/EVERY, and integer bitwise AND/OR/XOR. Its argument
and optional FILTER must be completely supported by the existing scalar emitter.
DISTINCT, aggregate ORDER BY, ordered sets, windows, host calls, named/collatable
types, decimal and float reductions retain the existing executor. File hosts
retain their existing aggregation, including the streaming and spill paths.

The generated method preserves two phases. It first evaluates FILTER, charges
the aggregate input, and evaluates the argument, for every input in the same
order. It collects non-NULL scalars without the old one-element argument arrays.
Only after all inputs succeed does it reduce those values. A later argument
error therefore still wins over an earlier partial-sum overflow discovered during
reduction. Boolean aggregates also evaluate every input even when the answer
is already known. Each integer SUM addition retains its i64 overflow check.

Grouping, HAVING, projection, result caching, host callbacks, output charges,
and logical memory accounting retain their existing execution. The table-level
WHERE predicate also retains its ordinary evaluator. That limits the benefit of
a selective SUM whose time is dominated by scanning and filtering rather than
reducing values.

Generated source contains only fixed templates and validated numeric ordinals.
Names, values, and error text remain data. Methods receive the current engine,
group, and bindings; none is captured. A separate 64-entry LRU uses exact emitted
source and immutable scalar pools as keys, with materialization after reuse.
Lazy cursors share that bounded cache with their session. Kernels retain the
existing 192-node, 32-level, 256-pool-entry, and 32 KiB source limits. An exact
FILTER-plus-COUNT-star boundary test verifies that the implicit star cannot
exceed the node limit.

## Measurement method

The baseline is commit `8243d0bc660c116dc3ae77928328ea61e09037be`, with implementation
SHA-256 `ac5c78008942148b55e7f13f4a9c133d4bc1bc730741999a147e5d0dd9e4040d`.
The measured new implementation is
`04bfeb536b5e30381a8f5a05942afe16fdb9e3cc207f5c7a46d2344b138d2170`.
Hashes concatenate the contents of lexicographically sorted `lib/**/*.rb` paths.

All whole-query jobs use the unchanged 1,000-row fixture. Existing workloads
retain their previous schedules. New explicit workloads cover multiple
aggregates, nullable FILTER arguments, boolean/bit folds, and 100 groups; scalar
and DISTINCT queries are controls. Each query is fully consumed, GC stays enabled,
and every timed job retains five samples. Prepared, ordinary query, and eager
execution use their respective public Ruby APIs.

For each runtime, four fresh processes use the same final source in disabled,
enabled, enabled, disabled order. The switch disables only the new aggregate
tier; expression, scan-pipeline, and ordering compilation stay enabled. Main
tables use the mean of the two per-process medians in each state. This small
alternating experiment is not a confidence interval. Pre-change source timings
are retained separately. Ruby 4.0.7 interpreter/YJIT and 4.1.0-preview1 ZJIT are
different runtime builds, so absolute comparisons do not isolate the two JITs.

Raw measurements, validation digests, commands, runtime/source/harness hashes,
resource reports, and a reproducible summary are retained in
[aggregate-followup](../../bench/ruby-native/results/aggregate-followup/).
Timed jobs run sequentially, without overlapping tests, builds, or other
benchmarks. Final controls follow the completed correctness audits.

## Results

Stable Ruby 4.0.7 YJIT improves the six reduction-heavy shapes by **1.10–1.72×**
across the three APIs. Every one of those 18 shape/API comparisons has both
enabled run medians below both disabled medians. The simple selective SUM is
unchanged: its reduction touches only about 100 of the 1,000 scanned rows, and
ordinary WHERE evaluation remains the dominant work.

Mean paired medians, in microseconds per complete query:

| Query | Prepared disabled | Prepared compiled | Speedup | Eager disabled | Eager compiled | Speedup |
|---|---:|---:|---:|---:|---:|---:|
| Selective SUM | 294.78 | 295.40 | 1.00× | 286.21 | 287.20 | 1.00× |
| Grouped COUNT/SUM | 704.02 | 553.04 | 1.27× | 667.46 | 544.10 | 1.23× |
| Computed SUM argument | 746.41 | 641.39 | 1.16× | 756.15 | 635.55 | 1.19× |
| Six primitive aggregates | 989.10 | 594.30 | 1.66× | 1016.22 | 592.07 | 1.72× |
| Nullable arguments with FILTER | 1150.70 | 784.11 | 1.47× | 1166.76 | 793.21 | 1.47× |
| Boolean/bit folds | 1084.59 | 834.42 | 1.30× | 1091.90 | 798.87 | 1.37× |
| 100 groups | 1956.83 | 1718.29 | 1.14× | 1677.99 | 1400.30 | 1.20× |

Ordinary `query` improves 1.10–1.58× on those six shapes. Its selective SUM moves
1.5% slower; prepared/eager SUM move less than 0.4%. Scalar control timings shift
by at most 3%; DISTINCT timings shift by at most 4% despite using no
new aggregate kernel. Those control movements are not evidence of compilation
benefits for those queries.

The same six heavier shapes improve in every runtime:

| Runtime | Prepared gains | Ordinary query gains | Eager gains |
|---|---:|---:|---:|
| Ruby 4.0.7 interpreter | 1.12–1.67× | 1.11–1.68× | 1.10–1.76× |
| Ruby 4.0.7 YJIT | 1.14–1.66× | 1.10–1.58× | 1.19–1.72× |
| Ruby 4.1 preview ZJIT | 1.13–1.87× | 1.07–1.85× | 1.05–2.01× |

All 54 heavier-shape/API/runtime comparisons have both enabled medians below
both disabled medians. Selective SUM gains only 1.02–1.06× in the interpreter;
ZJIT is mixed, from 0.98× in ordinary queries to 1.06× in eager queries.
The unsupported ZJIT DISTINCT control ranges from 3.8% less time to 5.3% more time.
There is no uniform gain for queries that do not execute these reductions.

### Other cores and the Ruby Rust binding

The final prepared-query comparison uses the same fixture, parameters, warmup,
iteration counts, and fully consumed row checksums in all four cores. Microseconds
per query (each is the median of five samples):

| Query | Ruby YJIT | Go | Rust | TypeScript |
|---|---:|---:|---:|---:|
| Selective SUM | 282.48 | 64.91 | 75.10 | 212.49 |
| Grouped COUNT/SUM | 537.90 | 238.63 | 172.72 | 446.04 |
| Computed SUM argument | 622.66 | 274.11 | 224.45 | 521.40 |
| Six primitive aggregates | 581.31 | 248.64 | 224.24 | 456.85 |
| Nullable arguments with FILTER | 762.92 | 330.68 | 264.62 | 663.46 |
| Boolean/bit folds | 811.67 | 286.34 | 242.21 | 582.57 |
| 100 groups | 1785.72 | 337.53 | 256.38 | 557.99 |

The six heavier shapes take **2.25–5.29× Go's time** and **2.59–6.97× Rust's**.
Five of the six take 2.25–2.84× Go's time; the 100-group query is the larger gap.
TypeScript remains faster on all these aggregate queries. These are in-memory
measurements; the file audit verifies fallback correctness, not file speedups.
The cross-core adapter compares values and row counts, not cost counters.

A separate Ruby 4.0.7 YJIT run measures the actual Rust binding alongside all
three native Ruby APIs. Both libraries keep a long-lived connection/session;
the wrapper reparses and materializes every query. Its eager API is closest to
native `execute`. This run verifies equal values, metadata, and exact costs.

| Query | Rust binding | Native Ruby execute | Native / binding time |
|---|---:|---:|---:|
| Selective SUM | 66.94 | 295.43 | 4.41× |
| Grouped COUNT/SUM | 248.40 | 561.80 | 2.26× |
| Computed SUM argument | 269.10 | 643.98 | 2.39× |
| Six primitive aggregates | 280.27 | 604.66 | 2.16× |
| Nullable arguments with FILTER | 321.43 | 809.61 | 2.52× |
| Boolean/bit folds | 303.56 | 815.36 | 2.69× |
| 100 groups | 401.59 | 1441.72 | 3.59× |

The Rust binding still wins these aggregate workloads. The scalar control goes
the other way: native eager execution takes 5.55 µs versus the binding's 8.24 µs.
This extension changes no host-function execution and makes no new host-function
performance claim.

### Allocations, compilation, and JIT execution

The separate whole-query allocation profile compares the original source with
the final source using eager execution on stable YJIT. Median objects allocated
per complete query:

| Query | Before | Compiled |
|---|---:|---:|
| Grouped COUNT/SUM | 5176 | 3237 |
| Computed SUM argument | 2403 | 1666 |
| Six primitive aggregates | 7248 | 1649 |
| Nullable arguments with FILTER | 3357 | 1961 |
| Boolean/bit folds | 6105 | 2406 |
| 100 groups | 10116 | 7457 |

Scalar allocations stay at 39. Selective SUM changes from 909 to 901; the
unsupported DISTINCT control rises from 2637 to 2649 because of the admission
check. Profile tracing is separate from timing and is not used for the main
speedup claims.

Direct diagnostics isolate five reducers over 512 already-grouped rows. Their
control is the existing `compute_aggregate`, including its warmed expression
argument compiler. Values, exact costs, failure prefixes, and checksums agree.
Speedups are 2.00–6.12× in the interpreter, 3.42–7.76× with YJIT, and 4.12–7.72×
with preview ZJIT. Depending on the shape and group count, allocations per
aggregate call fall from 37–517 to one scalar array. These isolated gains are
larger than whole-query gains because scanning, grouping, and resolution remain.

Both JITs demonstrably compile all five tested generated methods: YJIT exposes
their compiled instruction counts and ZJIT logs their generated-method entries.
Across the runtimes and shapes, repeated compilation medians range from
9.8–118.8 µs, first compilation from 43.7–1796.9 µs, and first invocation from
4.4–862.0 µs. Those first-use costs are outside the warmed whole-query timings;
the bounded reuse policy helps amortize them but does not make one-off queries
free. Direct diagnostic workers peak below 76 MiB.

Cache churn generates 256 distinct methods, calls each 200 times, and retains
at most 64 cache entries. The original diagnostic's in-method post-GC snapshot
still sees 64 kernels under ZJIT, while the interpreter and YJIT see zero. A
separate probe repeats the churn inside a helper and collects after the helper
returns: all three runtimes then have zero live kernels after each of three
full collections. This supports dead stack references as the explanation for
the original ZJIT observation; both raw measurements are retained.

VM-retained classes are a separate limitation. After that helper returns, the
interpreter retains zero generated classes, YJIT retains 21, and ZJIT retains
all 256. A bounded engine cache does not bound Ruby's retained classes or JIT
machine code, and these measurements do not establish long-term reclamation.
The final artifact manifest hashes the completed ZJIT log; the diagnostic's
in-process log hash is an earlier snapshot because serialization itself can
trigger further JIT entries. Every saved per-job proof window appears in the
final log.

The remaining work is visible in the profile: selective SUM still dispatches
3,001 ordinary expression evaluations per query, and the 100-group query still
performs 3,300. Group construction, per-group projection/substitution, and
general-executor ordering also remain. Compiling those surrounding paths is a
separate target; extending reducer arithmetic alone will not remove this work.

## Correctness and resources

All final audits used the same source hash above. Corpus runs force compilation
on first use because most corpus statements execute only once.

| Audit | Passed / attempted | Compiled aggregate kernels |
|---|---:|---:|
| Memory execute, Ruby 4.1 preview ZJIT | 20,267 / 20,267 | 289 |
| File execute, Ruby 4.0.7 YJIT | 18,250 / 18,250 | 0 |
| Memory mixed query/execute, Ruby 4.0.7 YJIT | 20,267 / 20,267 | 289 |
| Memory pure query, Ruby 4.0.7 YJIT | 20,221 / 20,267 | 289 |

Each audit examines all 679 corpus files and preserves the existing storage-mode
skips: two memory files and 77 disk files. There are no unsupported files,
timeouts, memory-limit hits, or new failures. Every pure-query failure record
exactly matches the preceding ordering audit's 46 collector-threshold mismatches.
The mixed audit routes 159 finite-memory invocations to their authored `execute`
API and 19,928 others to `query`; it does not establish a full pure-query pass.
The [correctness follow-up](CORRECTNESS_FOLLOWUP.md) explains these collector
contracts. Zero aggregate kernels in the disk audit verifies its fallback.

All 53 host-test files pass. The new test covers primitive widths and reducers,
NULL/empty groups, FILTER, contextual parameters, deferred row faults, exact
statement/lifetime failure prefixes, input-error versus reduction-overflow
precedence, all four APIs, finite query memory, grouping sets/HAVING, host calls,
windows/file fallback, source safety and bounds, cache reuse/eviction, code
lifetimes, captured parameters, publications, schema and permission changes.

Workers retain the 512 MiB RSS guard, 2 GiB address-space limit, and 10-second
per-corpus-file deadline. The largest corpus worker used 177.85 MiB; the complete
host suite peaked at 198.30 MiB. These are test-process guards, not a whole-engine
heap guarantee for hosts.

## Reproduction

```sh
mise run ruby-native:test
JED_NATIVE_COMPILATION=immediate mise run ruby-native:conformance

mise run ruby-native:binding-compare -- \
  --workloads scalar,aggregate,group,aggregate_argument,aggregate_multi,aggregate_filter,aggregate_bool_bits,aggregate_many_groups,aggregate_distinct \
  --lanes native-prepared,native-query,native-eager \
  --jit yjit --output /tmp/aggregate-enabled.json
# Repeat with --disable-aggregate for the control.

JED_NATIVE_ADDRESS_SPACE_BYTES=2147483648 mise run ruby-native:bounded -- \
  ruby --yjit bench/ruby-native/aggregate_diagnostics.rb \
  --jit yjit --report /tmp/aggregate-diagnostic.json

JED_NATIVE_ADDRESS_SPACE_BYTES=2147483648 mise run ruby-native:bounded -- \
  ruby --yjit bench/ruby-native/aggregate_retention.rb \
  --jit yjit --report /tmp/aggregate-retention.json

RUBY_NATIVE_COMPARE_PLAN=bench/ruby-native/compare/aggregate-plan.json \
  RUBY_NATIVE_PHASE=aggregate-comparison RUBY_NATIVE_LANES=ruby-yjit,go,rust,ts \
  RUBY_NATIVE_COMPARE_RUBY_ADDRESS_SPACE_BYTES=2147483648 \
  mise run ruby-native:compare
```

Use `--jit none` for interpreter controls. For ZJIT, select the separately built
4.1 preview with `--ruby` in the binding comparison or invoke its binary with
`--zjit` in the bounded diagnostic. The saved diagnostic command includes the
per-method ZJIT log flag. The normal repository Ruby configuration is unchanged.
