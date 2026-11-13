# Swift native experiment: embedding and harness acceptance matrix

## Authorized refresh (2026-11-12)

The current semantic target is **`b1a6affca7725e0b07a4f734eddb1c569ef76bcf`**, superseding the
original November 5 target. The mechanical snapshot pins **763 SQL files**, **116
byte fixtures**, **19 simulation regressions**, 22 simulation profiles and the
unchanged 31 process scenarios. Format v52 includes fixed numeric slots, key-column
elision, narrow directories, shared key prefixes, offset/dictionary/array regions,
and multi-way leaf splits. SQL additions include timestamp aliases, LIMIT/OFFSET
parameters, INSERT/CTE parameter inference, whole-row references, cast-derived
output names and contextual container literals. Streaming and aggregate lane
selection follows the refreshed resource-accounting contract.

[VERIFICATION.md](../../impl/swift-native/VERIFICATION.md) is the acceptance record
for this refresh. The detailed audit below preserves its original baseline and
actor-specific evidence; its earlier test counts and timing results do not certify
this source. The generated snapshot is the exact current file inventory. The
experiment remains opt-in and excluded from default CI and website integration;
Apple platforms and production delivery remain unverified/undecided.

## Preserved November 5–10 audit

Durable preoptimization evidence is preserved in the ignored ext4
[verification archive](../../impl/swift-native/results/verification-preoptimization-20261106/manifest.json):
50 successful report/log/TSV files, 11,386,296 bytes, with original paths and
SHA-256 hashes in its manifest. Originals remain intact; binaries and simulation
images are excluded. This archive remains preoptimization evidence; the final
actor-specific correctness and measured comparison are recorded in
[VERIFICATION.md](../../impl/swift-native/VERIFICATION.md) and
[BENCHMARKS.md](../../impl/swift-native/BENCHMARKS.md).

Semantic audit target: **`0b76dd6e9c2675b552eb8af9e820065351ffa4b3`**. The frozen
Swift implementation is `fb34b0e8cb3d82287fbd3401e3426f04b3b86c7a`. The companion
[scope matrix](swift-native-scope.md) inventories SQL, shared data and storage;
Ruby's frozen experiment is not an acceptance baseline. Linux acceptance and the
original measured optimization comparison are recorded in VERIFICATION.md and
BENCHMARKS.md. Structural-follow-up diagnostic measurements are complete. Each gate
retains its actual actor and scope. Apple platforms are unverified; Swift remains
opt-in, outside default tasks and automatic future
maintenance. Production delivery and promotion are undecided. Commits remain
local-only; no push, master merge or package publication is implied.

This matrix began with every row **missing**. It now links implementation and
acceptance evidence for the frozen baseline. The current release passes
543 native tests including all 749 frozen SQL files. Full exact accounting,
encrypted SQL, interchange and saved-simulation postoptimization checks are mapped
to their captured actors in VERIFICATION.md. The original `3db8fc2` before/after
benchmark comparison covers 143 workloads, four cores and three rounds. The completed
follow-up compares 22 peer, eight paired Swift and three Ruby workloads separately,
each with three rounds per actor.
BENCHMARKS.md records measured scope and limits; neither comparison establishes
universal performance parity or selects production delivery.
Compiling an interface, declaring a capability, or passing an unrelated SQL query
does not establish another row's acceptance. Preserve each gate's provenance.

### Preserved preoptimization checkpoint

These earlier counts and hashes retain their own actors; final affected-path
verification is recorded separately above and in VERIFICATION.md.

Checkpoint (**2026-11-06**, Linux x86-64, Swift 6.4.2): all 407 shared
diagnostic cases, 10 object-field cases and 20 runtime metadata probes pass.
The earlier normal CI passed all 18 steps after the shared Go typed-float
correction, **local-only** commit `05a4e59c`. The first shared-GiST integration
attempt passed 16/18: formatting and a Rust binary with a stale private manifest
path failed. The canonical retry now passes 18/18. Prior process acceptance
passes all 496 ordered pairs, and saved simulation acceptance passes 18 traces, 54 comparisons
and 153 independently checked images. The final 42-test diagnostic/GIN/deep-view
gate passes, including four RETURNING cases. Sixth ordinary SQL passes all
749 files and 21,597 records. The 504-native-test run had only two stale GIN
assertions; their corrected serialization boundary passes in that focused gate.
The earlier release suite passed all 505 native tests, including all 749 SQL
files and the corrected GIN boundary. Release forced-TEMP passes 28 files,
1,255 records, 571 cells and 23 cost assertions with 26 scratch files. The
corrected full release V04 matrix passes 241,412 measurements with zero failures
and complete_matrix=true: all 12 runs, 21,477 memory records and 19,438 records
in each disk mode per core. Ordinary assertions, peaks and storage bytes agree.
The generated 22-profile × eight-seed refresh passes all 176 traces per core,
459 transcript comparisons and 7,049 checked Swift images. Service SQL refresh
passes all six directions and 116,628 frozen records with zero failures:
`/tmp/jed-swift-service-corpus-20261106-1040687-2wheah/report.json`.
Independent host/crypto/root review traced the historical 376 V04 differences to
Go/Rust/TypeScript GiST loaders retaining reconstruction staging after committed
load, contrary to memory.md §7. Swift correctly retains zero pending bytes;
the shared loader correction, six host cases and public regression are now
integrated. The corrected complete comparison now passes without added exceptions.
The Swift-only GiST reopen regression passes 33 records/10 cells in each of
memory and disk; its focused process had four service-host assertions, now
resolved (`/tmp/jed-swift-native-release-gist-service-first.log`). The reviewed
shared regressions are committed locally; their frozen Swift copies remain unchanged.
Refreshed encrypted SQL passes 77,612 records, and refreshed interchange passes
all 2,691 phases over 112 fixtures. These are correctness checks, not performance
measurements.

A guard audit found an SRF/JSON_TABLE view-definition dependency gap outside the
frozen SQL assertions. Its reviewed correction passes 41 records and 23 cells in
both memory and disk, plus host capability checks. The dependent release refresh
passes all 749 SQL files. The fourth release refresh passes all 509 native
tests with zero failures, resolving the prior handoff/client and old-pin
assertions. Both supplemental Swift-client/Rust-pages stream scenarios pass.
The fresh full release process matrix passes 496/496; saved simulations pass
18 traces per core, 54 comparisons and 153 checked Swift images
(`/tmp/jed-swift-sim-sweep-20261106-2057112-ozakk7/summary.json`). The final generated release
simulation sweep passes all 176 traces per core, 459 comparisons and 7,049
checked Swift images, zero failures (/tmp/jed-swift-sim-sweep-20261106-2104641-awof5y/summary.json).
Performance was unmeasured at this checkpoint; it is now recorded separately in
BENCHMARKS.md. Exact-account acceptance passes (A). Running or incomplete lanes
never establish acceptance.

Logs, immutable actor hashes and commands are retained in
[VERIFICATION.md](../../impl/swift-native/VERIFICATION.md). In particular,
`/tmp/jed-swift-native-default-ci-float-fields.log` records the earlier CI pass;
`/tmp/jed-swift-native-default-ci-gist-load.log` preserves the failed 16/18
attempt; `/tmp/jed-swift-native-default-ci-gist-canonical-retry.log`
records the passing canonical 18/18 retry.
The logs `/tmp/jed-swift-native-process-mixed-second.log` and
`/tmp/jed-swift-native-sim-regress-final.log` record the completed refreshes.
The broad acceptance actor `b7d500a9…` predates the diagnostic-only RETURNING
ordering follow-up; that edit now passes the separate focused gate at
`/tmp/jed-swift-native-diagnostics-final.log`. The view-source correction passes
its focused gate at `/tmp/jed-swift-native-view-producers-retry.log`; the dependent
full release refresh is recorded in
`/tmp/jed-swift-native-release-all749-view-second.log` with the six earlier host
assertions, now resolved. The third release run in
`/tmp/jed-swift-native-release-all749-handoff-third.log` retained one old-pin
assertion. The fourth release run passes all 509 native tests and all 749 SQL
files in `/tmp/jed-swift-native-release-all749-eof-fourth.log`.
The prior release correctness run is recorded
in `/tmp/jed-swift-native-release-all749-first.log`, and refreshed interchange
in `/tmp/jed-swift-native-interop-full-second.log`. The corrected full release accounting report is
`/tmp/jed-swift-query-memory-20261106-1996511-bxwolu/report.json` (`complete_matrix=true`, zero
failures). Its XCTest hash is `944718bbc37e4af7e56f13d13981611503dab04f1af454caaf9f0270fbdea4da`
and resource digest is `0568c671be49ea6e8e3ff902f41b48a110b612ab428fd105a8bbd67cabb5ba10`.
The log is `/tmp/jed-swift-native-query-memory-release-gist-third.log`.
Earlier complete debug/release reports retain the 376 GiST-load differences,
and the stale-manifest/cache attempt remains a failed run; neither is erased
by the correction. Historical debug evidence:
`/tmp/jed-swift-query-memory-20261106-939537-ml7e92/report.json`.
The prior generated sweep is retained at
`/tmp/jed-swift-sim-sweep-20261106-980401-9e3v3g/summary.json`; the final generated
release refresh also passes at `/tmp/jed-swift-sim-sweep-20261106-2104641-awof5y/summary.json`.

