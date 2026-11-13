# C# experiment progress

## Current development state

The complete `5adaba0d` SQL inventory passes every applicable memory, plain-file
and encrypted file, including all 2,761 encrypted cost/EXPLAIN events against
frozen Go. The detailed counts and source identity are in `VERIFICATION.md`.
Reopened composite defaults, CTAS interval qualifiers, dependent views and fixture
handling are repaired. Full host, mixed-process and threaded-stress evidence is
recorded separately; passing SQL is not treated as proof of the embedding API.

Immutable environments, host-function registration and native binding/result
helpers now pass full JIT and NativeAOT host tests. Streaming host batching and
the complete simulation matrix remain active work. The opt-in diagnostic benchmark
host passes matched-answer smoke checks; quiet JIT/NativeAOT measurements remain.
It records startup/cold-open timings, allocation/GC observations and enabled
durability barriers. The experiment remains incomplete and unfrozen.

## 2026-11-12 — charter, tooling and first vertical slice

Authorization follows the Swift experiment lifecycle: an independent engine,
opt-in tasks, full parity as the endpoint and a separate production decision.
Semantic baseline: `b1a6affca7725e0b07a4f734eddb1c569ef76bcf` (763 SQL files,
31 process corpus files, 19 saved simulation regressions, 1,199 spec files).

The maintainer requested idiomatic modern async C# and consideration of the newly
released .NET 11. The implementation targets net11.0 with SDK 11.0.100. Public
queries use Task, cursor advancement uses ValueTask, rows support async iteration
and asynchronous disposal, and cancellation retains both .NET cancellation state
and jed's SQLSTATE. No async file I/O has been implemented yet.

Implemented the initial lexer/parser, independent static resolver and evaluator,
owned scalar values, width-checked integer arithmetic, Boolean NULL logic, UTF-8
text comparison, scalar casts, SELECT/WHERE/LIMIT/OFFSET, exact cost/lifetime seams
and parser depth protection. Cost weights, scalar facts, casts and error codes
are generated as data from the pinned spec, never from another engine.

The shared supplement `query/scalar_foundations.test` contains 45 records and
runs as a complete file. Host/invariant tests exercise async completion and
cancellation, ownership, disposal, locale independence and nesting. Negative
adapter tests prove mismatched types/costs/errors/rows and unsupported directives
fail. The adapter drains the public cursor and verifies errors at drain time.

Implementation review and verification corrected an ambiguous ConfigureAwait overload,
offset/filter ordering, a parser nesting off-by-one and canceled-task state.
PostgreSQL planning-time constant folding differs from jed's existing projection
and filter rules in four supplement queries. Supported peers agree with jed;
the exact differences are recorded in oracle_overrides.toml. The oracle harness
also needed the line-comment case written with a subsequent SQL line rather than
ending the entire SQL string in a comment; no oracle behavior was changed.

The first SDK installation used 10.0.404 before the maintainer's .NET 11 steering;
the final target/pin and subsequent tests use 11.0.100. Both installations are
ignored local tooling, not package dependencies. No third-party C# dependency,
publication, push, merge, performance result or experiment freeze is implied.

The final full-baseline attempt ran all 763 files: one file passed
(`cast/boolean_prefixes.test`, five records), and 762 failed on missing engine or
harness support. It did not skip files or establish parity. Final source identities,
Debug/Release/Runtime Async/NativeAOT results and peer/oracle checks are recorded
in [VERIFICATION.md](VERIFICATION.md).

## Initial implementation sequence (historical)

1. Expand the static type/value foundations and existing expression corpus;
   implement handwritten exact decimals and recursive type descriptors.
2. Add byte codecs, shared vectors and the real pager/packed-record path before
   table SQL. No eager whole-database substitute.
3. Extend DDL/DML/planning/execution, prepared and host APIs, accounting and spill
   in shared-corpus-driven vertical slices.
4. Audit and obtain approval for concrete crypto providers; implement validated
   durable commits, recovery, encryption and byte-exact interchange.
5. Implement Linux protocol-v2 host locks and page service; run concurrency,
   mixed-core processes and deterministic simulation.
6. Complete full baseline acceptance, JIT/Runtime Async/NativeAOT verification,
   then measure and record performance before considering a freeze.

