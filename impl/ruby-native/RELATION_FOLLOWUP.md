# Compiled materialized filters and grouping keys

Measured 2026-10-19. This explicitly requested extension follows the primitive aggregate compiler at
`fa4cbcc18dd15ee5af577ece91a01e20d1c28e4b`. Ruby remains a frozen, opt-in experiment;
normal tests, benchmarks, CI, supported cores, and public APIs are unchanged.

## Scope

The general memory executor can compile two separate operations over materialized
arrays: its final residual WHERE filter and its complete GROUP BY key matrix.
Both reuse the bounded integer/boolean scalar emitter. Each generated method
contains the row loop as well as the expression operations, so Ruby's JIT can
optimize the entire loop.

Filtering retains accepted row objects and input order. All source materialization,
access-path decisions, scan charges, and scan errors precede it. Under finite
query memory, each rejected row releases exactly its existing logical bytes
after predicate evaluation; the original owner transfer remains in place.

Every predicate finishes before any grouping key runs. This preserves the case
where a later row's WHERE error must precede an earlier survivor's key error.
Keys run row-major, left-to-right, and the full matrix finishes before group
construction. Grouping sets, hash grouping, aggregate reduction, HAVING,
projection, and grouped-expression substitutions retain their existing paths.

Unsupported complete expressions select the ordinary evaluator. There is no
captured fallback subtree. File hosts and non-array row sequences retain their
original loops, as do pushed-down source filters and join algorithms.

Source uses only fixed templates and validated numeric ordinals. Literals and
diagnostics remain in an immutable scalar pool. A 64-entry LRU uses exact source
and pool values, with compilation after reuse and the existing per-method bounds:
192 nodes, 32 levels, 256 pool entries, and 32 KiB source. Engines, rows, parameters,
and memory owners are passed on each invocation, never captured. Lazy cursors
share this bounded session cache. An internal relation-disable switch isolates
this tier while leaving all previous compilers active.

## Measurements

The baseline implementation hash is
`04bfeb536b5e30381a8f5a05942afe16fdb9e3cc207f5c7a46d2344b138d2170`.
The final measured implementation hash is
`c23bce9c406d04797d08f2bd155ba5380ba61521df6aeec4a923f9df5831d276`.
Hashes concatenate lexicographically sorted `lib/**/*.rb` file contents.

The unchanged 1,000-row fixture is used throughout. Existing queries retain
their earlier schedules. Two explicit additions cover combined WHERE/nullable
group keys and an unsupported text-predicate control. Every result is fully
consumed and GC stays enabled. Prepared, ordinary query, and eager execution
use their respective public Ruby APIs.

For each runtime, four fresh processes run disabled, enabled, enabled, disabled
against the same final implementation. The switch disables only this tier.
Summary speedups use the mean of the two per-process medians in each state;
each process retains five samples. This alternating check is not a confidence
interval. Separate pre-change source measurements and allocation profiles are
also retained. Ruby 4.0.7 YJIT and 4.1.0-preview1 ZJIT are different builds;
their absolute timings do not isolate JIT implementation effects.

Raw reports, commands, source/runtime/harness hashes, resource guards, summaries,
and the artifact manifest live in
[relation-followup](../../bench/ruby-native/results/relation-followup/).
Timed jobs run serially, without overlapping tests, builds, or other benchmarks.

## Whole-query results

The four queries containing materialized predicates or computed grouping keys
improve consistently. For each of their 36 runtime/API combinations, both
enabled process medians are below both disabled medians.

| Runtime | Prepared gains | Ordinary query gains | Eager gains |
|---|---:|---:|---:|
| Ruby 4.0.7 interpreter | 1.08–1.72× | 1.10–1.74× | 1.13–1.97× |
| Ruby 4.0.7 YJIT | 1.11–1.38× | 1.05–1.33× | 1.13–1.44× |
| Ruby 4.1 preview ZJIT | 1.16–1.79× | 1.15–1.73× | 1.27–2.03× |