The Rust-pages supplement uses `--pages-owner rust` with `JED_PROCESS_CORES=swift-native`
and `JED_PROCESS_FILTER=_stream`; both selected scenarios pass in
`/tmp/jed-swift-native-process-rust-stream.log`. Immutable actor hashes are Swift
`8265ffa840d4f8fe4bb8bee024712097716970604ed2c398d73f77d749e5b19e` and Rust pages
`e909fdfd8551c78798c2647928c5cfc0e71e22f624d994c2fcd1cf1f7dc55399`.

## 1. Audited sources and interpretation

The normative contracts are [api.md](api.md), [session.md](session.md),
[environment.md](environment.md), [streaming.md](streaming.md),
[memory.md](memory.md), [hosts.md](hosts.md), [attached-databases.md](attached-databases.md),
[page-service.md](page-service.md), [concurrency-testing.md](concurrency-testing.md),
and [simulation-testing.md](simulation-testing.md), together with the shared data
and suites they name. The actual API audit used:

- Rust: `impl/rust/src/lib.rs`, `api.rs`, `shared.rs`, `file.rs`,
  `ergonomic.rs`, `environment.rs`, `extension.rs`, `cancel.rs`, `seam.rs`,
  `integrity.rs`, `backup.rs`, and `src/bin/{conformance,process_actor,sim}.rs`.
- Go: `impl/go/{shared,api,ergonomic,file,engine,environment,extension,privileges,
  seam,serialize,backup,integrity,split,pageservice_remote_unix}.go` and
  `cmd/{conformance,process_actor}`. `jed.go` is not the complete public API index.
- TypeScript: `impl/ts/src/{api,shared,ergonomic,tooling,environment,extension}.ts`,
  `async/`, `browser/`, `pageservice/`, and `bin/`. Its application API is async
  and worker-owned; its synchronous API is internal tooling. Swift must preserve
  the semantic and lifetime contracts, not reproduce JavaScript worker packaging.
- Host-only evidence: Rust `tests/`, Go `*_test.go`, and TypeScript `tests/`.
  These contain byte, cancellation, resource, lifetime and API behavior that SQL
  corpus success alone does not exercise.
- Integration: `mise.toml`, `mise-tasks/conformance/{all,query_memory,encrypted}`,
  `scripts/{gate,process_conformance,sim,sim_gen}.rb`,
  `scripts/lib/query_memory_peaks.rb`, and the shared corpus.

Some old narrative sections describe earlier implementation stages. Resolve
status using the baseline code, current subsystem contract and shared tests.
For example, the baseline has encrypted files and format v50; historical
statements about deferred encryption or whole-image commits do not narrow it.

## 2. Embedding operations

The evidence column names the existing test family, generally present as
`impl/rust/tests/<family>.rs`, `impl/go/<family>_test.go`, and
`impl/ts/tests/<family>.test.ts`. Exceptions are named explicitly.

| ID | Required operation and observable contract | Baseline evidence | Swift status |
|---|---|---|---|
| H01 | `create(options)` supports absent path as memory and present path as new durable file; `open(path, options)` opens only an existing file. Validate page size, preserve duplicate/missing/corrupt/I/O errors, and never clobber a pre-existing path. | api.md §2.1; `api`, `fileformat_golden` | Implemented: `DatabaseFiles`, `FileDatabaseHost`; FileHost/StorageDomain/Automatic boundary tests. Included in the passing fourth release refresh of all 509 native tests; exact-account and mixed-process/saved-simulation gates also pass; the final generated release sweep also passes. |
| H02 | Open/create options preserve defaults and distinguish absent from explicit zero: cache, work memory, read-only, skip-fsync testing knob, synchronous mode, flush interval, locking, election timeout, owner limits/preference, environment, storage allowance, credential and scratch encryption. File page size is immutable. | Rust `file.rs`/`shared.rs`; Go `file.go`/`shared.go`; `api`, `file_sessions`, `encryption` | Implemented: `CreateOptions`, `OpenOptions`, `SessionOptions`, `PageServiceOptions`; FileHost, SessionSettingsAccess, BatchFlush, CredentialWriter tests. Included in the passing fourth release refresh of all 509 native tests; exact-account and mixed-process/saved-simulation gates also pass; the final generated release sweep also passes. |
| H03 | Database owns storage identity; convenience calls mint fresh sessions, and explicit sessions retain semantic settings and safety envelopes. Database and environment lifetimes outlive temporary host references. Close is idempotent where specified and discards pending writes, while flushing already-committed relaxed durability batches as required. | api.md §§1–2; session.md §1 update; `environment`, `session`, `file_sessions`, `shared` | Implemented: `Database`, `Session`, retained `DomainReadBundle`; Database/SessionCursor/SessionAdmission and file close/promotion tests. Included in the passing fourth release refresh of all 509 native tests; exact-account and mixed-process/saved-simulation gates also pass; the final generated release sweep also passes. |
| H04 | The total `query` operation returns a cursor for every statement; DDL, transaction control and DML without RETURNING have no columns. Command count is optional, with no-count distinct from zero. `execute` drains the same cursor and returns its command result. | api.md §§1,4; `api` tests `QueryOnNonQueryStatementIsTotal` and `RowsAffectedReportsDMLCounts` | Implemented: `Rows`, `CommandTag`, optional affected count; RowsMetadata tests and whole-file query adapter. All 749 SQL files and all 509 native tests pass in the fourth release refresh. |
| H05 | Explicit begin/read-write/read-only, commit, rollback, scoped view/update, autocommit and SQL transaction control use one state machine. Expose idle/open/failed status; block errors poison the transaction, including errors discovered while pulling rows; savepoint rollback restores the required state. Scoped callbacks cannot terminate the transaction underneath their owner. | api.md §2.3–2.5; session.md §2.2; `api`, `session`; shared transaction/savepoint/concurrency corpus | Implemented: Session state machine, `DatabaseScopes`, savepoint/failed-block hooks; seven shared concurrency schedules and transaction corpus; scoped/session boundary tests. Included in the passing fourth release refresh of all 509 native tests; exact-account and mixed-process/saved-simulation gates also pass; the final generated release sweep also passes. |
| H06 | Prepared statement parse/resolve/bind behavior and parameter count/types; execution on database/session/transaction; relation-scoped cache invalidation and exact binding-sensitive plans. Host NULL, integer, exact decimal, byte, date/time and recursive values obey SQL static typing. Do not substitute SQL interpolation for binding. | api.md §2.4; estimator.md §6.1; `api`, `ergonomic`; parameter and planning corpus | Implemented; focused 10 prepared and six planning-probe tests pass. Included in the passing fourth release native/SQL refresh; corrected full shared GiST accounting also passes. |
| H07 | Cursor metadata is available without materializing all rows; values are lossless. Cost advances as rows are consumed, is final only after full drain, and remains inspectable after close. End-of-stream and terminal error remain distinguishable. Closing a prefix cancels remaining work and releases reader pins and retained allocations. | streaming.md §§4–6; `streaming`, `api`, `query_memory`, `lifetime_cost` | Verified by direct-cursor admission and escaped-file lifetime tests, native batching, the full exact-account comparison and native/service gates. Each has its own contract and actor evidence in VERIFICATION.md. |
| H08 | Cursor captures the snapshot and execution settings required by streaming.md, keeps its pin after its session finishes when required, and does not accidentally read a newer commit. One cursor's closure must not release another cursor's or transaction's pin. Read-only cursors do not take the writer gate. | `streaming`, `shared`; `concurrency/{snapshot_isolation,watermark_refcount,storage_watermark}.test` | Implemented: `SnapshotPin`, `DomainReadBundle`, `SessionTransaction`; SessionCursor, DatabaseCore, PageServiceClient and shared watermark schedules. Included in the passing fourth release refresh of all 509 native tests; exact-account and mixed-process/saved-simulation gates also pass; the final generated release sweep also passes. |
| H09 | Idiomatic value conversions and typed row access preserve NULL, widths, narrowing checks, exact i64/decimal, copied bytes and open-type identity. Single-row and collecting helpers propagate cursor errors and meter engine-owned collection. Swift protocols/helpers may replace reflection-specific Go and JavaScript spellings. | api.md §11; Rust `ergonomic.rs`; Go `ergonomic.go`; `ergonomic`, `query_memory` | Implemented: `ValueConversion`, `Row`, `Ergonomic`; six ergonomic host tests including finite collector/streaming budgets. Included in the passing fourth release refresh of all 509 native tests; exact-account and mixed-process/saved-simulation gates also pass; the final generated release sweep also passes. |
| H10 | Cancellation can be requested independently from the running operation; pre-cancel, execution checkpoints, zero-row loops, batch kernels and mid-cursor cancellation return `57014`, clean up pins, preserve cost limits and restore session cancellation context. Canceled statements do not publish partial writes. | api.md §11.4; `cancellation`; Go `TestCancellationWithoutRowEmission`, `TestCancellationSamplingIndependentOfCost`, `TestCancellationRestoresSession` | Implemented: independent atomic CancellationToken, work checkpoints, cleanup; Resource, HostBatch, Session and threaded writer-admission tests. Included in the passing fourth release refresh of all 509 native tests; exact-account and mixed-process/saved-simulation gates also pass; the final generated release sweep also passes. |
| H11 | Structured errors expose SQLSTATE and the implemented detail/hint/position/object fields; host conversion errors retain their specified category. Rendering errors also propagate. Swift traps, fatal preconditions, native errno text and arbitrary host exceptions must not replace contract errors for malformed SQL/files. | api.md §7; error-fields.md; shared registry and error-field corpus | Direct acceptance passes: all 407 shared Detail/Position/Hint cases, 10 object-field cases and 20 runtime metadata probes. Final four-case RETURNING-order follow-up also passes in the 42-test diagnostic/GIN/deep-view gate. The fourth release refresh also passes these diagnostics and all native host tests. Evidence: ErrorFieldCorpusTests and RuntimeTypeDiagnosticsTests. |
| H12 | Standalone statement splitting returns exact spans/offsets across quotes, dollar quotes and nested comments. Script execution on an idle session wraps the run in one implicit transaction; on an open session it joins the existing transaction. It rejects in-script transaction/savepoint control, stops on error and returns an O(1) count/cost summary. A host-managed splitter loop instead owns its transaction policy. | session.md §4; api.md §1; Rust `split.rs`; Go `split.go`; `split`, `split_shape`, `session` | Implemented: `ScriptSplitter`, `ScriptExecution`; LexerBoundary and ScriptExecutionBoundary host tests. Included in the passing fourth release refresh of all 509 native tests; exact-account and mixed-process/saved-simulation gates also pass; the final generated release sweep also passes. |
| H13 | Table SELECT/INSERT/UPDATE/DELETE and function EXECUTE grants/revokes, default table privileges and persistent/temporary DDL gates apply to resolved SQL objects, prepared statements, subqueries, views and host functions. They are session capabilities, not an auth catalog. | session.md §5.3; `privileges`, `host_functions`; session/capability corpus | Implemented: `StatementPrivileges`, `PrivilegePreflight`, SessionSettings capabilities; privilege/session corpus and source-view/CTE tests. Included in the passing fourth release refresh of all 509 native tests; exact-account and mixed-process/saved-simulation gates also pass; the final generated release sweep also passes. |
| H14 | Session variables, reset, timezone and default collation settings use the contract's scopes and validation. Rendering and timestamp input use the originating session zone. TEMP objects remain private, with ON COMMIT behavior and independent DDL capability. | `session`, `timestamptz_session_input`, `timestamptz_session_render`, `temp_on_commit`; temp/session corpus | Implemented: `SessionSettingsAccess`, `TemporaryDomain`, per-session routing/rendering; TemporarySession, TempType, SessionSettingsAccess and frozen session/TEMP corpus. Included in the passing fourth release refresh of all 509 native tests; exact-account and mixed-process/saved-simulation gates also pass; the final generated release sweep also passes. |
| H15 | Immutable environment builder loads Unicode/timezone bytes, registers functions and injects clock/random providers; derived builders and independent environments do not mutate each other or become process-global. Loaded-collation/timezone introspection is deterministic. | environment.md; `environment`, `collation_host`, `timezone`, `seam`, `date_clock` | Implemented: Environment builder, Unicode/timezone loaders, introspection; Environment, SessionSettingsAccess, StatementServices and shared vector tests. Included in the passing fourth release refresh of all 509 native tests; exact-account and mixed-process/saved-simulation gates also pass; the final generated release sweep also passes. |
| H16 | Host scalar and batch kernels specify argument/result types, NULL behavior, volatility, cross-core declaration, cost, component ID and semantic version. Validate registration and every result/batch shape, preserve scalar/batch semantics, reject uncosted functions under finite budgets, and fail closed on unavailable persisted components. | api.md §12; extensibility.md; Rust/Go/TS `extension`; `host_functions`; host-function byte fixtures | Implemented: HostFunction scalar/batch registration, HostReadAhead/replay, fixed/multirow shapes; Environment and HostBatch* host tests. Included in the passing fourth release refresh of all 509 native tests; exact-account and mixed-process/saved-simulation gates also pass; the final generated release sweep also passes. |
| H17 | Clock/entropy seams produce shared seeded/manual/advancing vectors, correct statement versus wall-clock observations and transactional UUID behavior. Encryption key entropy is separate; insecure deterministic key entropy is visibly test-only and cannot silently inherit SQL seeded randomness. | entropy.md; environment.md; encryption.md; `seam`, `date_clock`, `encryption`; `spec/encoding/prng.toml` | Implemented: sanctioned random/clock providers and independent key source; StatementServices, Encryption, PRNG vectors and deterministic simulation. Included in the passing fourth release refresh of all 509 native tests; exact-account and mixed-process/saved-simulation gates also pass; the final generated release sweep also passes. |

