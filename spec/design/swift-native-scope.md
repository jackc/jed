# Native Swift experiment — baseline scope and completion matrix

Durable preoptimization evidence is preserved in the ignored ext4
[verification archive](../../impl/swift-native/results/verification-preoptimization-20261106/manifest.json):
50 successful report/log/TSV files, 11,386,296 bytes, with original paths and
SHA-256 hashes in its manifest. Originals remain intact; binaries and simulation
images are excluded. This archive remains preoptimization evidence; the final
actor-specific correctness and measured comparison are recorded in
[VERIFICATION.md](../../impl/swift-native/VERIFICATION.md) and
[BENCHMARKS.md](../../impl/swift-native/BENCHMARKS.md).

This is the acceptance inventory for the explicitly requested independent Swift
experiment under `impl/swift-native`. The target is the supported Rust, Go, and
TypeScript surface at **`0b76dd6e9c2675b552eb8af9e820065351ffa4b3`**. It is not the
frozen Ruby experiment's coverage. Inventory date: **2026-11-05**.

The language-neutral contracts and corpus are authoritative. Existing cores are
implementation-pattern evidence, not reference implementations. The inventory
was recorded before implementation with every target marked U.
The completed audit reconciles those rows to the handwritten implementation and
recorded Linux gates. The experiment is frozen at
`fb34b0e8cb3d82287fbd3401e3426f04b3b86c7a`; the semantic baseline above is unchanged.
The final [verification report](../../impl/swift-native/VERIFICATION.md) maps each
acceptance gate to its tested actor, and the [benchmark report](../../impl/swift-native/BENCHMARKS.md)
preserves the original `3db8fc2` release baseline, optimization comparison and
measured limits. Structural-follow-up measurements are complete for separate
22-workload peer, eight-workload paired Swift and three-workload Ruby diagnostics.
The current release run passes all 543 native tests and all 749 frozen SQL files.
Direct diagnostics, exact accounting and affected postoptimization checks pass as
recorded there. The separately corrected view-source regression passes 41 records
and 23 cells in memory and disk. Earlier handoff/client/old-pin failures and their
corrections remain below; the historical canonical default-CI retry passed 18/18.

Swift remains experimental and opt-in, excluded from default tasks and automatic
future feature maintenance. Apple platforms are unverified, production delivery
and promotion are undecided, and no package publication or push is implied.

The companion [host and runner matrix](swift-native-host-matrix.md) specifies the
embedding, process, and test-adapter obligations in greater detail. Progress and
verification evidence must update these matrices without moving the baseline.
When later work changes a row to verified, record the implementation path, exact
opt-in command, result, platform, and revision; preserve failed/remaining lanes.

### Frozen evidence and state legend

Implementation remains on local-only branch `codex/swift-native-experiment`.
The platform exercised is Linux x86-64, Swift 6.4.2. The
[verification report](../../impl/swift-native/VERIFICATION.md) and
[progress record](../../impl/swift-native/PROGRESS.md) retain exact commands,
logs, actor hashes, failures and later corrections. The benchmark report records
measured results separately from correctness. State abbreviations distinguish the
following contracts; none broadens the frozen baseline:

- **O** — implemented; its shared SQL tests were included in the successful
  whole frozen run. The current release run passes all 543 native tests,
  including all 749 files and 21,597 executed SQL records. Earlier passing
  505-/509-/515-test runs retain their own source and actor identities.
  O does not cover combinations absent from those files or claim that
  SQLSTATE-only checks cover structured diagnostic fields. The view-source
  correction has its own postbaseline regression evidence.
- **H** — implemented; the cited host, byte/vector, process or simulation gate
  passed. Each gate has its own actor and scope; it does not substitute for O.
- **M** — baseline mechanical facts are frozen/generated and reproducibility
  checked; semantic implementation is established by the functional rows.
- **D** — direct diagnostic checks pass: all 407 shared cases (61 Detail and
  346 Position/Hint), 10 structured object-field cases and 20 runtime metadata
  probes. The final focused gate also passes four RETURNING-order cases and the
  GIN serialization/deep-view checks. This supplements the SQLSTATE-only corpus.
- **A** — exact-account acceptance passes. The corrected full release comparison
  contains 241,412 measurements with zero failures and `complete_matrix=true`:
  21,477 memory records and 19,438 disk/disk-spill records per core, all 12 runs
  complete. Peaks and committed-storage bytes agree without comparator changes.
  Current actors and evidence are mapped in VERIFICATION.md. Original report:
  `/tmp/jed-swift-query-memory-20261106-1996511-bxwolu/report.json`.
  Earlier debug/release runs each had 376 GiST-load differences; the integrated
  supported-core correction resolves them. Earlier stack/socket failures and
  the stale private Rust manifest incident remain historical evidence.
- **P** — the original `3db8fc2` release baseline and optimization comparison
  are measured for all 143 workloads, four cores and three rounds. The current
  structural follow-up adds completed diagnostic comparisons of 22 peer, eight
  paired Swift and three Ruby workloads, each with three rounds per actor.
  BENCHMARKS.md records commands, provenance, checksums, row counts,
  results and limits. P does not credit later source with earlier measurements,
  imply universal performance parity or select production Swift delivery.

Final affected-path verification is mapped to actors in VERIFICATION.md.
Preoptimization checks below retain their original scope; any explicitly
requested future edits require appropriate revalidation.
V07's additional Swift-client/Rust-pages combination passes both frozen
plaintext/encrypted stream scenarios (`/tmp/jed-swift-native-process-rust-stream.log`).
The prior 496-pair matrix uses Swift pages for Swift/Go/Node clients; Rust clients
self-launch Rust pages. The fresh full release process matrix passes all 496
pairings; saved simulations pass 18 traces per core, 54 transcript comparisons
and 153 checked Swift images (`/tmp/jed-swift-sim-sweep-20261106-2057112-ozakk7/summary.json`).
The final generated release sweep also passes all 176 traces per core,
459 comparisons and 7,049 checked Swift images, with zero failures
(`/tmp/jed-swift-sim-sweep-20261106-2104641-awof5y/summary.json`).
No required Linux omission is reclassified as an exclusion. Apple platforms remain
unverified. States apply to the exact row contract; D records direct diagnostic
evidence; A records completed exact accounting, and P records measured performance
with the workload and host limits in BENCHMARKS.md.

### Preserved preoptimization checkpoints

These counts and actor identities preceded the optimized snapshot. The final
postoptimization gate map is in VERIFICATION.md; no historical actor is relabeled.

| Completed gate | Evidence and scope | Later evidence / scope boundary |
|---|---|---|
| Full ordinary/native | Sixth SQL run: all 749 files, 21,597 records, 39,148 cells, 2,271 cost / 1,154 name / 100 type assertions pass. The earlier release suite passed all 505 native tests, including the corrected GIN boundary and all 749 SQL files | Fourth release refresh passes all 509 native tests and all 749 SQL files; recheck affected paths after any later edits |
| Active-account repairs | Current complete debug and release memory lanes: 21,477 records per core each, zero exact differences; three prior residuals also match all four cores in all three modes | A passes: corrected full release matrix has 241,412 exact measurements, zero failures, complete_matrix=true and all 12 runs complete |
| Encrypted / service SQL | Refreshed 77,612 encrypted records across four cores pass; refreshed 116,628 service records in all six Swift/peer client-owner directions pass | Service actor refresh passes all six directions; new postbaseline files require their own supported-core checks |
| Bytes / interchange | Shared vector consumers; refreshed 112 images and all 2,691 interchange phases pass; 246 coercion captures and 23 estimator vectors pass | Refresh affected paths if subsequent changes warrant it; view-source SQL/diagnostic checks pass |
| Structured diagnostics | Final 42-native-test gate passes all 407 shared cases, 10 object-field cases, 20 runtime probes, four RETURNING cases, GIN boundary and three SQL files / 78 records including unchanged deep-view | Later full release suites include these checks; actor-specific results are in VERIFICATION.md |
| Public integrity | 24 damaged images / 50 exact API reports; 112 goldens with 108 opens and four exact refusals; native/remote API and 61 CLI probes pass | Fresh saved release simulations pass 18/54/153; final generated release sweep passes 176 per core/459 comparisons/7,049 checked Swift images |
| Concurrency / process / simulation | Seven shared schedules; refreshed 496 mixed process cases and 18 saved traces / 54 comparisons / 153 checked images pass; refreshed generated sweep passes all 22 profiles × seeds 1–8, 176 traces per core, 459 comparisons and 7,049 images; Layer 3 stress 20,000 invariants/5,000 writes | Fresh release mixed process passes 496/496; saved simulations pass 18/54/153; Rust-pages supplement passes 2/2. Final generated release sweep passes 176 per core/459 comparisons/7,049 checked Swift images |
| Default/oracle | Verify 17 checks and canonical shared-GiST CI retry 18/18 pass; relevant oracle corpus regenerates byte-identically | Preserve failed 16/18 attempt and default isolation; recheck affected paths after later edits |
| Package / performance | Public examples pass; pinned package and approved native boundaries; reviewed/compiled benchmark harness | The later full measured comparison and its limitations are recorded in BENCHMARKS.md |

The earlier passing CI log is `/tmp/jed-swift-native-default-ci-float-fields.log`.
The shared-GiST attempt at `/tmp/jed-swift-native-default-ci-gist-load.log`
failed 16/18 because of formatting and a cached private Rust manifest path.
The canonical retry passes 18/18 in `/tmp/jed-swift-native-default-ci-gist-canonical-retry.log`.
The corresponding shared Go typed-float correction is **local-only** commit
`05a4e59c`; all 93 supported-core public metadata probes passed. Diagnostic logs
are `/tmp/jed-swift-native-diagnostics-complete-first.log` and
`/tmp/jed-swift-native-runtime-diagnostics-retry.log`; the final follow-up is
`/tmp/jed-swift-native-diagnostics-final.log`. Refreshed process and saved-trace
logs are `/tmp/jed-swift-native-process-mixed-second.log` and
`/tmp/jed-swift-native-sim-regress-final.log`. Exact actor hashes are retained in
[VERIFICATION.md](../../impl/swift-native/VERIFICATION.md). The broad captured
acceptance actor `b7d500a9…` precedes the diagnostic-only RETURNING ordering edit;
its broad runs do not verify that later edit, which now passes the separate
42-test focused gate. Sixth ordinary SQL and the earlier release suite of
505 native tests including all 749 SQL files pass. That release log is
`/tmp/jed-swift-native-release-all749-first.log`; its 44.386 seconds of test time
is not a benchmark measurement. The 507-test release refresh in
`/tmp/jed-swift-native-release-all749-view-second.log` passed all SQL files but
had six handoff/client assertions, now resolved. The third release refresh
retained one old-pin assertion in
`/tmp/jed-swift-native-release-all749-handoff-third.log`. The fourth refresh
passes all 509 native tests and all 749 SQL files in
`/tmp/jed-swift-native-release-all749-eof-fourth.log`. Release
forced-TEMP also passes 28 files,
1,255 records, 571 cells and 23 cost assertions with 26 scratch files. Refreshed
encrypted SQL and 2,691-phase interchange pass. Generated simulations now pass
176 traces per core, 459 transcript comparisons and 7,049 checked Swift images
at `/tmp/jed-swift-sim-sweep-20261106-980401-9e3v3g/summary.json`. Service SQL
refresh passes all six directions and 116,628 frozen records with zero failures:
`/tmp/jed-swift-service-corpus-20261106-1040687-2wheah/report.json`.
The corrected full release V04 matrix passes 241,412 measurements with zero
failures, complete membership and all 12 processes passing. Each core contributes
21,477 memory records and 19,438 records in each disk mode. Report:
`/tmp/jed-swift-query-memory-20261106-1996511-bxwolu/report.json`; log:
`/tmp/jed-swift-native-query-memory-release-gist-third.log`. Captured XCTest:
`944718bbc37e4af7e56f13d13981611503dab04f1af454caaf9f0270fbdea4da`; resources:
`0568c671be49ea6e8e3ff902f41b48a110b612ab428fd105a8bbd67cabb5ba10`.
The earlier debug/release reports each retain 376 GiST-load differences;
Swift's zero pending balance followed memory.md §7, and the supported loaders
were corrected without changing the comparator. The intervening stale-Rust-
manifest attempt failed visibly and remains preserved; the canonical CI retry
and corrected comparison pass. Historical release report:
`/tmp/jed-swift-query-memory-20261106-1135756-exgp9z/report.json`.
The Swift-only GiST reopen regression passes 33 records/10 cells in each of memory
and disk. Its focused process had four service-host assertion failures, now
resolved; `/tmp/jed-swift-native-release-gist-service-first.log` separates those
historical outcomes. Both reviewed postbaseline regressions are committed locally
under shared `ddl/views_table_functions.test` and
`resource/query_memory_gist_reopen.test`; frozen Swift copies are unchanged.
Performance was unmeasured at these checkpoints; the completed measurement record
is BENCHMARKS.md. Complete main logs, not the shared mutable progress trace,
establish counts for these overlapping queued runs.

