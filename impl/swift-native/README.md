# Native Swift experiment

The explicitly requested 2026-11-12 refresh targets the supported jed surface at
`b1a6affca7725e0b07a4f734eddb1c569ef76bcf`, including format **v52**, **763 SQL files**,
**116 database fixtures**, and **19 saved simulation regressions**. It remains a
pinned, opt-in experiment. See [VERIFICATION.md](VERIFICATION.md) for the refresh's
actual Linux checks and remaining platform limits, and [PROGRESS.md](PROGRESS.md)
for implementation history. The refresh preserves the experiment’s opt-in status
and pinned maintenance scope.

[BENCHMARKS.md](BENCHMARKS.md) preserves the older actors' measurements. Those
v50 results are historical and do not establish performance for this v52 refresh.
The [experiment design](../../spec/design/swift-native-experiment.md) and scope/host
matrices record the acceptance boundaries.

The independent engine is handwritten Swift. Approved crypto utilities are isolated
in `JedCrypto`, using exact-pinned SwiftPM dependencies and pinned reference Argon2
source; applications need no system crypto library. This experiment does not select
production Swift delivery or claim macOS/iOS support. Linux uses Swift 6.4.2.

Run from the repository through explicit opt-in tasks:

```
mise run swift-native:resolve
mise run swift-native:codegen --check
mise run swift-native:build
mise run swift-native:examples
mise run swift-native:test
mise run swift-native:concurrency
mise run swift-native:stress
mise run swift-native:spill
mise run swift-native:spill --temp
mise run swift-native:query-memory
mise run swift-native:interop
mise run swift-native:encrypted
mise run swift-native:service
mise run swift-native:check -- --format json example.jed
mise run swift-native:sim:regress
mise run swift-native:test --all-sql --filter 'SQLCorpusBoundary|PostbaselineSQL'
mise run swift-native:build --configuration release
mise run swift-native:unicode -- --help
```

Normal repository CI, tests, benchmarks and generated website references do not
include Swift. Generated metadata is pinned to the recorded baseline Git blobs;
updating supported cores does not refresh this frozen snapshot. Further work
requires explicit authorization; promotion and production delivery remain
separate decisions. Apple platforms remain unverified.

The runnable [embedding examples](Examples/main.swift) import only public `Jed`
APIs. `mise run swift-native:examples` runs memory, durable-file and cancellation
examples; pass `memory`, `durable` or `cancellation` to select one. The durable
example creates and removes its own temporary directory and verifies a committed
row after reopening. The memory example uses prepared bound parameters, a scoped
transaction, typed row access and explicit cursor cleanup.

`--all-sql` runs every frozen SQL file through the same session/cursor adapter;
unhandled directives are reported as failures. The ordinary host test run retains a
smaller SQL integration set. Concurrency-format files
route to the shared schedule adapter. `swift-native:concurrency` executes all seven frozen
schedules in stepped-sequential mode, checking pinned snapshots, writer handoffs,
version changes, and reader watermarks; it does not claim threaded race coverage.
Later shared corrections run separately and do not alter the frozen corpus fingerprint.

`swift-native:stress` runs the frozen Layer 3 reader/writer workload on real
Swift threads in memory and durable file modes, including durable reopen. It
compares every snapshot invariant and final checksum with Rust threads, Go
under its race detector, and TypeScript's seeded sequential interleaver. It is
explicitly opt-in and does not claim Swift race-detector coverage or benchmark
performance from the debug correctness run.

`swift-native:spill` forces operator spilling over all applicable frozen disk
files; `--temp` selects forced TEMP spilling with memory main and real native
scratch. `swift-native:query-memory` compares every sequential record's exact
peak and committed-main storage across Swift/Rust/Go/TS in memory, disk and
disk-spill modes. Filters are diagnostic subsets; see
[current verification](VERIFICATION.md) for the refreshed 245,756-measurement
comparison; [resource acceptance evidence](Tools/RESOURCE_ACCEPTANCE.md) preserves
the original baseline investigation.

`swift-native:interop` exercises all 116 frozen images with Swift and each
supported core in both writer orders, plus generated plaintext/key/passphrase
and attachment cases. Corruption and recovery fixtures have explicit probes;
missing phases fail. See [interchange acceptance](Tools/INTERCHANGE_PLAN.md).
`swift-native:encrypted` runs the applicable frozen disk corpus using the shared
fixed test credential and compares all cost/EXPLAIN transcripts across the four
cores. It checks ordinary values/errors and exact file/record coverage, retains
an immutable Swift actor and logs, and supports labelled diagnostic filters.
The refresh passes 79,060 encrypted record measurements and all 2,787 interchange
phases. VERIFICATION.md records the current gates and actor provenance separately
from the older service/process evidence.