The experiment is started, not complete. The first scalar gate must not be
presented as full type, grammar, memory-safety-under-resource-exhaustion, storage
or file-format acceptance.

## Full-core implementation underway — 2026-11-12/13

The maintainer approved the scoped BouncyCastle.Cryptography 2.7.0 dependency
and subagent implementation/review. Package and restore locks are present. The
engine now includes handwritten limb decimals and transcendental arithmetic,
IEEE float input/rendering, date/timestamp/interval foundations, scalar built-ins,
packed v52 leaves and interiors, persistent paged B+trees, catalog codecs for all
entry kinds, bounded page caching, validated-COW commits and recovery. The
async database/session path supports initial table DDL/DML, retained read cursors,
explicit transactions and basic grouping/aggregation. Linux locks and the
page-service transport are under integration; no public file-host acceptance is
implied by their presence.

The latest complete frozen SQL probe (source SHA-256
`cc0d2150e7f423cd696b795e179b02ba1eff7116522fead906ac657d447e4d5d`)
passed **69 of 763 files**, containing **1,331 records**, and failed **694 files**.
This includes complete scalar, aggregate, table and transaction files. Missing
features and harness directives still fail; no capability-based skip was added.
The full-core endpoint remains open. This supersedes the initial one-file probe
as a progress measurement, not as acceptance evidence.


The subsequent complete frozen SQL probe at source
`9d999e38bd013c74a286e78346ec120df44a74e2bf6aac20ba489b276043a070`
passed **126 of 763 files**, containing **2,930 records in passing files**,
and failed **637 files**. This includes initial VALUES/UNION/INTERSECT/EXCEPT,
recursive array/composite/range values, expanded DDL and foreign keys. The
remaining failures include unimplemented SQL and host/harness capabilities,
planner and cost differences, and correctness gaps. This is a diagnostic result;
full acceptance and final JIT/NativeAOT revalidation remain outstanding.

### Full corpus diagnostic after resolver reuse

The completed opt-in frozen-baseline run at source digest
`84d3bb46384654be0dba9fbdea04db2547c52736f98000fcab2d2e5883dca26d`
passed 180 of 763 complete files (4,625 records in passing files); 583 files
failed. This supersedes the earlier 126-file checkpoint for measured breadth.
The intervening CTE diagnostic was interrupted at a resolver complexity defect
and is not a complete result. The completed run includes the operand-resolution
reuse fix. Subsequent async correlated-query, DML, formatting, and encrypted
storage work requires its own verification; this checkpoint does not cover it.

### Async correlated query checkpoint

The completed frozen-baseline run at source digest
`855f8b127bc7bf58c183f4e3a592837bfeef67067fdbe6efa2bd0a0caaee0c4c`
passed 204 of 763 complete files (5,685 records in passing files); 559 files
failed. The focused subquery run passed nine complete files and 162 records,
including correlated reads, primary-key correlated bounds, quantified
comparisons, and mutation subqueries. Grouped correlations passed the initial
scalar/HAVING/order cases but stopped at unsupported windows; this is not a
complete grouped-query claim. Recursive CTEs, LATERAL, views, and point-set
access were added after this checkpoint and require subsequent verification.

### Window and aggregate diagnostic before integration checkpoint

The window run at source digest
`6dcb31d68eb7317375ff40116a141f946fc8eb1447dc39702ce19e4a707a8778`
passed 19 of 22 complete files (216 records in passing files). Remaining files
failed on a RANGE/NULL frame cost, named-window validation requiring COLLATE syntax,
and the unimplemented collation harness. This includes grouped and named windows,
ranking, offsets, explicit frames, exclusion, and primary-key top-N execution.
The accompanying aggregate diagnostic passed 18 of 52 complete files (280 records
in passing files). Array/string aggregate tests advanced through value and cost
checks but still failed later typing cases; ordered-set, statistical, grouping-set
and JSON aggregate coverage remains incomplete. These selected diagnostics do not
supersede the last complete 763-file run.