The historical foundation checkpoint (2026-11-05) covered decimal/float/temporal,
regex, Unicode and collation kernels before SQL integration. Its limited
133-/146-test results and then-missing integrations remain in the progress record;
they are superseded here by the complete ordinary run and the separate gates above.

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
   or authorized for this experiment. The completed snapshot is frozen against this
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
| [Design documents](README.md), including README | 93 Markdown files | Audited; functional rows below |
| [Conformance manifest](../conformance/manifest.toml) | 475 capabilities; 19 profiles | M/O; all 475 capabilities inventoried |
| [Shared suites](../conformance/suites/) | 749 `.test` files in 26 suite directories | O; sixth full SQL refresh passes; view-source gap is outside frozen assertions |
| SQL-format suite files | 742 | O/A; all applicable modes and exact-account matrix pass |
| [Concurrency schedules](../conformance/suites/concurrency/) | 7 | O/H; seven schedules and stress pass |
| SQL statement / query directives | 11,944 / 9,581 | O; authored counts remain frozen |
| `# cost:` occurrences in suite files | 2,330 | O; ordinary run 2,271 cost assertions |
| [Real-process histories](../conformance/process/) | 31 `.process.toml` files | H; refreshed 496 ordered pairings pass |
| [Simulation profiles](../conformance/sim/profiles/) / [regressions](../conformance/sim/regress/) | 22 / 18 | H; refreshed 18 regressions pass; refreshed 176 generated traces per core / 459 comparisons / 7,049 Swift images pass |
| [Scalar descriptors](../types/scalars.toml) | 17 | M/O; shared consumers and ordinary SQL pass |
| [Cast rows](../types/casts.toml) / [range descriptors](../types/ranges.toml) | 70 / 6 | M/O; shared consumers and ordinary SQL pass |
| [Operator descriptors](../functions/catalog.toml) | 401 | M/O; shared consumers and ordinary SQL pass |
| Operator row kinds | 249 functions; 84 comparisons; 45 arithmetic; 13 containment; 5 concat; 3 logical; 2 null tests | M/O; shared consumers and ordinary SQL pass |
| Aggregate / window / set-returning descriptors | 53 / 11 / 18 | M/O; shared consumers and ordinary SQL pass |
| [Argument-coercion capture rows](../functions/coercion_snapshot.toml) | 246 | H; all 246 captures pass |
| [SQLSTATE registry](../errors/registry.toml) | 114 | M/O/D; 407 shared cases plus object/runtime probes pass |
| [Runtime cost units](../cost/schedule.toml) | 30 | M/O; shared costs and host tests pass |
| [Estimator vectors](../cost/estimator_vectors.toml) | 4 arithmetic; 9 predicate; 10 candidate | H; all 23 vectors pass |
| [Golden database files](../fileformat/fixtures/) | 112, including 6 encrypted files | H; 112 files/2,691 interchange phases pass |
| [Integrity image/report pairs](../fileformat/integrity/) | 24 | H; 24 images/50 exact reports pass |
| [Integrity result registry](../integrity/findings.toml) | 32 findings; 5 skips | M/H; 32 findings/5 skips + API/CLI gates |
| [Page-service message catalog](../pageservice/messages.toml) | 22 messages | M/H; codecs and real service gates pass |
| [Page-service fixtures](../pageservice/fixtures.toml) | 27 frames; 2 owner records; 14 malformed cases | H; all frame/owner/malformed vectors pass |
| [Encoding vector files](../encoding/) | 15 TOML files | H; shared byte/vector consumers pass |
| [Regex vectors](../regex/) | 33 programs; 41 matches; 19 SIMILAR cases | H; all 93 shared program/match/SIMILAR cases pass |
| [LZ4 vectors](../fileformat/lz4_vectors.toml) | 11 | H; all 11 vectors pass |
| [Crypto vectors](../fileformat/encryption_vectors.toml) | 3 XAES; 1 subkey input set (2 outputs); 1 host-KEK; 1 key-block; 1 rewrap; 1 Argon2id | H; approved backend and exact vector gates pass |
| [Collation vectors](../collation/vectors/) | 1 bundle; 2 compiler; 46 sort-key cases | H; compiler/runtime vectors and canonical artifacts pass |
| [Timezone vectors](../tz/vectors/) | 1 bundle; 16 TZif cases | H; bundle and all 16 TZif cases pass |
| [PostgreSQL keyword table](../grammar/pg_keywords.toml) | 164 keywords | M/O; shared consumers and ordinary SQL pass |
| [Datetime token tables](../types/datetime_units.toml) | 61 delta; 72 date; 13 interval qualifiers | M/O; shared consumers and ordinary SQL pass |
| [Formatting vocabulary](../types/formatting.toml) | 112 datetime; 36 numeric; 7 suffixes; 15 word-list groups | M/O; shared consumers and ordinary SQL pass |


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
| L01 | UTF-8 lexer, quoted/unquoted and Unicode-escaped names, keyword categories, nested comments, literal forms, source locations, identifier/input/parameter limits | [grammar](grammar.md), [EBNF](../grammar/grammar.ebnf), [keywords](../grammar/pg_keywords.toml) | [query](../conformance/suites/query/), [resource](../conformance/suites/resource/) | O; D; [Lexer.swift](../../impl/swift-native/Sources/Jed/Lexer.swift), [LexerBoundaryTests.swift](../../impl/swift-native/Tests/JedTests/LexerBoundaryTests.swift) |
| L02 | Every statement production: query, DDL/DML, transactions, ANALYZE, EXPLAIN; syntax/error precedence and parser nesting gates | [grammar](grammar.md), [errors](error-fields.md) | All SQL suites; [error positions](../errors/position_cases.txt) | O; D; [Parser.swift](../../impl/swift-native/Sources/Jed/Parser.swift), [ParserStatements.swift](../../impl/swift-native/Sources/Jed/ParserStatements.swift), [ParserBoundaryTests.swift](../../impl/swift-native/Tests/JedTests/ParserBoundaryTests.swift) |
| L03 | Name/scope/alias resolution, correlated levels, unknown literal adaptation, strict static inference, output names and result types | [types](types.md), [grammar](grammar.md) | [types](../conformance/suites/types/), [subquery](../conformance/suites/subquery/), [params](../conformance/suites/params/) | O; D; [ExpressionResolver.swift](../../impl/swift-native/Sources/Jed/ExpressionResolver.swift), [QueryPlan.swift](../../impl/swift-native/Sources/Jed/QueryPlan.swift), [ResolutionDiagnostics.swift](../../impl/swift-native/Sources/Jed/ResolutionDiagnostics.swift) |
| L04 | Explicit/assignment/implicit casts, numeric promotion, overload selection, named/optional/VARIADIC arguments and coercion capture | [casts](../types/casts.toml), [comparison data](../types/compare.toml), [arg coercion](arg-coercion.md), [functions](functions.md) | [cast](../conformance/suites/cast/), [argument coercion](../conformance/suites/expr/arg_coercion.test), capture snapshot | O/H; D; [TypeRules.swift](../../impl/swift-native/Sources/Jed/TypeRules.swift), [FunctionResolver.swift](../../impl/swift-native/Sources/Jed/FunctionResolver.swift), [CoercionCaptureTests.swift](../../impl/swift-native/Tests/JedTests/CoercionCaptureTests.swift) |
| L05 | Structured SQLSTATE, message/detail/hint/position fields, SQL Unicode position accounting and deterministic failure visitation | [error fields](error-fields.md), [registry](../errors/registry.toml) | [detail cases](../errors/detail_cases.txt), [position cases](../errors/position_cases.txt), error suites | O; D; [Error.swift](../../impl/swift-native/Sources/Jed/Error.swift), [ErrorFieldCorpusTests.swift](../../impl/swift-native/Tests/JedTests/ErrorFieldCorpusTests.swift) |
| T01 | Signed i16/i32/i64 and aliases, checked arithmetic, boolean and three-valued NULL rules | [types](types.md), [scalars](../types/scalars.toml), [compare](../types/compare.toml) | [types](../conformance/suites/types/), [null](../conformance/suites/null/), [compare](../conformance/suites/compare/) | O/H; [ScalarFunctions.swift](../../impl/swift-native/Sources/Jed/ScalarFunctions.swift), [ValueComparison.swift](../../impl/swift-native/Sources/Jed/ValueComparison.swift), [ComparisonBoundaryTests.swift](../../impl/swift-native/Tests/JedTests/ComparisonBoundaryTests.swift) |
| T02 | Exact finite decimals, scale preservation, capped and uncapped arithmetic, PG division scale/rounding, numeric functions and transcendental kernels | [decimal](decimal.md), [functions](../functions/catalog.toml) | [decimal types](../conformance/suites/types/decimal.test), [decimal arithmetic](../conformance/suites/expr/decimal_arithmetic.test), numeric/cost suites | O/H; [Decimal.swift](../../impl/swift-native/Sources/Jed/Decimal.swift), [DecimalTranscendental.swift](../../impl/swift-native/Sources/Jed/DecimalTranscendental.swift), [DecimalTests.swift](../../impl/swift-native/Tests/JedTests/DecimalTests.swift) |
| T03 | f32/f64, total NaN/infinity order, underflow/overflow and domain behavior, exact text render, allowed tolerance only under the ledger | [float](float.md), [determinism](determinism.md) | [float types](../conformance/suites/types/float.test), float expression suites, [float vectors](../encoding/float.toml) | O/H; [FloatText.swift](../../impl/swift-native/Sources/Jed/FloatText.swift), [FloatFunctions.swift](../../impl/swift-native/Sources/Jed/FloatFunctions.swift), [FloatBoundaryTests.swift](../../impl/swift-native/Tests/JedTests/FloatBoundaryTests.swift) |
| T04 | UTF-8 text/varchar, raw bytea, UUID, scalar codecs and text I/O; code-point string operations | [types](types.md), [string functions](string-functions.md), [encoding](encoding.md) | text/bytea/uuid type suites; string/bytea/UUID expression suites | O/H; [Text.swift](../../impl/swift-native/Sources/Jed/Text.swift), [ByteaFunctions.swift](../../impl/swift-native/Sources/Jed/ByteaFunctions.swift), [ValueCodecTests.swift](../../impl/swift-native/Tests/JedTests/ValueCodecTests.swift) |
| T05 | Date/timestamp/timestamptz infinities, precision typmods, interval months/days/micros, parsing/rendering, exact temporal arithmetic | [date](date.md), [timestamp](timestamp.md), [interval](interval.md) | temporal type/expression suites; date/timestamp/interval encoding vectors | O/H; [Temporal.swift](../../impl/swift-native/Sources/Jed/Temporal.swift), [TemporalFunctions.swift](../../impl/swift-native/Sources/Jed/TemporalFunctions.swift), [TemporalBoundaryTests.swift](../../impl/swift-native/Tests/JedTests/TemporalBoundaryTests.swift) |
| T06 | Environment-loaded JTZ/TZif, aliases, POSIX tails, transitions/gaps/folds, fixed zones, session-zone input/output/arithmetic, persisted timezone dependency pins | [timezones](timezones.md), [timezone data](../tz/README.md), [index dependencies](index-dependencies.md) | timezone expressions/types; [TZ vectors](../tz/vectors/); timezone index golden | O/H; [TimeZone.swift](../../impl/swift-native/Sources/Jed/TimeZone.swift), [StoredExpressionDependencies.swift](../../impl/swift-native/Sources/Jed/StoredExpressionDependencies.swift), [StatementServicesTests.swift](../../impl/swift-native/Tests/JedTests/StatementServicesTests.swift) |
| T07 | Jed UCA compiler/executor, loaded Unicode data, embedded/reference collation artifacts, sort keys and skew checks, collated text/arrays/indexes | [collation](collation.md), [collation data](../collation/README.md) | [collation suite](../conformance/suites/collation/), collation vectors and golden files | O/H; [Collation.swift](../../impl/swift-native/Sources/Jed/Collation.swift), [CollationMigration.swift](../../impl/swift-native/Sources/Jed/CollationMigration.swift), [UnicodeCompilerTests.swift](../../impl/swift-native/Tests/JedTests/UnicodeCompilerTests.swift) |
| T08 | Nominal named composite types, recursive fields, ROW/inline record query values, projection/assignment, 3VL comparison, text I/O, dependency/drop rules | [composite](composite.md), [extensibility](extensibility.md) | composite type/DDL/DML suites; composite goldens/vectors | O/H; [Types.swift](../../impl/swift-native/Sources/Jed/Types.swift), [ContainerCasts.swift](../../impl/swift-native/Sources/Jed/ContainerCasts.swift), [ContainerBoundaryTests.swift](../../impl/swift-native/Tests/JedTests/ContainerBoundaryTests.swift) |
| T09 | Structural arrays with value-level shape/lower bounds, multidimensional values, subscripting/slices, recursive codecs/comparison and functions | [arrays](array.md), [array functions](array-functions.md) | array types/expressions/aggregates/unnest; array goldens | O/H; [ArrayFunctions.swift](../../impl/swift-native/Sources/Jed/ArrayFunctions.swift), [ContainerText.swift](../../impl/swift-native/Sources/Jed/ContainerText.swift), [ArrayBoundaryTests.swift](../../impl/swift-native/Tests/JedTests/ArrayBoundaryTests.swift) |
| T10 | Six structural range types, discrete canonicalization, bounds/empty/infinite semantics, constructors/operators/functions and range keys | [ranges](ranges.md), [range functions](range-functions.md), [range data](../types/ranges.toml) | range type/expression suites; range vectors/goldens | O/H; [RangeFunctions.swift](../../impl/swift-native/Sources/Jed/RangeFunctions.swift), [ValueComparison.swift](../../impl/swift-native/Sources/Jed/ValueComparison.swift), [ContainerBoundaryTests.swift](../../impl/swift-native/Tests/JedTests/ContainerBoundaryTests.swift) |
| T11 | Recursive order-preserving keys, nullable and ordered slots, prefix-free composition, scalar-array/range/composite key eligibility | [encoding](encoding.md), [format](../fileformat/format.md) | All [encoding vectors](../encoding/); array/composite/range/index key suites and goldens | O/H; [KeyEncoding.swift](../../impl/swift-native/Sources/Jed/KeyEncoding.swift), [IndexKeyBoundary.swift](../../impl/swift-native/Sources/Jed/IndexKeyBoundary.swift), [ValueCodecTests.swift](../../impl/swift-native/Tests/JedTests/ValueCodecTests.swift) |
| F01 | Every one of the 401 operator rows with its family resolution, null contract, volatility/dependency, trapping, return type and cost | [catalog](../functions/catalog.toml), [functions](functions.md) | [expr](../conformance/suites/expr/), [cast](../conformance/suites/cast/), [json](../conformance/suites/json/) | O/H; D; [FunctionResolver.swift](../../impl/swift-native/Sources/Jed/FunctionResolver.swift), [ScalarFunctions.swift](../../impl/swift-native/Sources/Jed/ScalarFunctions.swift), [FunctionBoundaryTests.swift](../../impl/swift-native/Tests/JedTests/FunctionBoundaryTests.swift) |
| F02 | Short-circuit AND/OR, CASE/COALESCE/NULLIF/GREATEST/LEAST, IN/BETWEEN/quantified comparison, exact filter conjunct ordering | [grammar](grammar.md), [planner](planner.md), [cost](cost.md) | expression and query rewrite suites | O; A/D; [Expression.swift](../../impl/swift-native/Sources/Jed/Expression.swift), [ExpressionQuantified.swift](../../impl/swift-native/Sources/Jed/ExpressionQuantified.swift), [PredicateOrdering.swift](../../impl/swift-native/Sources/Jed/PredicateOrdering.swift) |
| F03 | Text building/escaping/digest functions, LIKE/ILIKE, formatting vocabulary and exact datetime/numeric formatting | [string functions](string-functions.md), [formatting](formatting.md) | string/hash/formatting expression suites; scalar-output resource suites | O/H; [TextFunctions.swift](../../impl/swift-native/Sources/Jed/TextFunctions.swift), [NumericFormat.swift](../../impl/swift-native/Sources/Jed/NumericFormat.swift), [TemporalFormat.swift](../../impl/swift-native/Sources/Jed/TemporalFormat.swift) |
| F04 | Handwritten bounded regex compiler/Pike VM, regexp functions/SRFs, SIMILAR/substring syntax, compile/step costs | [regex](regex.md) | [regex vectors](../regex/), regexp expression/SRF/resource suites | O/H; [Regex.swift](../../impl/swift-native/Sources/Jed/Regex.swift), [Similar.swift](../../impl/swift-native/Sources/Jed/Similar.swift), [RegexBoundaryTests.swift](../../impl/swift-native/Tests/JedTests/RegexBoundaryTests.swift) |
| F05 | Verbatim json, canonical exact-numeric jsonb, codecs, nesting gates, comparison/containment, builders/accessors/mutation/aggregation | [json](json.md), [JSON SQL functions](json-sql-functions.md) | [json suite](../conformance/suites/json/), json/jsonb goldens | O/H; [JSON.swift](../../impl/swift-native/Sources/Jed/JSON.swift), [JSONFunctions.swift](../../impl/swift-native/Sources/Jed/JSONFunctions.swift), [JSONBoundaryTests.swift](../../impl/swift-native/Tests/JedTests/JSONBoundaryTests.swift) |
| F06 | First-class query jsonpath compiler/render/evaluator, strict/lax, methods/predicates/variables/recursive descent, SQL/JSON behaviors and PASSING | [jsonpath](jsonpath.md), [JSON SQL functions](json-sql-functions.md) | jsonpath/JSON behavior/float-source suites; JSON resource suites | O/H; [JSONPath.swift](../../impl/swift-native/Sources/Jed/JSONPath.swift), [JSONPathFunctions.swift](../../impl/swift-native/Sources/Jed/JSONPathFunctions.swift), [JSONBoundaryTests.swift](../../impl/swift-native/Tests/JedTests/JSONBoundaryTests.swift) |
| F07 | JSON record/populate functions, column-definition lists, JSON_TABLE nested paths and standard default behaviors | [JSON table](json-table.md), [composite](composite.md) | json_record/json_populate/json_table suites | O; A; [JSONRecordPlan.swift](../../impl/swift-native/Sources/Jed/JSONRecordPlan.swift), [JSONTablePlan.swift](../../impl/swift-native/Sources/Jed/JSONTablePlan.swift) |
| F08 | Every one of the 18 SRF descriptors: generation/unnest/regexp/JSON rows, lateral binding, names/ordinality where implemented, source costs | [functions](functions.md), [catalog](../functions/catalog.toml), [grammar](grammar.md) | generate_series/unnest/SRF suites; JSON and joins | O; A; [TableFunctionPlan.swift](../../impl/swift-native/Sources/Jed/TableFunctionPlan.swift), [ExpressionRegexFunctions.swift](../../impl/swift-native/Sources/Jed/ExpressionRegexFunctions.swift) |
| Q01 | SELECT/VALUES, FROM-less row, WHERE/projection/order/limit/offset, aliases, deterministic ties and sort/null behavior | [grammar](grammar.md), [streaming](streaming.md) | [query](../conformance/suites/query/), [setops](../conformance/suites/setops/) | O; A/D; [QueryPlan.swift](../../impl/swift-native/Sources/Jed/QueryPlan.swift), [OrderedSelectExecution.swift](../../impl/swift-native/Sources/Jed/OrderedSelectExecution.swift) |
| Q02 | Inner/cross/outer/lateral joins, nesting/scopes, ON/USING/NATURAL behavior where accepted, nested-loop/hash/INL physical execution | [grammar](grammar.md), [planner](planner.md), [estimator](estimator.md) | [joins](../conformance/suites/joins/), cost-plan and INL query suites | O; A/D; [JoinPlan.swift](../../impl/swift-native/Sources/Jed/JoinPlan.swift), [NWayJoinPlan.swift](../../impl/swift-native/Sources/Jed/NWayJoinPlan.swift), [BlockingJoinBoundaryTests.swift](../../impl/swift-native/Tests/JedTests/BlockingJoinBoundaryTests.swift) |
| Q03 | Scalar/row/EXISTS/IN/ANY/ALL subqueries, correlations including grouped outer levels, derived tables and values bodies | [grammar](grammar.md), [planner](planner.md) | [subquery](../conformance/suites/subquery/) | O; A/D; [SubqueryPlan.swift](../../impl/swift-native/Sources/Jed/SubqueryPlan.swift), [SubqueryExecution.swift](../../impl/swift-native/Sources/Jed/SubqueryExecution.swift) |
| Q04 | UNION/INTERSECT/EXCEPT with ALL/distinct, precedence, common types, ordering and materialization | [grammar](grammar.md), [memory](memory.md) | [setops](../conformance/suites/setops/) | O; A/D; [CompoundQueryPlan.swift](../../impl/swift-native/Sources/Jed/CompoundQueryPlan.swift), [CompoundQueryExecution.swift](../../impl/swift-native/Sources/Jed/CompoundQueryExecution.swift) |
| Q05 | All 53 aggregates, null handling, exact widening, DISTINCT/FILTER/order, statistical/regression/ordered/hypothetical sets and collation | [aggregates](aggregates.md), [catalog](../functions/catalog.toml) | [aggregates](../conformance/suites/aggregates/) | O; A/D; [Aggregate.swift](../../impl/swift-native/Sources/Jed/Aggregate.swift), [AggregateOrderedSet.swift](../../impl/swift-native/Sources/Jed/AggregateOrderedSet.swift), [BlockingAggregate.swift](../../impl/swift-native/Sources/Jed/BlockingAggregate.swift) |
| Q06 | GROUP BY expressions and functional dependency, grouping sets/ROLLUP/CUBE/GROUPING, HAVING, DISTINCT and DISTINCT ON | [aggregates](aggregates.md), [grammar](grammar.md) | grouping aggregate suites, query distinct suites | O; A/D; [GroupingExpansion.swift](../../impl/swift-native/Sources/Jed/GroupingExpansion.swift), [BlockingDistinctExecution.swift](../../impl/swift-native/Sources/Jed/BlockingDistinctExecution.swift), [BlockingDistinctOnExecution.swift](../../impl/swift-native/Sources/Jed/BlockingDistinctOnExecution.swift) |
| Q07 | All 11 window descriptors and aggregate windows; named windows, partitions/peers, ROWS/RANGE/GROUPS bounds, exclusions/sliding frames and shared work | [window](window.md), [catalog](../functions/catalog.toml) | [window](../conformance/suites/window/), grouping_sets_window | O; A/D; [WindowQueryPlan.swift](../../impl/swift-native/Sources/Jed/WindowQueryPlan.swift), [WindowFrame.swift](../../impl/swift-native/Sources/Jed/WindowFrame.swift), [BlockingWindowExecution.swift](../../impl/swift-native/Sources/Jed/BlockingWindowExecution.swift) |
| Q08 | CTE inlining/materialization, recursion/work tables and dedup, writable CTE snapshot/effect sequencing | [CTE](cte.md), [recursive CTE](recursive-cte.md), [writable CTE](writable-cte.md) | [cte](../conformance/suites/cte/), CTAS/DML/transaction suites | O; A/D; [CTEExecution.swift](../../impl/swift-native/Sources/Jed/CTEExecution.swift), [RecursiveCTEExecution.swift](../../impl/swift-native/Sources/Jed/RecursiveCTEExecution.swift), [WritableCTEExecution.swift](../../impl/swift-native/Sources/Jed/WritableCTEExecution.swift) |
| P01 | Safe conjunct ordering/pushdown and contradictions, error-preservation proofs, derived/group/window/set-operation/CTE rewrites | [planner](planner.md), [casts](../types/casts.toml) | contradiction/pushdown/where-rewrite/filter-order suites | O; A/D; [PredicateContradiction.swift](../../impl/swift-native/Sources/Jed/PredicateContradiction.swift), [BodyPushdown.swift](../../impl/swift-native/Sources/Jed/BodyPushdown.swift), [PushdownSafety.swift](../../impl/swift-native/Sources/Jed/PushdownSafety.swift) |
| P02 | Complete access candidate inventory, bound PK/B-tree/GIN/GiST, prefix/range/point-set/interval-set/partial-index rules, physical order | [planner](planner.md), [indexes](indexes.md), [GIN](gin.md), [GiST](gist.md) | access_candidate_inventory and all index/GIN/GiST query suites | O/H; A; [AccessInventory.swift](../../impl/swift-native/Sources/Jed/AccessInventory.swift), [IntervalPlan.swift](../../impl/swift-native/Sources/Jed/IntervalPlan.swift), [GistScanPlan.swift](../../impl/swift-native/Sources/Jed/GistScanPlan.swift) |
| P03 | Exact rational/saturating estimator, complete tie order, single-relation pipeline, N-way Pareto search through 8 relations and greedy larger islands, hard fences | [estimator](estimator.md), [estimator data](../cost/estimator.toml) | estimator vectors, cost_plan_access/cost_plan_join suites | O/H; [PlanEstimate.swift](../../impl/swift-native/Sources/Jed/PlanEstimate.swift), [NWayJoinPlan.swift](../../impl/swift-native/Sources/Jed/NWayJoinPlan.swift), [EstimatorVectorTests.swift](../../impl/swift-native/Tests/JedTests/EstimatorVectorTests.swift) |
| P04 | Custom parameter plans; value-blind classification; cache signatures over domain/catalog/statistics; bound values at all specified reads | [estimator](estimator.md), [API](api.md) | [params](../conformance/suites/params/); host prepared-cache tests | O/H; [PreparedSelectCache.swift](../../impl/swift-native/Sources/Jed/PreparedSelectCache.swift), [PlanningProbe.swift](../../impl/swift-native/Sources/Jed/PlanningProbe.swift), [PlanningProbeBoundaryTests.swift](../../impl/swift-native/Tests/JedTests/PlanningProbeBoundaryTests.swift) |
| P05 | Transactional ANALYZE, deterministic samples/KMV/MCV/histograms/GIN terms, retained staleness and estimator consumption | [statistics](statistics.md), [estimator data](../cost/estimator.toml) | statistics query/introspection/transaction suites and two statistics goldens | O/H; [AnalyzeExecution.swift](../../impl/swift-native/Sources/Jed/AnalyzeExecution.swift), [GinEstimate.swift](../../impl/swift-native/Sources/Jed/GinEstimate.swift), [PostbaselineSQLTests.swift](../../impl/swift-native/Tests/JedTests/PostbaselineSQLTests.swift) |
| P06 | EXPLAIN/ANALYZE/VERBOSE/COSTS/LANE, selected shape and canonical expressions, integer estimates and inclusive actual attribution across query/DML/CTE | [explain](explain.md) | explain query/DML/CTAS/MERGE suites | O; A/D; [ExplainTree.swift](../../impl/swift-native/Sources/Jed/ExplainTree.swift), [ExplainMutation.swift](../../impl/swift-native/Sources/Jed/ExplainMutation.swift), [ExplainWith.swift](../../impl/swift-native/Sources/Jed/ExplainWith.swift) |
| D01 | Table/type/index create/drop/rename, namespace and auto-name byte rules, IF EXISTS variants, ALTER operations and dependencies | [grammar](grammar.md), [alter](alter.md), [composite](composite.md) | [ddl](../conformance/suites/ddl/), catalog goldens | O; D; [CatalogDDL.swift](../../impl/swift-native/Sources/Jed/CatalogDDL.swift), [AlterTableDDL.swift](../../impl/swift-native/Sources/Jed/AlterTableDDL.swift), [AlterColumnType.swift](../../impl/swift-native/Sources/Jed/AlterColumnType.swift) |
| D02 | NOT NULL/default expressions/CHECK/PK/UNIQUE, v50 constraint ownership, end-state all-or-nothing validation and dependency naming | [constraints](constraints.md), [format](../fileformat/format.md) | constraint DDL/DML suites, unique/check/default goldens | O/H; A/D; [MutationExecutor.swift](../../impl/swift-native/Sources/Jed/MutationExecutor.swift), [AlterConstraintValidation.swift](../../impl/swift-native/Sources/Jed/AlterConstraintValidation.swift), [IntegrityConstraints.swift](../../impl/swift-native/Sources/Jed/IntegrityConstraints.swift) |
| D03 | Foreign keys, compatible key families, MATCH SIMPLE/FULL, every action, child probes, rekey/cascade/end-state validation | [constraints](constraints.md) | foreign_key suites; fk/malformed_fk_type goldens | O/H; A/D; [ForeignKeyActions.swift](../../impl/swift-native/Sources/Jed/ForeignKeyActions.swift), [ForeignKeyValidation.swift](../../impl/swift-native/Sources/Jed/ForeignKeyValidation.swift), [ForeignKeyBoundaryTests.swift](../../impl/swift-native/Tests/JedTests/ForeignKeyBoundaryTests.swift) |
| D04 | Ordered secondary B-trees, expression/partial keys, dependency admission/pins, partial implication, REINDEX and persisted unparseable definitions | [indexes](indexes.md), [index dependencies](index-dependencies.md), [unparsable definitions](unparsable-definitions.md) | expression/partial/dependency/reindex/index-order suites and goldens | O/H; [OrderedIndexPlan.swift](../../impl/swift-native/Sources/Jed/OrderedIndexPlan.swift), [IndexDependencies.swift](../../impl/swift-native/Sources/Jed/IndexDependencies.swift), [ReindexExecution.swift](../../impl/swift-native/Sources/Jed/ReindexExecution.swift) |
| D05 | GIN arrays/jsonb, every admitted scalar element, multi-column tags, term encoding/posting runs, scans/rechecks/INL/statistics | [GIN](gin.md), [term vectors](../encoding/gin_jsonb_terms.toml), [runs](../encoding/gin_posting_runs.toml) | GIN DDL/query suites and GIN goldens | O/H; [GinIndex.swift](../../impl/swift-native/Sources/Jed/GinIndex.swift), [GinPostingRun.swift](../../impl/swift-native/Sources/Jed/GinPostingRun.swift), [GinBoundaryTests.swift](../../impl/swift-native/Tests/JedTests/GinBoundaryTests.swift) |
| D06 | GiST ranges/fixed scalars, canonical R-tree, exclusion constraints including btree/partial, probe/maintenance/REINDEX | [GiST](gist.md), [constraints](constraints.md) | GiST/exclusion query and DDL suites; GiST/exclusion goldens | O/H; [GistTree.swift](../../impl/swift-native/Sources/Jed/GistTree.swift), [ExclusionValidation.swift](../../impl/swift-native/Sources/Jed/ExclusionValidation.swift), [GistBoundaryTests.swift](../../impl/swift-native/Tests/JedTests/GistBoundaryTests.swift) |
| D07 | INSERT expressions/multi-row/SELECT/defaults, UPDATE/DELETE two-phase edits/rekeys, RETURNING, privileges and resource rollback | [grammar](grammar.md), [constraints](constraints.md), [transactions](transactions.md) | [dml](../conformance/suites/dml/), [mutation](../conformance/suites/mutation/) | O; A/D; [MutationExecutor.swift](../../impl/swift-native/Sources/Jed/MutationExecutor.swift), [MutationAccess.swift](../../impl/swift-native/Sources/Jed/MutationAccess.swift), [MutationResult.swift](../../impl/swift-native/Sources/Jed/MutationResult.swift) |
| D08 | ON CONFLICT arbitration including partial/exclusion cases, DO NOTHING/UPDATE, excluded row and updates/rekeys | [upsert](upsert.md) | insert_on_conflict family | O; A/D; [ConflictArbiter.swift](../../impl/swift-native/Sources/Jed/ConflictArbiter.swift), [MutationConflict.swift](../../impl/swift-native/Sources/Jed/MutationConflict.swift) |
| D09 | MERGE source joins, matched/not-matched actions, cardinality/errors, action order, view target, parameters, EXPLAIN/cost | [merge](merge.md) | MERGE DML suite family | O; A/D; [MergeExecutor.swift](../../impl/swift-native/Sources/Jed/MergeExecutor.swift), [MergePreparation.swift](../../impl/swift-native/Sources/Jed/MergePreparation.swift) |
| D10 | Sequences/serial/identity, ownership and ALTER, transactional counters/currval/default application and per-row mutation cost | [sequences](sequences.md) | sequence/serial/identity suites and goldens | O/H; D; [SequenceExecution.swift](../../impl/swift-native/Sources/Jed/SequenceExecution.swift), [SequenceDDL.swift](../../impl/swift-native/Sources/Jed/SequenceDDL.swift), [ScopedSequenceBoundaryTests.swift](../../impl/swift-native/Tests/JedTests/ScopedSequenceBoundaryTests.swift) |
| D11 | CTAS/SELECT INTO, declared/inferred column metadata, temporary and writable-CTE forms, constraints and rollback | [CTAS](create-table-as.md) | create_table_as/select_into suites | O/H; A/D; [CreateTableAs.swift](../../impl/swift-native/Sources/Jed/CreateTableAs.swift), [CreateTableAsBoundaryTests.swift](../../impl/swift-native/Tests/JedTests/CreateTableAsBoundaryTests.swift) |
| D12 | Views: canonical text, frozen stars, replace/alter/defaults/options/dependencies/rename/cascade, temporary/recursive, updatable and check option | [views](views.md), [unparsable definitions](unparsable-definitions.md) | view DDL/DML/query/introspection suites; view goldens | O/H/D for exercised corpus; SRF/JSON_TABLE dependency correction passes 41 records/23 cells in both memory and disk plus host capability checks; fourth release refresh passes all 509 native tests and all 749 SQL files; [ViewDDL.swift](../../impl/swift-native/Sources/Jed/ViewDDL.swift), [UpdatableView.swift](../../impl/swift-native/Sources/Jed/UpdatableView.swift), [ViewBoundaryTests.swift](../../impl/swift-native/Tests/JedTests/ViewBoundaryTests.swift) |
| D13 | Session-local temporary tables/views/types/indexes/owned sequences, ON COMMIT semantics, temp-domain reclamation and file spill | [temp tables](temp-tables.md) | temp DDL/attachment/session/resource suites | O/H; A; [TemporaryDomain.swift](../../impl/swift-native/Sources/Jed/TemporaryDomain.swift), [TemporarySessionBoundaryTests.swift](../../impl/swift-native/Tests/JedTests/TemporarySessionBoundaryTests.swift), [TempTypeBoundaryTests.swift](../../impl/swift-native/Tests/JedTests/TempTypeBoundaryTests.swift) |
| D14 | SQL introspection tables, names/types/options/dependencies/statistics and session/attachment visibility | [introspection](introspection.md) | [introspection](../conformance/suites/introspection/) | O; [SystemRelationPlan.swift](../../impl/swift-native/Sources/Jed/SystemRelationPlan.swift), [CatalogDomain.swift](../../impl/swift-native/Sources/Jed/CatalogDomain.swift) |
| X01 | Single committed snapshot plus one writer pending set, read/write handles, gates and watermark; statement rollback, SQL transaction failure/savepoints/read-only | [transactions](transactions.md), [concurrency](concurrency-testing.md) | [transactions](../conformance/suites/transactions/), [concurrency](../conformance/suites/concurrency/) | O/H; [DatabaseCore.swift](../../impl/swift-native/Sources/Jed/DatabaseCore.swift), [SessionCursorTests.swift](../../impl/swift-native/Tests/JedTests/SessionCursorTests.swift), [ConcurrencyCorpusBoundaryTests.swift](../../impl/swift-native/Tests/JedTests/ConcurrencyCorpusBoundaryTests.swift) |
| X02 | Sessions, capabilities including column read/function execution/DDL, variables, per-statement/lifetime budgets and frozen settings | [session](session.md), [environment](environment.md) | [session](../conformance/suites/session/), host matrix | O/H; A; [SessionSettings.swift](../../impl/swift-native/Sources/Jed/SessionSettings.swift), [StatementPrivileges.swift](../../impl/swift-native/Sources/Jed/StatementPrivileges.swift), [SessionSettingsAccessTests.swift](../../impl/swift-native/Tests/JedTests/SessionSettingsAccessTests.swift) |
| X03 | Prepared/bound/batched execution, genuine public pull cursor, snapshot/cost/account ownership, early close/errors, cancellation and collectors | [API](api.md), [streaming](streaming.md) | [params](../conformance/suites/params/), bounded streaming suites, host matrix | O/H; A; [Rows.swift](../../impl/swift-native/Sources/Jed/Rows.swift), [PreparedStatement.swift](../../impl/swift-native/Sources/Jed/PreparedStatement.swift), [HostBatchScanBoundaryTests.swift](../../impl/swift-native/Tests/JedTests/HostBatchScanBoundaryTests.swift) |
| X04 | Immutable environment, injected SQL entropy/clock, Unicode/TZ data, scalar batched host functions, overload/cost/dependency/reopen contracts | [environment](environment.md), [entropy](entropy.md), [extensibility](extensibility.md) | clock/UUID/host-function suites and host matrix | O/H; [Environment.swift](../../impl/swift-native/Sources/Jed/Environment.swift), [HostBatch.swift](../../impl/swift-native/Sources/Jed/HostBatch.swift), [StatementServicesTests.swift](../../impl/swift-native/Tests/JedTests/StatementServicesTests.swift) |
| R01 | All 30 deterministic work units and guard points, max/lifetime ceilings and error precedence, frozen/internal meters | [cost](cost.md), [schedule](../cost/schedule.toml) | Every cost assertion; resource/cost/lifetime suites | O/H; [Resources.swift](../../impl/swift-native/Sources/Jed/Resources.swift), [ResourceTests.swift](../../impl/swift-native/Tests/JedTests/ResourceTests.swift), [FrozenResourceBoundaryTests.swift](../../impl/swift-native/Tests/JedTests/FrozenResourceBoundaryTests.swift) |
| R02 | Parser/expression/JSON/composite/view/regex/identifier/input/parameter/list limits and fail-closed SQL surface | [cost](cost.md), subsystem nesting rules | [resource](../conformance/suites/resource/) | O/H; [Parser.swift](../../impl/swift-native/Sources/Jed/Parser.swift), [JSON.swift](../../impl/swift-native/Sources/Jed/JSON.swift), [ParserBoundaryTests.swift](../../impl/swift-native/Tests/JedTests/ParserBoundaryTests.swift) |
| R03 | Scalar amplifier allowance and growth-kernel cost, allocation preflight, exact decimal scratch work | [memory](memory.md), [cost](cost.md) | scalar_memory/output, dos_amplification and decimal resource cases | O/H; [Resources.swift](../../impl/swift-native/Sources/Jed/Resources.swift), [DecimalTranscendental.swift](../../impl/swift-native/Sources/Jed/DecimalTranscendental.swift), [ExpressionBoundaryTests.swift](../../impl/swift-native/Tests/JedTests/ExpressionBoundaryTests.swift) |
| R04 | Q1 row/collector buffers, Q2 operator state, Q3 pending writes, live/released/peak logical accounting and all nested account sharing | [memory](memory.md), [sizes](../cost/memory_sizes.toml) | query_memory family plus peak/account runner comparisons | O/H; A; [Resources.swift](../../impl/swift-native/Sources/Jed/Resources.swift), [MemorySizes.swift](../../impl/swift-native/Sources/Jed/MemorySizes.swift), [SQLCorpusAccountingTests.swift](../../impl/swift-native/Tests/JedTests/SQLCorpusAccountingTests.swift) |
| R05 | Q4 memory committed pages/file live pages, temp storage budget, cache eviction, per-domain limits and unchanged-commit behavior | [memory](memory.md), [transactions](transactions.md) | storage_memory/storage_file/temp_budget, storage TSV and process histories | O/H; A; [StorageDomain.swift](../../impl/swift-native/Sources/Jed/StorageDomain.swift), [DomainCommitGroup.swift](../../impl/swift-native/Sources/Jed/DomainCommitGroup.swift), [StorageDomainBoundaryTests.swift](../../impl/swift-native/Tests/JedTests/StorageDomainBoundaryTests.swift) |
| S01 | Five-method byte device, memory/file hosts, pager/fault seam, packed leaf/PAX/lazy touched-column decode, bounded cache, logical page reads | [hosts](hosts.md), [pager](pager.md), [packed leaves](packed-leaf.md), [lazy record](lazy-record.md) | goldens, host/pager tests, row_materialization and spill suites | O/H; [BlockStore.swift](../../impl/swift-native/Sources/Jed/BlockStore.swift), [PackedPage.swift](../../impl/swift-native/Sources/Jed/PackedPage.swift), [NodePool.swift](../../impl/swift-native/Sources/Jed/NodePool.swift) |
| S02 | Full v50 meta/catalog/type/sequence/view/statistics/index codec; exact rejection of invalid/reserved fields, current-version-only open | [format](../fileformat/format.md) | All 112 goldens and corruption corpus | O/H; [CatalogCodec.swift](../../impl/swift-native/Sources/Jed/CatalogCodec.swift), [Meta.swift](../../impl/swift-native/Sources/Jed/Meta.swift), [CatalogTests.swift](../../impl/swift-native/Tests/JedTests/CatalogTests.swift) |
| S03 | B+tree insert/delete/split/merge/separators, dirty path COW, free-list persistence, reader-aware reclamation and bounded file growth | [B+tree](bplus-reshape.md), [storage](storage.md), [format](../fileformat/format.md) | tall-tree/separator/golden fixtures, storage corpus, simulations | O/H; A; [BPlusTree.swift](../../impl/swift-native/Sources/Jed/BPlusTree.swift), [PageReachability.swift](../../impl/swift-native/Sources/Jed/PageReachability.swift), [TreeBoundaryTests.swift](../../impl/swift-native/Tests/JedTests/TreeBoundaryTests.swift) |
| S04 | Exact large-value packing/externalization, handwritten pinned LZ4, lazy overflow/decompression, unchanged-chain retention and ownership | [large values](large-values.md), [LZ4](../fileformat/lz4.md) | LZ4 vectors, compressed/overflow goldens, simulator large-values profile | O/H; [RecordDisposition.swift](../../impl/swift-native/Sources/Jed/RecordDisposition.swift), [Overflow.swift](../../impl/swift-native/Sources/Jed/Overflow.swift), [RecordDispositionTests.swift](../../impl/swift-native/Tests/JedTests/RecordDispositionTests.swift) |
| S05 | Validated COW body/manifest/meta/barrier ordering, CRC32/CRC64, manifest dependency protection, recovery fallback/stabilization, poison-on-I/O | [validated COW](validated-cow.md), [format](../fileformat/format.md) | cow/malformed goldens and storage suites, process crash scenarios, simulator | O/H; [Pager.swift](../../impl/swift-native/Sources/Jed/Pager.swift), [PageCodec.swift](../../impl/swift-native/Sources/Jed/PageCodec.swift), [PagerTests.swift](../../impl/swift-native/Tests/JedTests/PagerTests.swift) |
| S06 | v49 synchronous-off floor/cumulative manifests, slot protection, cap/deadline/close flushes, client synchronization and truncation rules | [validated COW §4](validated-cow.md), [transactions](transactions.md) | service_relaxed_clients process case; relaxed sim profiles/regressions | O/H; [BatchFlushState.swift](../../impl/swift-native/Sources/Jed/BatchFlushState.swift), [BatchFlusher.swift](../../impl/swift-native/Sources/Jed/BatchFlusher.swift), [BatchFlushBoundaryTests.swift](../../impl/swift-native/Tests/JedTests/BatchFlushBoundaryTests.swift) |
| S07 | File/memory attachments, consistent pinned roots and name routing, ordered gates, independent unchanged roots and one-file-writer limit | [attachments](attached-databases.md), [transactions](transactions.md) | [attach](../conformance/suites/attach/), attachment process/simulation scenarios | O/H; A; [DatabaseDomains.swift](../../impl/swift-native/Sources/Jed/DatabaseDomains.swift), [DomainCommitGroup.swift](../../impl/swift-native/Sources/Jed/DomainCommitGroup.swift), [AttachmentCatalogBoundaryTests.swift](../../impl/swift-native/Tests/JedTests/AttachmentCatalogBoundaryTests.swift) |
| S08 | External sort, row spools/hash join/group/dedup partitions, deterministic replay and cost, admission-triggered early spill, cleanup/failure | [spill](spill.md), [memory](memory.md) | blocking_spill/query_memory_spill suites, spill modes and simulator | O/H; A; [SpillHashStore.swift](../../impl/swift-native/Sources/Jed/SpillHashStore.swift), [RowSpool.swift](../../impl/swift-native/Sources/Jed/RowSpool.swift), [BlockingOperatorBoundaryTests.swift](../../impl/swift-native/Tests/JedTests/BlockingOperatorBoundaryTests.swift) |
| S09 | Compaction/rekey scratch-domain rebuild, atomic rename/directory barrier, trailing truncation, no-held-reader/writer constraints and backup test seam | [API](api.md), [hosts](hosts.md), [page service](page-service.md) | compact/backup process cases, compaction simulation and host matrix | H; [DatabaseMaintenance.swift](../../impl/swift-native/Sources/Jed/DatabaseMaintenance.swift), [DomainRebuilder.swift](../../impl/swift-native/Sources/Jed/DomainRebuilder.swift), [BackupBoundaryTests.swift](../../impl/swift-native/Tests/JedTests/BackupBoundaryTests.swift) |
| S10 | Quick/full integrity traversal, findings/skips/order/truncation, all index kinds and constraints, corrupt-file safety | [integrity](integrity-check.md), [registry](../integrity/findings.toml) | All 24 [image/report pairs](../fileformat/integrity/) and live checks | H; [IntegrityCheck.swift](../../impl/swift-native/Sources/Jed/IntegrityCheck.swift), [IntegrityBoundaryTests.swift](../../impl/swift-native/Tests/JedTests/IntegrityBoundaryTests.swift), [PageServiceClientTests.swift](../../impl/swift-native/Tests/JedTests/PageServiceClientTests.swift) |
| E01 | Approved vetted crypto packages, XAES derivation glue, HKDF/key wrap/Argon2id credentials, independent crypto entropy and safe native boundaries | [encryption](encryption.md), [encrypted bytes](../fileformat/encrypted.md) | All crypto vector input/output sets; dependency/native-boundary review | H; [Encryption.swift](../../impl/swift-native/Sources/Jed/Encryption.swift), [EncryptionTests.swift](../../impl/swift-native/Tests/JedTests/EncryptionTests.swift) |
| E02 | Encrypted create/open/attach, authenticated metadata/key blocks, tag-bound pointers, seal order, credential failures and stale/replayed pages | [encrypted bytes](../fileformat/encrypted.md) | Six encrypted goldens, encrypted corpus and simulations | O/H; [DatabaseFiles.swift](../../impl/swift-native/Sources/Jed/DatabaseFiles.swift), [Encryption.swift](../../impl/swift-native/Sources/Jed/Encryption.swift), [HeldKeyBoundaryTests.swift](../../impl/swift-native/Tests/JedTests/HeldKeyBoundaryTests.swift) |
| E03 | Credential change two barriers, DEK rotation/conversion, encrypted scratch/temp/spill/backups, service unlocking/resealing and crash/replay recovery | [encryption](encryption.md), [encrypted bytes](../fileformat/encrypted.md) | encrypted process histories, encryption profiles/regressions, host matrix | O/H; [DatabaseMaintenance.swift](../../impl/swift-native/Sources/Jed/DatabaseMaintenance.swift), [SealedScratchStore.swift](../../impl/swift-native/Sources/Jed/SealedScratchStore.swift), [CredentialWriterBoundaryTests.swift](../../impl/swift-native/Tests/JedTests/CredentialWriterBoundaryTests.swift) |
| H01 | Linux stable protocol-v2 bundle, crash-clean OS locks, owner election/arrival/presence gates, auto/exclusive/none, aliases/timeout/fail-closed behavior | [locking](locking.md) | process histories and host lock tests | H; [FileLockBundle.swift](../../impl/swift-native/Sources/Jed/FileLockBundle.swift), [ServiceDatabaseHost.swift](../../impl/swift-native/Sources/Jed/ServiceDatabaseHost.swift), [PageServiceAutomaticTests.swift](../../impl/swift-native/Tests/JedTests/PageServiceAutomaticTests.swift) |
| H02 | Native Swift page-service owner/client; all framing/messages/bounds; pins, deltas/status, promotion/yield/drain, attachments, read-ahead and stream client | [page service](page-service.md), [wire data](../pageservice/README.md) | wire fixtures, all process pairings and service simulations | O/H; [PageServiceOwner.swift](../../impl/swift-native/Sources/Jed/PageServiceOwner.swift), [PageServiceClient.swift](../../impl/swift-native/Sources/Jed/PageServiceClient.swift), [PageTransportBoundaryTests.swift](../../impl/swift-native/Tests/JedTests/PageTransportBoundaryTests.swift) |
| H03 | Owner foreground zero coordination calls/meta rereads, owner-only maintenance/reuse and remote pin watermark accounting | [page service](page-service.md), [locking](locking.md) | owner instrumentation, process/simulator tests | H; [DatabaseCore.swift](../../impl/swift-native/Sources/Jed/DatabaseCore.swift), [PageServiceLeases.swift](../../impl/swift-native/Sources/Jed/PageServiceLeases.swift), [PageServiceMaintenanceTests.swift](../../impl/swift-native/Tests/JedTests/PageServiceMaintenanceTests.swift) |
| V01 | Real shared-corpus runner with exact capability declaration, errors/types/rendering/sorts/hashes, params/options/directives and actual public cursor route | [conformance](conformance.md), [host matrix](swift-native-host-matrix.md) | All suites in every applicable runner mode | O/H; A/D; [SQLCorpus.swift](../../impl/swift-native/Tests/JedTests/SQLCorpus.swift), [SQLCorpusVariant.swift](../../impl/swift-native/Tests/JedTests/SQLCorpusVariant.swift), [SQLCorpusBoundaryTests.swift](../../impl/swift-native/Tests/JedTests/SQLCorpusBoundaryTests.swift) |
| V02 | Deterministic scheduler/real process actors/simulated disk+lock+network+spill; exact transcripts, durability floor and byte crash images | [simulation](simulation-testing.md), [concurrency](concurrency-testing.md) | 7 schedules, 31 process cases, 22 sim profiles, 18 regressions | O/H; [SimRunner.swift](../../impl/swift-native/Sources/Jed/SimRunner.swift), [SimFilesystem.swift](../../impl/swift-native/Sources/Jed/SimFilesystem.swift), [ConcurrencyCorpusBoundaryTests.swift](../../impl/swift-native/Tests/JedTests/ConcurrencyCorpusBoundaryTests.swift) |
| V03 | Bidirectional current-format interchange with Rust/Go/TS, plaintext/encrypted, incremental histories and fixture reading/writing | [format](../fileformat/format.md), [encrypted bytes](../fileformat/encrypted.md) | Goldens and cross-core process/round-trip runs | H; [DatabaseFiles.swift](../../impl/swift-native/Sources/Jed/DatabaseFiles.swift), [DomainImage.swift](../../impl/swift-native/Sources/Jed/DomainImage.swift), [SerializationBoundaryTests.swift](../../impl/swift-native/Tests/JedTests/SerializationBoundaryTests.swift) |
| V04 | Oracle checks for relevant PG-comparable behavior; default repository gates remain healthy without Swift | [conformance](conformance.md), [oracle profile](../conformance/oracle_profile.toml) | `mise run corpus:check`; default verify/test/CI checks | H; earlier verify 17 and CI 18/18 passes retain their actors; DEFAULT_ISOLATION and VERIFICATION reports |
| V05 | Usable Swift package and local embedding examples; correctness report, release baseline, matched-checksum benchmark and measured optimization | [benchmarks](benchmarks.md), [cores](cores.md) | Opt-in release benchmark runs; recorded commands/hardware/toolchain | H; P; [Ergonomic.swift](../../impl/swift-native/Sources/Jed/Ergonomic.swift), [ErgonomicBoundaryTests.swift](../../impl/swift-native/Tests/JedTests/ErgonomicBoundaryTests.swift) |

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

