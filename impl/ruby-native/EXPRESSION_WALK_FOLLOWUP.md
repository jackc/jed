# Expression traversal follow-up

> Historical report: raw evidence and run-specific drivers are on the preserved
> experiment branch. See the [archive and retrieval instructions](../../bench/ruby-native/ARCHIVE.md).
> Reproduction commands below refer to that original checkout.

This explicitly requested optimization follows the
[query setup pass](QUERY_WALK_FOLLOWUP.md). The Ruby core remains experimental,
frozen and opt-in; normal tests, benchmarks, CI and future feature work do not
include it.

## Implementation

Resolved expression collection now appends to one fresh result array instead
of allocating and concatenating arrays at each node. It preserves preorder,
node identity, duplicate visits and nested argument arrays. It does not inspect
expressions stored in `value` or `extra`, and it never retains a result between
calls. Conjunction flattening uses the same approach while keeping a direct
single-element result for leaf predicates.

Index-key matching compares header fields directly and walks ordinary argument
arrays by index, avoiding temporary header tuples and `zip` pairs. Header
comparisons retain Ruby Array equality's identity-or-`==` rule, including shared
NaN values. Raw arguments still use direct `==`; only directly held resolved
expressions recurse through the key matcher. Nested CASE branch arrays retain
ordinary Array/Struct equality.

The integer body-column exception, cast type modifiers and caller-owned
collation checks remain in place. Unusual argument containers keep the original
`flat_map`/`zip` paths, and Array/Expression subclasses can use the original
helpers. The optimized paths operate on the engine's ordinary resolved nodes.
No new cache, dependency, SQL behavior or host API is introduced.

## Evidence and method

The baseline is pushed commit
`bd29129f6cb0b6a2c80d277bba1c436b01221697`, library SHA-256
`bb56f9a0b9592def93f4e0fb24f23098439b1d657e77f2d5fdbfdd14eeafc17d`,
archived at `/tmp/jed-expression-walk-before/impl/ruby-native`.
Final library SHA-256:
`577249f5b49bd67c369ca53eb256d91db73668a072bb1051c819a9698a40fc31`.
New evidence belongs in
[`expression-walk-followup`](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/expression-walk-followup`).
Earlier reports and artifacts remain unchanged.

The timing comparison keeps the existing nine-job
[multiway plan](../../bench/ruby-native/compare/multiway-plan.json) unchanged:
SQL, 1,000-row fixture, deterministic bindings, warmups, iterations and five
samples per process. Interpreter, YJIT and ZJIT each use serial
before/final/final/before processes for prepared cursor, repeated query cursor
and eager execute APIs. Results are fully drained. Reported paired times are
the means of two process medians per source. All samples and slower cells are
retained; process ranges are observations, not a statistical significance test.

Interpreter and YJIT use Ruby 4.0.7; ZJIT uses Ruby 4.1.0-preview1. Comparisons
between JIT modes also change Ruby version. The library does not enable a JIT.
Separate preflights check exact values, names, types and costs against the
actual Rust Ruby wrapper. Fresh supported-core and wrapper timings characterize
their different public API and collection paths, not the controlled source
change. Allocation profiles follow primary timing and use eager
`execute_prepared` with a reused statement; the timed `native-eager` lane uses
repeated `execute(SQL)`.

Source-paired direct diagnostics exercise expression and partial indexes beyond
the primary-key join fixture, in memory and file storage through four APIs.
They retain exact outcomes, costs, error prefixes and EXPLAIN access choices.
Bounded helper allocation observations demonstrate removal of intermediate
objects; whole-query timing determines the elapsed performance claim.

Workers run serially with a 512 MiB sampled RSS guard. Ruby workloads use at
most 2 GiB of address space; supported-core comparison workers retain their
established 16 GiB virtual reserve. These are process guardrails, not a claim
that an engine setting caps the whole Ruby heap.

The first host-suite attempt stopped on an allocation failure under a 1 GiB
virtual-address limit, at 114.2 MiB sampled RSS. A separate diagnostic ran the
same existing test prefix and recorded Linux memory mappings around the
thread/fork checks. After the eight-thread admission test, virtual memory was
1,036.9 MiB, including 511.1 MiB of inaccessible anonymous mappings; resident
memory was 63.7 MiB and the process had returned to its original two threads.
The final host check therefore uses a 2 GiB virtual reserve with the unchanged
512 MiB resident guard. No engine or test assertion was changed. The next
attempt passed those checks but was blocked by the sandbox's Unix-socket
restriction; the final run has access to the local page-service sockets.
Both stopped attempts and the reservation probe remain in the evidence.

## Correctness

All 66 host-test files passed. Focused checks passed under the interpreter,
YJIT and ZJIT. They verify traversal order and identity, duplicate nodes,
mutable argument arrays, fallback containers, NULL/NaN equality distinctions,
cast modifiers, expression/partial index decisions, and exact statement and
lifetime cost/error prefixes through four APIs and both storage modes.

All four complete corpus lanes passed **77,154/77,154 records**:

| Storage / API | Runtime | Passed records |
| --- | --- | ---: |
| Memory / query cursor | ZJIT | 20,297 |
| Memory / execute collector | ZJIT | 20,297 |
| File / query cursor | YJIT | 18,280 |
| File / execute collector | YJIT | 18,280 |

The final corpus runs have no failures, unsupported records, timeouts,
memory-limit exits or additional skips. Existing authored storage-mode skips
remain unchanged. The largest corpus worker used 181.9 MiB sampled RSS; the
complete host suite peaked at 447.4 MiB. All correctness workers exited before
the measurement queue began.

The six source-paired direct reports also agree exactly across all three
runtime modes. Each covers 72 API/storage groups, including matching and
nonmatching expression keys, partial predicates and compound text keys. The
comparison includes full result metadata, costs, error prefixes and EXPLAIN
choices. Both sources also pass the actual Ruby Rust-wrapper preflight for
all nine timed workloads.

## Results

This pass reduces allocation consistently, with its clearest elapsed-time
benefit under YJIT. Interpreter and ZJIT results are mixed. Across the 81 paired
cells, 23 have both final process medians below both baseline medians, 14 have
both final medians above both baselines, and 44 have overlapping ranges.
Thirty-three cells are slower on their paired means. All samples and slower
cells remain in [the complete tables](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/expression-walk-followup/tables.md`).
These process ranges are observations, not a statistical significance test.

