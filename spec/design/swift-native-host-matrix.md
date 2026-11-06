# Swift native experiment: embedding and harness acceptance matrix

Audit target: **`362e0798ac3a8bd04bd2da556044b6ec73bf468a`**. This is an
inventory of the supported Rust/Go/TypeScript surface at that revision, not a
claim that Swift implements it. The companion [scope matrix](swift-native-scope.md)
inventories SQL, shared data, and storage subsystems. Ruby's frozen experiment is
not an acceptance baseline.

Every row below initially has status **missing**. A row becomes **verified** only
when the progress record names its Swift implementation, executable checks,
commands, and results. Compiling an interface, declaring a capability, or passing
an unrelated SQL query does not satisfy a row. Substantial rows should be split
as their vertical slices land; retain the original identifiers for traceability.

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
| H01 | `create(options)` supports absent path as memory and present path as new durable file; `open(path, options)` opens only an existing file. Validate page size, preserve duplicate/missing/corrupt/I/O errors, and never clobber a pre-existing path. | api.md §2.1; `api`, `fileformat_golden` | missing |
| H02 | Open/create options preserve defaults and distinguish absent from explicit zero: cache, work memory, read-only, skip-fsync testing knob, synchronous mode, flush interval, locking, election timeout, owner limits/preference, environment, storage allowance, credential and scratch encryption. File page size is immutable. | Rust `file.rs`/`shared.rs`; Go `file.go`/`shared.go`; `api`, `file_sessions`, `encryption` | missing |
| H03 | Database owns storage identity; convenience calls mint fresh sessions, and explicit sessions retain semantic settings and safety envelopes. Database and environment lifetimes outlive temporary host references. Close is idempotent where specified and discards pending writes, while flushing already-committed relaxed durability batches as required. | api.md §§1–2; session.md §1 update; `environment`, `session`, `file_sessions`, `shared` | missing |
| H04 | The total `query` operation returns a cursor for every statement; DDL, transaction control and DML without RETURNING have no columns. Command count is optional, with no-count distinct from zero. `execute` drains the same cursor and returns its command result. | api.md §§1,4; `api` tests `QueryOnNonQueryStatementIsTotal` and `RowsAffectedReportsDMLCounts` | missing |
| H05 | Explicit begin/read-write/read-only, commit, rollback, scoped view/update, autocommit and SQL transaction control use one state machine. Expose idle/open/failed status; block errors poison the transaction, including errors discovered while pulling rows; savepoint rollback restores the required state. Scoped callbacks cannot terminate the transaction underneath their owner. | api.md §2.3–2.5; session.md §2.2; `api`, `session`; shared transaction/savepoint/concurrency corpus | missing |
| H06 | Prepared statement parse/resolve/bind behavior and parameter count/types; execution on database/session/transaction; relation-scoped cache invalidation and exact binding-sensitive plans. Host NULL, integer, exact decimal, byte, date/time and recursive values obey SQL static typing. Do not substitute SQL interpolation for binding. | api.md §5; estimator.md §6.1; `api`, `ergonomic`; parameter and planning corpus | missing |
| H07 | Cursor metadata is available without materializing all rows; values are lossless. Cost advances as rows are consumed, is final only after full drain, and remains inspectable after close. End-of-stream and terminal error remain distinguishable. Closing a prefix cancels remaining work and releases reader pins and retained allocations. | streaming.md §§4–6; `streaming`, `api`, `query_memory`, `lifetime_cost` | missing |
| H08 | Cursor captures the snapshot and execution settings required by streaming.md, keeps its pin after its session finishes when required, and does not accidentally read a newer commit. One cursor's closure must not release another cursor's or transaction's pin. Read-only cursors do not take the writer gate. | `streaming`, `shared`; `concurrency/{snapshot_isolation,watermark_refcount,storage_watermark}.test` | missing |
| H09 | Idiomatic value conversions and typed row access preserve NULL, widths, narrowing checks, exact i64/decimal, copied bytes and open-type identity. Single-row and collecting helpers propagate cursor errors and meter engine-owned collection. Swift protocols/helpers may replace reflection-specific Go and JavaScript spellings. | api.md §11; Rust `ergonomic.rs`; Go `ergonomic.go`; `ergonomic`, `query_memory` | missing |
| H10 | Cancellation can be requested independently from the running operation; pre-cancel, execution checkpoints, zero-row loops, batch kernels and mid-cursor cancellation return `57014`, clean up pins, preserve cost limits and restore session cancellation context. Canceled statements do not publish partial writes. | api.md §11.4; `cancellation`; Go `TestCancellationWithoutRowEmission`, `TestCancellationSamplingIndependentOfCost`, `TestCancellationRestoresSession` | missing |
| H11 | Structured errors expose SQLSTATE and the implemented detail/hint/position/object fields; host conversion errors retain their specified category. Rendering errors also propagate. Swift traps, fatal preconditions, native errno text and arbitrary host exceptions must not replace contract errors for malformed SQL/files. | api.md §7; error-fields.md; shared registry and error-field corpus | missing |
| H12 | Standalone statement splitting returns exact spans/offsets across quotes, dollar quotes and nested comments. Script execution on an idle session wraps the run in one implicit transaction; on an open session it joins the existing transaction. It rejects in-script transaction/savepoint control, stops on error and returns an O(1) count/cost summary. A host-managed splitter loop instead owns its transaction policy. | session.md §4; api.md §1; Rust `split.rs`; Go `split.go`; `split`, `split_shape`, `session` | missing |
| H13 | Table SELECT/INSERT/UPDATE/DELETE and function EXECUTE grants/revokes, default table privileges and persistent/temporary DDL gates apply to resolved SQL objects, prepared statements, subqueries, views and host functions. They are session capabilities, not an auth catalog. | session.md §5.3; `privileges`, `host_functions`; session/capability corpus | missing |
| H14 | Session variables, reset, timezone and default collation settings use the contract's scopes and validation. Rendering and timestamp input use the originating session zone. TEMP objects remain private, with ON COMMIT behavior and independent DDL capability. | `session`, `timestamptz_session_input`, `timestamptz_session_render`, `temp_on_commit`; temp/session corpus | missing |
| H15 | Immutable environment builder loads Unicode/timezone bytes, registers functions and injects clock/random providers; derived builders and independent environments do not mutate each other or become process-global. Loaded-collation/timezone introspection is deterministic. | environment.md; `environment`, `collation_host`, `timezone`, `seam`, `date_clock` | missing |
| H16 | Host scalar and batch kernels specify argument/result types, NULL behavior, volatility, cross-core declaration, cost, component ID and semantic version. Validate registration and every result/batch shape, preserve scalar/batch semantics, reject uncosted functions under finite budgets, and fail closed on unavailable persisted components. | api.md §12; extensibility.md; Rust/Go/TS `extension`; `host_functions`; host-function byte fixtures | missing |
| H17 | Clock/entropy seams produce shared seeded/manual/advancing vectors, correct statement versus wall-clock observations and transactional UUID behavior. Encryption key entropy is separate; insecure deterministic key entropy is visibly test-only and cannot silently inherit SQL seeded randomness. | entropy.md; environment.md; encryption.md; `seam`, `date_clock`, `encryption`; `spec/encoding/prng.toml` | missing |