## 3. Resource and storage-facing host operations

| ID | Required operation and observable contract | Baseline evidence | Swift status |
|---|---|---|---|
| R01 | Per-statement `max_cost` and session `lifetime_max_cost` charge the exact schedule, including cursor pulls, internal meters and errors. Admission after lifetime exhaustion and equality-at-limit aborts must match. A timeout is not a deterministic cost meter. | cost.md; `spec/cost/schedule.toml`; `cost_limit`, `lifetime_cost`, `operator_cost`, `overflow_cost`, `compressed_cost` | Implemented: Resource/Database/CursorAdmission tests and exact SQL costs. Charge-at-open/check-on-pull ambiguity resolved from all3 probes. Included in the passing fourth release refresh of all 509 native tests; exact-account and mixed-process/saved-simulation gates also pass; the final generated release sweep also passes. |
| R02 | SQL input, parser/expression nesting, parameter index, identifier, view expansion, regex program and JSON/composite depth limits reject before unsafe recursion or excessive allocation. UTF-8 byte length must not become Swift grapheme count. | api.md §8; cost.md §7; `suites/resource/` | Implemented: unchanged depth/UTF8 limits with parser/resolver/view stack repairs; all 749 frozen SQL files and unchanged deep-view fixtures pass in debug and release. Direct diagnostics pass under H11; the fourth release refresh passes all native host tests. |
| R03 | Finite default `max_scalar_bytes` is a cumulative logical statement allowance with frozen cursor/internal meter inheritance; covered amplification/output/scratch is checked before allocation and reports `54P04`. Non-positive values restore the default, not unlimited. | memory.md; `scalar_memory`; scalar resource corpus | Verified: Resource/FrozenResource and scalar kernel tests cover default/frozen cursor/inner meter semantics. Ordinary assertions and exact-account comparisons pass in the complete corrected V04 matrix; prior differences remain under R04. |
| R04 | `max_query_memory_bytes` charges live Q1 row buffers/engine collectors, Q2 operator/spill state and Q3 pending record versions on the shared logical schedule; reserve before allocating, release exactly once, and preserve pending holds between statements. Report `54P05`, compare exact peaks, detect under-release/over-release. | memory.md; `spec/cost/memory_sizes.toml`; `query_memory`, `memory_sizes`; resource corpus and query-memory peak runner | Verified: corrected full release V04 passes 241,412 measurements with zero peak/storage differences, complete_matrix=true and all 12 runs complete. Earlier 376 GiST-load differences and the cache incident remain historical evidence; see RESOURCE_ACCEPTANCE. |
| R05 | File cache is evict-only under `cache_bytes`, with at least one leaf, no semantic/cost change and no query failure. `work_mem` drives bounded external sort/hash/group/distinct spill; a query-account refusal spills earlier when possible. Real disk scratch, failure cleanup, ordered replay and identical cost are required. | pager.md; spill.md; `spill`, `blocking_spill`, `query_memory`; forced-spill corpus | Implemented and verified: bounded native/sealed spools, hash stores, sort/joins/aggregate/dedup; Spill*, ExternalSort and BlockingOperator tests. Corrected disk-spill passes all 19,438 records per core with exact peaks/storage; forced TEMP also passes. |
| R06 | TEMP residency (`temp_buffers`) differs from retained temporary storage (`max_temp_storage_bytes`, `54P03`); spill preserves values/errors/cost and ON COMMIT behavior. Encrypted handles seal scratch under the scratch policy. | temp-tables.md; encryption.md §6; Go/TS `temp_spill`, `temp_blockstore`, `temp_on_commit`; resource temp corpus | First complete forced-TEMP gate passed:28 files,1255 records,26 actual scratch files. TemporaryDomain and temp host tests cover lifecycle. Included in the passing fourth release refresh of all 509 native tests; exact-account and mixed-process/saved-simulation gates also pass; the final generated release sweep also passes. |
| R07 | Database/attachment-owned `max_storage_bytes` checks memory committed pages or file live pages before writes, with exact persisted live-page deltas and `54P06`. Setter and `storage_bytes(name)` do not mistake physical file size, dead pages or free list for live storage. | memory.md §8; `storage_memory`; resource/storage and process/storage_limit corpus | Verified: StorageDomain/DomainCommitGroup no-write admission and allocator retention tests; process storage_limit passed. Committed-storage bytes and query peaks agree in the corrected complete V04 matrix; prior differences remain under R04. |
| S01 | Attach/detach memory and file databases with exact name resolution, read-only flags, per-file credential/storage settings and one environment. Capture all participating roots consistently; allow only one durable written domain per transaction; take writer gates in real-path byte order. | attached-databases.md; `attach`; shared attach/concurrency/process suites | Implemented: DatabaseDomains, per-domain roots/group commit and target validation; shared attachment/concurrency/process corpus and domain host tests. Included in the passing fourth release refresh of all 509 native tests; exact-account and mixed-process/saved-simulation gates also pass; the final generated release sweep also passes. |
| S02 | Compact a selected database/attachment through the storage seam, with correct gate/pin exclusions, same rows and cost, and durable rename/directory-sync ordering. Close/reopen and mid-operation failure do not publish a partial replacement. | api.md §2.6; `compact`, `reclaim_compaction`; process/compact and sim compaction traces | Implemented: DatabaseMaintenance/DomainRebuilder, attachment routing; Maintenance/AttachmentMaintenance tests and simulation/process replacement histories. Included in the passing fourth release refresh of all 509 native tests; exact-account and mixed-process/saved-simulation gates also pass; the final generated release sweep also passes. |
| S03 | Integrity check produces the shared finding kinds/order and truncation/completeness behavior across structural, key/index and optional constraint checks; supports cancellation and does not mutate the database. Unavailable host definitions are reported/skipped only as specified. | api.md §2.7; integrity-check.md; Rust `integrity_check`; Go/TS `integrity`; `spec/integrity/` fixtures | Implemented and independently reviewed. Focused gate passes all 24 frozen damaged images/50 exact reports,108 opening goldens/4 exact refusals, local host and real encrypted/plaintext page-client API tests. CLI passes50 reports+11 boundary probes. The generated integrity-hook refresh passes 176 traces per core and independently checks 7,049 Swift images. The fourth release refresh passes all 509 native tests; fresh process/saved-simulation gates pass; the final generated release sweep also passes. |
| S04 | Serialize committed `main` or a named attachment into canonical clean bytes while pinning a snapshot without holding the writer gate; exclude uncommitted/temp state. Deserialize copies input, preserves page size/version and recovery selection, creates independent memory storage, and rejects encrypted input. Encrypted serialization is unsupported (`0A000`) rather than plaintext export. | api.md §2.8; `serialize`; Go `integrity_deserialize_test.go` | Implemented: DomainImage/DatabaseSerialization/DatabaseFiles; Serialization/FileHost tests and exact frozen image rebuilds. Included in the passing fourth release refresh of all 509 native tests; exact-account and mixed-process/saved-simulation gates also pass; the final generated release sweep also passes. |
| S05 | Consistent backup through the implemented page-service/tooling seam, compact-copy versus page-identical mode, destination-not-exists behavior, concurrent readers/writers and encrypted current-key behavior. Baseline `backup_for_testing`/`BackupForTesting` is tooling API, not a promise of a new general Swift application API. | page-service.md §9; `backup`; process/backup*.process.toml; simulation service-backup regressions | Implemented: bounded native/service backup, public tooling SPI; BackupBoundary and31-process/mixed-role byte comparisons passed on prior actor. Included in the passing fourth release refresh of all 509 native tests; exact-account and mixed-process/saved-simulation gates also pass; the final generated release sweep also passes. |
| S06 | Encrypted create/open and attachments: raw key/passphrase credentials, bounded Argon2 parameters, missing/wrong/plaintext-credential errors, page authentication, nonce/replay binding, secret redaction and no accidental plaintext spill. Credentials are host input and never SQL literals/options. | encryption.md; encrypted file spec/vectors; `encryption`, Go `encryption_header`, TS `encryption_vectors` | Implemented: pinned swift-crypto/Argon2 utilities, native Swift encryption codec/policy; crypto/golden tests and full V05 encrypted comparison passed on prior actor. Included in the passing fourth release refresh of all 509 native tests; exact-account and mixed-process/saved-simulation gates also pass; the final generated release sweep also passes. |
| S07 | Credential change rewraps while preserving the data key; rekey/conversion rewrites live pages atomically. Exercise old/new credential selection after every crash boundary, service clients with held keys, attachment encryption and backup restoration. | encryption.md; `encryption`; encryption simulation profiles/regressions and encrypted process scenarios | Implemented: rewrap/rekey, attachment routing, held-key promotion and idle-client writer admission; CredentialWriter/HeldKey/Maintenance tests and176 simulation traces. Included in the passing fourth release refresh of all 509 native tests; exact-account and mixed-process/saved-simulation gates also pass; the final generated release sweep also passes. |
| S08 | Storage inspectors used by embedding/tooling expose version, page size/count, path, read-only, resident leaves and oldest live txid without changing storage behavior. Catalog inspectors required by invariant tools remain available to those tools even if kept internal to the Swift package. | Rust/Go `shared`; `api`, `shared`, `streaming`, golden/integrity tests | Implemented: Database/DatabaseStorageInfo inspectors and internal pin/catalog instrumentation; host/process/sim tools exercise actual metadata. Included in the passing fourth release refresh of all 509 native tests; exact-account and mixed-process/saved-simulation gates also pass; the final generated release sweep also passes. |