All seven YJIT join workloads improve through ordinary query and eager execute,
with separated process medians in all 14 of those comparisons:

| Workload | Query speedup | Eager speedup | Fewer eager objects |
| --- | ---: | ---: | ---: |
| Indexed join | 1.044× | 1.037× | 11.7% |
| Three-table hit | 1.071× | 1.055× | 14.6% |
| Three-table miss | 1.063× | 1.026× | 10.1% |
| Three-table range | 1.092× | 1.104× | 24.4% |
| Three-table computed key | 1.029× | 1.038× | 10.7% |
| Four-table join | 1.059× | 1.040× | 16.0% |
| Three-table LEFT | 1.059× | 1.029× | 5.5% |

For example, YJIT ordinary three-table query time falls from 239.940 to
223.987 µs. Eager range time falls from 1,493.220 to 1,352.749 µs.
Prepared YJIT joins improve 1.004–1.082× on their means, but only range has
separated process medians: 1,478.798→1,366.655 µs. Prepared scalar and PK are
0.644% and 2.950% slower, respectively, with overlapping ranges.

The interpreter's prepared indexed and range joins improve 1.037× and 1.024×,
with separated medians. Its eager three-table hit/miss, computed-key,
four-table and LEFT cases instead slow by 2.565–4.992%; ordinary scalar is
1.214% slower. Those six slower comparisons have separated medians.

ZJIT prepared miss and four-table joins improve 1.048× and 1.057× with
separated medians. Its ordinary scalar and PK slow by 4.460% and 7.914%;
ordinary range, computed-key and LEFT slow by 3.097–3.449%. Eager PK,
three-table hit and range slow by 3.999%, 5.365% and 2.005%. Those eight slower
comparisons have separated medians. The largest slower mean is ordinary
three-table ZJIT at +9.001%, with overlapping process ranges. No adaptive
rerun replaces any of these primary observations.

