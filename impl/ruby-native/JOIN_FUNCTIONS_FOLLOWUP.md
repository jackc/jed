# Scalar functions in prepared JOIN resolution

This explicitly requested follow-up extends the bounded prepared JOIN
resolution cache to ordinary scalar built-ins. The preceding
[admission pass](JOIN_ADMISSION_FOLLOWUP.md) identified repeated resolution
in a query containing `abs`; the underlying exclusion affected every function
call, including common text functions.

The Ruby core remains experimental, frozen and opt-in. Normal tests,
benchmarks, CI and future feature updates do not include it.

## Implementation

The existing cache now admits these 26 ordinary scalar built-ins:

| Family | Functions |
| --- | --- |
| Integer | `abs`, `mod`, `gcd`, `lcm` |
| Text case and order | `lower`, `upper`, `initcap`, `reverse` |
| Text measurements and characters | `length`, `char_length`, `character_length`, `octet_length`, `bit_length`, `ascii`, `chr` |
| Text selection and predicates | `left`, `right`, `substr`, `strpos`, `starts_with`, `split_part` |
| Text trimming and replacement | `btrim`, `ltrim`, `rtrim`, `replace`, `translate` |

Admission keeps the existing plain integer, boolean and text type boundary.
Named/variadic arguments, aggregate/window modifiers, and any same-name host
function registration decline conservatively. Other functions keep their
ordinary path. Rejecting a registered name also covers cases where ordinary
resolution would select the builtin overload; execution still uses that
ordinary resolver and preserves its precedence.

Syntax remains bounded by 192 nodes, depth 32 and 32 KiB of strings. Before
publishing a newly resolved root containing calls, a second bounded walk
checks the resulting expression kinds and types. Literal overload resolution
can rewrite arguments, so syntax alone is insufficient. Warm root hits do
not repeat this walk.

Only successfully resolved, unmetered roots are retained. Contextual parameter
casts still replay at the original compile call against the current binding.
Function evaluation, collation handling, scalar-memory charges and errors
stay in the existing runtime path; the cache does not precompute function
results.

The existing table-descriptor, environment, type, collation and binding-type
guards remain. Cache limits stay at 64 statement entries, 128 roots per entry,
4,096 copied key units and 64 KiB of key strings. Value-sensitive physical
selection remains fresh; eligible value-blind winners can use the existing
bounded physical cache. A changed or planner-generated root that fails
admission disables physical retention for that execution, including a plan
whose selection is already underway.

This adds no function evaluator or emitter and changes no SQL behavior, cost
units, host APIs, dependencies or supported cores.

## Evidence and method

The baseline is pushed commit
`1056d146c294dbb5f17f5c4a0eaec25bd957bcdd`, library SHA-256
`5feb7b064f414131a1c5f5d4aba8a64be8facc0885e3b4e5299ba0b5527ef29c`,
archived at `/tmp/jed-join-functions-before/impl/ruby-native`.
New evidence belongs in
[`join-functions-followup`](../../bench/ruby-native/results/join-functions-followup/).
Earlier reports, plans and measurements remain unchanged.

Final library SHA-256:
`405f5536969b71c80fa30bd45d962bda33af642b0185039d76e6443005c35336`.

The separate [13-workload plan](../../bench/ruby-native/compare/join-functions-plan.json)
preserves the ten previous workload objects
and schedules, including the 1,000-row fixture and arithmetic residual.
Additional bounded cases exercise a text predicate, text projection, and
integer functions in projection, filtering and ordering.

Each runtime runs serial before/final/final/before processes across prepared
cursors, ordinary query cursors and eager execute. All results are drained.
Paired times average two process medians per source, each retaining five raw
samples. Process-range separation is descriptive, not a significance test.
Every slower observation remains in the evidence.

Interpreter and YJIT use Ruby 4.0.7; ZJIT uses Ruby 4.1.0-preview1. Comparing
the JITs also changes Ruby version. The library does not enable a JIT.
Fresh supported-core and actual Ruby Rust-binding observations use different
public APIs and collection paths and are separate from source-change effects.
Exact wrapper preflights precede timing. Profiles run last using reused
`execute_prepared` statements; the timed eager lane repeats `execute(SQL)`.

Workload processes run serially under a 512 MiB sampled RSS guard.
Ruby measurement, host and corpus workers have 2 GiB of address space;
focused checks have 1 GiB. Supported-core comparisons retain their established
16 GiB virtual reserve. These guardrails are not whole-engine memory limits.

## Correctness