Stable YJIT mean paired medians, in microseconds per complete query:

| Query | Prepared disabled | Prepared compiled | Speedup | Eager disabled | Eager compiled | Speedup |
|---|---:|---:|---:|---:|---:|---:|
| Selective SUM | 300.46 | 259.16 | 1.16× | 290.82 | 242.78 | 1.20× |
| Computed SUM argument + WHERE | 664.08 | 589.21 | 1.13× | 640.94 | 567.66 | 1.13× |
| 100 computed groups | 1709.75 | 1543.68 | 1.11× | 1381.99 | 1218.38 | 1.13× |
| WHERE + nullable computed keys | 1845.79 | 1339.91 | 1.38× | 1670.32 | 1160.46 | 1.44× |
| Plain-column grouping | 561.31 | 541.45 | 1.04× | 550.35 | 527.59 | 1.04× |

Plain-column grouping is a weaker result: YJIT's ordinary-query API takes 1.8%
more time, with overlapping process medians. Interpreter ordinary/eager grouping
also has overlapping medians. Its small apparent gains do not establish a
consistent improvement across APIs.

Controls matter: scalar, six-aggregate-without-WHERE, and unsupported text-filter
queries execute no new relation loop. YJIT ordinary-query controls take roughly
3.8–4.5% more time in enabled runs, while its prepared/eager controls generally
move faster. Those shifts are not compilation benefits or proof of regressions
in unrelated operators. The repeated experiment supports the four targeted
shapes, not a universal improvement in arbitrary queries.

## Cross-core and Ruby binding comparison

The final prepared-query run uses the same fixture, parameters, schedules, and
fully consumed row checksums in all four cores. Microseconds per query, median
of five samples:

| Query | Ruby YJIT | Go | Rust | TypeScript |
|---|---:|---:|---:|---:|
| Selective SUM | 262.80 | 65.21 | 75.13 | 165.16 |
| Computed SUM argument + WHERE | 589.89 | 299.45 | 229.70 | 529.60 |
| 100 computed groups | 1550.38 | 390.16 | 267.62 | 554.03 |
| WHERE + nullable computed keys | 1342.89 | 407.28 | 300.37 | 698.90 |
| Plain-column grouping | 554.92 | 254.50 | 185.98 | 417.74 |
| Six aggregates, no WHERE | 596.81 | 283.04 | 244.07 | 445.87 |
| Unsupported text predicate | 2307.94 | 182.50 | 110.71 | 1677.39 |

The four targeted shapes take **1.97–4.03× Go's time** and **2.57–5.79× Rust's**.
TypeScript remains faster on these queries. The unsupported text control is
still 12.65× Go and 20.85× Rust; this extension does not improve general string
evaluation. These are in-memory measurements. The cross-core adapter checks
values and row counts, not cost counters, and these fresh timings should not
be treated as a controlled before/after comparison with earlier Go/Rust runs.

A separate stable-YJIT run compares the actual Rust-backed Ruby library with
all three native APIs, verifying equal values, metadata, and exact costs.
Both keep a long-lived connection/session. The binding reparses and materializes
each query, so native `execute` is the closest API comparison:

| Query | Rust binding | Native execute | Native / binding time |
|---|---:|---:|---:|
| Selective SUM | 63.25 | 244.12 | 3.86× |
| Computed SUM argument + WHERE | 270.48 | 559.88 | 2.07× |
| 100 computed groups | 393.45 | 1247.07 | 3.17× |
| WHERE + nullable computed keys | 369.97 | 1166.11 | 3.15× |
| Plain-column grouping | 238.55 | 533.11 | 2.23× |
| Six aggregates, no WHERE | 280.27 | 576.86 | 2.06× |
| Unsupported text predicate | 120.48 | 2225.35 | 18.47× |