## 4. Linux host and concurrency

| ID | Required operation and observable contract | Baseline evidence | Swift status |
|---|---|---|---|
| L01 | Byte-addressed positioned read/write, size, data barrier and durable growth/truncation form the host seam. Handle short I/O and interruption, validate offsets, isolate descriptor/pointer code, and preserve the difference between data sync, allocation metadata sync and parent-directory sync. Hosts do not select page layout or commit ordering. | hosts.md §2; `blockstore`, file hosts; validated-cow.md | Implemented: UnixBlockStore/FileOperations/CJedHost narrow OS boundary; BlockStore/StorageDomain/Scratch tests and crash simulation. Included in the passing fourth release refresh of all 509 native tests; exact-account and mixed-process/saved-simulation gates also pass; the final generated release sweep also passes. |
| L02 | Database sharing allows concurrent readers and one writer with immutable pinned roots, independent sessions and reader watermark refcounts; commit publication is short. Real threaded writer-gate blocking and cancellation/timeouts do not deadlock or release someone else's ownership. | concurrency-testing.md Layers 1–3; shared concurrency corpus; Rust/Go threaded conformance; `shared`, `file_sessions` | Implemented: DatabaseCore/WriterGate and root-domain boundary; seven shared schedules, threaded admission tests and complete Layer3 stress passed. Included in the passing fourth release refresh of all 509 native tests; exact-account and mixed-process/saved-simulation gates also pass; the final generated release sweep also passes. |
| L03 | Stable `protocol-v2` lock bundle uses crash-clean OS locks; `auto`, `exclusive`, `none` and ownership prefer/yield/never obey election and timeout rules. Unsupported hosts fail closed. Owner foreground operations make zero coordination syscalls and zero per-transaction meta reads. | locking.md; page-service.md; process corpus; lock simulation | Implemented: native protocol-v2 claims and automatic election; Automatic/Claim/Role tests and refreshed 496 ordered process pairings pass with exact final bytes. Fourth release refresh passes all 509 native tests, including the old-pin boundary; the fresh full release mixed-process run also passes all 496 pairings. |
| L04 | Owner/client page protocol matches message encodings, credentials and bounded framing. Owner alone accesses file bytes/reclaims pages; client readers pin remotely and commits send deltas to the owner. Gate acquisition, dirty-domain/no-op commits, resource limits and durability batches remain identical through the service. | `spec/pageservice/{messages,fixtures}.toml`; `pageservice_codec`; process service/attachment/encrypted scenarios | Implemented: PageService codec/owner/client/delta; all message vectors and native tests; V06 refreshed full six directions passed 116,628 records. Included in the passing fourth release refresh of all 509 native tests; exact-account and mixed-process/saved-simulation gates also pass; the final generated release sweep also passes. |
| L05 | Handoff, graceful shutdown/drain, owner crash, client reconnect/promotion, revoked idle transactions, max connections, read-only-owner yielding and lost commit acknowledgment resolution use the real service path. Transactions crossing owner changes fail with the specified state; commit ambiguity is not hidden by blind retry. | page-service.md; process corpus; Go/TS page-service owner/remote/close tests; service simulation profiles | Implemented: role epochs, poison tenure, reconnect/yield/drain/history; refreshed process and saved simulation gates pass. The prior generated simulation refresh passes all 176 traces per core. Handoff/client and old-pin assertions pass in the fourth release refresh; fresh mixed-process and saved simulation gates pass; the final generated release sweep passes 176 per core/459 comparisons/7,049 checked Swift images. |
| L06 | Remote stream client opens a host-supplied command once per connection, performs the same protocol on stdin/stdout, uses client read-ahead without changing cost, cleans up child processes and exposes I/O failure. Linux coverage uses the local process transport, so it does not require an external SSH account. The baseline serving CLI role is Rust-specific. | api.md §2.1; Rust `shared.rs::open_remote`, Go `pageservice_remote_unix.go`, TS async Node API; `remote_stream`/`pageservice_remote`; process/service_stream and encrypted_stream | Implemented: native stream transport/owner relay, pidfd child lifetime and bounded read-ahead; PageTransport and plaintext/encrypted process stream scenarios passed. Included in the passing fourth release refresh of all 509 native tests; exact-account and mixed-process/saved-simulation gates also pass; the final generated release sweep also passes. |
| L07 | Deterministic simulated byte device, filesystem, locks, network, clock/entropy and spill host run the same storage/service algorithms as Linux hosts. No simulated-only commit/recovery implementation. Content-keyed faults, durability floors, reader pins, structure checks and image generation follow simulation-testing.md. | Rust `src/sim/`; Go `sim_*_test.go`; TS `src/sim/`; shared profiles/regressions | Implemented: deterministic host/file/network/fault seams, same native policies; refreshed 18 regressions pass against all three peers with 54 transcript comparisons and 153 independently checked images. The generated integrity-hook refresh passes all 176 traces per core, 459 transcript comparisons and 7,049 checked Swift images. |

## 5. Proposed Swift interfaces and ownership boundaries

These are the original implementation recommendations. Their illustrative names
are not an alternate public API: the package README and source describe the frozen
Swift interfaces. Acceptance is established by the independent rows and checks.

Use a SwiftPM library product `Jed` with handwritten modules separated by responsibility:
`Value`/`SQLType`/exact arithmetic; lexer/parser/resolver; estimator/planner;
execution/cursors; page codecs/trees/catalog/pager; transaction/shared database;
session/environment/extensions; and Linux OS/crypto adapters. Keep command-line
conformance, process actor, simulation, fixtures and benchmark products separate
from the application product. Do not import a subprocess SQL adapter into `Jed`.