## 3. Resource and storage-facing host operations

| ID | Required operation and observable contract | Baseline evidence | Swift status |
|---|---|---|---|
| R01 | Per-statement `max_cost` and session `lifetime_max_cost` charge the exact schedule, including cursor pulls, internal meters and errors. Admission after lifetime exhaustion and equality-at-limit aborts must match. A timeout is not a deterministic cost meter. | cost.md; `spec/cost/schedule.toml`; `cost_limit`, `lifetime_cost`, `operator_cost`, `overflow_cost`, `compressed_cost` | missing |
| R02 | SQL input, parser/expression nesting, parameter index, identifier, view expansion, regex program and JSON/composite depth limits reject before unsafe recursion or excessive allocation. UTF-8 byte length must not become Swift grapheme count. | api.md §8; cost.md §7; `suites/resource/` | missing |
| R03 | Finite default `max_scalar_bytes` is a cumulative logical statement allowance with frozen cursor/internal meter inheritance; covered amplification/output/scratch is checked before allocation and reports `54P04`. Non-positive values restore the default, not unlimited. | memory.md; `scalar_memory`; scalar resource corpus | missing |
| R04 | `max_query_memory_bytes` charges live Q1 row buffers/engine collectors, Q2 operator/spill state and Q3 pending record versions on the shared logical schedule; reserve before allocating, release exactly once, and preserve pending holds between statements. Report `54P05`, compare exact peaks, detect under-release/over-release. | memory.md; `spec/cost/memory_sizes.toml`; `query_memory`, `memory_sizes`; resource corpus and query-memory peak runner | missing |
| R05 | File cache is evict-only under `cache_bytes`, with at least one leaf, no semantic/cost change and no query failure. `work_mem` drives bounded external sort/hash/group/distinct spill; a query-account refusal spills earlier when possible. Real disk scratch, failure cleanup, ordered replay and identical cost are required. | pager.md; spill.md; `spill`, `blocking_spill`, `query_memory`; forced-spill corpus | missing |
| R06 | TEMP residency (`temp_buffers`) differs from retained temporary storage (`max_temp_storage_bytes`, `54P03`); spill preserves values/errors/cost and ON COMMIT behavior. Encrypted handles seal scratch under the scratch policy. | temp-tables.md; encryption.md §6; Go/TS `temp_spill`, `temp_blockstore`, `temp_on_commit`; resource temp corpus | missing |
| R07 | Database/attachment-owned `max_storage_bytes` checks memory committed pages or file live pages before writes, with exact persisted live-page deltas and `54P06`. Setter and `storage_bytes(name)` do not mistake physical file size, dead pages or free list for live storage. | memory.md §8; `storage_memory`; resource/storage and process/storage_limit corpus | missing |
| S01 | Attach/detach memory and file databases with exact name resolution, read-only flags, per-file credential/storage settings and one environment. Capture all participating roots consistently; allow only one durable written domain per transaction; take writer gates in real-path byte order. | attached-databases.md; `attach`; shared attach/concurrency/process suites | missing |
| S02 | Compact a selected database/attachment through the storage seam, with correct gate/pin exclusions, same rows and cost, and durable rename/directory-sync ordering. Close/reopen and mid-operation failure do not publish a partial replacement. | api.md §2.6; `compact`, `reclaim_compaction`; process/compact and sim compaction traces | missing |
| S03 | Integrity check produces the shared finding kinds/order and truncation/completeness behavior across structural, key/index and optional constraint checks; supports cancellation and does not mutate the database. Unavailable host definitions are reported/skipped only as specified. | api.md §2.7; integrity-check.md; Rust `integrity_check`; Go/TS `integrity`; `spec/integrity/` fixtures | missing |
| S04 | Serialize committed `main` or a named attachment into canonical clean bytes while pinning a snapshot without holding the writer gate; exclude uncommitted/temp state. Deserialize copies input, preserves page size/version and recovery selection, creates independent memory storage, and rejects encrypted input. Encrypted serialization is unsupported (`0A000`) rather than plaintext export. | api.md §2.8; `serialize`; Go `integrity_deserialize_test.go` | missing |
| S05 | Consistent backup through the implemented page-service/tooling seam, compact-copy versus page-identical mode, destination-not-exists behavior, concurrent readers/writers and encrypted current-key behavior. Baseline `backup_for_testing`/`BackupForTesting` is tooling API, not a promise of a new general Swift application API. | page-service.md §9; `backup`; process/backup*.process.toml; simulation service-backup regressions | missing |
| S06 | Encrypted create/open and attachments: raw key/passphrase credentials, bounded Argon2 parameters, missing/wrong/plaintext-credential errors, page authentication, nonce/replay binding, secret redaction and no accidental plaintext spill. Credentials are host input and never SQL literals/options. | encryption.md; encrypted file spec/vectors; `encryption`, Go `encryption_header`, TS `encryption_vectors` | missing |
| S07 | Credential change rewraps while preserving the data key; rekey/conversion rewrites live pages atomically. Exercise old/new credential selection after every crash boundary, service clients with held keys, attachment encryption and backup restoration. | encryption.md; `encryption`; encryption simulation profiles/regressions and encrypted process scenarios | missing |
| S08 | Storage inspectors used by embedding/tooling expose version, page size/count, path, read-only, resident leaves and oldest live txid without changing storage behavior. Catalog inspectors required by invariant tools remain available to those tools even if kept internal to the Swift package. | Rust/Go `shared`; `api`, `shared`, `streaming`, golden/integrity tests | missing |