| Lane | Required comparison / exercised boundary | Verified result / scope limit |
|---|---|---|
| SQL memory | Every applicable suite, real prepare/query/cursor path, all directives and capabilities | O/A; all 21,477 records per core pass with exact accounting; final actor mapping in VERIFICATION.md |
| SQL durable | File-backed create/commit/reopen and same corpus values/types/errors/costs | O/A; all 19,438 disk records per core pass with exact accounting; final actor mapping in VERIFICATION.md |
| SQL spill | Tiny `work_mem`, native file spill and deterministic hash/sort replay | O/H/A; all 19,438 disk-spill records per core pass with exact accounting; forced TEMP separately passes 28 files/26 actual spills |
| Query memory | Memory/disk/disk-spill account peaks, release invariants, pending writes and storage TSV parity | A verified; corrected full release passes all 241,412 exact measurements across 12 runs with complete membership and zero failures; historical GiST differences/cache incident preserved |
| Encryption | Encrypted file mode, encrypted cost/EXPLAIN transcript parity and credentials | H; full 77,612-record comparison and postoptimization encrypted refresh pass; view-source checks have separate memory/disk evidence |
| SQL via service | Same engine SQL through page-client storage against supported owners | H; refreshed 116,628 records, six directions, zero failures |
| Shared concurrency | All seven schedules, real writer/read handles, pins/watermark/gate blocking | O/H; all seven shared schedules pass in the final native suite |
| Real processes | 31 scenarios with Swift owner/client and supported-core pairings | H; refreshed 496 ordered pairings pass; exact final bytes retained |
| Simulation | 18 regressions, seeded sweeps covering all 22 profiles, matching transcripts and crash images | H; refreshed 18 regressions / 54 comparisons / 153 images pass; refreshed generated 22 × 8 sweep passes 176 traces per core / 459 comparisons / 7,049 images |
| Bytes | Every encoding/LZ4/crypto/wire/vector contract; all golden reads/writes and integrity reports | H; shared vectors, 112 goldens and 24 integrity/50 reports pass; diagnostic D also passes |
| Interchange | Swift-to-each-supported-core and reverse, plaintext/encrypted, initial/incremental states | H; all 112 images/2,691 phases and postoptimization interchange refresh pass; view-source checks have separate evidence |
| Host APIs | Lifecycle/prepared/cursors/cancel/environment/functions/capabilities/resources/attachments/maintenance | H/D/A; lifecycle/batch/settings/resource/integrity, direct diagnostics, full exact-account comparison and final native suite pass |
| Independent review | Storage durability, approved crypto integration, resource accounting, conformance adapters | H; independent review covers authored durability/crypto/resources/adapters, corrections and the measured optimization; final gate map in VERIFICATION.md |
| Oracle | Relevant PG-clean corpus checks under the checkout's declared oracle profile | H; relevant shared changes checked; earlier short_circuit/correlated regeneration unchanged |
| Default gates | No implicit Swift dependency in normal CI/tests/benchmarks/codegen | H; earlier verify 17 and CI 18/18 passes retain their actors; current repository checks are mapped in VERIFICATION.md; default isolation retained |
| Release performance | Correctness first, preserved baseline, matched workload/durability/checksums, measured optimization | P for the original `3db8fc2` before/after release runs: 143 workloads × four cores × three rounds with matching checksums/row counts. Current diagnostic comparisons cover 22 peer, eight paired Swift and three Ruby workloads, each with three rounds per actor; results and limits in BENCHMARKS.md. |
| Apple builds | macOS/iOS builds and tests, separately from Linux acceptance | Unverified platform; Linux scope is unaffected |


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

