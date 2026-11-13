# C# experiment scope and acceptance

Baseline: `b1a6affca7725e0b07a4f734eddb1c569ef76bcf`. Full supported
Rust/Go/TypeScript behavior at that revision is the target. This table is a work
inventory, not a capability declaration. The baseline manifest and spec tree
remain the detailed inventory; `csharp-native:inventory` reports their sizes.

| Area | Canonical contracts | Status |
|---|---|---|
| SQL syntax and static resolution | grammar/, types/, functions/, errors/; design/grammar.md, types.md | Native parser/resolver, parameters and prepared ASTs implemented; broad SQL and validation gaps remain |
| Scalar expressions and built-ins | functions/catalog.toml; design/functions.md and individual type/function designs | Integer/Boolean/text/encoding/crypto and formatting subsets verified; regex, collation and other gaps remain |
| Exact numerics and temporal types | design/decimal.md, float.md, date.md, timestamp.md, interval.md, timezones.md | Handwritten exact decimal and temporal/timezone implementation with fixture and selected-corpus checks; full acceptance pending |
| Recursive types and extensions | design/extensibility.md, array.md, ranges.md | Arrays, composites, ranges and stored codecs implemented; host extension surface incomplete |
| JSON and JSON path | design/json*.md | Native parser, values, binary codecs and initial operators/functions; path evaluation under verification; SQL/JSON and aggregate gaps remain |
| Relational planning/execution | design/planner.md, estimator.md, joins.md, aggregates.md, window.md | Async scans, joins, correlated/lateral queries, CTEs, recursive CTEs, sets, initial grouping/windows and PK access; planner, aggregate and window coverage incomplete |
| DDL/DML, constraints, indexes, views | grammar/; design/ indexes, constraints, dependencies, views | Native DDL/DML, basic constraints/FKs, expression/partial indexes, sequences and views implemented; many advanced and validation paths pending |
| Transactions and session semantics | design/transactions.md, session.md, concurrency-testing.md | Commit/rollback/savepoints/read pins implemented; full shared concurrency and session gates pending |
| Page storage, encoding, durable commits | fileformat/, encoding/; design/storage.md, validated-cow.md | Native v52 tree/catalog/COW, compressed large values and file owner/client protocol; substantial fixture/interop checks pass, remaining corruption cases and full acceptance pending |
| Encryption and maintenance | design/encryption.md, fileformat/encrypted.md | Approved provider and encrypted page/file integration implemented; vectors and encrypted interop pass; maintenance and final acceptance pending |
| Resource accounting, spilling | cost/; design/cost.md, memory.md, spill.md | Statement/lifetime cost and scalar allocation seams implemented; exact costs incomplete; query/storage accounts and spill pending |
| Host tooling and failure simulation | csharp-native-host-matrix.md | Expanded host/fixture runner and cross-process Go interoperability; full simulation and final platform gates pending |

Each slice must run complete selected shared files, including all their assertions.
Selected-file results must list the files; unselected baseline files remain
unverified. The eventual full gate attempts every applicable baseline file and
reports failures rather than filtering by an optimistic capability list.

The foundation supplement is `spec/conformance/suites/query/scalar_foundations.test`,
added after the pinned revision and checked against supported cores and PostgreSQL.
It isolates scalar behavior without implying that the broad capabilities in its
requires header are fully implemented in C#.
