# N-way JOIN candidate construction

> Historical report: raw evidence and run-specific drivers are on the preserved
> experiment branch. See the [archive and retrieval instructions](../../bench/ruby-native/ARCHIVE.md).
> Reproduction commands below refer to that original checkout.

This explicitly requested follow-up investigates allocations in bounded
JOIN-plan search. The preceding [bound-preparation pass](JOIN_BOUNDS_FOLLOWUP.md)
left candidate construction as a prominent allocation site in fresh planning.
Allocation counts alone do not establish a useful throughput improvement.

**Decision: reject this candidate.** It saves candidate allocations but does not
establish a consistent whole-query speedup. The tested implementation, its
focused test and all evidence are archived at commit
`c4094c6a3f205639908fd8aa7f1a3832f6e73d70`. The preceding planner is restored,
with library hash identical to the baseline below. The candidate-specific test
lives in that archival commit. The implementation and measurements below
describe the rejected candidate, not a retained optimization.

The Ruby core remains experimental, frozen and opt-in. Normal tests,
benchmarks, CI and future feature updates do not include it.

## Candidate

The bounded dynamic-programming search now passes its existing frontier to
extension generation. Each extension batch looks up the bucket for its source
subset and order property. After the original estimate and step-cost work,
it rejects a candidate only if a state already in that bucket has no greater
cost, physical rows or logical rows, and at least one strictly smaller value.
Rejected candidates need no candidate Hash or copied order/access/step vectors.