The identifiers and numeric counts below remain the mechanically enumerated
baseline inventory. All 26 suite directories and 475 capability declarations are
included in the successful whole frozen run (O); the 19 profiles are not used
to narrow membership. Coverage of their assertions does not prove every allowed
combination. The corrected view-source combination in D12 has separate memory/disk
regression evidence and is included in the subsequent full release suites.
Resource capabilities marked A have completed the exact all-mode accounting gate.
Structured fields D are an orthogonal host contract established by the direct
shared cases and host probes, not by the two SQL error capabilities alone. Final
actor-specific verification is mapped in
[VERIFICATION.md](../../impl/swift-native/VERIFICATION.md).

### Suite files and SQL directives

| Suite | Files | Statements | Queries | State |
|---|---:|---:|---:|---|
| [aggregates](../conformance/suites/aggregates/) | 52 | 373 | 521 | O |
| [attach](../conformance/suites/attach/) | 6 | 109 | 35 | O |
| [cast](../conformance/suites/cast/) | 13 | 144 | 156 | O |
| [collation](../conformance/suites/collation/) | 14 | 147 | 225 | O |
| [compare](../conformance/suites/compare/) | 1 | 3 | 8 | O |
| [concurrency](../conformance/suites/concurrency/) | 7 | 0 | 0 | O |
| [cte](../conformance/suites/cte/) | 14 | 141 | 137 | O |
| [ddl](../conformance/suites/ddl/) | 101 | 3,524 | 687 | O |
| [dml](../conformance/suites/dml/) | 42 | 1,217 | 485 | O |
| [expr](../conformance/suites/expr/) | 113 | 1,080 | 2,260 | O |
| [introspection](../conformance/suites/introspection/) | 17 | 234 | 209 | O |
| [joins](../conformance/suites/joins/) | 34 | 315 | 305 | O |
| [json](../conformance/suites/json/) | 51 | 696 | 905 | O |
| [mutation](../conformance/suites/mutation/) | 3 | 50 | 22 | O |
| [null](../conformance/suites/null/) | 1 | 6 | 14 | O |
| [params](../conformance/suites/params/) | 5 | 95 | 150 | O |
| [query](../conformance/suites/query/) | 99 | 1,071 | 1,410 | O |
| [resource](../conformance/suites/resource/) | 33 | 650 | 369 | O |
| [rqg](../conformance/suites/rqg/) | 5 | 420 | 150 | O |
| [session](../conformance/suites/session/) | 5 | 94 | 25 | O |
| [setops](../conformance/suites/setops/) | 9 | 153 | 165 | O |
| [storage](../conformance/suites/storage/) | 26 | 155 | 79 | O |
| [subquery](../conformance/suites/subquery/) | 21 | 182 | 298 | O |
| [transactions](../conformance/suites/transactions/) | 11 | 292 | 55 | O |
| [types](../conformance/suites/types/) | 44 | 652 | 746 | O |
| [window](../conformance/suites/window/) | 22 | 141 | 165 | O |
| **Total** | **749** | **11,944** | **9,581** | **O/D/A; final actor mapping in VERIFICATION.md** |

