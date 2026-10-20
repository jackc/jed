# Broader compiled literal LIKE patterns

This follows method sharing in `f8fa53cd`. The pure Ruby core remains a frozen,
opt-in experiment; no dependency, public API or normal CI/test/benchmark changes.

## Scope

The existing bounded compiler now admits two additional literal LIKE/NOT LIKE
shapes under static C collation:

- Fixed-position `_`, optionally followed by a final `%`, such as `row_5%` or
  `_😀_`. A pooled immutable program drives a forward byte loop; `_` advances
  one UTF-8 code point, including multibyte characters.
- Interior `%` without unescaped `_`, such as `r%w%5%` or `a%b`. Ordered literal
  searches preserve start/end anchors and prevent segment overlap.

The four existing equality/prefix/suffix/contains fast paths remain. Patterns
requiring mixed-wildcard retry, trailing escapes, dynamic patterns, explicit
ESCAPE, ILIKE, linguistic collations and nominal types retain the existing
evaluator. Unexpected runtime values or encodings use the original guarded
fallback. SQL patterns remain data, never Ruby source or regular expressions.

Admission permits at most 64 decoded runs and retains the cumulative 32 KiB
pool budget, including the original literal and packed program or decoded
literal bytes. Matching creates no per-row token or character arrays. Existing
operator charges, operand order, text snapshots, NULL behavior, deferred errors
and string encodings remain part of correctness verification.

## Evidence

The baseline is pushed commit `f8fa53cdee02ff400624e6e81e1c9c4cce79eb8e`,
library SHA-256 `fec686e7484eda683c1692576175671b6c11fb5accaca78b6ccb71466f0ee70a`.
Final library SHA-256:
`e430431f7e9264c3cc3b9a75436c46841004a707f9c9ec0b72744beeb4c27f88`.
Commands, runtime identities, complete samples and resource bounds are retained
in [literal-like-followup](../../bench/ruby-native/results/literal-like-followup/).

Whole-query measurements use the unchanged 1,000-row fixture and three native
APIs, with five samples per process and source ABBA order in the interpreter,
YJIT and ZJIT. The six new target queries cover filters, NOT LIKE, CASE
projection and grouping. Scalar, the previous simple-prefix fast path and a
mixed-wildcard fallback remain controls. Values, column metadata, exact costs
and consumed row counts must agree before comparing timings.

Direct diagnostics separately measure admission, generated methods, warmup,
allocations and code retention. Allocation/TracePoint profiles run outside the
latency measurements. GC remains enabled. Each whole-query source comparison
uses the mean of two per-process medians for each source; all raw samples and
individual process medians are retained. This is a repeated microbenchmark, not a
confidence interval or application-throughput prediction.

## Whole-query results

All six new targets improve across all three APIs and runtimes. Speedups range
from **4.33–9.48× in the interpreter**, **2.35–6.11× with YJIT**, and
**2.92–7.87× with ZJIT**. Prepared-query YJIT results, in microseconds per
complete query:

| Query | Before | Final | Speedup |
| --- | ---: | ---: | ---: |
| Fixed-position underscore plus final percent | 2,016.70 | 763.20 | 2.64× |
| Fixed-width underscore pattern | 2,088.62 | 827.35 | 2.52× |
| Interior percent segments | 2,150.37 | 369.80 | 5.82× |
| NOT LIKE with interior percent | 2,228.80 | 414.11 | 5.38× |
| Wildcard in CASE projection | 3,280.61 | 944.89 | 3.47× |
| Wildcard in grouping key | 2,876.90 | 780.89 | 3.68× |

These gains do not establish neutrality elsewhere. The controls with both
final process medians above both baseline medians are interpreter prepared
simple-prefix LIKE (+2.93%), YJIT direct-query scalar (+3.38%), simple-prefix
LIKE (+4.38%) and mixed-pattern fallback (+5.33%), and ZJIT eager simple-prefix
LIKE (+6.46%). These measurements remain part of the result. The existing
matcher and simple-prefix emitted operations are unchanged; this experiment
does not establish the cause of those control shifts.