The new focused host test exercises all 26 admitted built-ins across all four
APIs in memory and file modes. It proves resolution reuse with physical reuse
disabled, compares values, types, collation labels and costs to the ordinary
resolver, and checks statement/lifetime error prefixes. Additional cases
cover contextual narrowing, NULL and minimum-integer overflow, mutable
syntax, host-name guards, schema/statistics/session-context changes,
host-environment fences, suspended cursors, capabilities, memory allowances,
cache bounds and ownership.

The five focused suites passed under the interpreter, YJIT and ZJIT at the
frozen source revision. Their largest sampled RSS was 91.0 MiB.
All 69 host-test files passed; the complete host worker peaked at 164.6 MiB.

All four complete corpus lanes passed **77,154/77,154 records**:

| Storage / API | Runtime | Passed records |
| --- | --- | ---: |
| Memory / query cursor | ZJIT | 20,297 |
| Memory / execute collector | ZJIT | 20,297 |
| File / query cursor | YJIT | 18,280 |
| File / execute collector | YJIT | 18,280 |

There are no failures, unsupported records, timeouts, memory-limit exits or
additional skips. Existing authored storage-mode skips remain unchanged.
The largest corpus worker used 182.9 MiB sampled RSS. Corpus lanes force
immediate compiler admission; focused and direct diagnostics exercise the
normal cache policy. All correctness workers exited before measurement work
began.

The initial final-source diagnostic smoke incorrectly required the entry
created by the first cold call to survive. `lower()` can lazily initialize the
text environment during that call; the environment identity guard correctly
replaces the initial entry on the next execution. Probes confirmed that the
replacement is then reused, with saved builtin-root hits and reduced original
compilation in both storage modes, including with physical reuse disabled.
The diagnostic now requires continuity of an actually repeated entry. The
failed smoke, its script and the probes are preserved under `smoke-fixture/`.
This correction changes no core or correctness test.
Both corrected baseline/final YJIT smokes passed before source/helper pins;
their reports are retained under `harness-smoke/`.

All six formal before/final runtime diagnostics passed. Each covers 56
workload/API/storage groups, 40 separate observations with physical reuse
disabled, 40 NULL/overflow/recovery outcomes, exact statement/lifetime budget
prefixes, and 60 write/statistics/index/EXPLAIN observations. Complete
semantic payloads match across sources and runtimes. The final reports trace
2,429 saved builtin-root hits each, rather than merely observing populated
caches. Churn retains at most 64 entries; completed-thread/GC probes observe
zero surviving weakly referenced entries. File-mode checks do not by
themselves establish that a query spilled.

## Results

**Decision: retain this change.** All 36 targeted workload/API/runtime cells
have both final process medians below both baseline medians. Speedups below
span prepared cursors, ordinary query cursors and eager execution:

| Workload | Interpreter speedup | YJIT speedup | ZJIT speedup |
| --- | ---: | ---: | ---: |
| Arithmetic residual JOIN | 1.99–2.19× | 1.81–2.29× | 2.02–2.24× |
| Text predicate (`length`) | 8.79–9.57× | 8.28–8.99× | 7.21–7.94× |
| Text projection (`lower`) | 6.70–7.36× | 6.15–6.76× | 5.42–6.23× |
| Integer projection/filter/order (`abs`) | 8.30–9.17× | 7.66–8.60× | 5.19–7.77× |

Representative YJIT prepared times, in microseconds per complete query:

| Workload | Before | Final |
| --- | ---: | ---: |
| Arithmetic residual | 1,904.519 | 1,052.288 |
| Text predicate | 1,497.894 | 180.946 |
| Text projection | 956.151 | 151.217 |
| Integer outputs | 1,396.607 | 180.491 |

This removes repeated resolution and enables existing eligible physical
reuse. It does not make the underlying function evaluator faster. The
separate physical-disabled diagnostics establish resolution reuse on its
own; timings measure the combined normal execution path.

The retained YJIT profiles show substantially less setup work:

| Workload | Objects/query before → final | Allocation reduction | Base `Engine#compile` calls/query before → final |
| --- | ---: | ---: | ---: |
| Arithmetic residual | 15,784.2 → 5,278.2 | 66.56% | 220 → 30 |
| Text predicate | 12,738 → 618 | 95.15% | 223 → 25.667 |
| Text projection | 8,579 → 591 | 93.11% | 134 → 0 |
| Integer outputs | 11,820 → 648 | 94.52% | 199 → 0 |