### Named profiles

All profile definitions and inheritance come from [manifest.toml](../conformance/manifest.toml).

`core`, `mutation`, `casts`, `comparison`, `expression`, `decimal`, `functions`, `joins`, `outer_joins`, `constraints`, `aggregates`, `grouping`, `having`, `predicates`, `timestamps`, `set_operations`, `transactions`, `subqueries`, `cte`.

### All baseline capabilities

The identifier is the exact key of its full description in
[manifest.toml](../conformance/manifest.toml). No capability is inferred from a
Swift implementation name or a profile membership. O records complete frozen
runner inclusion and its successful ordinary results; it does not claim each
capability has a separate exhaustive host or resource proof.

#### attach — 1 capabilities

| Capability | State |
|---|---|
| `attach.in_memory` | O |

#### cast — 13 capabilities

| Capability | State |
|---|---|
| `cast.array` | O |
| `cast.bool_int` | O |
| `cast.composite` | O |
| `cast.datetime` | O |
| `cast.explicit` | O |
| `cast.numeric_input_forms` | O |
| `cast.operator` | O |
| `cast.runtime_text` | O |
| `cast.scalar_text` | O |
| `cast.string_literal` | O |
| `cast.text_date` | O |
| `cast.text_string_native` | O |
| `cast.uuid` | O |