The Rust binding continues to win these aggregate queries. The scalar control
still favors native Ruby: 5.32 µs eager versus 8.22 µs through the binding.
Host-function execution is unchanged; no new host-function speedup is claimed.

## Allocations and remaining overhead

The separate YJIT eager profile compares original and final source. Median objects
allocated per complete query:

| Query | Before | Compiled |
|---|---:|---:|
| Selective SUM | 901 | 986 |
| Computed SUM argument + WHERE | 1666 | 1751 |
| 100 computed groups | 7457 | 7561 |
| WHERE + nullable computed keys | 5348 | 5230 |
| Plain-column grouping | 3237 | 3287 |
| Six aggregates, no WHERE | 1649 | 1649 |
| Unsupported text predicate | 14918 | 14944 |

Scalar allocations remain 39. This is primarily an execution-dispatch improvement,
not a general allocation reduction. Source-and-pool lookup currently emits a
bounded program on each fresh resolution; those setup objects increase several
totals. It avoids compiling a new Ruby method on each lookup, but does not avoid
emission. Smaller or one-off queries may not amortize this work.

Ordinary expression evaluations fall from 3,001 to one for selective SUM and
from 3,300 to 300 for 100 computed groups. Its remaining grouped projections,
substitutions, and comparisons still use the general executor. Selective SUM
also retains roughly 1,000 contextual parameter casts per execution; complete
query setup and those casts now matter more. The text control still performs
3,001 ordinary evaluations. These are concrete further opportunities, not work
completed by this extension. Profile tracing is separate from timing and does
not supply the main speedup claims.

## Direct compilation and lifetime diagnostics

Six isolated jobs operate on 512 already-materialized rows: parameterized and
nullable filters, an accounted filter, and plain/computed/nullable key vectors.
Their ordinary controls retain the existing `select` and nested `map` behavior.
Values, errors, original filter-row identity/order, exact costs, small-budget
failure prefixes, and rejected-row accounting agree before and after timing.

Direct phase speedups are 1.70–4.43× in the interpreter, 1.31–4.40× with YJIT,
and 1.43–4.91× with ZJIT. YJIT's accounted filter improves only 1.31× because
byte inspection and memory release stay in place. Its plain key matrix improves
1.56× in isolation, which does not imply that much improvement in complete
grouped queries.

Direct allocations are mostly unchanged: one array for an unaccounted filter,
454 objects for the accounted fixture, and 513 for plain/computed key matrices.
The nullable key vector falls from 758 to 513. The full row-major matrix is still
materialized; grouping/storage are excluded from these isolated timings.

Both JITs directly compile all six generated methods, verified through YJIT
instruction counts and per-job ZJIT log windows. Repeated compile medians range
from 9.7–184.9 µs; first compilation from 75.7–2041.0 µs and first invocation
from 15.1–2796.1 µs. Those first-use measurements include runtime warmup effects
and are outside the warmed query timings. Direct workers peak below 67 MiB.

Cache churn generates 256 distinct methods, calls each 200 times, and retains
at most 64 cache entries. In the warmed main diagnostic, interpreter/YJIT release
all kernel instances after the population helper returns; ZJIT still observes
64 after three full collections. That raw observation is preserved.

A separate fresh process checks after the entire churn method returns, and
then after a second population batch runs in a completed thread. All runtimes
observe zero kernels after each of three collections at both boundaries. The
fresh ZJIT probe also sees zero at the inner boundary, so it does not reproduce
the warmed diagnostic's 64. These results support execution-state retention,
but do not identify the precise JIT root behind the original observation.

VM classes and code have longer lifetimes. After the two 256-kernel batches,
the interpreter retains zero generated classes, YJIT retains 21, and ZJIT retains
512, with about 9.13 MB in reported whole-process JIT memory. The engine's bounded
cache therefore does not bound VM-retained classes or machine code. Neither
immediate code reclamation nor a whole-engine memory ceiling is established.