The profile averages three traced parameter cases, so call counts can be
fractional and differ from the repeated-entry probes above. These are call/allocation
observations, not CPU-time attribution. All nine other workloads retain
identical complete call maps; eight retain identical allocation counts,
and range changes from 10,559.8 to 10,559.9 objects/query.

The [complete 117-cell tables](../../bench/ruby-native/results/join-functions-followup/tables.md)
retain all results: 66 cells have separated faster process medians, ten
have separated slower medians, and 41 overlap. Thirty paired means are
slower. These counts include unaffected controls and do not estimate the
fraction of all SQL improved by the change.

The largest mean slowdown is ZJIT ordinary-query three-table equality,
**13.582%**, with overlapping process ranges. Its largest separated
slowdown is prepared four-table equality, **325.098 → 364.890 µs,
12.240%**. ZJIT prepared computed-key is also 7.825% slower with separated
medians. Interpreter prepared scalar and PK are 4.123% and 4.130% slower
with separated medians. YJIT's only slower mean is prepared three-table
equality, 0.021%, with overlapping ranges. The unchanged YJIT diagnostic
call maps do not establish the cause of these timings, particularly ZJIT
generated-code behavior. No replacement runs discard them.

Fresh comparisons against supported cores and the actual Ruby Rust binding
are separate observations. Ratios above one mean Ruby takes longer:

| Workload | Ruby YJIT / Go | Ruby YJIT / Rust | Ruby YJIT / TypeScript | Native prepared / Ruby Rust binding |
| --- | ---: | ---: | ---: | ---: |
| Arithmetic residual | 2.414× | 1.332× | 0.634× | 1.188× |
| Text predicate | 2.806× | 74.646× | 1.428× | 4.363× |
| Text projection | 1.991× | 67.970× | 1.425× | 3.013× |
| Integer outputs | 1.719× | 70.006× | 0.944× | 2.966× |

The residual eager path is 894.745 µs versus the binding's 900.774 µs in
that fresh observation; this small difference does not establish a general
advantage. The four target shapes are within 2.81× of Go, but other
multi-table controls remain much farther away, including a three-table
miss at 190.94× Go. The supported Rust adapter is especially fast on the
three new singleton JOINs; the Ruby binding has different preparation and
collection overhead. These public paths must not be treated as equivalent
measurements of just the engine.

Formal binding/preflight workers peaked at 97.9 MiB sampled RSS, direct
diagnostics at 75.9 MiB and profiles at 72.7 MiB. The fresh Ruby cross-core
worker used 69.8 MiB; the largest supported-core worker was TypeScript at
251.9 MiB. All guards passed.

The verifier checked 24 serial commands, exact semantic signatures,
source/runtime/helper hashes, every paired calculation and lossless JSON
compaction. The manifest covers 87 files totaling 14,173,644 bytes, including
the preserved smoke correction. Manifest SHA-256:
`e1a818b30f537149c2568a1426cf798e766deeebdb02b6a77c1367588b062b46`.

## Remaining opportunity

Repeated indexed probes still reconstruct bound comparison structure.
The final range profile records 34 probes, 101 comparison extractions and
354 structural comparisons per query, with about 10,560 allocations.
These counts include planning and do not show how much time is removable.
A next measured pass could prepare bounded ordered descriptors once per
JOIN execution while retaining all per-row bound evaluation, normalization,
encoding, costs and errors. This is an unmeasured candidate for range and
other repeated-probe queries; the residual workload uses no indexed probes.
The experiment remains opt-in and unpromoted.

## Reproduction

The evidence directory contains explicit `check.rb` and `run.rb` drivers.
Invoke them through `mise run ruby-native:bounded`; every workload has its own
resource guard. Use at least 16 GiB of outer address space for supported-core
comparisons. The existing page-service host tests also need local Unix-socket
access.

Run `check.rb --phase PHASE --source-sha256 FINAL_HASH` for `focus`, `host`
and `corpus`. After correctness passes and measured source/helpers are frozen,
run `run.rb --phase PHASE --final-source-sha256 FINAL_HASH` in this order:
`pins`, `verify`, `diagnostics`, `controls`, `cross`, `binding`, `profiles`.
The drivers refuse to replace existing reports; preserve earlier artifacts
before an intentional rerun.

After every worker exits, `summarize.rb` verifies source/runtime/helper hashes,
schedules, exact semantic signatures, raw-sample arithmetic and serial
command intervals. It produces the summary, tables and manifest while
preserving parsed values when compacting JSON evidence.