#### compare — 1 capabilities

| Capability | State |
|---|---|
| `compare.promotion` | O |

#### ddl — 57 capabilities

| Capability | State |
|---|---|
| `ddl.alter_identity` | O |
| `ddl.alter_sequence` | O |
| `ddl.alter_table` | O |
| `ddl.check` | O |
| `ddl.collate_column` | O |
| `ddl.column_default` | O |
| `ddl.column_default_expr` | O |
| `ddl.composite_default` | O |
| `ddl.composite_key` | O |
| `ddl.composite_primary_key` | O |
| `ddl.constraint_name_schema` | O |
| `ddl.create_index_if_not_exists` | O |
| `ddl.create_table` | O |
| `ddl.create_table_as` | O |
| `ddl.create_table_as_writable_with` | O |
| `ddl.create_table_if_not_exists` | O |
| `ddl.drop_table` | O |
| `ddl.drop_table_if_exists` | O |
| `ddl.drop_table_multi` | O |
| `ddl.exclusion_btree` | O |
| `ddl.exclusion_constraint` | O |
| `ddl.exclusion_partial` | O |
| `ddl.explain_create_table_as` | O |
| `ddl.foreign_key` | O |
| `ddl.foreign_key_child_index` | O |
| `ddl.foreign_key_cross_type` | O |
| `ddl.foreign_key_match_full` | O |
| `ddl.gin_index` | O |
| `ddl.gin_multi_column` | O |
| `ddl.gist_index` | O |
| `ddl.gist_scalar_index` | O |
| `ddl.identity` | O |
| `ddl.index_expr` | O |
| `ddl.index_key_order` | O |
| `ddl.index_partial` | O |
| `ddl.not_null` | O |
| `ddl.primary_key` | O |
| `ddl.reindex` | O |
| `ddl.secondary_index` | O |
| `ddl.select_into` | O |
| `ddl.sequence` | O |
| `ddl.sequence_as_type` | O |
| `ddl.serial` | O |
| `ddl.temp_on_commit` | O |
| `ddl.temp_table` | O |
| `ddl.unique` | O |
| `ddl.view` | O |
| `ddl.view_alter` | O |
| `ddl.view_cascade` | O |
| `ddl.view_default` | O |
| `ddl.view_dependencies` | O |
| `ddl.view_long_definition` | O |
| `ddl.view_options` | O |
| `ddl.view_recursive` | O |
| `ddl.view_rename` | O |
| `ddl.view_replace` | O |
| `ddl.view_temp` | O |

