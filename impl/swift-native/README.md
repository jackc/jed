# Native Swift experiment

**Work in progress. SQL and host API implementation is incomplete.**
The acceptance target is the full supported jed surface at
`362e0798ac3a8bd04bd2da556044b6ec73bf468a`. See [PROGRESS.md](PROGRESS.md) for
implemented and verified foundations, current failures/omissions and resume steps.
The [experiment design](../../spec/design/swift-native-experiment.md) and its linked
scope/host matrices define completion.

The independent engine is handwritten Swift. Approved crypto utilities are isolated
in `JedCrypto`, using exact-pinned SwiftPM dependencies and pinned reference Argon2
source; applications need no system crypto library. This experiment does not select
production Swift delivery or claim macOS/iOS support. Linux uses Swift 6.4.2.

Run from the repository through explicit opt-in tasks:

```
mise run swift-native:resolve
mise run swift-native:codegen --check
mise run swift-native:build
mise run swift-native:test
mise run swift-native:build --configuration release
mise run swift-native:unicode -- --help
```

Normal repository CI, tests, benchmarks and generated website references do not
include Swift. Generated metadata is pinned to the recorded baseline Git blobs;
updating supported cores does not refresh the experiment. Completion is followed
by a freeze, with promotion subject to a separate decision.

The native file host supports explicit exclusive ownership. For example:

```swift
let database = try Database.open("example.jed", options: OpenOptions(locking: .exclusive))
try database.compact()
try database.close()
```

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
the progress record for the currently admitted SQL subset.

`Database.create`, `deserialize` and `serialize` provide creation and byte import/
export. `attach(_:source:readOnly:)` and `detach(_:)` manage named memory or file
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

Linux's default `.automatic` locking mode connects the native page-service
owner/client host. `PageServiceOptions` controls serving, ownership preference,
grace periods, connection limits, and idle timeouts. `Database.openRemote` accepts
a `PageTransport` factory or a command array for a native page-service stream.
The command form requires Linux pidfd spawn support (glibc 2.39+); an unsupported
host returns `0A000` and can supply its own transport. Credentials travel in the
protocol hello, not the command arguments. Run the frozen process scenarios with
`mise run swift-native:process`; consult the progress record for verified modes
and outstanding acceptance work. These experimental APIs do not change supported
core delivery or normal repository verification.