The separate eager YJIT profiles show 1,000 generic LIKE calls per query before
and zero after for each new target. Median allocations fall from 28,727–31,543
to 2,046–4,627 objects per query. The mixed-pattern fallback still makes 1,000
generic calls and allocates about 30,800 objects per query. This explains a
concrete part of the benefit without treating traced execution as latency data.

## Generated methods and retention

Direct diagnostics cover six new wildcard consumers and the existing prefix
control. The before source rejects the six new consumers and still admits the
control. All seven final generated methods have direct YJIT or per-window ZJIT
compilation evidence. Isolated compiled-versus-ordinary evaluator speedups for
the six new consumers are 3.11–17.55× in the interpreter, 2.07–12.09× with YJIT,
and 2.71–25.71× with ZJIT. These are five alternating sample pairs in a final-source
process, separate from the before/final whole-query comparisons above.

First emission/materialization observations span 0.16–2.15 ms. Repeated emission
plus shared-method-cache-hit materialization has medians of 12.2–55.0 µs across
the seven consumers and three runtimes. Parsing/resolution is outside these
direct measurements; first-call and warmup observations are retained separately.
Compiling still has an upfront cost, so the existing second-use policy remains.

The 256-pattern churn probe respects the existing 64-kernel cache limit. Each
runtime reports 64 live kernels while the owning cache is retained and zero
after the owning thread exits and each of three GC observations. Shared methods
remain cached: this probe ends with 11 process-cache entries. The cache bound
does not bound VM-retained classes or machine code. Direct diagnostic RSS peaks
at 50.35 / 60.21 / 69.03 MiB for interpreter / YJIT / ZJIT respectively.

## Correctness

All four full-corpus lanes pass with immediate compiler admission and uniform
public API use. Cursor results are drained externally; there is no failure
allowlist, mixed API routing, changed expected result or new corpus exclusion.

| Corpus lane | Records passed | Failures |
| --- | ---: | ---: |
| Memory, ZJIT, execute | 20,267 | 0 |
| Memory, ZJIT, query | 20,267 | 0 |
| Disk, YJIT, execute | 18,250 | 0 |
| Disk, YJIT, query | 18,250 | 0 |

Every lane visits all 679 corpus files. Existing authored mode exclusions remain
2 files for memory and 77 for disk. No worker timed out or hit a memory limit.
The highest corpus worker RSS was 172.43 MiB, under the 512 MiB guard. All 61
host test files pass in the interpreter, peaking at 245.04 MiB. Ruby workers
also retain the 2 GiB address-space limit.

The independent classifier audit checks 1,592 pattern/operator cases, admitting
1,092 and rejecting 500. Its 187,824 compiled and 86,000 fallback comparisons,
264 runtime guards, 630 statement/lifetime cost/error prefixes, and 22 unlabeled
parameter rejections all pass. It compares with the unchanged ordinary evaluator,
including multibyte/combining characters, escapes, anchors, nonoverlap, NULL,
unexpected encodings/classes and host-method event logs. Focused LIKE, text and
method-sharing tests pass in both the interpreter and YJIT.

## Reproduction

Restore `impl/ruby-native` from baseline commit `f8fa53cd` into a separate
directory. The measurement runner defaults to
`/tmp/jed-literal-like-before/impl/ruby-native` and accepts `--baseline-source`.
Run `bench/ruby-native/results/literal-like-followup/run.rb` with
`--phase controls|profiles|diagnostics|admission --final-source-sha256 HASH`,
using the final hash above. Run these phases serially, without overlapping
tests, builds or other performance work; each child uses an opt-in mise task
and its own resource guard.

`run_audits.rb` accepts `--mode memory|disk --api execute|query` and requires
the source hash plus conformance-harness hash
`9935e8bfaf7d1780b050a77f652291d3e2c1c1ac74669ea35e29d1852ddb53ab`.
`correctness_summary.rb` verifies the completed corpus, host, focused and
classifier evidence against the current source and harness. `summarize.rb`
additionally validates measured answers, schedules, runtime identities, source
hashes and bounds before writing the summary and artifact manifest.
