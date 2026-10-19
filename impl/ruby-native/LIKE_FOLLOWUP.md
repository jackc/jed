# Compiled literal LIKE predicates

This optimization follows the cursor correctness fixes in `96b2f163`. Ruby remains
an explicitly selected, frozen experiment. The parser, planner and matcher remain
handwritten Ruby; no dependency or normal test/benchmark/CI task changes.

## Scope

Simple literal `LIKE` and `NOT LIKE` predicates now participate in the existing
bounded runtime compiler. Exact, prefix, suffix and contains patterns become
fixed Ruby String operations. Repeated boundary `%` tokens and escaped literals
are supported. The compiler decodes a pattern once, rather than making the
ordinary matcher interpret it for every input row. Existing compiler consumers
carry the predicate into filters, projections, CASE expressions, grouping keys
and supported aggregate inputs.

Admission requires plain text, static C collation labels and a valid UTF-8
literal pattern. Interior `%`, unescaped `_`, trailing escapes, `ILIKE`, dynamic
patterns, explicit `ESCAPE`, linguistic collations and nominal types keep the
ordinary evaluator. A runtime guard also retains the matcher for unexpected
host values or encodings. This is a bounded extension, not a replacement matcher.

The generated code preserves operator charges, eager operand order, NULL
propagation, snapshots and deferred errors. LIKE has no comparison-length cost
surcharge. Patterns remain pooled data and never become Ruby source or regexps.
Both the original pattern and its decoded literal count against the existing
32 KiB pool limit, checked before allocating their copies.

## Evidence

The baseline archive is the pushed correctness revision `96b2f163`, library
SHA-256 `7724a4418421e56948d036dc04e54008df97b19d2609cf8b65a7ca0f63a2fe02`.
The optimized library SHA-256 is
`3e749562c6b764681d6917604cf02aa73b3194cfdbb8c5e756616588de91dc88`.
Raw measurements, commands, runtime identities, bounds and independently checked
source manifests are retained in
[like-followup](../../bench/ruby-native/results/like-followup/).

The explicitly selected [plan](../../bench/ruby-native/compare/like-plan.json)
uses the unchanged 1,000-row fixture. Its scalar control and historical
`text_like_fallback` prefix query retain their existing schedules. That historical
name is preserved for comparability; its simple prefix is now compiled. Seven
LIKE targets cover filters, CASE projection and CASE grouping, with a complex
underscore-pattern fallback control. Whole-query validation compares exact rows,
column metadata and costs across all APIs before timing.

Whole-query measurements use source ABBA order (baseline, final, final, baseline)
separately for the Ruby 4.0.7 interpreter, Ruby 4.0.7 YJIT and Ruby
4.1.0-preview1 ZJIT. Prepared execution, direct cursor queries and eager execution
are measured independently. GC stays enabled. Direct kernel comparisons use the
unchanged evaluator as their reference; the baseline rejects all seven direct
LIKE targets, so there is no claimed baseline compiled-kernel timing.

## Whole-query results

All seven targets improve in all 63 runtime/API comparisons. In every case, both
optimized process medians are below both baseline medians. Across prepared,
direct-query and eager APIs, gains range from 5.06–12.21× for the interpreter,
3.36–8.16× for YJIT and 3.35–11.30× for ZJIT.

The table uses prepared queries, with the mean of each source's two process
medians. Times are microseconds per fully consumed query; these measurements
are source ABBA comparisons, not confidence intervals.

| Query shape | YJIT before → final, µs | Interpreter speedup | YJIT speedup | ZJIT speedup |
| --- | ---: | ---: | ---: | ---: |
| Prefix filter + sum | 1,987.44 → 293.19 | 9.22× | 6.78× | 7.55× |
| Exact filter + count | 1,998.07 → 257.15 | 10.32× | 7.77× | 8.56× |
| Suffix filter + sum | 1,700.24 → 276.51 | 9.55× | 6.15× | 7.35× |
| Contains filter + sum | 2,058.24 → 305.59 | 9.62× | 6.74× | 7.43× |
| NOT prefix filter + count | 2,126.22 → 386.14 | 7.58× | 5.51× | 5.99× |
| CASE predicate projection | 3,058.66 → 891.76 | 5.31× | 3.43× | 3.87× |
| CASE predicate grouping | 2,718.07 → 715.75 | 5.40× | 3.80× | 3.92× |

