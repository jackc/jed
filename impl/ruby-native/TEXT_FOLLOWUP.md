# Compiled C-text comparisons

Measured 2026-10-19. This explicitly requested extension follows the materialized
relation compiler at `8347075b8b235bba4a3a173460ba74963f7049aa`. Ruby remains a frozen,
opt-in experiment. Normal tests, benchmarks, CI, supported cores, and public APIs
are unchanged.

## Scope and contracts

The existing scalar emitter now admits plain text literals, columns, parameters,
identity text casts, and the seven comparisons `=`, `<>`, `!=`, `<`, `<=`, `>`,
`>=` under the fixed `C` collation. This extends complete expression trees inside
the existing compiled projection, pipeline, aggregate-input, and materialized
relation loops. Integer CASE results can contain text comparison conditions.

It does not add general string functions, concatenation, text-returning CASE,
explicit COLLATE expression nodes, text ORDER BY comparators, or text aggregate
reductions. Static linguistic, conflicting, indeterminate, and nominal types
retain their original evaluator. No planner, access path, row source, grouping,
storage, or host callback algorithm changes.

Text results retain their exact `CollatedText` wrapper metadata. Generated code
uses fixed, frozen compiler-owned C labels, preserving nil/false/true flags;
it never retains a resolved label or host-loaded collation table. Text literals
remain copied, frozen pool data and never become Ruby source. String-subclass
literals are rejected from the pool. Copied strings have a cumulative 32 KiB
budget per kernel, checked before duplication; oversized literals select the
evaluator. Existing node/depth/entry/source bounds and second-use admission
remain in force. Failed root admission restores the copied-byte budget before
the next root is considered.

Both comparison operands are evaluated before the NULL decision. The operator
charge precedes those reads; variable-width work follows them. The charge uses
the shorter operand's Unicode character length, minus the already charged unit,
exactly as the ordinary evaluator does. Boolean short-circuiting, filter NULL
short-circuiting, lazy CASE branches, and statement/lifetime failure prefixes
keep their original order.

For a statically C comparison, generated guards distinguish a freshly wrapped
C child from an unlabeled parameter. Exact ordinary strings and those proven
C wrappers compare directly after the same character-work charge. Unexpected
classes and dynamic labels use a fallback helper preserving the original
comparison semantics. A dynamic
`CollatedText` parameter cannot pass merely because another operand supplied a
static C label. The direct branch removes generic type/collation dispatch and
the temporary array used to compute the shorter length; it retains text wrappers.

The internal `--disable-text` diagnostic rejects only the new text admission.
All preceding compiler tiers remain active. Separate mode slots prevent warmed
text methods from bypassing that switch; source-and-pool caches remain bounded.
Ordinary operation has one 64 KiB source state per resolved plan. Toggling the
internal diagnostic can retain two independent states, up to 128 KiB per plan;
the shared 64-entry LRUs retain their existing combined limits.
The library does not enable or configure a JIT.

## Measurement method

The baseline implementation hash is
`c23bce9c406d04797d08f2bd155ba5380ba61521df6aeec4a923f9df5831d276`.
The final implementation hash is
`d65f93d3ce50d13866b970c096dcad9b2f9a1f988fa02844833d683971fd9ec7`.
Implementation hashes concatenate lexicographically sorted `lib/**/*.rb` contents.
The baseline is an archive of the preceding commit, executed with the same final
benchmark harness and plan.

Seven workloads use the unchanged 1,000-row fixture, with short ASCII labels
`row0` through `row999`. The existing scalar and
text-filter queries retain their schedules. Additional jobs cover equality,
projection, integer sort keys, and integer grouping keys containing text
predicates. LIKE remains an unsupported control. Real text parameter bindings,
NULLs, Unicode, and changing/mutable inputs have separate direct diagnostics;
the cross-core plan's parameter protocol is unchanged.
The whole-query timings do not establish long-string or linguistic-collation
performance.

For each runtime, four fresh processes run disabled, enabled, enabled, disabled
against identical final source. The main statistic is the mean of the two
per-process medians per state; each process records five samples. This is an
alternating control, not a confidence interval. Every result is fully consumed,
GC remains enabled, and timed jobs run serially without tests or builds.

