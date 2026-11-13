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

## Remaining sequence

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