Passing complete `window` files at `6dcb31d68eb7317375ff40116a141f946fc8eb1447dc39702ce19e4a707a8778`:
`aggregate.test`, `aggregate_filter.test`, `base_extend.test`, `expr_keys.test`, `frame.test`, `frame_exclude.test`, `frame_invalid.test`, `frame_range.test`, `frame_sliding.test`, `group_by.test`, `grouped_order_by.test`, `named.test`, `ntile.test`, `offset.test`, `order_by_key.test`, `ranking.test`, `ratio.test`, `row_number.test`, `topn.test`.

Passing complete `aggregates` files at `6dcb31d68eb7317375ff40116a141f946fc8eb1447dc39702ce19e4a707a8778`:
`avg.test`, `bit_and.test`, `bit_or.test`, `bit_xor.test`, `bool_and.test`, `bool_or.test`, `count.test`, `distinct.test`, `every.test`, `filter.test`, `group_by.test`, `group_by_types.test`, `having.test`, `min_max.test`, `min_max_types.test`, `select_distinct.test`, `sum.test`, `whole_table.test`.

Passing complete `json` files at `fce909c426c81a584af4892172d6c8d7d9b456045a1503138682a846cf23a872`:
`json_access.test`, `json_casts.test`, `json_contains.test`, `json_literal.test`, `json_mutate.test`, `json_object.test`, `json_object_dims.test`, `json_path_walk.test`, `json_processing.test`, `json_set.test`, `json_set_lax.test`, `json_storage.test`, `json_to_jsonb.test`, `jsonb_order_root.test`, `jsonpath_literal.test`, `jsonpath_member_names.test`, `unicode_bom_preservation.test`.

The JSON diagnostic passes 17 of 51 files (305 records in passing files).
Remaining failures include set-returning functions, SQL/JSON syntax, aggregate
integration, regex/path method gaps and cost differences.

### Integrated native-engine full SQL diagnostic

After correcting view expansion in sequence-write discovery, the complete run at
source SHA-256
`8d5b2a68950da35d5b73653205a281f9afb30c0b1e17bd6574e1d9db6a047d8c`
completed all 763 frozen files: **293 passed**, containing **7,148 records in
passing files**; **470 files failed**. No capability skips were used. The prior
`85011fc9` run aborted with stack overflow and is not complete evidence.

This improves measured breadth from the previous complete run's 204 passing
files. Remaining failures include unimplemented syntax, host/harness directives,
planner/cost differences, SQL correctness/validation gaps, resource accounting,
and maintenance behavior. Full baseline parity remains the completion criterion;
this is an implementation checkpoint, not a completed or frozen experiment.

### Post-checkpoint aggregate and window work

Following local checkpoint `55948a11`, selected diagnostics pass 38 of 52 aggregate
files (656 records in passing files), including all one- and two-argument
statistical aggregate files, array aggregation and string aggregation. JSON array
and object aggregate files also pass in the JSON suite. Window diagnostics pass
20 of 22 files (244 records); the remaining files require collation support.
These are development diagnostics while other modules evolve, not a new frozen
acceptance run. Grouping sets and ordered-set aggregates are the next relational
slices; resource accounts and spill remain open.

### Integrated development diagnostic — 2026-11-13

The complete 763-file run at source SHA-256
`e567343811c95f4508035ce94c9763fcd51782292881b0afb6952e8035f7f06b`
passes **439 files**, containing **11,467 records in passing files**; **324 files
fail**. This includes 50/52 aggregate files, 21/22 window files, 50/51 JSON files,
and 9/11 transaction files. Failures remain explicit. The raw diagnostic is
`/tmp/jed-csharp-full-development.log`; it supersedes earlier full SQL attempts
as a breadth measurement, not as final acceptance.

Recent implementation includes writable CTEs, updatable views, SQL/JSON and
JSONPath, handwritten regex, collation metadata and keys, grouping/ordered-set
aggregates, retained tree structure, initial predicate pushdown and index joins,
file maintenance, and temporary/attached database routing. The remaining work
includes complete access and join selection, GIN/GiST/exclusion, EXPLAIN and
statistics, catalog relations, resource accounting and spill, shared concurrency
adapters, typing/error edge cases, and final host/process/JIT/AOT verification.
Subsequent fixes are not implicitly covered by this source digest.

### Integrated SQL diagnostic after host and resource integration — 2026-11-13