`swift-native:service` runs all six Swift/supported-core client-owner directions
through real external owner processes. It captures actors, asserts actual client
roles, and reports the baseline fixture-local exceptions separately.

The native file host supports automatic page-service ownership and clients,
as well as explicit exclusive ownership. For example:

```swift
let database = try Database.open("example.jed", options: OpenOptions(locking: .exclusive))
try database.compact()
try database.close()
```

`Database.integrityCheck` checks one committed main or attached database without
waiting for an open writer. Full checking is the default; quick checking omits
row/index agreement, and optional constraint checking adds NOT NULL, typmod,
CHECK, UNIQUE and foreign-key validation:

```swift
let report = try database.integrityCheck("main", options: IntegrityCheckOptions(
    level: .full, constraints: true, maxFindings: 100))
for finding in report.findings { print(finding.kind, finding.object ?? "") }
print(report.ok, report.pages, report.rows)
```

The finding limit defaults to 100; zero means unlimited. `report.truncated`
means a further finding exceeded that limit. Skips report unavailable stored
definitions or dependencies explicitly. Cancellation, role loss and I/O errors
throw rather than returning a partial success report. The command
`mise run swift-native:check -- --format json example.jed` opens read-only and
exits 0 for a clean report, 2 for findings or an `XX001` open failure, and 1 for
other errors. Encrypted files use `--key-file`, `--passphrase-file`, or the
no-echo `--passphrase` prompt. See [integrity acceptance](Tools/INTEGRITY_PLAN.md).

`CreateOptions` and `OpenOptions` accept `synchronous` (default `true`) and
`flushIntervalMilliseconds` (default `1000`, `0` disables deadlines). Relaxed
local-file batches use the environment's injected clock. The same foreground
policy runs on native and simulated file hosts; native file owners also run an
idle waiter. Memory and remote client planners do not time an owner's batches.
Read-only handles do not start a waiter. File attachments inherit the root
handle's interval and synchronous setting.

`swift-native:sim:regress` builds and snapshots the native simulator, then attempts
all 18 frozen regression traces twice. It writes per-step diagnostic progress,
canonical transcripts, images, and explicit results to a unique temporary directory.
All 18 frozen regressions, including page-service histories, pass deterministic
double replay. Separate four-core comparison and generated-sweep acceptance also
pass. The postoptimization saved replay passes all 18 traces per core, 54 transcript
comparisons and 153 independently checked Swift images; earlier generated sweeps
retain their own actors. These finite checks are not exhaustive fault coverage;
see [SIMULATION_PLAN.md](Tools/SIMULATION_PLAN.md).

The SQL path uses explicit sessions and pull cursors:

```swift
import Jed

let database = try Database()
let session = try database.session()
try session.execute("CREATE TABLE item (id i32 PRIMARY KEY, label text)")
try session.execute("INSERT INTO item VALUES (1, 'first')")
let rows = try session.query("SELECT label FROM item WHERE id = $1",
                             parameters: [.integer(1)])
while let row = try rows.next() { print(row) }
session.close()
try database.close()
```

Keep a session for transactions, variables and sequence history. Convenience
queries on `Database` create fresh sessions. Result-column `type` is optional:
nil represents the unresolved SQL `unknown` type of an all-NULL output. Consult
the [scope matrix](../../spec/design/swift-native-scope.md) for the verified baseline
surface and deliberate exclusions.

Prepare once and execute on the session that should supply the transaction,
settings and privileges:

```swift
let lookup = try database.prepare("SELECT label FROM item WHERE id = $1")
let rows = try session.queryPrepared(lookup, parameters: [.integer(1)])
while let row = try rows.next() { print(row) }
```

`PreparedStatement` owns parsed SQL and detached plan metadata, so it can outlive
the preparing handle. It is thread-affine, like sessions and cursors. Both
`Database` and `Session` provide `queryPrepared` and `executePrepared`; the latter
drains the same cursor and returns its command tag. Eligible persistent-table
SELECT and plain VALUES INSERT plans are reused with current execution state.
SELECT validity includes each referenced relation's schema and estimator
revision; parameter-sensitive plans retain up to eight exact bound-value plans.
Temporary, derived, CTE and subquery plans are resolved on each execution.