The scalar and complex-pattern controls vary from 6.08% less elapsed time to
4.59% more. Three scalar comparisons have both final process medians above both
baselines: interpreter eager (+3.21%), YJIT direct query (+4.59%) and YJIT eager
(+1.45%). These small regressions remain in the original mixed-workload evidence;
the target speedups do not erase them.

An isolated scalar-only source ABBA, run after the planned measurements with
the same schedules and APIs, gives elapsed-time changes of +2.63% / +1.99% /
+1.66% for interpreter prepared/query/eager and +0.60% / +2.67% / −1.17% for
YJIT. The YJIT direct-query slowdown remains separated: 5.22321 → 5.36273 µs.
The interpreter eager separation does not repeat, but interpreter prepared now
separates with a 0.319 µs increase. These small control costs remain part of the
result; the experiment does not demonstrate neutrality for every unrelated query.
A separate non-timing check confirms identical emitted scalar source, immutable
pool, type and sample values/costs across the two revisions. The scalar profile
also retains 39 allocations per query. These facts do not explain or erase the
measured differences, and no causal attribution is claimed.

## Other cores

The separate prepared-query cross-core run retains the same SQL, fixture,
schedule and checksums. Its Ruby lane fully materializes through `Rows#rows`,
whereas the source ABBA and binding harness drain cursor rows externally. Keep
the two measurement series separate. All times below are µs/query.

| Query shape | Ruby YJIT | Go | Rust | TypeScript |
| --- | ---: | ---: | ---: | ---: |
| Prefix filter + sum | 298.64 | 160.47 | 142.22 | 272.26 |
| Exact filter + count | 265.25 | 138.94 | 136.46 | 214.23 |
| Suffix filter + sum | 282.50 | 162.68 | 128.08 | 263.96 |
| Contains filter + sum | 307.88 | 168.38 | 151.11 | 278.75 |
| NOT prefix filter + count | 388.52 | 147.57 | 135.76 | 225.88 |
| CASE predicate projection | 929.94 | 455.66 | 319.78 | 747.08 |
| CASE predicate grouping | 739.87 | 455.00 | 321.04 | 645.95 |
| Scalar integer control | 4.92 | 0.81 | 1.41 | 1.93 |
| Complex LIKE fallback | 1,886.41 | 163.81 | 144.68 | 277.45 |

The seven optimized targets take 1.63–2.63× Go's time, 1.94–2.91× Rust's time
and 1.07–1.72× TypeScript's time. The complex-pattern control remains 11.52× Go
and 13.04× Rust. This result supports the simple literal specialization; it does
not establish comparable performance for the general matcher or all SQL shapes.

## Ruby Rust binding

This is a separate run through the actual Ruby binding, with exact values,
column metadata and costs checked across all four lanes. All times are µs/query.
The binding reparses and materializes each call; native prepared parsing is
outside timing. Native direct query uses its existing statement cache, and
native eager execution materializes its result. All outputs are fully consumed.

| Query shape | Rust binding | Native prepared | Native query | Native eager |
| --- | ---: | ---: | ---: | ---: |
| Prefix filter + sum | 168.45 | 299.84 | 298.27 | 279.88 |
| Exact filter + count | 155.58 | 258.86 | 257.06 | 251.65 |
| Suffix filter + sum | 139.48 | 282.08 | 278.37 | 262.74 |
| Contains filter + sum | 168.56 | 308.38 | 301.79 | 287.26 |
| NOT prefix filter + count | 161.24 | 387.70 | 377.28 | 386.51 |
| CASE predicate projection | 904.93 | 920.16 | 775.05 | 657.78 |
| CASE predicate grouping | 412.92 | 747.63 | 727.34 | 754.05 |
| Scalar integer control | 7.93 | 5.12 | 5.20 | 5.70 |
| Complex LIKE fallback | 176.09 | 2,071.44 | 2,013.74 | 2,088.83 |

The binding remains faster for the simple filters and grouped query. Native
eager execution wins the projection by 1.38× and the scalar control by 1.39×.
This is an API comparison; the direct Rust core remains faster on all seven
target queries. No host-function implementation changes in this optimization.

## Allocations

Separate YJIT eager profiles count allocations before enabling tracing:

| Query shape | Before / query | Final / query |
| --- | ---: | ---: |
| Prefix filter + sum | 26,790 | 2,024 |
| Exact filter + count | 28,717 | 1,952 |
| Suffix filter + sum | 22,790 | 2,022 |
| Contains filter + sum | 26,790 | 2,024 |
| NOT prefix filter + count | 26,710 | 1,948 |
| CASE predicate projection | 28,376.04 | 3,371.04 |
| CASE predicate grouping | 29,366 | 4,529 |
| Scalar integer control | 39 | 39 |
| Complex LIKE fallback | 28,790 | 28,808 |

Every target eliminates 1,000 calls to the generic matcher per query. Allocation
reductions range from 84.6% to 93.2%. The fallback retains its 1,000 matcher
calls and has 18 additional allocations per query; it is not counted as an
optimization win. Traces identify removed work, not CPU-time attribution.

## Direct methods, compilation and retention

These diagnostics compare final-source generated methods with the unchanged
ordinary evaluator. They are phase measurements, distinct from the preceding
whole-query source comparisons. The scalar job returns one projection value;
the other jobs consume complete 512-row fixtures. Values, NULLs, metadata,
statement/lifetime cost prefixes, row identity and memory-account ownership are
checked before and after timing. Dynamic patterns and actual mutable text
bindings remain semantic fallback controls.

| Direct job | YJIT ordinary → compiled, µs | Interpreter speedup | YJIT speedup | ZJIT speedup |
| --- | ---: | ---: | ---: | ---: |
| Scalar prefix projection | 1.728 → 0.127 | 17.60× | 13.61× | 22.16× |
| Exact filter | 768.960 → 76.016 | 22.76× | 10.12× | 38.37× |
| Suffix filter | 640.996 → 77.554 | 21.28× | 8.27× | 31.96× |
| Contains filter | 709.877 → 80.916 | 21.88× | 8.77× | 29.41× |
| NOT prefix filter | 710.559 → 83.659 | 20.15× | 8.49× | 32.24× |
| Filter with memory accounting | 444.180 → 164.352 | 3.39× | 2.70× | 3.39× |
| Two CASE grouping keys | 2,128.612 → 263.459 | 15.11× | 8.08× | 16.93× |

All seven generated methods have direct JIT evidence in each JIT runtime:
102–276 compiled instructions per method under YJIT, and isolated per-method
compilation log windows under ZJIT. Enabling a JIT alone is not counted as proof.

Median template emission plus method creation ranges from 45.89–121.34 µs in
the interpreter, 46.87–121.36 µs in YJIT and 32.01–85.27 µs in ZJIT. Across the
seven YJIT jobs, the first compilation observed per job ranges from
119.91–1,501.75 µs and first invocation from 6.13–1,894.16 µs. These observations
include prior process/JIT state, are recorded separately from warmed timings,
and do not predict arbitrary-query amortization. All compilation samples and
first-call observations are retained.

The cache probe creates 256 distinct literal-pattern kernels and calls each 200
times through the actual second-use admission and 64-entry LRU. Every runtime
retains exactly 64 kernels, 70,784 source bytes and 1,728 pooled string bytes.
After the owning thread exits and its reference is released, each of three full
GC observations reports zero live kernel instances. VM retention is separate:

| Runtime | Anonymous kernel classes before → after | Process JIT code region before → after |
| --- | ---: | ---: |
| Interpreter | 4 → 4 | — |
| YJIT | 27 → 37 | 1.36 → 4.50 MiB |
| ZJIT | 24 → 280 | 2.66 → 5.99 MiB |

YJIT reports no freed code; ZJIT reports 10,727,512 total JIT bytes afterward,
including its other allocation/frame regions. These counters include helper
methods, not solely query code. Engine cache bounds therefore continue to offer
no bound on process-wide JIT retention.

## Correctness

The preceding correctness report explains and fixes the former cursor failures:
[CURSOR_CONFORMANCE_FIX.md](CURSOR_CONFORMANCE_FIX.md). No failure allowance,
mixed API routing, changed expected value or extra corpus exclusion is accepted
for this optimization.

Focused host tests cover each admitted shape, escapes, Unicode and NUL values,
NULLs, statement/lifetime cost prefixes, lazy control flow, encoding guards,
custom host strings, exact pool bounds, generated-source safety and fallback
admission. They also exercise existing pipeline, relation and aggregate consumers
and warmed direct/prepared APIs on memory and file hosts.