Ruby 4.0.7 interpreter/YJIT and 4.1.0-preview1 ZJIT are distinct runtime builds;
their absolute timings do not isolate the effect of the JIT implementation.
Cross-core prepared timings compare equal row values/counts, not cost counters.
The separate actual Rust-backed Ruby library comparison verifies values,
metadata, and exact costs; native eager execution is its closest API match.

Raw reports, runtime/source/harness hashes, guards, complete commands, the
derived summary, and artifact manifest are in
[text-followup](../../bench/ruby-native/results/text-followup/).

## Whole-query results

The five target shapes improve in all 45 runtime/API comparisons. Both enabled
process medians are lower than both disabled medians in every target comparison.
Across the three APIs, speedups span 3.91–5.13× for the interpreter, 2.41–3.70×
for YJIT, and 2.64–4.28× for ZJIT. The scalar and LIKE controls change by between
−4.31% and +3.85% elapsed time; they provide no evidence of a material improvement.

Prepared-query results below use the paired final-source controls. Times are
microseconds per fully consumed query; a larger speedup is better.

| Query shape | YJIT disabled → enabled, µs | Interpreter speedup | YJIT speedup | ZJIT speedup |
|---|---:|---:|---:|---:|
| Text range filter + sum | 2,183.27 → 610.39 | 4.66× | 3.58× | 3.43× |
| Text equality filter + count | 1,270.32 → 493.82 | 4.30× | 2.57× | 2.64× |
| Projection with text predicate | 3,263.66 → 1,162.31 | 3.94× | 2.81× | 3.03× |
| Integer sort key with text predicate | 2,874.25 → 1,018.27 | 4.71× | 2.82× | 3.46× |
| Integer group key with text predicate | 2,895.02 → 986.58 | 4.00× | 2.93× | 2.95× |

The archived preceding commit's YJIT prepared medians for these five shapes are
2,010.84, 1,141.71, 3,196.52, 2,935.83, and 2,937.89 µs respectively. The final
enabled means therefore improve by 3.29×, 2.31×, 2.75×, 2.88×, and 2.98× against
that historical run. The paired controls above are the stronger isolation of this
change; the historical comparison also includes process and time-of-run variation.
All interpreter/ZJIT historical measurements and every API's samples are retained.

The separate cross-core prepared run produces these fully drained medians:

| Query shape | Ruby YJIT, µs | Go, µs | Rust, µs | TypeScript, µs | Ruby / Go |
|---|---:|---:|---:|---:|---:|
| Scalar integer control | 4.94 | 0.80 | 1.38 | 2.19 | 6.15× |
| Text range filter + sum | 621.43 | 174.44 | 109.88 | 1,598.76 | 3.56× |
| Text equality filter + count | 504.95 | 45.28 | 27.91 | 83.72 | 11.15× |
| Projection with text predicate | 1,260.58 | 469.75 | 276.95 | 1,862.17 | 2.68× |
| Integer sort key with text predicate | 1,117.56 | 402.30 | 247.59 | 1,335.46 | 2.78× |
| Integer group key with text predicate | 1,051.76 | 441.31 | 285.32 | 1,468.45 | 2.38× |
| LIKE fallback control | 1,990.34 | 143.07 | 148.47 | 315.41 | 13.91× |

Four target shapes are now within 3.6× of Go; equality remains 11.2× slower.
Ruby is 3.69–5.66× Rust on those four shapes and 18.09× on equality. These
results do not establish parity across the SQL surface or justify promotion.

The actual Rust-backed Ruby library has additional boundary and result-conversion
costs. Its eager API comparison, on Ruby 4.0.7 YJIT, is:

| Query shape | Rust-backed Ruby, µs | Native Ruby eager, µs | Native / binding |
|---|---:|---:|---:|
| Scalar integer control | 7.78 | 5.32 | 0.68× |
| Text range filter + sum | 116.14 | 572.02 | 4.93× |
| Text equality filter + count | 34.65 | 505.38 | 14.59× |
| Projection with text predicate | 847.81 | 923.75 | 1.09× |
| Integer sort key with text predicate | 309.21 | 1,020.03 | 3.30× |
| Integer group key with text predicate | 332.34 | 1,103.84 | 3.32× |
| LIKE fallback control | 175.29 | 1,981.82 | 11.31× |