#### dml — 30 capabilities

| Capability | State |
|---|---|
| `dml.costed_access` | O |
| `dml.delete` | O |
| `dml.explain` | O |
| `dml.explain_analyze` | O |
| `dml.insert` | O |
| `dml.insert_alias` | O |
| `dml.insert_column_list` | O |
| `dml.insert_default_values` | O |
| `dml.insert_multi_row` | O |
| `dml.insert_on_conflict` | O |
| `dml.insert_on_conflict_decorations` | O |
| `dml.insert_on_conflict_default` | O |
| `dml.insert_on_conflict_exclusion` | O |
| `dml.insert_on_conflict_rekey` | O |
| `dml.insert_select` | O |
| `dml.insert_select_container` | O |
| `dml.insert_select_setop` | O |
| `dml.insert_values_expr` | O |
| `dml.merge` | O |
| `dml.returning` | O |
| `dml.returning_old_new` | O |
| `dml.update` | O |
| `dml.update_container` | O |
| `dml.update_default` | O |
| `dml.upsert_partial_arbiter` | O |
| `dml.view_check_option` | O |
| `dml.view_on_conflict` | O |
| `dml.view_write` | O |
| `dml.where_contradiction` | O |
| `dml.with_clause` | O |

#### error — 2 capabilities

| Capability | State |
|---|---|
| `error.division_by_zero` | O |
| `error.overflow_trap` | O |

#### expr — 43 capabilities

| Capability | State |
|---|---|
| `expr.arithmetic` | O |
| `expr.array_slice` | O |
| `expr.array_subscript` | O |
| `expr.at_time_zone` | O |
| `expr.between` | O |
| `expr.case` | O |
| `expr.coalesce` | O |
| `expr.collate` | O |
| `expr.comparison_value` | O |
| `expr.date_arithmetic` | O |
| `expr.date_trunc` | O |
| `expr.decimal_arithmetic` | O |
| `expr.extract` | O |
| `expr.greatest_least` | O |
| `expr.ilike` | O |
| `expr.in_list` | O |
| `expr.interval_arithmetic` | O |
| `expr.interval_scale` | O |
| `expr.is_json` | O |
| `expr.json_behavior_default` | O |
| `expr.json_query_fns` | O |
| `expr.jsonpath_any` | O |
| `expr.jsonpath_arith` | O |
| `expr.jsonpath_filter` | O |
| `expr.jsonpath_match` | O |
| `expr.jsonpath_methods` | O |
| `expr.jsonpath_predicates` | O |
| `expr.jsonpath_vars` | O |
| `expr.like` | O |
| `expr.like_escape` | O |
| `expr.not_equal` | O |
| `expr.nullif` | O |
| `expr.numeric_literal_forms` | O |
| `expr.numeric_transcendental` | O |
| `expr.parens` | O |
| `expr.precedence` | O |
| `expr.regex_imatch` | O |
| `expr.regex_match` | O |
| `expr.similar_to` | O |
| `expr.text_build` | O |
| `expr.text_concat` | O |
| `expr.timestamp_arithmetic` | O |
| `expr.unary_minus` | O |

#### file — 1 capabilities

| Capability | State |
|---|---|
| `file.page_service` | O |

#### func — 92 capabilities

