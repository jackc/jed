# C# experiment progress

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
