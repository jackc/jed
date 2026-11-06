# Native Swift experiment — baseline scope and completion matrix

This is the acceptance inventory for the explicitly requested independent Swift
experiment under `impl/swift-native`. The target is the supported Rust, Go, and
TypeScript surface at **`362e0798ac3a8bd04bd2da556044b6ec73bf468a`**. It is not the
frozen Ruby experiment's coverage. Inventory date: **2026-11-05**.

The language-neutral contracts and corpus are authoritative. Existing cores are
implementation-pattern evidence, not reference implementations. This document
records the inventory **before Swift engine implementation**. Every target below
starts **U — unimplemented and unverified**. A package scaffold, generated catalog,
parser, or passing subset must not turn a subsystem into a completed row.

The companion [host and runner matrix](swift-native-host-matrix.md) specifies the
embedding, process, and test-adapter obligations in greater detail. Progress and
verification evidence must update these matrices without moving the baseline.
When later work changes a row to verified, record the implementation path, exact
opt-in command, result, platform, and revision; preserve failed/remaining lanes.

### Foundation evidence (2026-11-05; acceptance remains unverified)

The implementation is now in progress on local-only branch
`codex/swift-native-experiment`. The initial `U` entries below remain acceptance
states: no SQL capability is verified until the resolver/executor and full shared
corpus exercise it. The [implementation ledger](../../impl/swift-native/PROGRESS.md)
records integrated command results and local revision boundaries.

| Matrix rows | Handwritten foundation and focused evidence | Remaining acceptance work |
|---|---|---|
| T02 | [Decimal arithmetic](../../impl/swift-native/Sources/Jed/Decimal.swift), exact numeric functions and guarded transcendental kernels; all 41 shared decimal key vectors and internal arithmetic/admission invariants pass | SQL resolution, every numeric corpus result/error/type/cost, aggregate integration |
| T03, F03 | [Numeric formatting](../../impl/swift-native/Sources/Jed/NumericFormat.swift), [float input/output](../../impl/swift-native/Sources/Jed/FloatText.swift) and [float kernels](../../impl/swift-native/Sources/Jed/FloatFunctions.swift); exact IEEE expansion, ties-even/Roman invariants, and three float width/strict-interval/round-trip tests pass through the 133-test green integrated run | Full float and numeric formatting SQL corpus, including resolved casts, errors, types and costs |
| T05, T06, F03 | [Temporal values](../../impl/swift-native/Sources/Jed/Temporal.swift), [timezone reader](../../impl/swift-native/Sources/Jed/TimeZone.swift), scalar functions and datetime templates; shared date/timestamp/interval vectors, JTZ bundle round trip and all 16 TZif vectors pass | SQL temporal/formatting corpus, session/environment lifecycle and persisted timezone dependency integration |
| F04 | [Regex compiler/VM](../../impl/swift-native/Sources/Jed/Regex.swift) and [SIMILAR](../../impl/swift-native/Sources/Jed/Similar.swift); all 33 shared program listings, 41 capture/step vectors and 19 SIMILAR vectors pass; capture/replacement admission and retained SRF producer invariants pass through the 146-test green integrated run | SQL regexp/SRF/substring and resource corpus; environment Unicode fold and per-execution scratch integration |
| T07 | [Unicode bundle/property reader](../../impl/swift-native/Sources/Jed/UnicodeData.swift) and [collation tables/executor](../../impl/swift-native/Sources/Jed/Collation.swift); three tests pass in the 146-test green run, including frozen artifact bytes, sparse delta/full-table merge identity, all 46 shared sort-key vectors, 2,933 simple and 103 special case mappings, explicit environment isolation and regex fold injection. The separate handwritten [DUCET/LDML compiler](../../impl/swift-native/Sources/JedUnicodeCompiler/) and artifact writer pass four further tests in the 2026-11-05 integrated run, including all compiler/bundle vectors and byte-for-byte production artifact rebuilding | CLI smoke test; SQL collation/casing corpus, environment lifecycle, persisted collation dependencies and skew handling |

These are bounded foundation results, not completion claims for the matrix rows.
The platform for these checks is Linux x86-64 with Swift 6.4.2, using the opt-in
`mise run swift-native:test` task. Apple builds, whole-file interchange, process
schedules, simulations, and complete cost/memory conformance remain unverified.

## 1. Scope rules

1. Implement the currently implemented contract, including deliberately rejected
   forms, deterministic error precedence, selected plans, estimates, costs, byte
   encodings, and host-boundary behavior. Matching rows alone is insufficient.
2. The engine is handwritten Swift: parsing, resolution, planning, evaluation,
   exact arithmetic, codecs, storage algorithms, and noncrypto built-ins. Mechanical
   descriptor generation from shared catalogs is allowed. No engine translation,
   subprocess engine, Rust wrapper, or foreign database delegation is allowed.
3. Linux native memory/file hosts are required. Supported-core browser/OPFS and
   Node worker packaging are platform-specific adapters, not Swift delivery
   requirements. Their shared semantics (cursor/account lifetimes, cancellation,
   encrypted-page codecs, async host seams where exposed) remain relevant.
   Apple platform builds/tests are a separate unverified lane, not a Linux blocker.
4. Tests, tooling, benchmarks, and codegen integration are opt-in. Normal CI and
   default tasks must remain independent of Swift. No website changes are required
   or authorized for this experiment. Freeze the completed experiment against this
   baseline; promotion and production Swift delivery require separate decisions.
5. Keep the implemented/future distinction below. Historical prose in some README
   files and early slice descriptions is stale: for example encryption, timezones,
   collation, array keys, and hash spill are implemented at this baseline. Conversely,
   a future design document does not make its proposed feature part of acceptance.

Frozen host clarification: although the general session prose describes a lazy
bare `BEGIN`, all three baseline session drivers default an unspecified mode to
READ WRITE and acquire the gate eagerly (Rust `shared.rs::begin_block`, Go
`shared.go::beginBlock`, TypeScript `shared.ts::beginBlock`). Swift retains that
observable baseline behavior. An invalid session-option timezone similarly
falls back to UTC in all three constructors; the explicit timezone setter
validates and reports `22023`.