## 4. Linux host and concurrency

| ID | Required operation and observable contract | Baseline evidence | Swift status |
|---|---|---|---|
| L01 | Byte-addressed positioned read/write, size, data barrier and durable growth/truncation form the host seam. Handle short I/O and interruption, validate offsets, isolate descriptor/pointer code, and preserve the difference between data sync, allocation metadata sync and parent-directory sync. Hosts do not select page layout or commit ordering. | hosts.md §2; `blockstore`, file hosts; validated-cow.md | missing |
| L02 | Database sharing allows concurrent readers and one writer with immutable pinned roots, independent sessions and reader watermark refcounts; commit publication is short. Real threaded writer-gate blocking and cancellation/timeouts do not deadlock or release someone else's ownership. | concurrency-testing.md Layers 1–3; shared concurrency corpus; Rust/Go threaded conformance; `shared`, `file_sessions` | missing |
| L03 | Stable `protocol-v2` lock bundle uses crash-clean OS locks; `auto`, `exclusive`, `none` and ownership prefer/yield/never obey election and timeout rules. Unsupported hosts fail closed. Owner foreground operations make zero coordination syscalls and zero per-transaction meta reads. | locking.md; page-service.md; process corpus; lock simulation | missing |
| L04 | Owner/client page protocol matches message encodings, credentials and bounded framing. Owner alone accesses file bytes/reclaims pages; client readers pin remotely and commits send deltas to the owner. Gate acquisition, dirty-domain/no-op commits, resource limits and durability batches remain identical through the service. | `spec/pageservice/{messages,fixtures}.toml`; `pageservice_codec`; process service/attachment/encrypted scenarios | missing |
| L05 | Handoff, graceful shutdown/drain, owner crash, client reconnect/promotion, revoked idle transactions, max connections, read-only-owner yielding and lost commit acknowledgment resolution use the real service path. Transactions crossing owner changes fail with the specified state; commit ambiguity is not hidden by blind retry. | page-service.md; process corpus; Go/TS page-service owner/remote/close tests; service simulation profiles | missing |
| L06 | Remote stream client opens a host-supplied command once per connection, performs the same protocol on stdin/stdout, uses client read-ahead without changing cost, cleans up child processes and exposes I/O failure. Linux coverage uses the local process transport, so it does not require an external SSH account. The baseline serving CLI role is Rust-specific. | api.md §2.1; Rust `shared.rs::open_remote`, Go `pageservice_remote_unix.go`, TS async Node API; `remote_stream`/`pageservice_remote`; process/service_stream and encrypted_stream | missing |
| L07 | Deterministic simulated byte device, filesystem, locks, network, clock/entropy and spill host run the same storage/service algorithms as Linux hosts. No simulated-only commit/recovery implementation. Content-keyed faults, durability floors, reader pins, structure checks and image generation follow simulation-testing.md. | Rust `src/sim/`; Go `sim_*_test.go`; TS `src/sim/`; shared profiles/regressions | missing |