The profiles show eager allocations falling from 648.5 to 572.5 objects per
indexed join, 999 to 853 per three-table hit, 1,304 to 1,095 per four-table
join, and 13,961.7 to 10,558.2 per range query. Scalar and PK remain at 39 and
31 objects per query and do not call the changed helpers in these profiles.
Their elapsed changes therefore cannot be attributed to work removed from
these helpers. Fewer allocations alone do not explain all timing differences.

The untimed helper probes agree across the interpreter, YJIT and ZJIT: the
sample expression tree falls from 68 allocated objects to one, its nested
array form from 150 to one, conjunction flattening from 49 to one, and equal
key comparison from 75 to zero. The disabled control restores the original
counts and preserves the same result/order/identity.

### Fresh cross-core and binding comparisons

The separate supported-core adapter measures Ruby YJIT at 4.814 µs for scalar
and 3.782 µs for PK: 6.160× and 2.277× Go, or 3.401× and 1.702× Rust.
Indexed join is 3.163× Go; the remaining join shapes range from 15.941× to
199.602× Go. The allocation improvement does not close those larger gaps.
All four cores' absolute times are retained in the complete tables.

The actual Ruby Rust-wrapper comparison includes different public API and
collection overhead. Native prepared elapsed time is 0.608× the wrapper for
scalar, 0.376× for PK and 0.716× for the four-table join. It is slower for
indexed join (3.895×), three-table hit (1.643×), miss (1.135×), range (5.690×),
computed key (1.740×) and LEFT (13.687×). These are fresh package-path
comparisons, separate from the controlled source change.

All 24 formal commands completed within their guards. Ruby binding/timing
workers peaked at 84.5 MiB sampled RSS; direct diagnostics peaked at 82.4 MiB;
the supported TypeScript comparison peaked at 180.5 MiB. Timings use memory
storage. File storage is covered by correctness and direct diagnostics.

### Next candidate

Multiway LEFT joins still repeat expression resolution: the final eager
profile records 110 base `Engine#compile` calls and 124 outer compile-wrapper
calls per query, with no prepared-root reuse and 5,922.9 objects per query.
These are call counts, not CPU-time percentages.

A promising next step is bounded resolution reuse for eligible multiway LEFT
joins while retaining fresh physical-plan selection. The existing admission
only allows LEFT with two inputs. Any extension must preserve outer-join
boundaries, ON versus WHERE behavior, NULLs, trapping expressions, exact cost
prefixes and contextual invalidation. It must not implicitly enable cached
physical winners for outer joins. No such extension is included in this pass.

## Reproduction

The evidence directory contains `check.rb` for the explicit focused, host and
full-corpus checks, and `run.rb` for measurements. They use the opt-in
`mise run ruby-native:*` tasks, run serially and refuse to replace existing
output. Preserve earlier artifacts before intentionally rerunning.

After correctness passes and the source and measured helpers are frozen, run
`run.rb --phase PHASE --final-source-sha256 FINAL_HASH` in this order:
`pins`, `verify`, `diagnostics`, `controls`, `cross`, `binding`, `profiles`.
Use the bounded mise wrapper with at least 16 GiB of outer address space for
this orchestration, so supported-core workers can obtain their virtual reserve.
Each workload still has its own 512 MiB RSS guard. The correctness driver
requires 2 GiB of outer address space and local Unix-socket access for the
host suite.

After workers exit, `summarize.rb` verifies source/runtime/helper hashes, exact
validation signatures, unchanged schedules, raw-sample arithmetic and serial
completion intervals. It produces `summary.json`, `tables.md` and an artifact
manifest without running any timed workload. Retained failures and diagnostic
probes remain distinguishable from the successful final checks.

The final verifier passed all 24 formal command records and all 81 paired
cells. Its manifest covers 82 entries totaling 6,411,825 bytes; SHA-256:
`dff3e9ffdc45ec3b5be837c35a41f004ed650e77a8ff6e371806d70fcd623d82`.
The serialization ledger verifies 62 losslessly compacted JSON files. Nested
failure, reservation-probe and pre-pin smoke evidence retains its original
bytes. The earlier green focus reports retain their original checker hash;
the final checker differs only in the host virtual-address allowance and its
explanatory comments.