The existing Pareto dominance contract in
[estimator.md §10.2](../../spec/design/estimator.md#102-partial-state-and-exhaustive-dp)
already excludes these candidates. Exact triples still use the existing
structural tie-breaker. All estimates, expression work and step costs remain
in their original order, and the entire extension batch still completes
before its surviving candidates enter the frontier. No state from earlier in
the same batch is used for early rejection. Fixed outer edges and the
larger-than-eight-relation greedy path keep their ordinary calls.

There is no new cache, retained estimator input, query rewrite, SQL behavior,
host API or dependency. Physical-plan reuse and binding-sensitive planning
keep their current rules. The retention decision follows correctness checks
and whole-query measurements.

## Baseline opportunity

The baseline is pushed commit
`7028f9bca642d85c138d3608d565d43be115df4d`, library SHA-256
`66afc525fcc2f4d0290ef70d03cc2f95e7521bd467029ecc653fe7b418f55341`,
archived at `/tmp/jed-nway-frontier-before/impl/ruby-native`.
New evidence belongs in
[`nway-frontier-followup`](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/nway-frontier-followup`).
Earlier reports and measurements remain unchanged.

The baseline-only opportunity probe uses unchanged production methods and
snapshots frontier dimensions before each entire extension batch. It covers
the thirteen preceding workloads and one bounded five-table JOIN, each cold,
warm and with changed parameters. Exact outcomes match separate untraced
controls in all 42 cases. ObjectSpace independently observes six Arrays and
one Hash at each candidate-construction site.

| Shape | Constructed candidates | Early strict rejections available | Exact-dimension ties |
| --- | ---: | ---: | ---: |
| Three-table range | 39 | 18 | 4 |
| LEFT fence | 10 in DP, 1 fixed | 3 | 2 |
| Five-table bounded range | 306 | 176 | 25 |

These counts hold for each cold/warm/changed observation of those shapes.
They imply 126, 21 and 1,232 potentially avoided construction objects per
query, before accounting for the optimization's own overhead. Value-blind
three-/four-table queries have cold opportunities but correctly skip fresh
planning after physical-plan reuse. The probe is not a timing result; its
largest sampled RSS was 150.1 MiB.

## Verification and measurement

Measured candidate library SHA-256:
`d7e02baf0647e72b8640b081854bb097d88ff5842a3183429f46901066b57ac6`.
The new [timing plan](../../bench/ruby-native/compare/nway-frontier-plan.json)
preserves all thirteen previous workload objects and adds the five-table
bounded-range case with bindings from 1 through 10. It uses the same 1,000-row
fixture and five samples per process. All measured queries are fully drained.

Interpreter and YJIT use Ruby 4.0.7; ZJIT uses Ruby 4.1.0-preview1. Comparing
the JITs also changes Ruby version. The library does not enable a JIT.
Each runtime uses serial before/final/final/before processes over prepared
cursors, ordinary query cursors and eager execute. Paired times average
two process medians per source; raw samples and every slower observation
remain available. Process-range separation is descriptive, not a statistical
significance test.

Fresh supported-core and actual Ruby Rust-binding measurements use different
public APIs and collection paths and are separate from source-change effects.
Exact wrapper preflights precede timing. Profiles run last using reused
`execute_prepared` statements; the timed eager lane repeats `execute(SQL)`.

Workloads run serially under a 512 MiB sampled RSS guard. Focused checks have
1 GiB of address space; host, corpus and Ruby measurement workers have 2 GiB.
Supported-core comparisons retain their established 16 GiB virtual reserve.
These process guardrails are not whole-engine memory limits.

## Correctness

The new focused test observes complete frontiers and estimator visitation
through three-, five- and eight-input dynamic-programming searches, and
verifies that nine-input greedy planning has no early frontier check. It
covers strict dominance, incomparable dimensions, exact structural ties,
zero and saturated estimates, subset/order bucket isolation, replacement of
a prior dominating state, immutable prefixes and independent surviving vectors.
Injected estimator faults still occur before a candidate can be rejected.

All four public query APIs are checked in memory and file modes against a
control with only this optimization disabled. Cases cover three- through
five-table joins, post-LEFT islands, ordered LIMIT/OFFSET, NULL and absent
bindings, runtime errors and exact statement/lifetime cost prefixes. Complete
frontiers and physical plans match, and cold/warm controls establish actual
fresh planning followed by actual physical-plan reuse.

All three development YJIT preflights passed without core or fixture failures;
the final one took 8.6 seconds and peaked at 52.7 MiB sampled RSS. The library,
focused tests and correctness driver were then frozen for formal verification.

All seven focused suites passed under the interpreter, YJIT and ZJIT. Their
largest sampled RSS was 94.1 MiB. All 71 host-test files passed; the complete
host worker peaked at 329.2 MiB.

All four complete corpus lanes passed **77,154/77,154 records**:

| Storage / API | Runtime | Passed records |
| --- | --- | ---: |
| Memory / query cursor | ZJIT | 20,297 |
| Memory / execute collector | ZJIT | 20,297 |
| File / query cursor | YJIT | 18,280 |
| File / execute collector | YJIT | 18,280 |

There are no failures, unsupported records, timeouts, memory-limit exits or
additional skips. Existing authored storage-mode skips remain unchanged.
The largest corpus worker used 180.3 MiB sampled RSS. Corpus lanes force
immediate compiler admission; focused and direct diagnostics exercise normal
cache policy as well as explicit controls. All formal correctness workers
exited before diagnostic smokes and measurement work began.

The baseline/final YJIT diagnostic smokes both passed without fixture
corrections. Complete semantic payloads, normalized frontier/winner hashes
and private file bytes match. The largest smoke worker used 346.2 MiB sampled
RSS. Helpers were then frozen and pinned for formal measurements.

## Results

Prepared-cursor target times do not show a consistent benefit:

| Runtime | Range before → candidate (µs) | LEFT before → candidate (µs) | Five-table before → candidate (µs) |
| --- | ---: | ---: | ---: |
| Interpreter | 2,580.722 → 2,633.430 | 756.248 → 771.038 | 12,238.901 → 12,790.099 |
| YJIT | 1,315.706 → 1,332.904 | 444.486 → 444.014 | 6,746.464 → 6,675.449 |
| ZJIT | 1,504.319 → 1,491.866 | 455.057 → 467.728 | 6,596.228 → 6,302.552 |

Across the three targets, three APIs and three runtimes, only three of 27
cells have separated faster process medians: interpreter eager five-table
(−1.164%), YJIT ordinary-query five-table (−1.162%), and ZJIT ordinary-query
range (−2.179%). Two have separated slower medians: interpreter ordinary-query
range (+3.696%) and ZJIT prepared LEFT (+2.784%). The other 22 overlap.

All **126 cells** remain in the
[complete tables](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/nway-frontier-followup/tables.md`):
11 have separated faster process medians, 20 have separated slower medians,
and 95 overlap. Eighty-nine paired means are slower. The largest observed
regressions are ZJIT prepared text projection (**142.912 → 153.560 µs,
+7.451%**) and ZJIT eager computed-key (**227.678 → 243.840 µs, +7.099%**),
both with separated process medians. Unaffected scalar/PK controls also move,
so these observations do not establish that the candidate caused every change.
No replacement runs discard slower results.

The allocation reduction is real but small in the whole query:

| Workload | Objects/query before → candidate | Reduction |
| --- | ---: | ---: |
| Range | 9,453.100 → 9,309.200 | 1.522% |
| LEFT | 2,196.800 → 2,174.800 | 1.001% |
| Five-table | 50,981.100 → 49,546.950 | 2.813% |

These are medians from the YJIT reused-`execute_prepared` allocation profile,
not timings from ordinary `execute(SQL)`. The other eleven workloads have
identical warmed allocation medians. Estimates and step-cost work still run;
early dominance checks add work before some candidates reach ordinary retention.
Call counts are not CPU-time percentages.

The trace covers three executions per workload after the allocation samples;
these can include a cache rebuild. Seven complete call maps are unchanged:
scalar, PK, indexed join, arithmetic residual,
text predicate, text projection and integer outputs. Other value-blind queries
show fewer retention calls during plan rebuilding even though their warmed
allocation medians are unchanged. These traces do not isolate warmed execution
cost and cannot explain ZJIT timing differences.

All six formal source/runtime direct reports agree on 128 API/storage/policy
groups, including 768 traced invocations, 30 catalog/data mutation checks,
cost/error prefixes and complete normalized frontier/winner hashes. The six
ObjectSpace allocation pairs independently confirm candidate-construction
savings in both memory and file modes: 23 candidate states for range, three
for LEFT and 310 for five-table, each avoiding six Arrays and one Hash.
These use a private 32-row fixture, so their counts differ from the earlier
1,000-row opportunity probe. Exact estimator visitation order is established
by the focused tests; direct reports also compare estimator call counts.

The fresh supported-core adapter measures candidate Ruby YJIT five-table at
6,539.444 µs versus Go 22.138 µs, Rust 17.499 µs and TypeScript 60.361 µs:
295.391× Go and 373.713× Rust. In the separate actual Ruby-package comparison,
native prepared five-table is 6,539.263 µs versus the Rust binding's
1,538.440 µs, or 4.251× its elapsed time. Native prepared range is 5.676×
the actual binding and LEFT is 8.909×. These public APIs and collection paths
differ; their ratios characterize this candidate rather than its source-change
effect. Both sources passed the actual binding's exact-value/metadata/cost
preflight before timing.

All 24 formal commands completed within their guards. The largest formal
diagnostic worker used 347.5 MiB sampled RSS; comparison workers peaked at
99.3 MiB for the Ruby-package runs and 227.5 MiB for the supported-core runs
(TypeScript). Allocation profiles peaked at 92.6 MiB. Avoiding 1–3% of
allocations without a consistent elapsed-time benefit does not justify adding
another dominance pass. The preceding implementation remains the baseline
for future work.

## Next candidate

Investigate prepared-resolution cache pressure on the five-table case. Its
final trace records 668 base expression-compilation calls and 155 key-size
admission checks per query, but no key-copy calls. This suggests repeated
resolution after the existing 4,096-unit/64-KiB key budget is exhausted;
it does not yet prove the rejection reason. A bounded occupancy/miss-reason
probe should establish that before any implementation change.

The three-query trace includes cold work, but separate warmed ObjectSpace
observations still record 566 contextual-expression outer Arrays and 1,698
associated pair Arrays, plus 566 admission-validation child Arrays. That is
a larger potential target than avoiding another small candidate vector.
Compact duplicated scope/AST descriptors within the existing budget if the
probe supports it; do not raise the cap or replace structural validation with
identity-only keys. Scope order, nullability, types, labels, mutable syntax,
environment invalidation and contextual-cast effects must remain correct.

Reusing JOIN predicate-placement work is another possibility, but its current
allocation counts include index eligibility walks as well as the outer rewrite.
Parameter-sensitive LIKE/ILIKE placement and outer-join fences also need explicit
handling. Neither target is implemented here or promises a speedup.

## Reproduction

The evidence directory contains explicit `check.rb` and `run.rb` drivers.
Invoke them through `mise run ruby-native:bounded`; every workload has its own
resource guard. Use at least 16 GiB of outer address space for supported-core
comparisons. Existing page-service host tests also need local Unix-socket
access.

Check out archival commit `c4094c6a3f205639908fd8aa7f1a3832f6e73d70` to reproduce
the rejected candidate and its focused test. The later branch restores the
baseline library; running candidate verification against that restored source
is intentionally rejected by the source pins.

Run `check.rb --phase PHASE --source-sha256 FINAL_HASH` for `focus`, `host`
and `corpus`. After correctness passes and measured source/helpers are frozen,
run `run.rb --phase PHASE --final-source-sha256 FINAL_HASH` in this order:
`pins`, `verify`, `diagnostics`, `controls`, `cross`, `binding`, `profiles`.
The drivers refuse to replace existing reports; preserve earlier artifacts
before an intentional rerun.

After every worker exits, `summarize.rb` verifies source/runtime/helper hashes,
schedules, exact semantic signatures, raw-sample arithmetic and serial command
intervals. It produces the summary, tables and manifest while preserving parsed
values when compacting JSON evidence.

The manifest covers 86 files totaling 61,571,263 bytes. Its SHA-256 is
`733a9978b1c3d511f4e2c4544801792f429699e145b945f9630d6a9dbeb4b7cc`.