The diagnostic records the exact byte length/hash of its ZJIT-log snapshot.
The final audit checks that prefix and all per-job generated-method proof windows
against the completed log; the artifact manifest hashes the full log as well.

## Correctness and resource checks

All final audits use the final source hash above and force first-use compilation
because most corpus statements execute once.

| Audit | Passed / attempted | Compiled relation kernels |
|---|---:|---:|
| Memory execute, Ruby 4.1 preview ZJIT | 20,267 / 20,267 | 443 |
| File execute, Ruby 4.0.7 YJIT | 18,250 / 18,250 | 0 |
| Memory mixed query/execute, Ruby 4.0.7 YJIT | 20,267 / 20,267 | 381 |
| Memory pure query, Ruby 4.0.7 YJIT | 20,221 / 20,267 | 381 |

Each audit examines all 679 files, preserving the existing two memory skips and
77 disk skips. There are no new failures, unsupported files, timeouts, or memory
limit hits. All 46 pure-query failure records exactly match the aggregate
baseline's collector-threshold differences. The mixed audit routes 159
finite-memory invocations to their authored execute API and 19,928 others to
query; it is not a full pure-query pass. See the
[correctness follow-up](CORRECTNESS_FOLLOWUP.md) for those collector contracts.
The disk audit's zero relation kernels confirms its fallback.

All 54 host-test files pass. The new internal suite compares compiled and ordinary
execution across all four Ruby APIs, all small statement/lifetime budget prefixes,
NULL/filter control flow, row-major key failures, rejected-row ownership release
(including nil owners), original row identity, source-fault precedence, grouping
sets, folded subqueries, snapshots, changed schemas and permissions, host callbacks,
source bounds, cache eviction, and file/non-array fallbacks.

A lifetime probe retains the kernel while checking that its engine, input, and
parameters are collected. It creates those objects in a completed thread to
remove conservative argument-stack roots; the original same-thread probe was
intermittent under the interpreter. All three collection assertions remain.
Focused interpreter and YJIT runs both pass without a core change.

Every worker retains the 512 MiB RSS guard and 2 GiB address-space limit. Corpus
files retain their 10-second deadline; the full host suite peaks at 122.19 MiB.
The largest corpus worker uses 167.65 MiB.
These are process guards, not a whole-engine heap guarantee.

## Reproduction

```sh
mise run ruby-native:test
JED_NATIVE_COMPILATION=immediate mise run ruby-native:conformance

mise run ruby-native:binding-compare -- \
  --workloads scalar,aggregate,aggregate_argument,aggregate_many_groups,group,aggregate_multi,general_where_keys,general_text_filter \
  --lanes native-prepared,native-query,native-eager \
  --jit yjit --output /tmp/relation-enabled.json
# Repeat with --disable-relation for the control.

JED_NATIVE_ADDRESS_SPACE_BYTES=2147483648 mise run ruby-native:bounded -- \
  ruby --yjit bench/ruby-native/relation_diagnostics.rb \
  --jit yjit --report /tmp/relation-diagnostic.json

JED_NATIVE_ADDRESS_SPACE_BYTES=2147483648 mise run ruby-native:bounded -- \
  ruby --yjit bench/ruby-native/relation_retention.rb \
  --jit yjit --report /tmp/relation-retention.json

RUBY_NATIVE_COMPARE_PLAN=bench/ruby-native/compare/relation-plan.json \
  RUBY_NATIVE_PHASE=relation-comparison RUBY_NATIVE_LANES=ruby-yjit,go,rust,ts \
  RUBY_NATIVE_COMPARE_RUBY_ADDRESS_SPACE_BYTES=2147483648 \
  mise run ruby-native:compare
```

Use `--jit none` for interpreter controls. For ZJIT, select the separately built
4.1 preview with `--ruby` in the binding comparison or invoke its binary with
`--zjit` in the bounded diagnostic. The saved diagnostic command includes the
per-method ZJIT log flag. Normal repository runtime configuration is unchanged.