An independent classifier audit checks 82,896 pattern/subject combinations,
200 encoding/custom-String cases and 20 unlabeled-parameter rejections against
the unchanged evaluator. Every value, error and cost matches. The audit script,
result and real bounded-process report are retained alongside the measurements.

The final optimized source passes all four complete corpus audits and all 60
host-test files:

| API | Storage / runtime | Passed | Failures |
| --- | --- | ---: | ---: |
| `execute` | Memory, ZJIT | 20,267 / 20,267 | 0 |
| `query`, external collection | Memory, ZJIT | 20,267 / 20,267 | 0 |
| `execute` | File, YJIT | 18,250 / 18,250 | 0 |
| `query`, external collection | File, YJIT | 18,250 / 18,250 | 0 |

The host suite runs on Ruby 4.0.7's interpreter; focused LIKE tests also pass on
YJIT. The four corpus runs use immediate compilation and demonstrate 18 admitted
LIKE kernels per memory lane and 16 per file lane. There are no unsupported
files, timeouts, memory-limit hits or omitted failures. Every record in the
query-API audits uses the cursor API with external collection. The corpus's two authored memory
exclusions and 77 file exclusions are unchanged. Peak corpus-worker RSS is
172.9 MiB; the host suite peaks at 216.8 MiB. All retain the 512 MiB RSS guard.

The independent evidence verifier checks source and harness hashes, all host-test
files, corpus identity, actual API routing, original exclusions, runtime binaries,
limits and compiler admission before generating the final summary and manifest.

## Reproduction

Restore the baseline into an isolated archive (the scripts verify its library
hash), and select it with `--baseline-source` if it is not at
`/tmp/jed-like-before/impl/ruby-native`. Both Ruby runtimes and the existing
cross-core benchmark executables must already be installed. No downloads are
part of this experiment.

The serial measurement runner exposes phases `controls`, `profiles`, `cross`,
`binding`, `diagnostics` and `admission`. Run them one at a time, with no other
benchmark or test workers active. Each phase records exact worker commands and
environment settings. For example:

```sh
JED_NATIVE_TIMEOUT=1800 JED_NATIVE_ADDRESS_SPACE_BYTES=17179869184 \
  mise run ruby-native:bounded -- ruby \
  bench/ruby-native/results/like-followup/run.rb --phase controls \
  --final-source-sha256 3e749562c6b764681d6917604cf02aa73b3194cfdbb8c5e756616588de91dc88
```

The outer address-space allowance permits the existing Go/Rust benchmark
runtimes' reservations; each Ruby measurement worker still has a 512 MiB RSS
and 2 GiB address-space guard. Existing output files require explicit `--replace`.
After the six phases, run `isolated_scalar.rb` with the same final source pin
and outer guards to reproduce the separate interpreter/YJIT scalar controls.
Run `scalar_source.rb` once per source with `RUBY_NATIVE_SOURCE` set and the
appropriate `scalar-source-before.json` or `scalar-source-final.json` output
argument. Retained bounded reports record the exact commands and guards for
these non-timing source probes and the independent classifier audit.
The corpus runner uses `--source-sha256`, unlike the measurement runner's
`--final-source-sha256`. Run both APIs separately:

```sh
JED_NATIVE_TIMEOUT=1800 JED_NATIVE_ADDRESS_SPACE_BYTES=2147483648 \
  mise run ruby-native:bounded -- ruby \
  bench/ruby-native/results/like-followup/run_audits.rb --api query \
  --source-sha256 3e749562c6b764681d6917604cf02aa73b3194cfdbb8c5e756616588de91dc88 \
  --harness-sha256 9935e8bfaf7d1780b050a77f652291d3e2c1c1ac74669ea35e29d1852ddb53ab
# Repeat with --api execute, then run the host suite separately:
JED_NATIVE_ADDRESS_SPACE_BYTES=2147483648 \
  JED_NATIVE_RUN_REPORT=bench/ruby-native/results/like-followup/host-tests-bounded.json \
  mise run ruby-native:test
```

After all workers finish, the retained `summarize.rb` verifies the evidence and
rebuilds the derived summary and artifact manifest.