## 5. Proposed Swift interfaces and ownership boundaries

These are implementation recommendations for the experiment; exact spelling can
change without changing the acceptance rows.

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

The `swift-native:` task names below are proposed until implemented. Their
existence is not verification. Existing default task core lists stay unchanged;
adding an explicitly selected runner registration must not add Swift to default
`test`, `ci`, `conformance`, `sim:*` or benchmark dependencies. The completed
experiment freezes at its recorded baseline; later supported-core features do
not silently expand its obligations.

| ID | Explicit Swift task / mode | Required evidence and integration | Status |
|---|---|---|---|
| V01 | `swift-native:build`, `swift-native:test` | Reproducible pinned SwiftPM dependency graph; Linux debug and release builds; host-only tests for H/R/S/L rows; no unapproved dependency or system crypto installation requirement. | missing |
| V02 | `swift-native:conformance` | All baseline applicable SQL and seven concurrency files in both memory and disk modes; baseline capability coverage ledger; every missing capability and file counted as a failure for completion rather than silently skipped. | missing |
| V03 | `swift-native:spill` | All applicable disk corpus with `JED_CONFORMANCE_WORK_MEM=256`; temporary spill modes as baseline tasks exercise them; rows/errors/cost unchanged. | missing |
| V04 | `swift-native:query-memory` | Huge-but-active account and per-record peak/main-storage TSVs for memory, disk and disk-spill; compare with supported cores using the same fixture exception as `scripts/lib/query_memory_peaks.rb`; assert complete record-key coverage separately because that comparator alone tolerates absent records. | missing |
| V05 | `swift-native:encrypted` | Encrypted disk corpus; rows/errors unchanged; costs and EXPLAIN recorded with the baseline transcript format and compared to encrypted supported cores. Only baseline documented encrypted-mode exclusions apply. | missing |
| V06 | `swift-native:service` | Shared SQL corpus runs through a real supported-core owner and Swift client, and Swift owner with supported clients. A fake local service or same-process fallback does not exercise L04. | missing |
| V07 | `swift-native:process` | Register Swift actor only for explicit `JED_PROCESS_CORES`; all applicable process scenarios across Swift↔Rust/Go/Node and Swift↔Swift, in both role orders; image checker and final byte comparisons retained. Verify pinned Rust-only serving roles separately. | missing |
| V08 | `swift-native:concurrency` | Deterministic stepped corpus plus genuine threaded gate waits and reader/writer stress with invariant/final-checksum checks. Swift Thread Sanitizer is supplemental where the installed platform supports it, never a replacement for scheduled corpus or process tests. | missing |
| V09 | `swift-native:sim:regress`, `:sweep`, `:replay` | Same `JED_SIM_IN`, `JED_SIM_OUT`, `JED_SIM_RESULTS`, optional `JED_SIM_IMAGES` protocol as `scripts/sim.rb`; complete shared regressions and profile sweep. Compare transcripts/images when required; per-core read-error/enospc/spill-error/corruption traces retain each core's invariant checks. | missing |
| V10 | `swift-native:fixtures`, `swift-native:interop` | Every baseline key/value/compression/crypto/page-service fixture and file golden; Swift writes and supported cores read, supported cores write and Swift reads; mutations continue after cross-core reopen. Include encrypted keys/passphrases, attachments, recovery and host-function requirements. | missing |
| V11 | `swift-native:oracle` | Relevant baseline PostgreSQL oracle checks through checkout mise environment, with declared oracle profile and overrides unchanged. PostgreSQL-comparable behavior only; host and file invariants use shared tests/fixtures. | missing |
| V12 | `swift-native:verify-defaults` | Existing default verification remains healthy without Swift installation or dependency resolution. Inspection must show unchanged default core/dependency lists; run applicable default gates and record infrastructure blockers distinctly. | missing |
| V13 | `swift-native:bench` and comparison task | Correctness first, then preserved release baseline and measured optimization pass with supported-core matching workload/data/durability and checksums; record CPU, OS, Swift version, flags, warmup, samples, commands and data paths. No speed claim from SQL subset or debug/release mismatch. | missing |
| V14 | Local package/examples/reports | Runnable in-memory and durable examples, bound parameters, explicit cursor cleanup, transactions and cancellation; internal design/progress/verification/benchmark records. No website docs or publication. Apple targets remain unverified until actually built/tested. | missing |

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
approved host-interface exception. It exposes one operation omitted by Swift's
Linux Glibc module: atomic no-replace rename. Linux calls `renameat2` with
`RENAME_NOREPLACE`; Darwin calls `renameatx_np` with `RENAME_EXCL`. Both inputs are
borrowed NUL-terminated paths for one synchronous call, no pointer or buffer is
retained, and the return is the OS status/errno. There is no third-party
library, allocation, SQL, codec, locking policy, or storage algorithm in this C
target. The file host retains the exclusive protocol-v2 bundle while publishing.
Unsupported hosts fail `0A000`; there is no link/unlink publication fallback that
could leave a hard-link crash window.