Native arguments and typed rows are available through an additive conversion
layer. `Optional` binds and reads SQL NULL; exact SQL values remain available
for decimals, temporal values and recursive types:

```swift
try session.execute("INSERT INTO item VALUES ($1, $2)", arguments: [2, "second"])
if let row = try session.queryRowPrepared(lookup, arguments: [2]) {
    let label: String = try row.get("label")
    print(label)
}
```

`queryRows` collects typed rows under the query-memory account. Use
`Rows.nextRow()` for streaming, or `forEachRow` with a callback returning false
to stop and close the cursor. Custom host types can implement
`SQLValueConvertible` and `SQLValueDecodable`; `Row.value` returns the raw value.
Native byte arrays and Foundation Data bind as bytea; other native arrays bind
as one-dimensional SQL arrays. SQLArray retains explicit dimensions and lower
bounds. `Database.view` and `Database.update` run a scoped transaction on a fresh
session; the same helpers on Session retain its settings and privileges.

`executeScript(_:)` on a database or session splits and drains a multi-statement
script, returning only `statementsRun`, `rowsAffectedTotal` and `cost`. An idle
session commits the whole script atomically; an open session leaves its caller's
transaction open, or failed on error. In-script transaction and savepoint control
is rejected. Use `splitStatements(_:)` and ordinary query calls when the host
needs to manage individual statement boundaries or retain result rows.

Session resource settings can change between statements: `maxSQLLength`,
`lockTimeoutMilliseconds`, `workMemoryBytes`, `temporaryBufferBytes` and
`maxTemporaryStorageBytes` supplement the cost/scalar/query-memory properties.
Runtime work-memory zero disables spilling; an initial option of zero selects
the finite default. Existing cursors retain their captured execution settings.
`defaultPrivileges`, `allowDDL` and `allowTemporaryDDL` are mutable capabilities;
explicit object denials still take precedence. Temporary DDL inherits the
persistent gate only when the session is created, then changes independently.

`Rows.rowsAffected` is an optional statement count: non-RETURNING DML reports
the affected count, including zero; query results, DDL and transaction control
report nil. CTAS reports its fill count, or nil for `WITH NO DATA` and skipped
`IF NOT EXISTS`; ANALYZE reports zero. The value is available before the first
pull and survives cursor closure. `finish()` still drains rows and returns a
`CommandTag` whose count is the affected count or the number of returned rows.

`Database.create`, `deserialize` and `serialize` provide creation and byte import/
export. `serialize(_:)` exports the committed `"main"` or exact attachment name
without waiting for an open writer; pending and TEMP data are excluded.
Encrypted export returns `0A000`. `attach(_:source:readOnly:)` and `detach(_:)` manage named memory or file
domains. `compact`, `changeCredential` and `rekey` accept an exact domain name,
defaulting to `"main"`. A nil rekey credential produces plaintext. Open a
plaintext root with `scratchEncryption: .on` before encrypting a file attachment
or main; the root's scratch policy governs the whole handle. Maintenance refuses
active writers immediately, and replacement also requires the target's readers
to finish. Credential rewrapping permits existing readers.

`Environment.loadedCollations()` lists loaded deterministic tables. Sessions and
`Database` expose `collations()`, `defaultCollation()`, `setDefaultCollation(_:)`
and `upgradeCollations()`. Migration atomically rebuilds keys under loaded table
versions, clears affected statistics, and repins the referenced collations.
These host catalog operations use the session's transaction snapshot; a failed
migration leaves an existing block usable. Affected expression or partial
indexes currently return the frozen baseline's `0A000` refusal.

`loadedTimeZones()` on Environment, Database or Session lists loaded named zones
and aliases in UTF-8 name order, with each entry's `tzdataVersion`. Built-in UTC
and fixed offsets are not listed. Loading or deriving a builder does not change
the inventory of an already-built environment.

Linux's default `.automatic` locking mode connects the native page-service
owner/client host. `PageServiceOptions` controls serving, ownership preference,
grace periods, connection limits, and idle timeouts. `Database.openRemote` accepts
a `PageTransport` factory or a command array for a native page-service stream.
The command form requires Linux pidfd spawn support (glibc 2.39+); an unsupported
host returns `0A000` and can supply its own transport. Credentials travel in the
protocol hello, not the command arguments. Run the frozen process scenarios with
`mise run swift-native:process`; consult VERIFICATION.md for verified modes and
their actor-specific evidence. These experimental APIs do not change supported
core delivery or normal repository verification.
