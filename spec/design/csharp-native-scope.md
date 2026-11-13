# C# experiment scope and acceptance

Baseline: `b1a6affca7725e0b07a4f734eddb1c569ef76bcf`. Full supported
Rust/Go/TypeScript behavior at that revision is the target. This table is a work
inventory, not a capability declaration. The baseline manifest and spec tree
remain the detailed inventory; `csharp-native:inventory` reports their sizes.

| Area | Canonical contracts | Status |
|---|---|---|
| SQL syntax and static resolution | grammar/, types/, functions/, errors/; design/grammar.md, types.md | Handwritten parser/resolver, prepared syntax and inferred parameters implemented; complete baseline inventory runs in all three storage modes |
| Scalar expressions and built-ins | functions/catalog.toml; design/functions.md and individual type/function designs | Full scalar corpus, regex, pinned collation, text/encoding and approved cryptographic functions exercised; final-source acceptance pending |
| Exact numerics and temporal types | design/decimal.md, float.md, date.md, timestamp.md, interval.md, timezones.md | Handwritten exact decimal and temporal/timezone implementation with complete relevant corpus and byte-vector checks |
| Recursive types and extensions | design/extensibility.md, array.md, ranges.md | Arrays, composites, ranges and recursive codecs implemented; immutable environment/host scalar registration and native API ergonomics pass host checks; held/streaming batch replay tests pass, with optimization reach described below |
| JSON and JSON path | design/json*.md | Native values, codecs, path evaluation, SQL/JSON and aggregates implemented and exercised by complete baseline files |
| Relational planning/execution | design/planner.md, estimator.md, joins.md, aggregates.md, window.md | Async access paths, join enumeration, lateral/recursive queries, sets, grouping and windows implemented; full applicable SQL and encrypted peer cost/plan checks pass at the recorded capture |
| DDL/DML, constraints, indexes, views | grammar/; design/ indexes, constraints, dependencies, views | Native DDL/DML, MERGE, constraints, FK/exclusion/index dependencies, sequences and writable views implemented; persistence fixes verified with reopened files |
| Transactions and session semantics | design/transactions.md, session.md, concurrency-testing.md | Shared concurrency schedules, savepoints, capabilities and session corpus exercised; real threaded memory/plain/encrypted stress matches pinned peer |
| Page storage, encoding, durable commits | fileformat/, encoding/; design/storage.md, validated-cow.md | Native v52 trees/catalog/COW and compressed values; golden/host checks and all 496 mixed-core process cases pass at recorded captures |
| Encryption and maintenance | design/encryption.md, fileformat/encrypted.md | Credentials, recovery, rekey, compact/export/backup and sealed scratch implemented; vectors, host checks and all-peer byte interchange exercised |
| Resource accounting, spilling | cost/; design/cost.md, memory.md, spill.md | Cost/scalar/Q1-Q4 accounts, live-page limits and bounded spill implemented; complete resource corpus and tiny-budget plaintext/encrypted operator tests pass at recorded captures |
| Host tooling and failure simulation | csharp-native-host-matrix.md | Complete SQL adapters, host/fixture runner, all-peer process matrix and stress implemented; saved/generated simulation matrix, runtime-async/final artifact verification and measurements remain open |

Each slice must run complete selected shared files, including all their assertions.
Selected-file results must list the files; unselected baseline files remain
unverified. The eventual full gate attempts every applicable baseline file and
reports failures rather than filtering by an optimistic capability list.

These statuses describe implementation progress, not a frozen acceptance claim.
See `impl/csharp-native/VERIFICATION.md` for exact source digests and remaining
failures. A passing SQL corpus cannot establish host extension or embedding API
completeness, which have separate host tests and audits.

Host batching follows the speculative-kernel/scalar-replay contract in
`extensibility.md` §4.2.1. Current multi-row sites are eager row buffers, resident
sort/DISTINCT projection, single-interval primary/full scans, spilled sort output
and grouped output spools. Held chunks are capped at 1,024 rows; pulled windows
at 64, further bounded by cost headroom and LIMIT. Disjoint-interval and
secondary-index streams retain batch-of-one calls, as do spilled sort merges
with an active query-memory account. Scalar-only eligibility exclusions still
apply. These reach limits affect callback chunking, not SQL values, errors or
cost; host-observable chunk and call counts are not cross-core contracts.
The JIT full-host capture
`f718b8c3aa739f79cb82e2cf9ffe5641688ef91d7b4fb1a5106ba1c243f41074`
passes batch/scalar equivalence checks for resident/file sort, grouping spools,
scan/filter projection, strict NULL, prefix failures, deferred values/source
errors, early close, cancellation, LIMIT and cost/query-memory ceilings.
Final runtime-async and NativeAOT evidence is recorded separately in
`impl/csharp-native/VERIFICATION.md`.

The foundation supplement is `spec/conformance/suites/query/scalar_foundations.test`,
added after the pinned revision and checked against supported cores and PostgreSQL.
It isolates scalar behavior without implying that the broad capabilities in its
requires header are fully implemented in C#.