Frozen sequence clarification: [sequences.md §6](sequences.md#6-currval--lastval--per-session-state-not-snapshot-state)
still describes a value-only `lastval` cache and says `setval` never changes it.
The baseline [shared sequence corpus](../conformance/suites/expr/sequence_value.test)
explicitly requires `setval` on the last-used sequence to change `lastval`, while
`setval` on another sequence leaves it unchanged. All three baseline cores retain
the last sequence name and read its session value. Swift follows that shared
corpus behavior; the stale prose is not an alternate expected result.

## 2. Audited baseline totals

These counts were read from the recorded Git tree, not inferred from README status
paragraphs. Catalog counts are authored rows, not expanded overload counts or SQL
names. Corpus header counts are literal line-start `statement ` and `query `
directives; concurrency steps have their own format and are not included in them.

| Canonical input | Baseline count | Swift status |
|---|---:|---|
| [Design documents](README.md), including README | 93 Markdown files | U |
| [Conformance manifest](../conformance/manifest.toml) | 475 capabilities; 19 profiles | U |
| [Shared suites](../conformance/suites/) | 749 `.test` files in 26 suite directories | U |
| SQL-format suite files | 742 | U |
| [Concurrency schedules](../conformance/suites/concurrency/) | 7 | U |
| SQL statement / query directives | 11,944 / 9,581 | U |
| `# cost:` occurrences in suite files | 2,330 | U |
| [Real-process histories](../conformance/process/) | 31 `.process.toml` files | U |
| [Simulation profiles](../conformance/sim/profiles/) / [regressions](../conformance/sim/regress/) | 22 / 18 | U |
| [Scalar descriptors](../types/scalars.toml) | 17 | U |
| [Cast rows](../types/casts.toml) / [range descriptors](../types/ranges.toml) | 70 / 6 | U |
| [Operator descriptors](../functions/catalog.toml) | 401 | U |
| Operator row kinds | 249 functions; 84 comparisons; 45 arithmetic; 13 containment; 5 concat; 3 logical; 2 null tests | U |
| Aggregate / window / set-returning descriptors | 53 / 11 / 18 | U |
| [Argument-coercion capture rows](../functions/coercion_snapshot.toml) | 246 | U |
| [SQLSTATE registry](../errors/registry.toml) | 114 | U |
| [Runtime cost units](../cost/schedule.toml) | 30 | U |
| [Estimator vectors](../cost/estimator_vectors.toml) | 4 arithmetic; 9 predicate; 10 candidate | U |
| [Golden database files](../fileformat/fixtures/) | 112, including 6 encrypted files | U |
| [Integrity image/report pairs](../fileformat/integrity/) | 24 | U |
| [Integrity result registry](../integrity/findings.toml) | 32 findings; 5 skips | U |
| [Page-service message catalog](../pageservice/messages.toml) | 22 messages | U |
| [Page-service fixtures](../pageservice/fixtures.toml) | 27 frames; 2 owner records; 14 malformed cases | U |
| [Encoding vector files](../encoding/) | 15 TOML files | U |
| [Regex vectors](../regex/) | 33 programs; 41 matches; 19 SIMILAR cases | U |
| [LZ4 vectors](../fileformat/lz4_vectors.toml) | 11 | U |
| [Crypto vectors](../fileformat/encryption_vectors.toml) | 3 XAES; 1 subkey input set (2 outputs); 1 host-KEK; 1 key-block; 1 rewrap; 1 Argon2id | U |
| [Collation vectors](../collation/vectors/) | 1 bundle; 2 compiler; 46 sort-key cases | U |
| [Timezone vectors](../tz/vectors/) | 1 bundle; 16 TZif cases | U |
| [PostgreSQL keyword table](../grammar/pg_keywords.toml) | 164 keywords | U |
| [Datetime token tables](../types/datetime_units.toml) | 61 delta; 72 date; 13 interval qualifiers | U |
| [Formatting vocabulary](../types/formatting.toml) | 112 datetime; 36 numeric; 7 suffixes; 15 word-list groups | U |

The exact baseline file format is **v50**, not the older versions mentioned in
some summaries. [format.md](../fileformat/format.md) adds UNIQUE-constraint ownership
at v50 (`index_flags` bit 5, legal only with the unique bit). V49 already requires
batched durability; v48 view defaults/options and spanning catalog entries; v47
multi-column GIN and posting runs; v46 ordered index slots. The
[encrypted variant](../fileformat/encrypted.md) has the same format version and its
own capacity, bound-pointer, key-block, seal-order, and recovery rules.

## 3. Functional completion matrix

Every row is required unless explicitly marked as a preserved deferral in §6.
The final capability inventory below prevents broad row labels from hiding an
unsupported capability. Suite links identify relevant evidence, not a permission
to run only those files.

| ID | Required implementation and observable contract | Canonical contract | Shared evidence | State |
|---|---|---|---|---|
| L01 | UTF-8 lexer, quoted/unquoted and Unicode-escaped names, keyword categories, nested comments, literal forms, source locations, identifier/input/parameter limits | [grammar](grammar.md), [EBNF](../grammar/grammar.ebnf), [keywords](../grammar/pg_keywords.toml) | [query](../conformance/suites/query/), [resource](../conformance/suites/resource/) | U |
| L02 | Every statement production: query, DDL/DML, transactions, ANALYZE, EXPLAIN; syntax/error precedence and parser nesting gates | [grammar](grammar.md), [errors](error-fields.md) | All SQL suites; [error positions](../errors/position_cases.txt) | U |
| L03 | Name/scope/alias resolution, correlated levels, unknown literal adaptation, strict static inference, output names and result types | [types](types.md), [grammar](grammar.md) | [types](../conformance/suites/types/), [subquery](../conformance/suites/subquery/), [params](../conformance/suites/params/) | U |
| L04 | Explicit/assignment/implicit casts, numeric promotion, overload selection, named/optional/VARIADIC arguments and coercion capture | [casts](../types/casts.toml), [comparison data](../types/compare.toml), [arg coercion](arg-coercion.md), [functions](functions.md) | [cast](../conformance/suites/cast/), [argument coercion](../conformance/suites/expr/arg_coercion.test), capture snapshot | U |
| L05 | Structured SQLSTATE, message/detail/hint/position fields, SQL Unicode position accounting and deterministic failure visitation | [error fields](error-fields.md), [registry](../errors/registry.toml) | [detail cases](../errors/detail_cases.txt), [position cases](../errors/position_cases.txt), error suites | U |
| T01 | Signed i16/i32/i64 and aliases, checked arithmetic, boolean and three-valued NULL rules | [types](types.md), [scalars](../types/scalars.toml), [compare](../types/compare.toml) | [types](../conformance/suites/types/), [null](../conformance/suites/null/), [compare](../conformance/suites/compare/) | U |
| T02 | Exact finite decimals, scale preservation, capped and uncapped arithmetic, PG division scale/rounding, numeric functions and transcendental kernels | [decimal](decimal.md), [functions](../functions/catalog.toml) | [decimal types](../conformance/suites/types/decimal.test), [decimal arithmetic](../conformance/suites/expr/decimal_arithmetic.test), numeric/cost suites | U |
| T03 | f32/f64, total NaN/infinity order, underflow/overflow and domain behavior, exact text render, allowed tolerance only under the ledger | [float](float.md), [determinism](determinism.md) | [float types](../conformance/suites/types/float.test), float expression suites, [float vectors](../encoding/float.toml) | U |
| T04 | UTF-8 text/varchar, raw bytea, UUID, scalar codecs and text I/O; code-point string operations | [types](types.md), [string functions](string-functions.md), [encoding](encoding.md) | text/bytea/uuid type suites; string/bytea/UUID expression suites | U |
| T05 | Date/timestamp/timestamptz infinities, precision typmods, interval months/days/micros, parsing/rendering, exact temporal arithmetic | [date](date.md), [timestamp](timestamp.md), [interval](interval.md) | temporal type/expression suites; date/timestamp/interval encoding vectors | U |
| T06 | Environment-loaded JTZ/TZif, aliases, POSIX tails, transitions/gaps/folds, fixed zones, session-zone input/output/arithmetic, persisted timezone dependency pins | [timezones](timezones.md), [timezone data](../tz/README.md), [index dependencies](index-dependencies.md) | timezone expressions/types; [TZ vectors](../tz/vectors/); timezone index golden | U |
| T07 | Jed UCA compiler/executor, loaded Unicode data, embedded/reference collation artifacts, sort keys and skew checks, collated text/arrays/indexes | [collation](collation.md), [collation data](../collation/README.md) | [collation suite](../conformance/suites/collation/), collation vectors and golden files | U |
| T08 | Nominal named composite types, recursive fields, ROW/inline record query values, projection/assignment, 3VL comparison, text I/O, dependency/drop rules | [composite](composite.md), [extensibility](extensibility.md) | composite type/DDL/DML suites; composite goldens/vectors | U |
| T09 | Structural arrays with value-level shape/lower bounds, multidimensional values, subscripting/slices, recursive codecs/comparison and functions | [arrays](array.md), [array functions](array-functions.md) | array types/expressions/aggregates/unnest; array goldens | U |
| T10 | Six structural range types, discrete canonicalization, bounds/empty/infinite semantics, constructors/operators/functions and range keys | [ranges](ranges.md), [range functions](range-functions.md), [range data](../types/ranges.toml) | range type/expression suites; range vectors/goldens | U |
| T11 | Recursive order-preserving keys, nullable and ordered slots, prefix-free composition, scalar-array/range/composite key eligibility | [encoding](encoding.md), [format](../fileformat/format.md) | All [encoding vectors](../encoding/); array/composite/range/index key suites and goldens | U |
| F01 | Every one of the 401 operator rows with its family resolution, null contract, volatility/dependency, trapping, return type and cost | [catalog](../functions/catalog.toml), [functions](functions.md) | [expr](../conformance/suites/expr/), [cast](../conformance/suites/cast/), [json](../conformance/suites/json/) | U |
| F02 | Short-circuit AND/OR, CASE/COALESCE/NULLIF/GREATEST/LEAST, IN/BETWEEN/quantified comparison, exact filter conjunct ordering | [grammar](grammar.md), [planner](planner.md), [cost](cost.md) | expression and query rewrite suites | U |
| F03 | Text building/escaping/digest functions, LIKE/ILIKE, formatting vocabulary and exact datetime/numeric formatting | [string functions](string-functions.md), [formatting](formatting.md) | string/hash/formatting expression suites; scalar-output resource suites | U |
| F04 | Handwritten bounded regex compiler/Pike VM, regexp functions/SRFs, SIMILAR/substring syntax, compile/step costs | [regex](regex.md) | [regex vectors](../regex/), regexp expression/SRF/resource suites | U |
| F05 | Verbatim json, canonical exact-numeric jsonb, codecs, nesting gates, comparison/containment, builders/accessors/mutation/aggregation | [json](json.md), [JSON SQL functions](json-sql-functions.md) | [json suite](../conformance/suites/json/), json/jsonb goldens | U |
| F06 | First-class query jsonpath compiler/render/evaluator, strict/lax, methods/predicates/variables/recursive descent, SQL/JSON behaviors and PASSING | [jsonpath](jsonpath.md), [JSON SQL functions](json-sql-functions.md) | jsonpath/JSON behavior/float-source suites; JSON resource suites | U |
| F07 | JSON record/populate functions, column-definition lists, JSON_TABLE nested paths and standard default behaviors | [JSON table](json-table.md), [composite](composite.md) | json_record/json_populate/json_table suites | U |
| F08 | Every one of the 18 SRF descriptors: generation/unnest/regexp/JSON rows, lateral binding, names/ordinality where implemented, source costs | [functions](functions.md), [catalog](../functions/catalog.toml), [grammar](grammar.md) | generate_series/unnest/SRF suites; JSON and joins | U |
| Q01 | SELECT/VALUES, FROM-less row, WHERE/projection/order/limit/offset, aliases, deterministic ties and sort/null behavior | [grammar](grammar.md), [streaming](streaming.md) | [query](../conformance/suites/query/), [setops](../conformance/suites/setops/) | U |
| Q02 | Inner/cross/outer/lateral joins, nesting/scopes, ON/USING/NATURAL behavior where accepted, nested-loop/hash/INL physical execution | [grammar](grammar.md), [planner](planner.md), [estimator](estimator.md) | [joins](../conformance/suites/joins/), cost-plan and INL query suites | U |
| Q03 | Scalar/row/EXISTS/IN/ANY/ALL subqueries, correlations including grouped outer levels, derived tables and values bodies | [grammar](grammar.md), [planner](planner.md) | [subquery](../conformance/suites/subquery/) | U |
| Q04 | UNION/INTERSECT/EXCEPT with ALL/distinct, precedence, common types, ordering and materialization | [grammar](grammar.md), [memory](memory.md) | [setops](../conformance/suites/setops/) | U |
| Q05 | All 53 aggregates, null handling, exact widening, DISTINCT/FILTER/order, statistical/regression/ordered/hypothetical sets and collation | [aggregates](aggregates.md), [catalog](../functions/catalog.toml) | [aggregates](../conformance/suites/aggregates/) | U |
| Q06 | GROUP BY expressions and functional dependency, grouping sets/ROLLUP/CUBE/GROUPING, HAVING, DISTINCT and DISTINCT ON | [aggregates](aggregates.md), [grammar](grammar.md) | grouping aggregate suites, query distinct suites | U |
| Q07 | All 11 window descriptors and aggregate windows; named windows, partitions/peers, ROWS/RANGE/GROUPS bounds, exclusions/sliding frames and shared work | [window](window.md), [catalog](../functions/catalog.toml) | [window](../conformance/suites/window/), grouping_sets_window | U |
| Q08 | CTE inlining/materialization, recursion/work tables and dedup, writable CTE snapshot/effect sequencing | [CTE](cte.md), [recursive CTE](recursive-cte.md), [writable CTE](writable-cte.md) | [cte](../conformance/suites/cte/), CTAS/DML/transaction suites | U |
| P01 | Safe conjunct ordering/pushdown and contradictions, error-preservation proofs, derived/group/window/set-operation/CTE rewrites | [planner](planner.md), [casts](../types/casts.toml) | contradiction/pushdown/where-rewrite/filter-order suites | U |
| P02 | Complete access candidate inventory, bound PK/B-tree/GIN/GiST, prefix/range/point-set/interval-set/partial-index rules, physical order | [planner](planner.md), [indexes](indexes.md), [GIN](gin.md), [GiST](gist.md) | access_candidate_inventory and all index/GIN/GiST query suites | U |
| P03 | Exact rational/saturating estimator, complete tie order, single-relation pipeline, N-way Pareto search through 8 relations and greedy larger islands, hard fences | [estimator](estimator.md), [estimator data](../cost/estimator.toml) | estimator vectors, cost_plan_access/cost_plan_join suites | U |
| P04 | Custom parameter plans; value-blind classification; cache signatures over domain/catalog/statistics; bound values at all specified reads | [estimator](estimator.md), [API](api.md) | [params](../conformance/suites/params/); host prepared-cache tests | U |
| P05 | Transactional ANALYZE, deterministic samples/KMV/MCV/histograms/GIN terms, retained staleness and estimator consumption | [statistics](statistics.md), [estimator data](../cost/estimator.toml) | statistics query/introspection/transaction suites and two statistics goldens | U |
| P06 | EXPLAIN/ANALYZE/VERBOSE/COSTS/LANE, selected shape and canonical expressions, integer estimates and inclusive actual attribution across query/DML/CTE | [explain](explain.md) | explain query/DML/CTAS/MERGE suites | U |
| D01 | Table/type/index create/drop/rename, namespace and auto-name byte rules, IF EXISTS variants, ALTER operations and dependencies | [grammar](grammar.md), [alter](alter.md), [composite](composite.md) | [ddl](../conformance/suites/ddl/), catalog goldens | U |
| D02 | NOT NULL/default expressions/CHECK/PK/UNIQUE, v50 constraint ownership, end-state all-or-nothing validation and dependency naming | [constraints](constraints.md), [format](../fileformat/format.md) | constraint DDL/DML suites, unique/check/default goldens | U |
| D03 | Foreign keys, compatible key families, MATCH SIMPLE/FULL, every action, child probes, rekey/cascade/end-state validation | [constraints](constraints.md) | foreign_key suites; fk/malformed_fk_type goldens | U |
| D04 | Ordered secondary B-trees, expression/partial keys, dependency admission/pins, partial implication, REINDEX and persisted unparseable definitions | [indexes](indexes.md), [index dependencies](index-dependencies.md), [unparsable definitions](unparsable-definitions.md) | expression/partial/dependency/reindex/index-order suites and goldens | U |
| D05 | GIN arrays/jsonb, every admitted scalar element, multi-column tags, term encoding/posting runs, scans/rechecks/INL/statistics | [GIN](gin.md), [term vectors](../encoding/gin_jsonb_terms.toml), [runs](../encoding/gin_posting_runs.toml) | GIN DDL/query suites and GIN goldens | U |
| D06 | GiST ranges/fixed scalars, canonical R-tree, exclusion constraints including btree/partial, probe/maintenance/REINDEX | [GiST](gist.md), [constraints](constraints.md) | GiST/exclusion query and DDL suites; GiST/exclusion goldens | U |
| D07 | INSERT expressions/multi-row/SELECT/defaults, UPDATE/DELETE two-phase edits/rekeys, RETURNING, privileges and resource rollback | [grammar](grammar.md), [constraints](constraints.md), [transactions](transactions.md) | [dml](../conformance/suites/dml/), [mutation](../conformance/suites/mutation/) | U |
| D08 | ON CONFLICT arbitration including partial/exclusion cases, DO NOTHING/UPDATE, excluded row and updates/rekeys | [upsert](upsert.md) | insert_on_conflict family | U |
| D09 | MERGE source joins, matched/not-matched actions, cardinality/errors, action order, view target, parameters, EXPLAIN/cost | [merge](merge.md) | MERGE DML suite family | U |
| D10 | Sequences/serial/identity, ownership and ALTER, transactional counters/currval/default application and per-row mutation cost | [sequences](sequences.md) | sequence/serial/identity suites and goldens | U |
| D11 | CTAS/SELECT INTO, declared/inferred column metadata, temporary and writable-CTE forms, constraints and rollback | [CTAS](create-table-as.md) | create_table_as/select_into suites | U |
| D12 | Views: canonical text, frozen stars, replace/alter/defaults/options/dependencies/rename/cascade, temporary/recursive, updatable and check option | [views](views.md), [unparsable definitions](unparsable-definitions.md) | view DDL/DML/query/introspection suites; view goldens | U |
| D13 | Session-local temporary tables/views/types/indexes/owned sequences, ON COMMIT semantics, temp-domain reclamation and file spill | [temp tables](temp-tables.md) | temp DDL/attachment/session/resource suites | U |
| D14 | SQL introspection tables, names/types/options/dependencies/statistics and session/attachment visibility | [introspection](introspection.md) | [introspection](../conformance/suites/introspection/) | U |
| X01 | Single committed snapshot plus one writer pending set, read/write handles, gates and watermark; statement rollback, SQL transaction failure/savepoints/read-only | [transactions](transactions.md), [concurrency](concurrency-testing.md) | [transactions](../conformance/suites/transactions/), [concurrency](../conformance/suites/concurrency/) | U |
| X02 | Sessions, capabilities including column read/function execution/DDL, variables, per-statement/lifetime budgets and frozen settings | [session](session.md), [environment](environment.md) | [session](../conformance/suites/session/), host matrix | U |
| X03 | Prepared/bound/batched execution, genuine public pull cursor, snapshot/cost/account ownership, early close/errors, cancellation and collectors | [API](api.md), [streaming](streaming.md) | [params](../conformance/suites/params/), bounded streaming suites, host matrix | U |
| X04 | Immutable environment, injected SQL entropy/clock, Unicode/TZ data, scalar batched host functions, overload/cost/dependency/reopen contracts | [environment](environment.md), [entropy](entropy.md), [extensibility](extensibility.md) | clock/UUID/host-function suites and host matrix | U |
| R01 | All 30 deterministic work units and guard points, max/lifetime ceilings and error precedence, frozen/internal meters | [cost](cost.md), [schedule](../cost/schedule.toml) | Every cost assertion; resource/cost/lifetime suites | U |
| R02 | Parser/expression/JSON/composite/view/regex/identifier/input/parameter/list limits and fail-closed SQL surface | [cost](cost.md), subsystem nesting rules | [resource](../conformance/suites/resource/) | U |
| R03 | Scalar amplifier allowance and growth-kernel cost, allocation preflight, exact decimal scratch work | [memory](memory.md), [cost](cost.md) | scalar_memory/output, dos_amplification and decimal resource cases | U |
| R04 | Q1 row/collector buffers, Q2 operator state, Q3 pending writes, live/released/peak logical accounting and all nested account sharing | [memory](memory.md), [sizes](../cost/memory_sizes.toml) | query_memory family plus peak/account runner comparisons | U |
| R05 | Q4 memory committed pages/file live pages, temp storage budget, cache eviction, per-domain limits and unchanged-commit behavior | [memory](memory.md), [transactions](transactions.md) | storage_memory/storage_file/temp_budget, storage TSV and process histories | U |
| S01 | Five-method byte device, memory/file hosts, pager/fault seam, packed leaf/PAX/lazy touched-column decode, bounded cache, logical page reads | [hosts](hosts.md), [pager](pager.md), [packed leaves](packed-leaf.md), [lazy record](lazy-record.md) | goldens, host/pager tests, row_materialization and spill suites | U |
| S02 | Full v50 meta/catalog/type/sequence/view/statistics/index codec; exact rejection of invalid/reserved fields, current-version-only open | [format](../fileformat/format.md) | All 112 goldens and corruption corpus | U |
| S03 | B+tree insert/delete/split/merge/separators, dirty path COW, free-list persistence, reader-aware reclamation and bounded file growth | [B+tree](bplus-reshape.md), [storage](storage.md), [format](../fileformat/format.md) | tall-tree/separator/golden fixtures, storage corpus, simulations | U |
| S04 | Exact large-value packing/externalization, handwritten pinned LZ4, lazy overflow/decompression, unchanged-chain retention and ownership | [large values](large-values.md), [LZ4](../fileformat/lz4.md) | LZ4 vectors, compressed/overflow goldens, simulator large-values profile | U |
| S05 | Validated COW body/manifest/meta/barrier ordering, CRC32/CRC64, manifest dependency protection, recovery fallback/stabilization, poison-on-I/O | [validated COW](validated-cow.md), [format](../fileformat/format.md) | cow/malformed goldens and storage suites, process crash scenarios, simulator | U |
| S06 | v49 synchronous-off floor/cumulative manifests, slot protection, cap/deadline/close flushes, client synchronization and truncation rules | [validated COW §4](validated-cow.md), [transactions](transactions.md) | service_relaxed_clients process case; relaxed sim profiles/regressions | U |
| S07 | File/memory attachments, consistent pinned roots and name routing, ordered gates, independent unchanged roots and one-file-writer limit | [attachments](attached-databases.md), [transactions](transactions.md) | [attach](../conformance/suites/attach/), attachment process/simulation scenarios | U |
| S08 | External sort, row spools/hash join/group/dedup partitions, deterministic replay and cost, admission-triggered early spill, cleanup/failure | [spill](spill.md), [memory](memory.md) | blocking_spill/query_memory_spill suites, spill modes and simulator | U |
| S09 | Compaction/rekey scratch-domain rebuild, atomic rename/directory barrier, trailing truncation, no-held-reader/writer constraints and backup test seam | [API](api.md), [hosts](hosts.md), [page service](page-service.md) | compact/backup process cases, compaction simulation and host matrix | U |
| S10 | Quick/full integrity traversal, findings/skips/order/truncation, all index kinds and constraints, corrupt-file safety | [integrity](integrity-check.md), [registry](../integrity/findings.toml) | All 24 [image/report pairs](../fileformat/integrity/) and live checks | U |
| E01 | Approved vetted crypto packages, XAES derivation glue, HKDF/key wrap/Argon2id credentials, independent crypto entropy and safe native boundaries | [encryption](encryption.md), [encrypted bytes](../fileformat/encrypted.md) | All crypto vector input/output sets; dependency/native-boundary review | U |
| E02 | Encrypted create/open/attach, authenticated metadata/key blocks, tag-bound pointers, seal order, credential failures and stale/replayed pages | [encrypted bytes](../fileformat/encrypted.md) | Six encrypted goldens, encrypted corpus and simulations | U |
| E03 | Credential change two barriers, DEK rotation/conversion, encrypted scratch/temp/spill/backups, service unlocking/resealing and crash/replay recovery | [encryption](encryption.md), [encrypted bytes](../fileformat/encrypted.md) | encrypted process histories, encryption profiles/regressions, host matrix | U |
| H01 | Linux stable protocol-v2 bundle, crash-clean OS locks, owner election/arrival/presence gates, auto/exclusive/none, aliases/timeout/fail-closed behavior | [locking](locking.md) | process histories and host lock tests | U |
| H02 | Native Swift page-service owner/client; all framing/messages/bounds; pins, deltas/status, promotion/yield/drain, attachments, read-ahead and stream client | [page service](page-service.md), [wire data](../pageservice/README.md) | wire fixtures, all process pairings and service simulations | U |
| H03 | Owner foreground zero coordination calls/meta rereads, owner-only maintenance/reuse and remote pin watermark accounting | [page service](page-service.md), [locking](locking.md) | owner instrumentation, process/simulator tests | U |
| V01 | Real shared-corpus runner with exact capability declaration, errors/types/rendering/sorts/hashes, params/options/directives and actual public cursor route | [conformance](conformance.md), [host matrix](swift-native-host-matrix.md) | All suites in every applicable runner mode | U |
| V02 | Deterministic scheduler/real process actors/simulated disk+lock+network+spill; exact transcripts, durability floor and byte crash images | [simulation](simulation-testing.md), [concurrency](concurrency-testing.md) | 7 schedules, 31 process cases, 22 sim profiles, 18 regressions | U |
| V03 | Bidirectional current-format interchange with Rust/Go/TS, plaintext/encrypted, incremental histories and fixture reading/writing | [format](../fileformat/format.md), [encrypted bytes](../fileformat/encrypted.md) | Goldens and cross-core process/round-trip runs | U |
| V04 | Oracle checks for relevant PG-comparable behavior; default repository gates remain healthy without Swift | [conformance](conformance.md), [oracle profile](../conformance/oracle_profile.toml) | `mise run corpus:check`; default verify/test/CI checks | U |
| V05 | Usable Swift package and local embedding examples; correctness report, release baseline, matched-checksum benchmark and measured optimization | [benchmarks](benchmarks.md), [cores](cores.md) | Opt-in release benchmark runs; recorded commands/hardware/toolchain | U |

## 4. Shared type and module constraints

These are integration constraints, not a replacement for each subsystem's spec.

- The scalar descriptor set is `i16`, `i32`, `i64`, `boolean`, `text`, `decimal`,
  `bytea`, `uuid`, `timestamp`, `timestamptz`, `interval`, `f32`, `f64`, `date`,
  `json`, `jsonb`, `jsonpath`. Described does not imply storable or keyable:
  `jsonpath` is currently a query value; the JSON/key narrowings still apply.
- The type system needs scalar, nominal composite reference, structural array,
  structural range, and inline record arms. Arrays carry dimensions/lower bounds
  in values, not type identity. Typmods and collation metadata must survive all
  relevant resolved columns/expressions and catalog boundaries.
- NULL is a value absence under a known static type. Boolean NULL is SQL UNKNOWN;
  composite comparison/NULL tests and array/range btree comparison have different
  recursive rules. A single host equality implementation cannot stand in for them.
- Swift's native `String` equality and hashing use Unicode canonical equivalence;
  jed text and identifiers require byte-exact UTF-8 equality under their specified
  collation rules. Name maps, JSON keys, stored SQL, ordinary text equality and
  ordering must not silently normalize. Character indexing/counting is grapheme
  based; SQL code-point operations require Unicode scalars. Test these boundaries.
- Decimal representation is handwritten signed magnitude plus display scale and
  coefficient limbs; limb base is internal, while arithmetic/rounding/results and
  base-10^4 stored digits are contract. Foundation Decimal is not the required
  arbitrary-precision finite decimal domain. Host Double rendering is not the
  required PostgreSQL float render algorithm.
- Storage is a page-backed persistent tree over a byte device, for memory and
  durable hosts alike. Keep storage-key bytes distinct from logical values, and
  packed/lazy records distinct from materialized row arrays. No full-image load
  or in-memory dictionary shortcut can satisfy S01–S08.
- Cost, scalar allowance and live query-memory account must be shared through
  nested evaluation and retained cursors; pending writes survive statement-account
  creation. Planner estimates use exact arithmetic and shared facts, not runtime
  floating-point heuristics. These interfaces must precede dependent executors.
- SQL entropy/clock live in immutable environment seams. Encryption entropy is a
  separate CSPRNG seam; seeded SQL entropy is not acceptable key material.
- File hosts isolate permitted OS/native operations. The approved crypto native
  backends must not broaden into parser, arithmetic, codec, or storage delegation.

## 5. Verification matrix and evidence rules

The host matrix defines adapter details. These are independent required lanes;
passing one does not imply any other.

| Lane | Required comparison / exercised boundary | Initial result |
|---|---|---|
| SQL memory | Every applicable suite, real prepare/query/cursor path, all directives and capabilities | Not run |
| SQL durable | File-backed create/commit/reopen and same corpus values/types/errors/costs | Not run |
| SQL spill | Tiny `work_mem`, native file spill and deterministic hash/sort replay | Not run |
| Query memory | Memory/disk/disk-spill account peaks, release invariants, pending writes and storage TSV parity | Not run |
| Encryption | Encrypted file mode, encrypted cost/EXPLAIN transcript parity and credentials | Not run |
| SQL via service | Same engine SQL through page-client storage against supported owners | Not run |
| Shared concurrency | All seven schedules, real writer/read handles, pins/watermark/gate blocking | Not run |
| Real processes | 31 scenarios with Swift owner/client and supported-core pairings | Not run |
| Simulation | 18 regressions, seeded sweeps covering all 22 profiles, matching transcripts and crash images | Not run |
| Bytes | Every encoding/LZ4/crypto/wire/vector contract; all golden reads/writes and integrity reports | Not run |
| Interchange | Swift-to-each-supported-core and reverse, plaintext/encrypted, initial/incremental states | Not run |
| Host APIs | Lifecycle/prepared/cursors/cancel/environment/functions/capabilities/resources/attachments/maintenance | Not run |
| Independent review | Storage durability, approved crypto integration, resource accounting, conformance adapters | Not run |
| Oracle | Relevant PG-clean corpus checks under the checkout's declared oracle profile | Not run |
| Default gates | No implicit Swift dependency in normal CI/tests/benchmarks/codegen | Not run |
| Release performance | Correctness first, preserved baseline, matched workload/durability/checksums, measured optimization | Not run |
| Apple builds | macOS/iOS builds and tests, separately from Linux acceptance | Unverified platform |

An unsupported capability is an omission, not a silently skipped success. During
development a runner may report honest partial support, but final acceptance needs
the complete baseline inventory plus the host-only contracts that capabilities
cannot express. Do not weaken corpus expectations or route the adapter through an
eager engine collector that changes cursor/memory semantics.

## 6. Preserved baseline exclusions and future work

These are examples of intentional baseline boundaries, traced to the actual
[TODO](../../TODO.md) and subsystem docs. Preserve their current error/behavior;
do not turn this experiment into an implementation of future jed features.

| Area | Deliberately outside the implemented baseline |
|---|---|
| New type axes | Enums, domains, separate `time`, multiranges/custom ranges, host-defined types/operator classes |
| Typmods/input | Negative or `s > p` decimal scale; array element typmods; `char(n)`; deferred temporal spellings/zone abbreviation input |
| JSON | String dictionary builder; jsonb ordered/PK keys; stored jsonpath columns; JSON_TABLE explicit PLAN; deferred composite/array JSON-record coldefs and anonymous populate-record base; nonfinite float PASSING; radix/underscore numbers inside jsonpath text |
| Recursive/container limits | Array-of-composite keys and composite FK pairing; unsupported recursive-CTE term/mutual-recursion forms; recorded recursive cast narrowings |
| Optimizer | Bushy/adaptive/parallel plans, costed outer/barrier search, automatic/extended statistics, broader constant folding/CSE, backward index walks and currently unimplemented predicate proofs |
| Indexes/constraints | Deferred GIN element/operator cases, combined multi-column gathers/BitmapOr; generic multi-column GiST, SP-GiST/KNN/new opclasses; deferred constraints; generated columns |
| Views | Materialized views and views in/over attached databases |
| Storage/maintenance | Multi-file atomic writes, old-version migration, page-size-changing/repacking compaction, replication/log/subscribe/replica APIs and encrypted replication key-block items |
| Backups | Existing main-file test seam is required; a new public backup API/CLI, attachment backup and OPFS destination are still product work |
| Extensions | Non-strict/container-argument host functions, host aggregates/table-valued functions, broad taint enforcement and full graded-open compatibility proposal |
| Runtime packaging | Production Swift delivery/promotion, Apple support claim without tests, browser/Node packaging, unrelated CLI/migration packages |
| Security review | Existing encryption's human security review/lifetime-bounds work remains open; reproducing the implemented crypto contract is required and does not certify it reviewed |

The baseline has known narrower behavior and some stale historical prose. Resolve
an apparent disagreement against the current shared data, actual assertions, and
the most specific current design rule, and record a shared ambiguity explicitly.
Never silently use a supported implementation's accident as a new contract.

## 7. Complete suite and capability inventories

The tables below are mechanically enumerated from the recorded baseline. Every
capability starts U even when it belongs to an older profile; satisfying all 19
profiles alone does not cover all 475 capabilities. The exact suite list includes
test areas absent from the old conformance README summary.

### Suite files and SQL directives

| Suite | Files | Statements | Queries | State |
|---|---:|---:|---:|---|
| [aggregates](../conformance/suites/aggregates/) | 52 | 373 | 521 | U |
| [attach](../conformance/suites/attach/) | 6 | 109 | 35 | U |
| [cast](../conformance/suites/cast/) | 13 | 144 | 156 | U |
| [collation](../conformance/suites/collation/) | 14 | 147 | 225 | U |
| [compare](../conformance/suites/compare/) | 1 | 3 | 8 | U |
| [concurrency](../conformance/suites/concurrency/) | 7 | 0 | 0 | U |
| [cte](../conformance/suites/cte/) | 14 | 141 | 137 | U |
| [ddl](../conformance/suites/ddl/) | 101 | 3,524 | 687 | U |
| [dml](../conformance/suites/dml/) | 42 | 1,217 | 485 | U |
| [expr](../conformance/suites/expr/) | 113 | 1,080 | 2,260 | U |
| [introspection](../conformance/suites/introspection/) | 17 | 234 | 209 | U |
| [joins](../conformance/suites/joins/) | 34 | 315 | 305 | U |
| [json](../conformance/suites/json/) | 51 | 696 | 905 | U |
| [mutation](../conformance/suites/mutation/) | 3 | 50 | 22 | U |
| [null](../conformance/suites/null/) | 1 | 6 | 14 | U |
| [params](../conformance/suites/params/) | 5 | 95 | 150 | U |
| [query](../conformance/suites/query/) | 99 | 1,071 | 1,410 | U |
| [resource](../conformance/suites/resource/) | 33 | 650 | 369 | U |
| [rqg](../conformance/suites/rqg/) | 5 | 420 | 150 | U |
| [session](../conformance/suites/session/) | 5 | 94 | 25 | U |
| [setops](../conformance/suites/setops/) | 9 | 153 | 165 | U |
| [storage](../conformance/suites/storage/) | 26 | 155 | 79 | U |
| [subquery](../conformance/suites/subquery/) | 21 | 182 | 298 | U |
| [transactions](../conformance/suites/transactions/) | 11 | 292 | 55 | U |
| [types](../conformance/suites/types/) | 44 | 652 | 746 | U |
| [window](../conformance/suites/window/) | 22 | 141 | 165 | U |
| **Total** | **749** | **11,944** | **9,581** | **U** |

### Named profiles

All profile definitions and inheritance come from [manifest.toml](../conformance/manifest.toml).

`core`, `mutation`, `casts`, `comparison`, `expression`, `decimal`, `functions`, `joins`, `outer_joins`, `constraints`, `aggregates`, `grouping`, `having`, `predicates`, `timestamps`, `set_operations`, `transactions`, `subqueries`, `cte`.

### All baseline capabilities

The identifier is the exact key of its full description in
[manifest.toml](../conformance/manifest.toml). No capability is inferred from a
Swift implementation name or a profile membership.

#### attach — 1 capabilities

| Capability | State |
|---|---|
| `attach.in_memory` | U |

#### cast — 13 capabilities

| Capability | State |
|---|---|
| `cast.array` | U |
| `cast.bool_int` | U |
| `cast.composite` | U |
| `cast.datetime` | U |
| `cast.explicit` | U |
| `cast.numeric_input_forms` | U |
| `cast.operator` | U |
| `cast.runtime_text` | U |
| `cast.scalar_text` | U |
| `cast.string_literal` | U |
| `cast.text_date` | U |
| `cast.text_string_native` | U |
| `cast.uuid` | U |

#### compare — 1 capabilities

| Capability | State |
|---|---|
| `compare.promotion` | U |

#### ddl — 57 capabilities

| Capability | State |
|---|---|
| `ddl.alter_identity` | U |
| `ddl.alter_sequence` | U |
| `ddl.alter_table` | U |
| `ddl.check` | U |
| `ddl.collate_column` | U |
| `ddl.column_default` | U |
| `ddl.column_default_expr` | U |
| `ddl.composite_default` | U |
| `ddl.composite_key` | U |
| `ddl.composite_primary_key` | U |
| `ddl.constraint_name_schema` | U |
| `ddl.create_index_if_not_exists` | U |
| `ddl.create_table` | U |
| `ddl.create_table_as` | U |
| `ddl.create_table_as_writable_with` | U |
| `ddl.create_table_if_not_exists` | U |
| `ddl.drop_table` | U |
| `ddl.drop_table_if_exists` | U |
| `ddl.drop_table_multi` | U |
| `ddl.exclusion_btree` | U |
| `ddl.exclusion_constraint` | U |
| `ddl.exclusion_partial` | U |
| `ddl.explain_create_table_as` | U |
| `ddl.foreign_key` | U |
| `ddl.foreign_key_child_index` | U |
| `ddl.foreign_key_cross_type` | U |
| `ddl.foreign_key_match_full` | U |
| `ddl.gin_index` | U |
| `ddl.gin_multi_column` | U |
| `ddl.gist_index` | U |
| `ddl.gist_scalar_index` | U |
| `ddl.identity` | U |
| `ddl.index_expr` | U |
| `ddl.index_key_order` | U |
| `ddl.index_partial` | U |
| `ddl.not_null` | U |
| `ddl.primary_key` | U |
| `ddl.reindex` | U |
| `ddl.secondary_index` | U |
| `ddl.select_into` | U |
| `ddl.sequence` | U |
| `ddl.sequence_as_type` | U |
| `ddl.serial` | U |
| `ddl.temp_on_commit` | U |
| `ddl.temp_table` | U |
| `ddl.unique` | U |
| `ddl.view` | U |
| `ddl.view_alter` | U |
| `ddl.view_cascade` | U |
| `ddl.view_default` | U |
| `ddl.view_dependencies` | U |
| `ddl.view_long_definition` | U |
| `ddl.view_options` | U |
| `ddl.view_recursive` | U |
| `ddl.view_rename` | U |
| `ddl.view_replace` | U |
| `ddl.view_temp` | U |

#### dml — 30 capabilities

| Capability | State |
|---|---|
| `dml.costed_access` | U |
| `dml.delete` | U |
| `dml.explain` | U |
| `dml.explain_analyze` | U |
| `dml.insert` | U |
| `dml.insert_alias` | U |
| `dml.insert_column_list` | U |
| `dml.insert_default_values` | U |
| `dml.insert_multi_row` | U |
| `dml.insert_on_conflict` | U |
| `dml.insert_on_conflict_decorations` | U |
| `dml.insert_on_conflict_default` | U |
| `dml.insert_on_conflict_exclusion` | U |
| `dml.insert_on_conflict_rekey` | U |
| `dml.insert_select` | U |
| `dml.insert_select_container` | U |
| `dml.insert_select_setop` | U |
| `dml.insert_values_expr` | U |
| `dml.merge` | U |
| `dml.returning` | U |
| `dml.returning_old_new` | U |
| `dml.update` | U |
| `dml.update_container` | U |
| `dml.update_default` | U |
| `dml.upsert_partial_arbiter` | U |
| `dml.view_check_option` | U |
| `dml.view_on_conflict` | U |
| `dml.view_write` | U |
| `dml.where_contradiction` | U |
| `dml.with_clause` | U |

#### error — 2 capabilities

| Capability | State |
|---|---|
| `error.division_by_zero` | U |
| `error.overflow_trap` | U |

#### expr — 43 capabilities

| Capability | State |
|---|---|
| `expr.arithmetic` | U |
| `expr.array_slice` | U |
| `expr.array_subscript` | U |
| `expr.at_time_zone` | U |
| `expr.between` | U |
| `expr.case` | U |
| `expr.coalesce` | U |
| `expr.collate` | U |
| `expr.comparison_value` | U |
| `expr.date_arithmetic` | U |
| `expr.date_trunc` | U |
| `expr.decimal_arithmetic` | U |
| `expr.extract` | U |
| `expr.greatest_least` | U |
| `expr.ilike` | U |
| `expr.in_list` | U |
| `expr.interval_arithmetic` | U |
| `expr.interval_scale` | U |
| `expr.is_json` | U |
| `expr.json_behavior_default` | U |
| `expr.json_query_fns` | U |
| `expr.jsonpath_any` | U |
| `expr.jsonpath_arith` | U |
| `expr.jsonpath_filter` | U |
| `expr.jsonpath_match` | U |
| `expr.jsonpath_methods` | U |
| `expr.jsonpath_predicates` | U |
| `expr.jsonpath_vars` | U |
| `expr.like` | U |
| `expr.like_escape` | U |
| `expr.not_equal` | U |
| `expr.nullif` | U |
| `expr.numeric_literal_forms` | U |
| `expr.numeric_transcendental` | U |
| `expr.parens` | U |
| `expr.precedence` | U |
| `expr.regex_imatch` | U |
| `expr.regex_match` | U |
| `expr.similar_to` | U |
| `expr.text_build` | U |
| `expr.text_concat` | U |
| `expr.timestamp_arithmetic` | U |
| `expr.unary_minus` | U |

#### file — 1 capabilities

| Capability | State |
|---|---|
| `file.page_service` | U |

#### func — 92 capabilities

| Capability | State |
|---|---|
| `func.abs` | U |
| `func.age` | U |
| `func.arg_coercion` | U |
| `func.array` | U |
| `func.array_agg` | U |
| `func.array_compatible` | U |
| `func.array_composite` | U |
| `func.array_containment` | U |
| `func.array_quantified` | U |
| `func.array_to_string` | U |
| `func.bit_and` | U |
| `func.bit_or` | U |
| `func.bit_xor` | U |
| `func.bool_and` | U |
| `func.bool_or` | U |
| `func.casing` | U |
| `func.clock_timestamp` | U |
| `func.coldeflist` | U |
| `func.corr` | U |
| `func.covar_pop` | U |
| `func.covar_samp` | U |
| `func.current_date` | U |
| `func.date_part` | U |
| `func.every` | U |
| `func.json_agg` | U |
| `func.json_builders` | U |
| `func.json_ctor` | U |
| `func.json_each` | U |
| `func.json_object` | U |
| `func.json_object_agg` | U |
| `func.json_populate` | U |
| `func.json_processing` | U |
| `func.json_record` | U |
| `func.json_set` | U |
| `func.json_set_lax` | U |
| `func.json_srf` | U |
| `func.json_table` | U |
| `func.json_verbatim` | U |
| `func.jsonb_access` | U |
| `func.jsonb_contains` | U |
| `func.jsonb_mutate` | U |
| `func.jsonb_path` | U |
| `func.make_date` | U |
| `func.make_timestamp` | U |
| `func.mixed_numeric_args` | U |
| `func.named_arguments` | U |
| `func.now` | U |
| `func.range_accessors` | U |
| `func.range_constructors` | U |
| `func.range_operators` | U |
| `func.range_set_operators` | U |
| `func.regexp_count` | U |
| `func.regexp_instr` | U |
| `func.regexp_like` | U |
| `func.regexp_match` | U |
| `func.regexp_matches` | U |
| `func.regexp_replace` | U |
| `func.regexp_split_to_array` | U |
| `func.regexp_split_to_table` | U |
| `func.regexp_substr` | U |
| `func.regr_avgx` | U |
| `func.regr_avgy` | U |
| `func.regr_count` | U |
| `func.regr_intercept` | U |
| `func.regr_r2` | U |
| `func.regr_slope` | U |
| `func.regr_sxx` | U |
| `func.regr_sxy` | U |
| `func.regr_syy` | U |
| `func.round` | U |
| `func.row_to_json` | U |
| `func.sequence` | U |
| `func.stddev` | U |
| `func.stddev_pop` | U |
| `func.stddev_samp` | U |
| `func.string_agg` | U |
| `func.string_to_array` | U |
| `func.substring_regex` | U |
| `func.substring_similar` | U |
| `func.to_char` | U |
| `func.to_jsonb` | U |
| `func.to_number` | U |
| `func.to_timestamp` | U |
| `func.unknown_args` | U |
| `func.unnest` | U |
| `func.unnest_record` | U |
| `func.uuid_extract` | U |
| `func.uuid_generate` | U |
| `func.var_pop` | U |
| `func.var_samp` | U |
| `func.variadic` | U |
| `func.variance` | U |

#### harness — 5 capabilities

| Capability | State |
|---|---|
| `harness.attach` | U |
| `harness.fixture_open` | U |
| `harness.fixture_open_error` | U |
| `harness.params` | U |
| `harness.upgrade_collations` | U |

#### introspect — 9 capabilities

| Capability | State |
|---|---|
| `introspect.columns` | U |
| `introspect.constraints` | U |
| `introspect.indexes` | U |
| `introspect.sequences` | U |
| `introspect.statistics` | U |
| `introspect.tables` | U |
| `introspect.type_fields` | U |
| `introspect.types` | U |
| `introspect.views` | U |

#### maintenance — 2 capabilities

| Capability | State |
|---|---|
| `maintenance.analyze` | U |
| `maintenance.analyze_gin_terms` | U |

#### null — 1 capabilities

| Capability | State |
|---|---|
| `null.three_valued` | U |

#### query — 145 capabilities

| Capability | State |
|---|---|
| `query.aggregate_distinct` | U |
| `query.aggregate_filter` | U |
| `query.aggregate_order_by` | U |
| `query.aggregate_select_distinct` | U |
| `query.aggregates` | U |
| `query.any_array_point_lookup` | U |
| `query.bare_column_alias` | U |
| `query.bounded_limit_streaming` | U |
| `query.column_alias` | U |
| `query.comma_from` | U |
| `query.comparison_order` | U |
| `query.composite_key_set` | U |
| `query.composite_pk_pushdown` | U |
| `query.correlated_pushdown` | U |
| `query.cross_join` | U |
| `query.cte` | U |
| `query.cte_data_modifying` | U |
| `query.cte_nested` | U |
| `query.cte_recursive` | U |
| `query.cte_search_cycle` | U |
| `query.derived_pushdown` | U |
| `query.derived_table` | U |
| `query.distinct` | U |
| `query.distinct_on` | U |
| `query.except` | U |
| `query.explain` | U |
| `query.explain_analyze` | U |
| `query.filter_conjunct_order` | U |
| `query.gin_any_eq` | U |
| `query.gin_array_eq` | U |
| `query.gin_bytea_elements` | U |
| `query.gin_decimal_elements` | U |
| `query.gin_disjunction` | U |
| `query.gin_element_types` | U |
| `query.gin_float_elements` | U |
| `query.gin_index_nested_loop` | U |
| `query.gin_interval_elements` | U |
| `query.gin_jsonb_jsonpath` | U |
| `query.gin_jsonb_ops` | U |
| `query.gin_mutation` | U |
| `query.gin_scan` | U |
| `query.gin_text_elements` | U |
| `query.gist_index_nested_loop` | U |
| `query.gist_scalar_scan` | U |
| `query.gist_scan` | U |
| `query.group_by` | U |
| `query.group_by_correlated` | U |
| `query.group_by_expr` | U |
| `query.group_by_functional_dependency` | U |
| `query.grouping_sets` | U |
| `query.grouping_sets_window` | U |
| `query.hash_join` | U |
| `query.having` | U |
| `query.hypothetical_set_aggregate` | U |
| `query.index_expr` | U |
| `query.index_mutation` | U |
| `query.index_nested_loop` | U |
| `query.index_partial` | U |
| `query.index_partial_access_paths` | U |
| `query.index_partial_full_scan` | U |
| `query.index_partial_implication` | U |
| `query.index_prefix` | U |
| `query.index_range` | U |
| `query.index_variable_width_tail` | U |
| `query.intersect` | U |
| `query.interval_set` | U |
| `query.is_distinct_from` | U |
| `query.is_null` | U |
| `query.join_full` | U |
| `query.join_inner` | U |
| `query.join_left` | U |
| `query.join_natural` | U |
| `query.join_parenthesized` | U |
| `query.join_pushdown` | U |
| `query.join_right` | U |
| `query.join_using` | U |
| `query.lateral` | U |
| `query.limit` | U |
| `query.limit_short_circuit` | U |
| `query.logical_connectives` | U |
| `query.offset` | U |
| `query.on_pushdown` | U |
| `query.or_in_point_lookup` | U |
| `query.order_by` | U |
| `query.order_by_alias` | U |
| `query.order_by_correlated` | U |
| `query.order_by_expr` | U |
| `query.order_by_grouped_window` | U |
| `query.order_by_grouping` | U |
| `query.order_by_index_scan` | U |
| `query.order_by_join_inl` | U |
| `query.order_by_join_scan` | U |
| `query.order_by_keys` | U |
| `query.order_by_ordinal` | U |
| `query.order_by_pk_scan` | U |
| `query.order_by_topk` | U |
| `query.order_by_window` | U |
| `query.ordered_set_aggregate` | U |
| `query.ordered_set_array_fraction` | U |
| `query.ordered_set_collation` | U |
| `query.ordered_set_interval` | U |
| `query.ordered_set_nonconstant_fraction` | U |
| `query.point_lookup` | U |
| `query.qualified_column` | U |
| `query.qualified_star` | U |
| `query.qualified_table` | U |
| `query.quoted_identifiers` | U |
| `query.safe_conjunct_gate` | U |
| `query.select` | U |
| `query.select_no_from` | U |
| `query.select_star` | U |
| `query.set_returning` | U |
| `query.setop_parenthesized` | U |
| `query.subquery_correlated` | U |
| `query.subquery_exists` | U |
| `query.subquery_in` | U |
| `query.subquery_quantified` | U |
| `query.subquery_row_valued` | U |
| `query.subquery_scalar` | U |
| `query.table_alias` | U |
| `query.table_column_alias` | U |
| `query.unicode_escapes` | U |
| `query.union` | U |
| `query.values` | U |
| `query.values_order_by` | U |
| `query.values_query` | U |
| `query.where_contradiction` | U |
| `query.where_eq` | U |
| `query.where_pushdown` | U |
| `query.window` | U |
| `query.window_aggregate` | U |
| `query.window_aggregate_filter` | U |
| `query.window_base_extend` | U |
| `query.window_collation` | U |
| `query.window_expr_keys` | U |
| `query.window_frame` | U |
| `query.window_frame_exclude` | U |
| `query.window_frame_range` | U |
| `query.window_grouped` | U |
| `query.window_named` | U |
| `query.window_ntile` | U |
| `query.window_offset` | U |
| `query.window_ranking` | U |
| `query.window_ratio` | U |
| `query.within_group_expr` | U |

#### resource — 19 capabilities

| Capability | State |
|---|---|
| `resource.composite_depth_limit` | U |
| `resource.cost_limit` | U |
| `resource.cost_metering` | U |
| `resource.depth_limit` | U |
| `resource.identifier_length_limit` | U |
| `resource.in_list_limit` | U |
| `resource.json_depth_limit` | U |
| `resource.key_metering` | U |
| `resource.param_index_limit` | U |
| `resource.pure_builtins` | U |
| `resource.query_memory` | U |
| `resource.regex_program_limit` | U |
| `resource.scalar_memory` | U |
| `resource.sql_length_limit` | U |
| `resource.storage_file` | U |
| `resource.storage_memory` | U |
| `resource.temp_budget` | U |
| `resource.view_depth_limit` | U |
| `resource.view_expansion_limit` | U |

#### session — 6 capabilities

| Capability | State |
|---|---|
| `session.allow_ddl` | U |
| `session.allow_temp_ddl` | U |
| `session.lifetime_cost` | U |
| `session.privileges` | U |
| `session.timezone` | U |
| `session.variables` | U |

#### storage — 1 capabilities

| Capability | State |
|---|---|
| `storage.unparsable_definitions` | U |

#### txn — 8 capabilities

| Capability | State |
|---|---|
| `txn.explicit` | U |
| `txn.failed_state` | U |
| `txn.gate_blocking` | U |
| `txn.read_handle` | U |
| `txn.read_only` | U |
| `txn.savepoint` | U |
| `txn.shared` | U |
| `txn.watermark` | U |

#### types — 39 capabilities

| Capability | State |
|---|---|
| `types.array` | U |
| `types.array_collation` | U |
| `types.array_composite` | U |
| `types.array_multidim` | U |
| `types.boolean` | U |
| `types.boolean_storable` | U |
| `types.bytea` | U |
| `types.composite` | U |
| `types.composite_array_field` | U |
| `types.composite_bare_row` | U |
| `types.composite_range_field` | U |
| `types.composite_row_assign` | U |
| `types.date` | U |
| `types.date_clock_literal` | U |
| `types.decimal` | U |
| `types.f32` | U |
| `types.f64` | U |
| `types.float_key` | U |
| `types.i16` | U |
| `types.i32` | U |
| `types.i64` | U |
| `types.interval` | U |
| `types.interval_qualifier` | U |
| `types.json` | U |
| `types.json_casts` | U |
| `types.jsonb` | U |
| `types.jsonb_compare` | U |
| `types.jsonb_literal` | U |
| `types.jsonpath` | U |
| `types.range` | U |
| `types.record_column` | U |
| `types.text` | U |
| `types.timestamp` | U |
| `types.timestamp_precision` | U |
| `types.timestamptz` | U |
| `types.timestamptz_session_input` | U |
| `types.timestamptz_session_render` | U |
| `types.uuid` | U |
| `types.varchar_length` | U |

### Enumeration method

Counts above were computed from `git ls-tree -r --name-only` and `git show` at
`362e0798ac3a8bd04bd2da556044b6ec73bf468a`, parsing the TOML arrays and counting
line-start SQL directives in tracked `.test` files. This is a one-time baseline
inventory; later changes to live specification files do not silently expand the
frozen experiment. Byte fixture families include deliberately malformed/recovery
images: those must produce their specified rejection/fallback result, not be
accepted as ordinary clean databases.