Suggested public currency is a lossless `Value` enum with explicit null and
scalar/container cases, a recursive `SQLType`, and `EngineError: Error` containing
the registered state and structured fields. Strings are UTF-8 contract data:
Swift's Unicode-canonical equality and grapheme iteration must not silently
replace jed byte/code-point operations. Decimal coefficients use handwritten
arbitrary precision arithmetic; Foundation Decimal/Double is not an exact stand-in.

`Database.create(_:)`, `.open(_:options:)`, `.deserialize(_:options:)` own shared
storage; `session(_:)`, `readSession()` and `writeSession()` create configured
contexts. `Session.query(_:parameters:cancellation:)` returns `Rows`, and
`execute` drains it. `prepare`, `queryPrepared`, `executePrepared`, `begin`,
`commit`, `rollback`, `view`, `update`, `executeScript`, settings and capability
methods map H04–H16. `Rows.next() throws -> [Value]?` can express terminal errors
idiomatically; expose names, types, accrued cost, optional affected rows and
explicit idempotent `close`. Do not use a nonthrowing `Sequence` that loses
iteration failures. Typed decoding/collection helpers remain wrappers over Rows.

A database may be safely shared only after its locks/root publication and host
closures are audited for Swift concurrency. A session is a serialized mutable
execution context; a plain `Sendable` declaration is not synchronization. A
separately synchronized `CancellationToken` must remain callable while execution
is blocked or computing. Swift actors alone do not make a synchronous kernel
cancelable from the same actor. Avoid crossing an async boundary with unprotected
mutable session or cursor state.

The internal `BlockStore` protocol is byte-addressed positioned I/O, `sync`,
`size`, and `setSize`; pager allocation/commit policy stays above it. Pair it
with file lifecycle, spill, lock and transport seams that simulated hosts can
implement. OS pointer scopes must be short, checked and confined to these
adapters. Crypto uses the separately approved/pinned libraries behind safe
digest/AEAD/KDF wrappers. Immutable `Environment` contents hold Unicode/timezone
bundles and host function registry; SQL never receives a path-loading escape hatch.

## 6. Opt-in runner and verification matrix

The `swift-native:` task names below describe the acceptance lanes; implemented
commands and aliases are recorded in VERIFICATION and the individual tool plans. Their
existence is not verification. Existing default task core lists stay unchanged;
adding an explicitly selected runner registration must not add Swift to default
`test`, `ci`, `conformance`, `sim:*` or benchmark dependencies. The completed
experiment is frozen at its recorded implementation revision and semantic baseline;
later supported-core features do not silently expand its obligations.

Historical row narratives retain their original actors and dates; current
structural revalidation is mapped separately in VERIFICATION.md.

| ID | Explicit Swift task / mode | Required evidence and integration | Status |
|---|---|---|---|
| V01 | `swift-native:build`, `swift-native:test` | Reproducible pinned SwiftPM dependency graph; Linux debug and release builds; host-only tests for H/R/S/L rows; no unapproved dependency or system crypto installation requirement. | Earlier Linux debug/release builds retain their own actors. The current release passes all 543 native tests including all 749 SQL files with the pinned dependency graph. Actor-specific final verification is in VERIFICATION.md; measured results are in BENCHMARKS.md. |
| V02 | `swift-native:conformance` | All baseline SQL in each applicable memory/disk mode, plus the seven canonical in-memory concurrency schedules (the shared disk runner excludes schedule files); baseline capability coverage ledger; every missing capability and file counted as a failure for completion rather than silently skipped. | Sixth full SQL passes 749 files / 21,597 records / 39,148 cells. Earlier release suite: 505 native tests pass, including the corrected GIN boundary and all 749 SQL files. The SRF/JSON_TABLE view-source correction outside frozen assertions passes 41 records/23 cells in both memory and disk plus host capability checks; fourth release refresh passes all 509 native tests and all 749 SQL files. |
| V03 | `swift-native:spill` | All applicable disk corpus with `JED_CONFORMANCE_WORK_MEM=256`; temporary spill modes as baseline tasks exercise them; rows/errors/cost unchanged. | Complete forced-TEMP passes 28 files/1,255 records/26 actual scratch files. Corrected full work_mem=256 disk-spill comparison passes all 19,438 records per core with ordinary assertions and exact accounting. |
| V04 | `swift-native:query-memory` | Huge-but-active account and per-record peak/main-storage TSVs for memory, disk and disk-spill; compare with supported cores using the same fixture exception as `scripts/lib/query_memory_peaks.rb`; assert complete record-key coverage separately because that comparator alone tolerates absent records. | Corrected full release passes 241,412 measurements, zero failures, complete_matrix=true and all 12 runs: 21,477 memory and 19,438 disk/disk-spill records per core. No comparator exception added. Report: /tmp/jed-swift-query-memory-20261106-1996511-bxwolu/report.json. Earlier 376 differences and the failed cached-manifest run remain preserved. |
| V05 | `swift-native:encrypted` | Encrypted disk corpus; rows/errors unchanged; costs and EXPLAIN recorded with the baseline transcript format and compared to encrypted supported cores. Only baseline documented encrypted-mode exclusions apply. | Full four-core encrypted comparison and the postoptimization refresh pass 77,612 records with exact retained transcripts. View-source memory/disk checks have their own evidence; final actors are recorded in VERIFICATION.md. |
| V06 | `swift-native:service` | Shared SQL corpus runs through a real supported-core owner and Swift client, and Swift owner with supported clients. A fake local service or same-process fallback does not exercise L04. | First complete six-direction service corpus passed116628records, zero failures. Four canonical fixture-local exceptions are explicitly counted. Final actor refresh passes all six directions and 116,628 records. |
| V07 | `swift-native:process` | Register Swift actor only for explicit `JED_PROCESS_CORES`; all applicable process scenarios across Swift↔Rust/Go/Node and Swift↔Swift, in both role orders; image checker and final byte comparisons retained. Verify pinned Rust-only serving roles separately. | The prior matrix passes 31 scenarios × 16 ordered pairings = 496 with exact final bytes. Its Swift/Go/Node clients use Swift pages; Rust clients self-launch Rust pages. Swift-client → Rust-pages remote stream coverage now passes service_stream and encrypted_stream (2/2), recorded in /tmp/jed-swift-native-process-rust-stream.log. The fresh full release 496-pair refresh also passes; these two cases supplement it. |
| V08 | `swift-native:concurrency` | Deterministic stepped corpus plus genuine threaded gate waits and reader/writer stress with invariant/final-checksum checks. Swift Thread Sanitizer is supplemental where the installed platform supports it, never a replacement for scheduled corpus or process tests. | Seven stepped schedules plus real-thread Layer3 stress passed:20000invariants5000writertransactions, final checksum f3e75496a3111181. Included in the passing fourth release refresh of all 509 native tests; exact-account and mixed-process/saved-simulation gates also pass; the final generated release sweep also passes. |
| V09 | `swift-native:sim:regress`, `:sweep`, `:replay` | Same `JED_SIM_IN`, `JED_SIM_OUT`, `JED_SIM_RESULTS`, optional `JED_SIM_IMAGES` protocol as `scripts/sim.rb`; complete shared regressions and profile sweep. Compare transcripts/images when required; per-core read-error/enospc/spill-error/corruption traces retain each core's invariant checks. | Refreshed saved regressions pass all 18 traces, 54 transcript comparisons and 153 independently checked images. The generated integrity-hook refresh also passes all 176 traces per core, 459 comparisons and 7,049 checked Swift images. |
| V10 | `swift-native:fixtures`, `swift-native:interop` | Every baseline key/value/compression/crypto/page-service fixture and file golden; Swift writes and supported cores read, supported cores write and Swift reads; mutations continue after cross-core reopen. Include encrypted keys/passphrases, attachments, recovery and host-function requirements. | Refreshed full 112-image interchange passes all 2,691 phases; shared vectors pass. Separate24 integrity images pass50 exact API and CLI reports; golden integrity and remote-client checks pass. |
| V11 | `swift-native:oracle` | Relevant baseline PostgreSQL oracle checks through checkout mise environment, with declared oracle profile and overrides unchanged. PostgreSQL-comparable behavior only; host and file invariants use shared tests/fixtures. | Relevant oracle profile/corpus probes passed and retained in DEFAULT_ISOLATION. Final shared-correction oracle refresh only where changes warrant it. |
| V12 | `swift-native:verify-defaults` | Existing default verification remains healthy without Swift installation or dependency resolution. Inspection must show unchanged default core/dependency lists; run applicable default gates and record infrastructure blockers distinctly. | Default verify passes 17 checks; earlier CI passed 18/18 after local-only shared fix 05a4e59c. The first shared-GiST attempt failed 16/18 because of formatting and a stale private Rust manifest path; the canonical retry passes 18/18 (/tmp/jed-swift-native-default-ci-gist-canonical-retry.log). Default core/dependency lists remain unchanged. |
| V13 | `swift-native:bench` and comparison task | Correctness first, then preserved release baseline and measured optimization pass with supported-core matching workload/data/durability and checksums; record CPU, OS, Swift version, flags, warmup, samples, commands and data paths. No speed claim from SQL subset or debug/release mismatch. | Original `3db8fc2` evidence: complete release baseline and after-optimization runs each cover 143 workloads × four cores × three rounds, with matching checksums and row counts. Current diagnostics cover 22 peer, eight paired Swift and three Ruby workloads, each with three rounds per actor. BENCHMARKS.md records provenance, raw samples, results and limitations; no universal parity claim. |
| V14 | Local package/examples/reports | Runnable in-memory and durable examples, bound parameters, explicit cursor cleanup, transactions and cancellation; internal design/progress/verification/benchmark records. No website docs or publication. Apple targets remain unverified until actually built/tested. | Linux public examples pass; the final verification and benchmark reports record the frozen snapshot. Swift remains opt-in, with no package publication or promotion; Apple platforms are unverified and production delivery is undecided. |

