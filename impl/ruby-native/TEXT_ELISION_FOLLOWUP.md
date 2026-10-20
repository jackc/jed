# Private text-comparison snapshots

> Historical report: raw evidence and run-specific drivers are on the preserved
> experiment branch. See the [archive and retrieval instructions](../../bench/ruby-native/ARCHIVE.md).
> Reproduction commands below refer to that original checkout.

The cursor failures reported below are historical. The
[cursor conformance correction](CURSOR_CONFORMANCE_FIX.md) fixes them and
supersedes the mixed-routing interpretation. Original measurements are retained.

Measured 2026-10-19. This explicitly requested optimization follows the C-text compiler at
`2f947c81012f6d1e9a29291b61dda6768409f510`. Ruby remains a frozen, opt-in
experiment; normal test, benchmark and CI tasks are unchanged.

## Scope

The preceding profile found 2,000 transient `CollatedText` wrappers per 1,000-row
text comparison query. The new path specializes a comparison only when it and
both immediate operands have admitted static C labels and those operands are
text literals, columns or parameters. It changes no SQL or public host API.

A frozen canonical literal already in the bounded pool needs no temporary copy.
A value whose exact class is String or CollatedText uses `String.new` at the
original wrapping point. This still copies its bytes before the other operand is read, and strips
singleton methods. Omitting that snapshot would change behavior when a later
read mutates the source string. A CollatedText input's old label would already
have been replaced by the child's static C label. Other subclasses, Bytea/UUID
and other unexpected values retain ordinary wrapping and fallback behavior.
The fallback restores each snapshot's own original C label before invoking the
existing helper; it never rereads either operand.

Mixed or unlabeled operands and casts retain the preceding compiler path.
All escaping text values keep their exact wrapper metadata. NULL propagation,
eager operand reads, boolean short-circuiting, error order, Unicode character
work, statement/lifetime costs and memory-account ownership remain unchanged.
Source/node/depth/pool/cache limits are unchanged. Runtime columns still allocate
private String snapshots; this is not an allocation-free comparison path.

## Evidence

The unchanged seven-query text plan compares the preceding source with the final
source in before/after/after/before process order on Ruby 4.0.7 interpreter,
4.0.7 YJIT and 4.1.0-preview1 ZJIT. Every result is consumed; GC stays enabled.
All preceding compiler tiers remain enabled in both sources. Each state is the
mean of two process medians, with five samples per process. This is an alternating
control, not a confidence interval. Absolute differences between Ruby versions
do not isolate the JIT implementation.

The source snapshot is archived from the preceding commit and identified by
`d65f93d3ce50d13866b970c096dcad9b2f9a1f988fa02844833d683971fd9ec7`.
The final implementation hash is
`ac3dfc224a5e38835bf5cddc3250f6ffb7678af917e805fb2692e3fed418f83a`.
Implementation hashes concatenate lexicographically sorted `lib/**/*.rb` contents.
The workload uses the same 1,000 short ASCII labels as the preceding report.
Direct diagnostics separately exercise metadata, changing inputs and Unicode;
whole-query timings do not establish long-string or linguistic performance.