The complete frozen 763-file run at source SHA-256
`bb40e2c4201433bc4539d8132762547bb15a42428baad77a527b605d6bf8682e`
passes **524 files**, containing **14,227 records in passing files**; **239 files
fail**. All seven shared concurrency files pass through real session admission
and snapshot/watermark APIs. The unmodified `expr/string_functions.test` passes
161 records after aligning the adapter's render-header behavior with the shared
runners; the frozen corpus was not changed. No capability skips were added.

The raw development log is `/tmp/jed-csharp-full-next.log`; the condensed failure
list is `/tmp/jed-csharp-full-next-failures.txt`. Remaining failures concentrate
in access/join planning, GIN/GiST/exclusion, DDL/DML, resource accounting and
spill, and EXPLAIN. This is a development breadth measurement, not final JIT/AOT,
process/simulation, performance, or full-core acceptance. Subsequent source edits
require their own verification.

### Indexes, maintenance and resource integration — 2026-11-13

Native GIN and GiST storage/query paths, exclusion constraints and REINDEX are now implemented.
Shared GIN image rebuilds and GiST structures pass their golden checks; encrypted rekeying matches
its shared golden byte-for-byte. Dependency lifecycle checks include timezone pinning and atomic
REINDEX behavior. GIN's full filename-selected diagnostic passes 20/30 files; its remaining gates
are EXPLAIN and one array join cost case. These are scoped development results in VERIFICATION.md.

A retained access inventory now supplies physical facts to execution and EXPLAIN. Complete
statistics-based selection, order-aware pipelines, join orientation/search, all query-memory and
spill paths, the remaining SQL features and final verification are still being built. Process
checks have cumulative evidence for 28/31 scenarios across separate diagnostics; this is not a
single passing full-suite result. Filesystem fault simulation and durable periodic reclamation
are in progress. No promotion or full-parity claim is made.

### Host and query-memory checkpoint — 2026-11-13

Integrated host checks are green, including periodic reclamation, remote maintenance, resident
large-value rewrite rules, encrypted/plaintext scratch and external-sort primitives. Complete
main, eager and operator-state query-memory files now have passing scoped diagnostics. SELECT INTO
and whole-row files pass. Process evidence has advanced to 30/31 scenarios across separate runs;
relaxed durability batching is still outstanding. VERIFICATION.md records exact source digests.

The entire experimental core is still incomplete. Remaining work includes physical access/join
planning, EXPLAIN estimates and actual counters, residual SQL cases, connecting bounded spill
operators to SQL, canonical fault-simulation traces and final full-source JIT/NativeAOT checks.

### SQL sorting and continued conformance — 2026-11-13

The latest full diagnostic at `0f9728cac64fc7ce0e1b371a423b14b9062f9a3e3f2ae56ebb932168eb62a2fe`
passes 595/763 files, with 15,999 records in passing files and 168 failed files. Subsequent scoped
checks cover real file-backed SQL external sorting, all main/state/transient query-memory files,
most MERGE files, and secondary-index ordering. Single-process fault simulations now execute the
real engine and compare transcripts with the pinned Go core; remaining trace differences are
explicit failures under investigation. See VERIFICATION.md for scoped evidence and remaining gates.

`DatabaseFileOptions.WorkMem` uses the 256 MiB default when zero; `Database.SetWorkMem(0)` selects
unlimited resident operators. This is experimental host API. Bounded hash/group/DISTINCT spill,
complete physical planning, residual SQL and final clean verification remain unfinished.

### Continued full-core implementation — 2026-11-13

The next complete diagnostic passes 630/763 files (16,944 records in passing files), with
133 failed files, at `bed160a7b699f6cc62d23a7d1939249f1a4baa1b0db887428cb50c1a2acc3774`.
Integrated host verification is green at
`c696f1094f649a3a566b882aa6b85203a46445f02ba40c80c57625a814cbd5f9`.
This includes the memory/transient fixes, SQL external sorting, MERGE implementation,
bound LIMIT/OFFSET parameters, simulator adapters and native storage corrections.
Five plaintext fault traces now match the pinned Go transcripts across scoped runs.
Encrypted fault traces, relaxed-durability host integration, complete SQL planning and
bounded hash/group/DISTINCT spill remain active work. The new keyed scratch-state utility
is scaffolding awaiting dedicated validation and operator integration.