## 7. Adapter correctness review checklist

The baseline conformance runner routes **both** `statement` and `query` records
through the real session `query` seam. Rust's `drain_query`, Go's `drainQuery`, and
TypeScript's `queryOutcome`/`drainOutcome` then pull to exhaustion, propagate
iteration failures, read final cost and release the cursor. Swift must use this
same operation, including DDL and DML with no columns. An eager executor entry
point could pass SQL answers while bypassing streaming cost, failed-block hooks
and reader liveness.

The runner's comparison buffers are host/tooling storage, outside the engine
query account in all three baseline runners. In contrast, engine-owned ergonomic
collectors reserve each retained row (`Rows::admit_collected`,
`Rows.admitCollected`, and the corresponding Go collector accounting). Do not
charge only Swift's test comparison arrays, omit Swift's engine collectors, or
replace actual buffered operators with host-owned materialization to escape the
account. Cursor/account lifetime and collector ownership require independent
review in addition to peak transcript parity.

Per-record directives must reset exactly as specified, with intentionally sticky
session state preserved: lifetime budgets, transactions and temp objects differ
from record-scoped limits/privileges/settings. Preserve parameter values as typed
host values; handle seed/clock, loaded bundles, fixture/reopen, attachments and
collation upgrades through their host APIs. Unknown directives, empty file
filters, unsupported selected modes and malformed inputs must fail visibly.

Render through the originating session while its timezone settings still apply;
an out-of-range rendering error is a record failure. Assert names and exact
static type names, not just stringified values. Apply only authored sort modes;
float tolerance is restricted to the corpus's designated `R` columns and
determinism ledger. Encryption changes page capacity and therefore legitimately
changes some costs/EXPLAIN output; compare its cross-core transcript instead of
silently dropping cost assertions or using plaintext expectations.

For concurrency, named sessions share one database, writer `blocks` opens really
queue in threaded mode, and read results keep their pin lifetimes. A sequential
interpreter of the scheduling language is useful but cannot validate real OS
locks, writer-gate parking or cancellation. For process tests, child actors run
the Swift engine and real page transport; for simulation, they run the same
storage algorithms over simulated seams. Neither runner may replay expected
answers, proxy SQL to a supported core, or reopen/serialize the entire database
between operations as a substitute for the required path.

Coverage reports must list files and records run, baseline-authorized mode
exclusions and remaining omissions. A permissive cross-core comparator can
compare only records present in each result, so verify membership/counts before
calling equal values or equal peaks a full pass. Preserve crash images,
transcripts, seeds and exact reproduction commands for failures.

### First-party native publication shim

`Sources/CJedHost/host.c` is a narrow ordinary-OS adapter under the experiment's
approved host-interface exception. Its initial publication operation is atomic no-replace rename, omitted by Swift's
Linux Glibc module. Linux calls `renameat2` with
`RENAME_NOREPLACE`; Darwin calls `renameatx_np` with `RENAME_EXCL`. Both inputs are
borrowed NUL-terminated paths for one synchronous call, no pointer or buffer is
retained, and the return is the OS status/errno. There is no third-party
library, SQL, codec, locking policy, or storage algorithm in this C
target. The stream adapter also uses a borrowed synchronous SIGPIPE-safe write
and ordinary `pidfd_spawnp`/close-on-exec pipe setup; cleanup signals and reaps
through the stable kernel process descriptor, never a potentially reused PID.
These retain no caller buffers and alter no process-wide signal handler. The file host retains the exclusive protocol-v2 bundle while publishing.
Unsupported hosts fail `0A000`; there is no link/unlink publication fallback that
could leave a hard-link crash window.

## 8. Linux host implementation milestones (2026-11-05)

The following historical milestones record the implementation sequence and the
acceptance still outstanding at each checkpoint. Current row statuses and
completed refreshes are above; these earlier focused checks alone did not
establish SQL, simulation or full host-surface completion.

| Scope | Native implementation and focused evidence | Remaining acceptance |
|---|---|---|
| H01–H03, file portions of H08 and R07 | `FileDatabaseHost`, `FileLockBundle`, `DatabaseCore`, `StorageDomain`, ordinary POSIX byte devices, reader pins and independent COW domains; file-host, coordinator and storage lifecycle tests pass. | Complete session/attachment matrix and cross-core mode runs. |
| Storage maintenance and encryption lifetime | `DatabaseMaintenance`, `DomainImage`, `DomainRebuilder`, `ScratchDomain`, key rewrap/rekey and no-clobber publication; ordinary and encrypted frozen images exercise the native page sink. | All canonical maintenance and process scenarios; fault coverage remains focused rather than exhaustive. |
| Linux automatic page service | Native codec, Unix transport, connection multiplexing, structural owner, client pager, automatic election, yielded ownership, lost-ack resolution, timeout/cancellation and close draining. Seven `PageServiceAutomaticTests` plus role/file checks passed. The full 31-scenario frozen Swift→Swift process run now passes, including attachment roles and native ordinary/encrypted streams. | Full mixed-core process matrix passed below; simulation remains. No Apple-platform claim. |
| Owner post-ack maintenance | Writer token spans reply, cap flush and one optional structural maintenance commit. The writer read pin is released after acknowledgement; registry epoch, reader set, committed identity and base version are rechecked before writes. Three `PageServiceMaintenanceTests` passed; independent review found no concrete durability/pin race. Frozen process reclamation and exact storage-limit page counts pass. | Mixed-core process verification passed below; simulation remains. |
| Internal backup | `Database.backup(_:form:)` streams page-identical and compacted forms with an ordinary pin retained through atomic destination publication. Five `BackupBoundaryTests` passed, covering all 87 ordinary frozen images (including six encrypted), retained relaxed generations, rewrapped key blocks, role revocation and cleanup before/after rename. Both canonical backup process scenarios pass. Independent review found no concrete defect. | Mixed-core process byte verification passed below; public API naming remains the shared deferred product decision. |
| Collation host metadata and migration | `DatabaseCollations` exposes exact-name defaults, referenced version verdicts and loaded content hashes. `CollationMigration` streams bounded row passes, atomically rebuilds affected table/ordered/GIN/GiST keys, clears affected statistics and advances pins only on success. All four `CollationMigrationBoundaryTests` passed, including the frozen skew fixture, retained old readers, no-op/default transaction behavior, atomic unsupported-index failure and shared GIN packing vectors. Independent review checked error ordering and publication. | Full collation corpus harness actions and end-to-end SQL acceptance remain separate. |

Focused checks are reproducible through
`mise run swift-native:test --filter 'PageServiceMaintenanceTests|BackupBoundaryTests'`
and `mise run swift-native:test --filter 'PageServiceAutomaticTests|PageServiceRoleTests|FileHostBoundaryTests'`.
Native socket/process checks require an execution environment that permits Unix
sockets. The recorded focused automatic run passed 16 tests; the backup run
passed five, with all 87 ordinary images asserted inside its fixture test.
`Tools/PAGE_SERVICE_PLAN.md` records the ownership, lock-order, epoch, structural
adoption, pin-watermark and publication invariants. All checkpoints reported in
this development session are local-only until explicitly described as pushed.
### Frozen real-process progress (2026-11-05)

The first complete 31-scenario native actor run executed every selected scenario:
22 passed and nine failed. Four failures were attachment API actions not yet
connected, two were stream transport opens not yet connected, two exposed a
close/election race, and one exposed the documented resident-root overflow
allocation exception. No scenario was skipped and no frozen expected value was
changed. The last three defects now have passing focused reruns of
`cow_stale_orphan`, `unchanged_commits`, and the complete `backup` scenario
through `mise run swift-native:process --no-build` with `JED_PROCESS_FILTER`.
The subsequent full run passed 28 scenarios and exposed three further issues:
attachment writes were rejected before the specified COMMIT phase; an idle
writer was unnecessarily asked to retain a revoked remote snapshot; and an
autocommit routing read pin prevented the owner's post-ack maintenance, producing
37 pages instead of the specified 33. Corrections preserve actual pinned reads
while using resident catalog metadata for provisional writer admission. All
three focused reruns passed.

The native grouped storage phase passed five host invariant tests, with the
root-leaf regression included in nine passing storage-domain tests. Attachment
actor routing and native stream client/serving code now pass their focused
integration checks. `Database.openRemote` supports an explicit byte-transport
factory and a process-command convenience; the opt-in actor task selects the
Swift serving process explicitly. The final complete frozen run on 2026-11-05,
`mise run swift-native:process --no-build`, exited zero with **31/31 PASS** for
Swift→Swift (`/tmp/jed-swift-native-process-all-rerun.log`). No selected scenario
was skipped or delegated to another SQL engine; ordinary and encrypted streams
used the Swift pages actor. This closes that native process mode only. Remaining
host acceptance includes cross-core pairings, simulation, named maintenance
verification, and other matrix rows without their own complete evidence.