Raw reports and a verified artifact manifest are retained in
[text-elision-followup](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/text-elision-followup`).

## Whole-query results

All five targets improve in all 45 runtime/API comparisons. For every target,
both final process medians are below both preceding-source process medians.
Across prepared, query and eager APIs, speedups span 1.18–1.67× for the
interpreter, 1.17–1.70× for YJIT, and 1.07–1.85× for ZJIT. These are incremental
gains over the preceding compiler, not gains over ordinary expression evaluation.

Prepared results use the mean of each source's two process medians. Times are
microseconds per fully consumed query; a larger speedup is better.

| Query shape | YJIT before → final, µs | Interpreter speedup | YJIT speedup | ZJIT speedup |
|---|---:|---:|---:|---:|
| Text range filter + sum | 610.59 → 402.84 | 1.36× | 1.52× | 1.50× |
| Text equality filter + count | 495.17 → 296.40 | 1.47× | 1.67× | 1.55× |
| Projection with text predicate | 1,155.05 → 894.63 | 1.18× | 1.29× | 1.14× |
| Integer sort key with text predicate | 1,068.98 → 836.17 | 1.28× | 1.28× | 1.29× |
| Integer group key with text predicate | 988.09 → 779.32 | 1.19× | 1.27× | 1.20× |

The unchanged scalar and LIKE controls range from 6.05% less elapsed time to
7.94% more. The largest increase is YJIT prepared LIKE: its two final medians
are 1,889.77 and 1,993.30 µs, versus 1,774.43 and 1,823.14 µs before. Every raw
sample is separated in that comparison, so it cannot be dismissed as a single
outlier. LIKE is outside this optimization's admitted surface.

An additional isolated LIKE-only YJIT source ABBA retains the same three APIs
and workload schedule, and runs after all other timers. Its before → final means
are 1,993.29 → 1,989.83 µs prepared (−0.17% elapsed), 1,924.98 → 1,936.36 µs
query (+0.59%), and 1,888.61 → 1,924.36 µs eager (+1.89%). The prepared source
separation does not reproduce. The unchanged profile calls and allocation counts
and this follow-up suggest workload/JIT/process context as possible causes;
they do not prove zero effect or erase the original mixed-workload result.

## Other cores and the Ruby binding

The separate prepared cross-core run checks equal result checksums and drained
row counts. It does not require cross-core cost equality. Medians are µs/query:

| Query shape | Ruby YJIT | Go | Rust | TypeScript | Ruby / Go |
|---|---:|---:|---:|---:|---:|
| Scalar integer control | 4.98 | 0.74 | 1.40 | 2.23 | 6.75× |
| Text range filter + sum | 418.50 | 176.50 | 106.83 | 1,802.36 | 2.37× |
| Text equality filter + count | 294.96 | 45.39 | 26.95 | 80.46 | 6.50× |
| Projection with text predicate | 875.08 | 471.19 | 266.09 | 1,878.39 | 1.86× |
| Integer sort key with text predicate | 895.20 | 403.21 | 237.90 | 1,410.58 | 2.22× |
| Integer group key with text predicate | 769.17 | 472.34 | 276.01 | 1,479.51 | 1.63× |
| LIKE fallback control | 1,941.07 | 149.27 | 137.51 | 316.58 | 13.00× |

All five targets are within 6.50× of Go; four are within 2.38×. Equality was
11.15× Go in the preceding report. These separately timed historical comparisons
include process/run variation. Equality is still 10.94× Rust, and the unchanged
LIKE control remains 13.00× Go; this result does not establish whole-surface
parity or justify promotion.

The actual Rust-backed Ruby library comparison validates values, column metadata
and exact costs. Eager native execution is its closest API match. These are
separate YJIT process medians, including the binding's boundary/result-conversion
costs:

| Query shape | Rust-backed Ruby, µs | Native Ruby eager, µs | Native / binding |
|---|---:|---:|---:|
| Scalar integer control | 8.07 | 5.33 | 0.66× |
| Text range filter + sum | 120.85 | 380.12 | 3.15× |
| Text equality filter + count | 35.88 | 274.03 | 7.64× |
| Projection with text predicate | 884.87 | 681.84 | 0.77× |
| Integer sort key with text predicate | 323.69 | 811.28 | 2.51× |
| Integer group key with text predicate | 343.40 | 792.84 | 2.31× |
| LIKE fallback control | 173.17 | 1,943.44 | 11.22× |

Native eager projection is 1.30× faster than the binding in this fixture. This
does not establish an advantage over Rust's engine: the direct Rust prepared
run is faster. No host-function execution path changes in this optimization.

## Allocation profile

The separate YJIT eager profile records allocation counts before any tracing:

| Query shape | Previous source | Final source | Reduction |
|---|---:|---:|---:|
| Scalar integer control | 39 | 39 | 0% |
| Text range filter + sum | 5,059 | 2,092 | 58.6% |
| Text equality filter + count | 4,986 | 2,019 | 59.5% |
| Projection with text predicate | 6,371 | 3,371 | 47.1% |
| Integer sort key with text predicate | 7,703 | 4,703 | 38.9% |
| Integer group key with text predicate | 7,575 | 4,608 | 39.2% |
| LIKE fallback control | 26,790 | 26,790 | 0% |

Each target now makes zero temporary `CollationRules.wrap` calls, down from
2,000. The column operand retains its private String snapshot; the frozen literal
needs neither a new String nor a wrapper. Profiles identify removed work and
allocations; traced counts are not CPU-time attribution.

## Direct methods, JIT and retention

Separate diagnostics compare the preceding compiled methods with the final
compiled methods using the same script. Each runtime has one before and one
final process; these are not ABBA controls. Scalar jobs measure a one-element
projection result, and row jobs measure complete 512-row loops. The unchanged
ordinary evaluator is a semantic control, with results and exact cost/lifetime
prefixes checked before and after timing. Fixtures include NULLs, Unicode,
changing/mutable inputs, wrapper metadata, row identity, and memory-account
ownership.

| Direct job | YJIT before → final, µs | Interpreter speedup | YJIT speedup | ZJIT speedup |
|---|---:|---:|---:|---:|
| Scalar text comparison projection | 0.333 → 0.143 | 1.83× | 2.33× | 2.25× |
| Equality filter | 187.197 → 87.501 | 2.13× | 2.14× | 3.30× |
| Nullable boolean/text filter | 56.682 → 58.918 | 1.42× | 0.96× | 2.01× |
| Same filter with memory accounting | 199.879 → 162.147 | 1.19× | 1.23× | 1.30× |
| Two integer keys containing text predicates | 481.306 → 278.746 | 1.66× | 1.73× | 2.06× |
| Escaping text projection control | 0.326 → 0.331 | 1.02× | 0.99× | 1.02× |
| Unlabeled text parameter control | 0.582 → 0.581 | 1.02× | 1.00× | 0.80× |

The nullable-filter YJIT result is 3.94% slower even though allocations fall
from 655 to 158 per call. ZJIT's unlabeled-parameter control is 25.75% slower
(0.56348 → 0.70861 µs), despite identical generated source and four allocations
per call. These exceptions remain in the results; the single before/final direct
processes do not isolate their causes. Both control methods have identical
before/final generated source in every runtime. All five final target methods
contain the new template, and all seven methods in both sources have direct
YJIT instruction-count or ZJIT per-method log-window proof of JIT compilation.

Final YJIT median template emission plus method creation ranges from 14.60 to
133.55 µs across the seven jobs. The first compilation measured for each job
ranges from 281.63 to 1,247.46 µs and its first invocation from 6.32 to
2,984.65 µs. These first-use timings include prior process/JIT state and remain
separate from hot calls; they do not predict arbitrary-query amortization.
Every runtime retains all 30 compilation samples and its first-call observation.

The churn probe creates 256 distinct kernels and calls each 200 times through
the actual second-use admission and 64-entry LRU. Each final cache retains
64 kernels, 87,360 source bytes and 768 copied string-pool bytes. The larger
snapshot template increases retained source from the baseline's 63,936 bytes;
the bounds and pool size are unchanged. After the owning thread exits and its
reference is released, all three full-GC observations leave zero live kernel
instances in every runtime. VM classes and code remain:

| Final-source runtime | Anonymous kernel classes before → after | Process JIT code region before → after |
|---|---:|---:|
| Interpreter | 4 → 4 | — |
| YJIT | 33 → 43 | 1.61 → 5.48 MiB |
| ZJIT | 21 → 277 | 2.72 → 7.46 MiB |

YJIT reports no freed code; ZJIT reports 12,795,946 total JIT bytes afterward,
including its other allocation/frame regions. These counters include helper
methods and are not solely query code. The engine's cache bounds therefore
continue to provide no bound on process-wide JIT retention.

## Correctness and resource checks

All 56 host-test files pass. Focused interpreter and YJIT tests compare against
the independent evaluator at every small statement/lifetime cost prefix. They
cover changes to an operand during the next read or work-charge callback,
singleton comparison/length methods, frozen and mutable String/CollatedText
values, incoming C/Unicode/conflict/none labels, per-child fallback labels,
Bytea/UUID and subclasses, eager deferred faults, boolean branches, and escaping
metadata across expression, pipeline, aggregate and relation consumers.
Admission assertions require the new template on eligible roots and reject it
on mixed/unlabeled or cast operands.

All corpus lanes force first-use compilation on the frozen final source:

| Audit | Passed / attempted | Materialized kernels containing the snapshot template |
|---|---:|---:|
| Memory execute, preview ZJIT | 20,267 / 20,267 | 213 |
| File execute, YJIT | 18,250 / 18,250 | 83 |
| Memory mixed query/execute, YJIT | 20,267 / 20,267 | 214 |
| Memory pure query, YJIT | 20,221 / 20,267 | 214 |

Each audit examines all 679 files. The two memory skips and 77 file skips are
unchanged. All 46 pure-query failure objects exactly match the preceding text
audit's collector-threshold differences; the mixed lane routes 159 finite-memory
invocations to their authored execute API and 19,928 other invocations to query.
It does not establish a full pure-query pass. See
[CORRECTNESS_FOLLOWUP.md](CORRECTNESS_FOLLOWUP.md) for those contracts.
No new failures, unsupported files, timeouts or memory-limit hits occurred.
File hosts still use their ordinary general aggregation and relation loops;
their compiled aggregate/relation counters remain zero.

The kernel counters record materialization, not dispatch frequency. Focused
tests and direct generated-method diagnostics provide separate execution checks.
Workers retain the 512 MiB RSS guard and 2 GiB address-space limit; corpus files
have a 10-second deadline. The largest corpus worker used 175.62 MiB and the full
host suite peaked at 231.16 MiB. These guards are not whole-engine heap limits.

The evidence verifier checks all stored values/costs, source/runtime/harness
hashes, schedules, JIT proofs and corpus outcomes before writing its summary.
After timing, `compact.rb` removes JSON whitespace only. It verifies equality
of every parsed value and records original/final byte hashes plus canonical
semantic hashes in `serialization.json`; no samples, metadata or cost-prefix
outcomes are removed or rounded. Logs remain unchanged. `manifest.json` hashes
the final artifact files.

## Reproduction

The saved `run-*.json` journals contain the exact serial commands, runtime paths,
environment, source hashes and start/end times. `run.rb` checks both source
snapshots before and after every phase and refuses to replace existing output
unless explicitly passed `--replace`. Run benchmark phases serially, with no
concurrent test or build work.

Individual opt-in checks and measurements can also be reproduced with:

```sh
mise run ruby-native:test
JED_NATIVE_COMPILATION=immediate mise run ruby-native:conformance

mise run ruby-native:binding-compare -- --jit yjit \
  --lanes native-prepared,native-query,native-eager \
  --workloads scalar,general_text_filter,text_equality_filter,text_predicate_projection,text_predicate_sort,text_predicate_group,text_like_fallback \
  --output /tmp/text-snapshots-final.json

RUBY_NATIVE_COMPARE_PLAN=bench/ruby-native/compare/text-plan.json \
  RUBY_NATIVE_PHASE=text-snapshots RUBY_NATIVE_LANES=ruby-yjit,go,rust,ts \
  RUBY_NATIVE_COMPARE_RUBY_ADDRESS_SPACE_BYTES=2147483648 \
  mise run ruby-native:compare

JED_NATIVE_ADDRESS_SPACE_BYTES=2147483648 mise run ruby-native:bounded -- \
  ruby --yjit bench/ruby-native/elision_diagnostics.rb --jit yjit \
  --expected-elision final --report /tmp/text-snapshots-diagnostic.json
```

Use `--jit none` for interpreter binding comparisons and the preview binary's
`--ruby` path with `--jit zjit` for ZJIT. For direct diagnostics, select that
binary with `--zjit`, adding the saved per-method logging flags to obtain direct
JIT proof. `--native-source` selects the archived library for the binding
comparison; `RUBY_NATIVE_SOURCE` and `--expected-elision before` select it for the
direct diagnostic. The source archive must contain the preceding commit's
`impl/ruby-native/lib` tree. No task enables or configures a JIT for normal library
use.