These are separately measured process medians, not substitutions into the ABBA
controls. No host-function execution path changes in this extension.

## Allocation profile and remaining work

The separate YJIT eager profile counts allocations per query before any tracing:

| Query shape | Previous source | Final source |
|---|---:|---:|
| Scalar integer control | 39 | 39 |
| Text range filter + sum | 14,944 | 5,059 |
| Text equality filter + count | 5,871 | 4,986 |
| Projection with text predicate | 16,930 | 6,371 |
| Integer sort key with text predicate | 18,259 | 7,703 |
| Integer group key with text predicate | 17,975 | 7,575 |
| LIKE fallback control | 26,785 | 26,790 |

The target queries still construct 2,000 `CollatedText` wrappers apiece: two
operands for each of 1,000 rows. They also retain the cost-meter calls at their
original positions. For equality, most allocation remains despite the dispatch
speedup. A future wrapper-elision experiment would need to prove that no transient
label or host-visible value changes and retain the dynamic-label fallback.
Counts identify work to investigate; they are not a time attribution.

C-text ORDER BY comparators are another bounded compilation candidate. This
extension only measures integer sort keys containing text predicates; actual
text comparators still use the evaluator's ordinary ordering path. A follow-up
must measure real text sort keys, including NULLs, ties, long prefixes and file
spill, and preserve ordering's distinct error and cost rules. Neither candidate
is implemented here.

## Direct methods, compilation and JIT retention

Separate diagnostics use real changing text bindings, NULLs and Unicode. Scalar
jobs measure one expression call; row jobs measure complete 512-row filter/key
loops. They compare against ordinary evaluation, so their speedups do not isolate
text admission from the previously implemented loop compilers.

| Direct job | YJIT ordinary → compiled, µs | Interpreter speedup | YJIT speedup | ZJIT speedup |
|---|---:|---:|---:|---:|
| Text parameter comparison | 2.025 → 0.529 | 5.38× | 3.83× | 5.38× |
| Text identity cast | 0.614 → 0.304 | 4.34× | 2.02× | 3.51× |
| Equality filter | 636.49 → 291.42 | 5.06× | 2.18× | 4.59× |
| Nullable boolean/text filter | 383.45 → 91.68 | 5.13× | 4.18× | 4.17× |
| Same filter with memory accounting | 518.38 → 240.35 | 3.26× | 2.16× | 3.33× |
| Two integer keys containing text predicates | 1,839.12 → 593.82 | 6.79× | 3.10× | 6.20× |

Allocations per call respectively fall from 13 to 4, remain at 2, and fall from
1,969 to 1,457, 1,911 to 485, 2,368 to 942, and 8,939 to 3,937. Validation before
and after timing checks values, exact wrapper labels, costs, lifetime costs,
changing/mutated inputs, and memory-account ownership. YJIT instruction counts
and ZJIT per-job log windows independently prove all six generated methods were
JIT-compiled; enabling the process JIT alone is not the evidence.

Across these six jobs, YJIT's median template emission plus Ruby method creation
cost is 7.68–105.71 µs after warming the compiler. First compilation is slower:
86.26–1,359.94 µs, and first invocation costs 23.62–3,159.28 µs. These timings
retain first-use work and possible JIT compilation separately from the hot-call
measurements. They are not estimates of amortization for arbitrary queries.
Every runtime's 30 compilation samples and first invocation are retained.

The churn probe creates 256 distinct kernels, calls each 200 times, and uses the
real second-use admission and 64-entry LRU. At the cache snapshot it retains
exactly 64 kernels, 63,936 source bytes and 768 copied string-pool bytes. After
the owning thread exits and its reference is released, three full collections
leave zero live kernel instances in all runtimes. VM retention differs:

| Runtime | Anonymous kernel classes before → after | Process JIT code region before → after |
|---|---:|---:|
| Interpreter | 4 → 4 | — |
| YJIT | 30 → 40 | 1.48 → 5.48 MiB |
| ZJIT | 20 → 276 | 2.51 → 7.11 MiB |

YJIT reports no freed code in this probe. ZJIT reports 12,651,772 total JIT bytes
afterward, including its additional allocation/frame regions. These counters
include the process's helper methods and are not solely query code. Repeated
collections did not reclaim those classes/code regions. Engine LRU and copied
pool bounds therefore do not establish bounded process-wide JIT memory; the
experiment retains this limitation and the complete observations.

## Correctness and resource checks

All final audits force first-use compilation and use the final source hash above.

| Audit | Passed / attempted | Kernels containing text specialization |
|---|---:|---:|
| Memory execute, Ruby 4.1 preview ZJIT | 20,267 / 20,267 | 432 |
| File execute, Ruby 4.0.7 YJIT | 18,250 / 18,250 | 269 |
| Memory mixed query/execute, Ruby 4.0.7 YJIT | 20,267 / 20,267 | 433 |
| Memory pure query, Ruby 4.0.7 YJIT | 20,221 / 20,267 | 433 |

Each audit examines all 679 files, retaining the existing two memory skips and
77 file skips. There are no new failures, unsupported files, timeouts, or memory
limit hits. All 46 pure-query failure objects exactly match the preceding
relation audit's collector-threshold differences. The mixed audit routes 159
finite-memory invocations to their authored execute API and 19,928 others to
query; it does not claim a full pure-query pass. See
[CORRECTNESS_FOLLOWUP.md](CORRECTNESS_FOLLOWUP.md) for those collector contracts.
File expression/pipeline paths exercise the new text surface; the file-host
general aggregate and materialized-relation kernel counts remain zero.

All 55 host-test files pass. Focused interpreter and YJIT tests cover exact text
wrapper/label metadata, Unicode character work, NULL and boolean evaluation
order, deferred row faults, every small statement/lifetime cost prefix, changed
bindings, default collations, snapshots, table replacement, permissions, host
callback ordering, cache modes and eviction, and no captured engine/row/parameter
state. Boundary tests cover the exact 32 KiB copied pool, overflow before copying,
cumulative strings, compiler-owned strings, and rollback after rejected emission.
Existing fallback tests continue to exercise unsupported complete roots.

Each worker uses a 512 MiB RSS guard and 2 GiB address-space limit. Corpus files
have a 10-second deadline. The full host suite peaks at 131.76 MiB and the largest
corpus worker at 175.14 MiB. These process guards and compiler bounds do not
establish a whole-engine heap ceiling.

## Reproduction

```sh
mise run ruby-native:test
JED_NATIVE_COMPILATION=immediate mise run ruby-native:conformance

mise run ruby-native:binding-compare -- --jit yjit \
  --lanes native-prepared,native-query,native-eager \
  --workloads scalar,general_text_filter,text_equality_filter,text_predicate_projection,text_predicate_sort,text_predicate_group,text_like_fallback \
  --output /tmp/text-enabled.json
# Repeat with --disable-text for the control.

RUBY_NATIVE_COMPARE_PLAN=bench/ruby-native/compare/text-plan.json \
  RUBY_NATIVE_PHASE=text-comparison RUBY_NATIVE_LANES=ruby-yjit,go,rust,ts \
  RUBY_NATIVE_COMPARE_RUBY_ADDRESS_SPACE_BYTES=2147483648 \
  mise run ruby-native:compare

JED_NATIVE_ADDRESS_SPACE_BYTES=2147483648 mise run ruby-native:bounded -- \
  ruby --yjit bench/ruby-native/text_diagnostics.rb \
  --jit yjit --report /tmp/text-diagnostic.json
```

Use `--jit none` for interpreter controls. For ZJIT, select the locally built
preview using `--ruby` in the binding comparison or invoke its binary with
`--zjit` in the bounded diagnostic. The saved diagnostic command records its
per-method ZJIT logging flag. Normal repository runtime configuration is unchanged.
