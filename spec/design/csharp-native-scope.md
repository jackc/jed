# C# experiment scope and acceptance

Baseline: `b1a6affca7725e0b07a4f734eddb1c569ef76bcf`. Full supported
Rust/Go/TypeScript behavior at that revision is the target. This table is a work
inventory, not a capability declaration. The baseline manifest and spec tree
remain the detailed inventory; `csharp-native:inventory` reports their sizes.

| Area | Canonical contracts | Status |
|---|---|---|
| SQL syntax and static resolution | grammar/, types/, functions/, errors/; design/grammar.md, types.md | 45-record scalar supplement verified; broad grammar missing |
| Scalar expressions and built-ins | functions/catalog.toml; design/functions.md and individual type/function designs | Initial integer/Boolean/text subset verified; remainder missing |
| Exact numerics and temporal types | design/decimal.md, float.md, date.md, timestamp.md, interval.md, timezones.md | Missing |
| Recursive types and extensions | design/extensibility.md, array.md, ranges.md | Missing |
| JSON and JSON path | design/json*.md | Missing |
| Relational planning/execution | design/planner.md, estimator.md, joins.md, aggregates.md, window.md | Missing |
| DDL/DML, constraints, indexes, views | grammar/; design/ indexes, constraints, dependencies, views | Missing |
| Transactions and session semantics | design/transactions.md, session.md, concurrency-testing.md | Missing |
| Page storage, encoding, durable commits | fileformat/, encoding/; design/storage.md, validated-cow.md | Missing |
| Encryption and maintenance | design/encryption.md, fileformat/encrypted.md | Missing; provider approval pending |
| Resource accounting, spilling | cost/; design/cost.md, memory.md, spill.md | Scalar statement/lifetime cost seam verified; memory/storage/spill missing |
| Host tooling and failure simulation | csharp-native-host-matrix.md | Missing beyond initial runner |

Each slice must run complete selected shared files, including all their assertions.
Selected-file results must list the files; unselected baseline files remain
unverified. The eventual full gate attempts every applicable baseline file and
reports failures rather than filtering by an optimistic capability list.

The foundation supplement is `spec/conformance/suites/query/scalar_foundations.test`,
added after the pinned revision and checked against supported cores and PostgreSQL.
It isolates scalar behavior without implying that the broad capabilities in its
requires header are fully implemented in C#.