### Bounded operators and canonical backend checks — 2026-11-13

The next canonical memory run passes 679 of 763 files, with 18,856 records in
passing files, 82 failures and two declared mode exclusions. Full host verification
passes at the same source digest, `db0ab23f0e9fced13fca929bd46b5319bf30d595699b10e1108d3d40833ed9a1`.
The adapter now honors each backend's exclusions and reopens disk files before
every record. Earlier mixed-backend counts are historical diagnostics.

Keyed scratch state, hash joins, aggregate state/collections, DISTINCT and external
sorting now have bounded file implementations. Real SQL checks compare resident
and spilled values, stable order and costs on plaintext and encrypted files with
a one-byte query-memory account. These scoped checks include INNER/LEFT/RIGHT/FULL
joins, ordered/hypothetical aggregates, grouping sets and DISTINCT windows. Shared
corpus work still exposes exact memory-threshold and planner differences; the
operators are not yet declared conformant across the full baseline.

Exact-memory follow-up found that ordinary two-input nested-loop joins must retain
their resident lane; only hash or physical multi-step joins, or a grouped/DISTINCT
parent, select bounded blocking execution. The broader experimental spill dispatch
was narrowed accordingly. Passing a standalone RIGHT/FULL spill comparison did not
establish that those shapes were allowed to change their memory-admission schedule.

All 31 process scenarios pass across scoped runs. Relaxed durability is integrated;
the relaxed-close trace and five other plaintext fault traces match the pinned Go
transcripts. Encrypted recovery still has physical-image differences under
investigation. Linux file growth uses geometric reserve and durable data barriers.
The separate encrypted SQL comparison establishes exact row/type/error/cost/plan
agreement for 542 files at an earlier snapshot; 610 row/type/error successes alone
were not complete encrypted acceptance. Current-source NativeAOT, full backend
parity, complete fault verification and performance measurements remain open.

### Join search, interval sets and recursive queries — 2026-11-13

The canonical memory diagnostic now passes 710 of 763 files (20,266 records), with
51 failures and two declared mode exclusions, at source
`b012d973d295869a3b8cfa9fd28dd71d2e55e1f1da3e3eb097d91d6af8e31bb4`.
The complete JIT host task passes at that capture. SEARCH/CYCLE, CTAS, views,
composite interval-set EXPLAIN and several two-way join files now pass completely.
N-way search, retained access identities, body pushdown and exact memory admission
remain under corpus-driven refinement. A full pass is not claimed.

Bounded aggregate execution passes all 52 aggregate files with forced disk
spilling at a prior capture, including ordered collections. Shared group/DISTINCT
spill thresholds and upstream SRF memory accounting have focused passing evidence.
Encrypted recovery now reproduces every physical image in its saved regression;
SQL cost checks and additional simulator traces remain separate acceptance work.

### Continued planner and service verification — 2026-11-13

The complete canonical memory run at
`c250b53b8116d2d0ddf45c6619d83f85c6dd3ef77c3b99a79c4d252ee5408a46`
passes 737 of 763 files, with 21,381 records in passing files, 24 failures and two
declared mode exclusions. Remaining failures concern join planning, scope,
EXPLAIN and resource scheduling. Later focused captures close further cases;
they do not replace a complete acceptance run.

The saved 403-step encrypted recovery trace now matches the pinned Go peer
exactly, including costs, images and key events. The complete 304-step
owner/client service trace also matches, using the production protocol handlers
over an injected transport. Reconnect, rekey/promotion and current-key backup
regressions have separate complete passing traces. Full backend, spill, host,
NativeAOT and performance verification remain required before completion.

2026-11-13 host matrix follow-up: all 496 canonical mixed C#/Go/Rust/Node
process pairings pass, including exact final and backup images. The threaded
stress profile passes on C# memory/plain/encrypted hosts and the independent Go
actor (16,000 snapshot checks, identical final checksum). Saved deterministic
simulation coverage is 18/19 complete exact traces; the remaining attachment
maintenance event is being corrected and rerun. Detailed immutable artifact
hashes and logs are in VERIFICATION.md. Immutable environment and host-function
APIs are being integrated before the next complete host/NativeAOT capture.