| Capability | State |
|---|---|
| `func.abs` | O |
| `func.age` | O |
| `func.arg_coercion` | O |
| `func.array` | O |
| `func.array_agg` | O |
| `func.array_compatible` | O |
| `func.array_composite` | O |
| `func.array_containment` | O |
| `func.array_quantified` | O |
| `func.array_to_string` | O |
| `func.bit_and` | O |
| `func.bit_or` | O |
| `func.bit_xor` | O |
| `func.bool_and` | O |
| `func.bool_or` | O |
| `func.casing` | O |
| `func.clock_timestamp` | O |
| `func.coldeflist` | O |
| `func.corr` | O |
| `func.covar_pop` | O |
| `func.covar_samp` | O |
| `func.current_date` | O |
| `func.date_part` | O |
| `func.every` | O |
| `func.json_agg` | O |
| `func.json_builders` | O |
| `func.json_ctor` | O |
| `func.json_each` | O |
| `func.json_object` | O |
| `func.json_object_agg` | O |
| `func.json_populate` | O |
| `func.json_processing` | O |
| `func.json_record` | O |
| `func.json_set` | O |
| `func.json_set_lax` | O |
| `func.json_srf` | O |
| `func.json_table` | O |
| `func.json_verbatim` | O |
| `func.jsonb_access` | O |
| `func.jsonb_contains` | O |
| `func.jsonb_mutate` | O |
| `func.jsonb_path` | O |
| `func.make_date` | O |
| `func.make_timestamp` | O |
| `func.mixed_numeric_args` | O |
| `func.named_arguments` | O |
| `func.now` | O |
| `func.range_accessors` | O |
| `func.range_constructors` | O |
| `func.range_operators` | O |
| `func.range_set_operators` | O |
| `func.regexp_count` | O |
| `func.regexp_instr` | O |
| `func.regexp_like` | O |
| `func.regexp_match` | O |
| `func.regexp_matches` | O |
| `func.regexp_replace` | O |
| `func.regexp_split_to_array` | O |
| `func.regexp_split_to_table` | O |
| `func.regexp_substr` | O |
| `func.regr_avgx` | O |
| `func.regr_avgy` | O |
| `func.regr_count` | O |
| `func.regr_intercept` | O |
| `func.regr_r2` | O |
| `func.regr_slope` | O |
| `func.regr_sxx` | O |
| `func.regr_sxy` | O |
| `func.regr_syy` | O |
| `func.round` | O |
| `func.row_to_json` | O |
| `func.sequence` | O |
| `func.stddev` | O |
| `func.stddev_pop` | O |
| `func.stddev_samp` | O |
| `func.string_agg` | O |
| `func.string_to_array` | O |
| `func.substring_regex` | O |
| `func.substring_similar` | O |
| `func.to_char` | O |
| `func.to_jsonb` | O |
| `func.to_number` | O |
| `func.to_timestamp` | O |
| `func.unknown_args` | O |
| `func.unnest` | O |
| `func.unnest_record` | O |
| `func.uuid_extract` | O |
| `func.uuid_generate` | O |
| `func.var_pop` | O |
| `func.var_samp` | O |
| `func.variadic` | O |
| `func.variance` | O |

#### harness — 5 capabilities

| Capability | State |
|---|---|
| `harness.attach` | O |
| `harness.fixture_open` | O |
| `harness.fixture_open_error` | O |
| `harness.params` | O |
| `harness.upgrade_collations` | O |

#### introspect — 9 capabilities

| Capability | State |
|---|---|
| `introspect.columns` | O |
| `introspect.constraints` | O |
| `introspect.indexes` | O |
| `introspect.sequences` | O |
| `introspect.statistics` | O |
| `introspect.tables` | O |
| `introspect.type_fields` | O |
| `introspect.types` | O |
| `introspect.views` | O |

#### maintenance — 2 capabilities

| Capability | State |
|---|---|
| `maintenance.analyze` | O |
| `maintenance.analyze_gin_terms` | O |

#### null — 1 capabilities

| Capability | State |
|---|---|
| `null.three_valued` | O |

#### query — 145 capabilities

| Capability | State |
|---|---|
| `query.aggregate_distinct` | O |
| `query.aggregate_filter` | O |
| `query.aggregate_order_by` | O |
| `query.aggregate_select_distinct` | O |
| `query.aggregates` | O |
| `query.any_array_point_lookup` | O |
| `query.bare_column_alias` | O |
| `query.bounded_limit_streaming` | O |
| `query.column_alias` | O |
| `query.comma_from` | O |
| `query.comparison_order` | O |
| `query.composite_key_set` | O |
| `query.composite_pk_pushdown` | O |
| `query.correlated_pushdown` | O |
| `query.cross_join` | O |
| `query.cte` | O |
| `query.cte_data_modifying` | O |
| `query.cte_nested` | O |
| `query.cte_recursive` | O |
| `query.cte_search_cycle` | O |
| `query.derived_pushdown` | O |
| `query.derived_table` | O |
| `query.distinct` | O |
| `query.distinct_on` | O |
| `query.except` | O |
| `query.explain` | O |
| `query.explain_analyze` | O |
| `query.filter_conjunct_order` | O |
| `query.gin_any_eq` | O |
| `query.gin_array_eq` | O |
| `query.gin_bytea_elements` | O |
| `query.gin_decimal_elements` | O |
| `query.gin_disjunction` | O |
| `query.gin_element_types` | O |
| `query.gin_float_elements` | O |
| `query.gin_index_nested_loop` | O |
| `query.gin_interval_elements` | O |
| `query.gin_jsonb_jsonpath` | O |
| `query.gin_jsonb_ops` | O |
| `query.gin_mutation` | O |
| `query.gin_scan` | O |
| `query.gin_text_elements` | O |
| `query.gist_index_nested_loop` | O |
| `query.gist_scalar_scan` | O |
| `query.gist_scan` | O |
| `query.group_by` | O |
| `query.group_by_correlated` | O |
| `query.group_by_expr` | O |
| `query.group_by_functional_dependency` | O |
| `query.grouping_sets` | O |
| `query.grouping_sets_window` | O |
| `query.hash_join` | O |
| `query.having` | O |
| `query.hypothetical_set_aggregate` | O |
| `query.index_expr` | O |
| `query.index_mutation` | O |
| `query.index_nested_loop` | O |
| `query.index_partial` | O |
| `query.index_partial_access_paths` | O |
| `query.index_partial_full_scan` | O |
| `query.index_partial_implication` | O |
| `query.index_prefix` | O |
| `query.index_range` | O |
| `query.index_variable_width_tail` | O |
| `query.intersect` | O |
| `query.interval_set` | O |
| `query.is_distinct_from` | O |
| `query.is_null` | O |
| `query.join_full` | O |
| `query.join_inner` | O |
| `query.join_left` | O |
| `query.join_natural` | O |
| `query.join_parenthesized` | O |
| `query.join_pushdown` | O |
| `query.join_right` | O |
| `query.join_using` | O |
| `query.lateral` | O |
| `query.limit` | O |
| `query.limit_short_circuit` | O |
| `query.logical_connectives` | O |
| `query.offset` | O |
| `query.on_pushdown` | O |
| `query.or_in_point_lookup` | O |
| `query.order_by` | O |
| `query.order_by_alias` | O |
| `query.order_by_correlated` | O |
| `query.order_by_expr` | O |
| `query.order_by_grouped_window` | O |
| `query.order_by_grouping` | O |
| `query.order_by_index_scan` | O |
| `query.order_by_join_inl` | O |
| `query.order_by_join_scan` | O |
| `query.order_by_keys` | O |
| `query.order_by_ordinal` | O |
| `query.order_by_pk_scan` | O |
| `query.order_by_topk` | O |
| `query.order_by_window` | O |
| `query.ordered_set_aggregate` | O |
| `query.ordered_set_array_fraction` | O |
| `query.ordered_set_collation` | O |
| `query.ordered_set_interval` | O |
| `query.ordered_set_nonconstant_fraction` | O |
| `query.point_lookup` | O |
| `query.qualified_column` | O |
| `query.qualified_star` | O |
| `query.qualified_table` | O |
| `query.quoted_identifiers` | O |
| `query.safe_conjunct_gate` | O |
| `query.select` | O |
| `query.select_no_from` | O |
| `query.select_star` | O |
| `query.set_returning` | O |
| `query.setop_parenthesized` | O |
| `query.subquery_correlated` | O |
| `query.subquery_exists` | O |
| `query.subquery_in` | O |
| `query.subquery_quantified` | O |
| `query.subquery_row_valued` | O |
| `query.subquery_scalar` | O |
| `query.table_alias` | O |
| `query.table_column_alias` | O |
| `query.unicode_escapes` | O |
| `query.union` | O |
| `query.values` | O |
| `query.values_order_by` | O |
| `query.values_query` | O |
| `query.where_contradiction` | O |
| `query.where_eq` | O |
| `query.where_pushdown` | O |
| `query.window` | O |
| `query.window_aggregate` | O |
| `query.window_aggregate_filter` | O |
| `query.window_base_extend` | O |
| `query.window_collation` | O |
| `query.window_expr_keys` | O |
| `query.window_frame` | O |
| `query.window_frame_exclude` | O |
| `query.window_frame_range` | O |
| `query.window_grouped` | O |
| `query.window_named` | O |
| `query.window_ntile` | O |
| `query.window_offset` | O |
| `query.window_ranking` | O |
| `query.window_ratio` | O |
| `query.within_group_expr` | O |

#### resource — 19 capabilities

| Capability | State |
|---|---|
| `resource.composite_depth_limit` | O |
| `resource.cost_limit` | O |
| `resource.cost_metering` | O |
| `resource.depth_limit` | O |
| `resource.identifier_length_limit` | O |
| `resource.in_list_limit` | O |
| `resource.json_depth_limit` | O |
| `resource.key_metering` | O |
| `resource.param_index_limit` | O |
| `resource.pure_builtins` | O |
| `resource.query_memory` | O; A |
| `resource.regex_program_limit` | O |
| `resource.scalar_memory` | O |
| `resource.sql_length_limit` | O |
| `resource.storage_file` | O; A |
| `resource.storage_memory` | O; A |
| `resource.temp_budget` | O; A |
| `resource.view_depth_limit` | O |
| `resource.view_expansion_limit` | O |

#### session — 6 capabilities

| Capability | State |
|---|---|
| `session.allow_ddl` | O |
| `session.allow_temp_ddl` | O |
| `session.lifetime_cost` | O |
| `session.privileges` | O |
| `session.timezone` | O |
| `session.variables` | O |

#### storage — 1 capabilities

| Capability | State |
|---|---|
| `storage.unparsable_definitions` | O |

#### txn — 8 capabilities

| Capability | State |
|---|---|
| `txn.explicit` | O |
| `txn.failed_state` | O |
| `txn.gate_blocking` | O |
| `txn.read_handle` | O |
| `txn.read_only` | O |
| `txn.savepoint` | O |
| `txn.shared` | O |
| `txn.watermark` | O |

#### types — 39 capabilities

| Capability | State |
|---|---|
| `types.array` | O |
| `types.array_collation` | O |
| `types.array_composite` | O |
| `types.array_multidim` | O |
| `types.boolean` | O |
| `types.boolean_storable` | O |
| `types.bytea` | O |
| `types.composite` | O |
| `types.composite_array_field` | O |
| `types.composite_bare_row` | O |
| `types.composite_range_field` | O |
| `types.composite_row_assign` | O |
| `types.date` | O |
| `types.date_clock_literal` | O |
| `types.decimal` | O |
| `types.f32` | O |
| `types.f64` | O |
| `types.float_key` | O |
| `types.i16` | O |
| `types.i32` | O |
| `types.i64` | O |
| `types.interval` | O |
| `types.interval_qualifier` | O |
| `types.json` | O |
| `types.json_casts` | O |
| `types.jsonb` | O |
| `types.jsonb_compare` | O |
| `types.jsonb_literal` | O |
| `types.jsonpath` | O |
| `types.range` | O |
| `types.record_column` | O |
| `types.text` | O |
| `types.timestamp` | O |
| `types.timestamp_precision` | O |
| `types.timestamptz` | O |
| `types.timestamptz_session_input` | O |
| `types.timestamptz_session_render` | O |
| `types.uuid` | O |
| `types.varchar_length` | O |

### Enumeration method

Counts above were computed from `git ls-tree -r --name-only` and `git show` at
`0b76dd6e9c2675b552eb8af9e820065351ffa4b3`, parsing the TOML arrays and counting
line-start SQL directives in tracked `.test` files. This is a one-time baseline
inventory; later changes to live specification files do not silently expand the
frozen experiment. Byte fixture families include deliberately malformed/recovery
images: those must produce their specified rejection/fallback result, not be
accepted as ordinary clean databases.