The first four-core matrix (`JED_PROCESS_CORES=swift-native,rust,go,node`) ran
496 scenario/pairings: **484 passed, 12 failed**. A separate rerun using an
immutable actor snapshot (SHA256
`d8575448e10190da480d379f4c001a6e1a98bf41b0c03e46f2a988e0c291b2ff`)
reproduced **484/12**. The first log is
`/tmp/jed-swift-native-process-mixed.log`; the snapshot run is
`/tmp/jed-swift-native-process-mixed-snapshot.log`. All functional protocol
steps passed. The failures are the `backup` scenario's cross-pairing byte
comparisons of `client_pi` and `owner_pi`; compacted copies match. These are
real acceptance failures, not exclusions. The opt-in process task now snapshots
its executable before spawning actors, so a concurrent SwiftPM gate cannot
change the executable partway through a matrix. `JED_PROCESS_ARTIFACTS` optionally
preserves successful scenario images for byte diagnosis without changing
normal driver behavior.

The retained allocation probe at `/tmp/jed-swift-backup-prefix` found the first
divergence at the second catalog commit after creation. Fresh Swift creation
accidentally seeded the reopened-file reclamation baseline; the supported cores
retain the zero sentinel until the first unpinned commit, which reclaims the
bootstrap page as an unpersisted one-page residue. That residue is reused by
the next commit. Consequently two surviving table leaves retained different
page indices in the page-identical backup while compacted images normalized
identically. A narrow fresh-file accounting correction and separate fresh-create
versus reopen allocation test are under verification; the original results above
remain recorded until the fixed matrix passes.

The corrected bootstrap gate passed all **10 StorageDomainBoundaryTests**,
including the new allocation-history test. The focused backup rerun then passed
**16/16 pairings**, including all required byte comparisons, with actor SHA256
`392161151fbe4add55a0219335a28c616b482262189958c99fe0d40a93c53a41`
(`/tmp/jed-swift-native-process-backup-fixed.log`). The complete 496-pairing rerun
is separately recorded when finished; a focused pass does not replace it.

The complete corrected matrix finished with **496/496 PASS, zero failures**
(`/tmp/jed-swift-native-process-mixed-fixed.log`): all 31 frozen scenarios across
all 16 ordered Swift/Rust/Go/Node pairings, including every required byte
comparison. It used the same `39216115…c53a41` immutable Swift executable as the
focused pass and verified the unchanged frozen spec fingerprint
`52909f248d3829bbbed754ca784fa0a4e5a3b105af0fa505028683b5cb7c6249`.
Both stream scenarios selected the native Swift page server. No selected scenario
was skipped; each Swift actor executed SQL through its native Swift core. This
verifies the frozen real-process matrix, including cross-core page service and
backup bytes. Simulation, exhaustive fault coverage and other incomplete host
matrix rows remain independent acceptance requirements.

### Historical simulation acceptance checkpoints

The real byte/filesystem/scratch model and single-process runner were implemented;
`swift-native:sim:regress` attempts all 18 frozen regression files twice and retains
canonical transcripts, images, byte-operation hashes, and immediate progress.
The production pager deadline policy is shared with native files. Eight clock,
Q4, service timing, native idle-close/read-only, and failure-boundary tests passed
in the 27-test focused gate. This advanced L07/V09 partially, not to accepted.

The complete first post-resolver run reported **8 PASS / 10 FAIL / 0 skipped**:
nine service files explicitly failed at unimplemented simulated service setup;
`stale-root-leaf-chains` failed its resident overflow integrity assertion at step 3.
The eight passing traces include the 403-step encrypted rekey/crash trace and
the 47-step deadline trace. Actor SHA-256 is
`fb88b3a47af72578f2cb40f465795943c177260640f20e2dff51f9352a5d1f3d`;
frozen input fingerprint is
`369ad9def9a39d16e8812714f9100f1ad86df6822910021c32cfa19e4c2e7bfc`.
Evidence: `/tmp/jed-swift-sim-results-20261106-19-eto6n2/results.jsonl` and
`/tmp/jed-swift-native-sim-resolver-fixed.log`.
The [simulation plan](../../impl/swift-native/Tools/SIMULATION_PLAN.md) records
exact passing/failing identities, the prior interrupted attempt, and the service,
22-profile sweep and cross-core transcript/image work then remaining.

S8 source status after that run: the simulated ownership/endpoint/byte-stream
host and full runner service-history routing were published and independently
reviewed, awaiting their first runtime gate and unchanged all-18 replay. The
runner counted every selected file; no service regression was excluded. Native
owner/client, backup, transport and deadline regressions still required repeats
after the host-policy extraction. At that checkpoint, the preceding
8-pass/10-failure result remained the
latest complete simulation result until the rerun finished.


The subsequent integrated S8 gate passed28/28 and the complete frozen simulation
rerun passed **18/18, zero failures and zero skipped files**, each replayed twice.
Actor `e5494608f618785d74297a9318875e77c8a3dcf19c251eae65369cd18b8692d0`;
results `/tmp/jed-swift-sim-results-20261106-3772157-pnbmp6/results.jsonl`;
log `/tmp/jed-swift-native-sim-service-integrated.log`. The same frozen input
fingerprint was verified and all eight prior passing transcript/operation hashes
remained unchanged. Profile sweeps, supported-core comparison, and native/mixed
process reruns after this extraction remained pending then; the refreshed
completed gates are listed at the top of this matrix.

The next native host gate passed 92/92 and native process rerun passed 31/31
(`/tmp/jed-swift-native-process-service-integrated.log`). The first completed
four-core simulation comparison returned 18 passing local-oracle results per
core, but 180 differential disagreements across seven traces; all 153 Swift
crash/backup images passed the independent byte-format checker. This is not
cross-core acceptance. See
[`SIMULATION_COMPARISON.md`](../../impl/swift-native/Tools/SIMULATION_COMPARISON.md)
for exact hashes, retained artifacts, and failure classification. The 22-profile
sweep and post-extraction mixed process matrix were pending then; both
refreshed gates now pass.

The corrected four-core regression comparison reduced the disagreements to three
transcript hashes at one credential-repair boundary; all emitted image bytes now
agree and all153 Swift images pass the independent checker. The remaining saved-
image versus post-open-cache adapter correction was reviewed and published; full
rerun acceptance was pending at that checkpoint and subsequently passed. Exact artifacts and identities remain in the
comparison record above.

The whole-file SQL adapter now supports explicit `--sql-mode auto|memory|disk`.
Default auto attempts all selected files, using real file hosts for the two
`#skip: memory` resource files. Disk mode reopens before each record and reapplies
main's sticky storage limit. Explicit modes list every canonical mode exclusion
and reject a wholly excluded selection. Fixtures keep the canonical in-memory
open-from-bytes path, including expected open-error assertions. These adapter
changes do not establish passing file/spill behavior; the shared corpus gate is
reported independently.

## Public embedding audit, H01–H17 (2026-11-06)

This audit retains the implementation evidence and earlier gate counts. The third
full SQL run completed all 749 files with seven mismatches; the subsequent 13-file
gate passed all 437 records covering those mismatches and neighbors. No SQL
expectations were changed. Later complete acceptance is recorded above and in
VERIFICATION.md; host behavior has its own checks rather than inheriting a pass
from ordinary SQL. Earlier 509-test references below are preoptimization evidence.

| Row | Implemented surface and evidence | Acceptance evidence / scope boundary |
|---|---|---|
| H01 | `DatabaseFiles` provides memory/durable create, existing-file open, deserialize; `FileDatabaseHost` and `ServiceDatabaseHost` provide no-clobber publication and format/error validation. `FileHostBoundaryTests`, frozen file fixtures and process runs exercise these. | Implemented: `DatabaseFiles`, `FileDatabaseHost`; FileHost/StorageDomain/Automatic boundary tests. Included in the passing fourth release refresh of all 509 native tests; exact-account and mixed-process/saved-simulation gates also pass; the final generated release sweep also passes. |
| H02 | `CreateOptions`, `OpenOptions`, `PageServiceOptions`, `SessionOptions` expose storage/host/session settings with explicit defaults. Work memory belongs to an explicit session in Swift. Runtime work-memory/input-length/lock-timeout properties are now implemented separately. | Implemented: `CreateOptions`, `OpenOptions`, `SessionOptions`, `PageServiceOptions`; FileHost, SessionSettingsAccess, BatchFlush, CredentialWriter tests. Included in the passing fourth release refresh of all 509 native tests; exact-account and mixed-process/saved-simulation gates also pass; the final generated release sweep also passes. |
| H03 | `Database` owns shared storage and an immutable environment; conveniences create fresh sessions. `DatabaseBoundaryTests`, `SessionAdmissionBoundaryTests`, `BatchFlushBoundaryTests` cover settings isolation, admission, close and relaxed durability. Escaped read bundles now retain the Database until all pins close; the cold native-file sibling-cursor lifetime test passes. | Implemented: `Database`, `Session`, retained `DomainReadBundle`; Database/SessionCursor/SessionAdmission and file close/promotion tests. Included in the passing fourth release refresh of all 509 native tests; exact-account and mixed-process/saved-simulation gates also pass; the final generated release sweep also passes. |
| H04 | Total `query`/`queryPrepared` returns `Rows` for all statements, `execute` drains it, and `Rows.rowsAffected` distinguishes absent from zero. `RowsMetadataBoundaryTests` covers RETURNING and CTAS/DDL count presence. | Implemented: `Rows`, `CommandTag`, optional affected count; RowsMetadata tests and whole-file query adapter. All 749 SQL files and all 509 native tests pass in the fourth release refresh. |
| H05 | Public begin/commit/rollback/view/update, SQL controls, savepoints and failed-block state share `SessionTransaction`. `DatabaseBoundaryTests`, `SessionCursorTests`, the transaction/savepoint corpus and concurrency adapter cover ownership and poisoning. | Implemented: Session state machine, `DatabaseScopes`, savepoint/failed-block hooks; seven shared concurrency schedules and transaction corpus; scoped/session boundary tests. Included in the passing fourth release refresh of all 509 native tests; exact-account and mixed-process/saved-simulation gates also pass; the final generated release sweep also passes. |
| H06 | `PreparedStatement`, detached INSERT/SELECT blueprints, exact binding-sensitive probe/custom cache, fresh-root rebinding and memo-8 FIFO are implemented. Ten original prepared and six probe tests passed; dynamic-zone exclusion adds another host test. | The dynamic-zone exclusion test passes in the latest 509-test release log; the fourth release refresh passes all 509 native tests. All subquery/derived/CTE/temp/SRF exclusions intentionally match the frozen cache baseline; this is not an omission of general SQL execution. |
| H07 | `Rows` supplies metadata, lossless values, cost, terminal error, explicit close, prefix cleanup and Q1 collectors. Direct full/contiguous-PK scans now charge their initial block at public open, defer fixed-only point leaf reads, and preserve one fused spillable-point fetch. Three admission tests pass. | Three actual supported-core probes established charge-at-open/checkpoint-on-pull; `BASELINE_FINDINGS.md` records setup, commands and all 15 matching observations. The narrow streaming prose correction changes no supported algorithm. Blocking/WITH/set bodies remain deferred by design; H16 batching and all-mode resource acceptance have separate completed evidence. |
| H08 | Snapshot pins, borrowed-writer pins, frozen `StatementServices` settings and independent row cleanup are implemented and covered by cursor/database/page-service tests plus all seven stepped concurrency files. The escaped-file test verifies two independent cold-cache cursors after their Session and external Database references are dropped. | Implemented: `SnapshotPin`, `DomainReadBundle`, `SessionTransaction`; SessionCursor, DatabaseCore, PageServiceClient and shared watermark schedules. Included in the passing fourth release refresh of all 509 native tests; exact-account and mixed-process/saved-simulation gates also pass; the final generated release sweep also passes. |
| H09 | `ValueConversion`, `Row`, `Ergonomic` and six host tests implement typed access, exact conversions and collecting helpers. | Implemented: `ValueConversion`, `Row`, `Ergonomic`; six ergonomic host tests including finite collector/streaming budgets. Included in the passing fourth release refresh of all 509 native tests; exact-account and mixed-process/saved-simulation gates also pass; the final generated release sweep also passes. |
| H10 | Public atomic `CancellationToken` flows through statement meters, row pulls, shared execution loops, gate waits and maintenance. `ResourceTests`, `DatabaseBoundaryTests`, prepared, service and gate tests exercise cancellation and restoration. | Implemented: independent atomic CancellationToken, work checkpoints, cleanup; Resource, HostBatch, Session and threaded writer-admission tests. Included in the passing fourth release refresh of all 509 native tests; exact-account and mixed-process/saved-simulation gates also pass; the final generated release sweep also passes. |
| H11 | `JedError` exposes SQLSTATE, detail, hint, position, constraint/table/database/column/type fields; error-field corpus and malformed codec tests exercise the boundary. Rendering remains throwing. | Direct acceptance passes: all 407 shared Detail/Position/Hint cases, 10 object-field cases and 20 runtime metadata probes. Final four-case RETURNING-order follow-up also passes in the 42-test diagnostic/GIN/deep-view gate. The fourth release refresh also passes these diagnostics and all native host tests. Evidence: ErrorFieldCorpusTests and RuntimeTypeDiagnosticsTests. |
| H12 | `ScriptSplitter` provides lazy exact spans. `ScriptExecution` and minimal `Session.admitScript` implement idle atomic wrapper, open-block joining, control rejection, O(1) totals and scalar-coordinate remapping through ordinary prepared cursors. Seven host tests cover wrapper behavior. | Implemented: `ScriptSplitter`, `ScriptExecution`; LexerBoundary and ScriptExecutionBoundary host tests. Included in the passing fourth release refresh of all 509 native tests; exact-account and mixed-process/saved-simulation gates also pass; the final generated release sweep also passes. |
| H13 | Statement capability preflight, view/default/function traversal and prepared admission exist; grants/revokes and initial defaults/DDL gates are public. Runtime default privilege and persistent/temp DDL properties are implemented in `SessionSettingsAccess`. | Implemented: `StatementPrivileges`, `PrivilegePreflight`, SessionSettings capabilities; privilege/session corpus and source-view/CTE tests. Included in the passing fourth release refresh of all 509 native tests; exact-account and mixed-process/saved-simulation gates also pass; the final generated release sweep also passes. |
| H14 | Public variables/reset/timezone/render/default collation, private TEMP domain and ON COMMIT behavior are implemented. Session/temp/clock/collation tests and full SQL corpus cover them. Runtime temp residency/storage properties use the existing next-statement configuration seam. | Implemented: `SessionSettingsAccess`, `TemporaryDomain`, per-session routing/rendering; TemporarySession, TempType, SessionSettingsAccess and frozen session/TEMP corpus. Included in the passing fourth release refresh of all 509 native tests; exact-account and mixed-process/saved-simulation gates also pass; the final generated release sweep also passes. |
| H15 | Immutable Environment builder, derived data isolation, shared/replaced source objects, loaded Unicode/timezone bundles and host registrations are implemented with EnvironmentBoundaryTests. `loadedCollations` and `loadedTimeZones` are public and deterministic. | Implemented: Environment builder, Unicode/timezone loaders, introspection; Environment, SessionSettingsAccess, StatementServices and shared vector tests. Included in the passing fourth release refresh of all 509 native tests; exact-account and mixed-process/saved-simulation gates also pass; the final generated release sweep also passes. |
| H16 | Scalar and multirow HostFunction ABI, strict NULL behavior, declaration metadata, static nonnegative cost, result kind/integer-width validation and persisted dependency checks are implemented. HostBatch/HostReadAhead preserve scalar replay, bounded safe peeks and cleanup across scan, spool, sort, aggregate and window emitters; dedicated host tests exercise them. | Implemented: HostFunction scalar/batch registration, HostReadAhead/replay, fixed/multirow shapes; Environment and HostBatch* host tests. Included in the passing fourth release refresh of all 509 native tests; exact-account and mixed-process/saved-simulation gates also pass; the final generated release sweep also passes. |
| H17 | Shared seeded PRNG and UUID byte vectors, lazy cached statement clock, wall clock, manual/advancing sources and independent key entropy are implemented. StatementServicesTests and EnvironmentBoundaryTests cover vectors, source call counts and key separation. | Implemented: sanctioned random/clock providers and independent key source; StatementServices, Encryption, PRNG vectors and deterministic simulation. Included in the passing fourth release refresh of all 509 native tests; exact-account and mixed-process/saved-simulation gates also pass; the final generated release sweep also passes. |

The initial audit found H12 script execution, H15 loaded-timezone introspection,
runtime session envelope/capability setters and H16 multirow host batching as
concrete missing surfaces. Those slices and the direct-scan/file-lifetime fixes
are now implemented. The focused host foundation gate passed 30/30; the later
H16/TEMP gate passed 30 tests, including all 15 batch tests and six resource SQL
files (255 records). The earlier 505-test release suite also passed these host
surfaces. The fourth release refresh passes all 509 native tests, including
these surfaces and the corrected old-pin observation boundary.

The refreshed generated simulation acceptance is **176 traces per core,
459 transcript comparisons, 7,049 independently checked Swift images and zero
failures**, covering all 22 frozen profiles at seeds 1–8. Actor and artifacts
are recorded in `Tools/SIMULATION_COMPARISON.md`; this refresh includes the
later H07/H16 and integrity hooks. It does not replace service SQL, resource or
native timing checks. The opt-in process task rejects omitted Swift,
duplicate/unknown/empty core tokens, a wrong frozen inventory count and an empty
scenario filter before building or running an actor. Three invalid-input mise
probes passed; default process selection remains unchanged.

## Final audit and freeze

The current implementation is `fb34b0e8cb3d82287fbd3401e3426f04b3b86c7a` against the
unchanged semantic baseline. Exact commands, source/actor hashes and retained
reports are mapped in [VERIFICATION.md](../../impl/swift-native/VERIFICATION.md).
The initial missing-state inventory, failed attempts and intermediate counts remain
historical evidence. Public integrity and structured diagnostics were required
work, not exclusions: their fixture/API/CLI checks, all 407 shared cases,
10 object-field cases, 20 runtime probes and four RETURNING-order checks pass.
The view-source correction has its own passing memory/disk regression.

[BENCHMARKS.md](../../impl/swift-native/BENCHMARKS.md) preserves the original `3db8fc2`
complete release baseline and measured optimization comparison, alongside the
completed 22/8/3-workload structural diagnostics. Current native/SQL, exact-account, encrypted,
interchange, service, process, stress and simulation gates cover the affected paths.
Earlier debug, CLI, oracle and default-CI checks retain their own actors and dates;
none is silently extended to later code.

This is a frozen, locally committed experiment, not a package release or promotion.
No default task or future lockstep obligation includes Swift. Apple platforms are
unverified, and production native-versus-wrapped delivery remains a separate decision.
