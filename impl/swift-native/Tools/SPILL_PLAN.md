# Native bounded spill primitives

Canonical contracts: frozen `spill.md` §§2–7 and `memory.md` §§6.4, 6.6–6.8.
Implementation is handwritten Swift; scratch records are private per-query data,
not database-format bytes or a generated engine. Existing native and simulated
`ScratchFileHost` supply private files and sealing. Normal storage/pager policy
is unchanged. The public SQL lane remains the acceptance path.

## Integration currency

```swift
internal struct SpillConfig {
    let workMemoryBytes: Int64
    let host: (any ScratchFileHost)?
    let meter: CostMeter
    var canSpill: Bool { host != nil && workMemoryBytes > 0 }
    init(workMemoryBytes: Int64, host: (any ScratchFileHost)?, meter: CostMeter)
}
```

`ExpressionContext.spill` is an optional config (nil means no scratch, no work
budget). `SessionSettings.scratchFileHost` retains the root database's approved
scratch policy. `StatementServices.install` supplies a config from those settings
and its statement meter. Derived/view/subquery contexts keep the same root host
policy while their current meter supplies the account. The Session constructor
sets the host from `database.scratchFileHost`. A memory root does not gain host
filesystem access because an operator grows. Zero work_mem disables spilling,
including rejection-driven spilling, but does not disable query-memory charges.

## Exact private row codec and sequential file

```swift
enum SpillRowCodec {
    static func encode(_ row: [SQLValue]) throws -> [UInt8]
    static func decode(_ bytes: [UInt8]) throws -> [SQLValue]
}
final class SpillRowFile {
    init(host: any ScratchFileHost, kind: String = "rows") throws
    var count: Int64 { get }
    func append(_ row: [SQLValue]) throws
    func cursor() -> Cursor
    func close() throws
}
// Nested in SpillRowFile:
final class Cursor {
    func next() throws -> [SQLValue]?
    func close()
}
```

The codec is self-describing: distinct scalar tags, exact integer/float bits,
decimal sign/scale/limbs, UTF-8 and byte strings, array shape/lower bounds,
composites, ranges and JSON-family values. No formatting/parsing roundtrip or SQL
evaluation. Container decoding checks lengths before allocation and uses an
explicit frame stack where nesting can reach admitted SQL limits. Untouched
slots reach these structures as NULL; no large-value body is read by this codec.
The current SQL row currency contains no unfetched references. Any future lazy
slot currency needs its own opaque retained reference instead of materialization.

`SpillRowFile` uses bounded framed append/read access atop ScratchStore, with
independent positioned cursors and a captured row count. It never collects file
contents. A frame can exceed the fixed I/O buffer; residency permits one current
record. Cursors retain their file but do not close it; the outer owner closes the file.
File cleanup is idempotent, and
I/O/framing failures are 58030. It performs no logical memory charging: its caller
owns rows, sort-run heads, and timing. It is therefore also the external sort run
writer/reader. The sorter owns its bounded run directory and stable merge order.

## Logical charge owner

```swift
final class SpillCharge {
    init(meter: CostMeter)
    var bytes: Int64 { get }
    func tryReserve(_ bytes: Int64) -> Bool
    func reserve(_ bytes: Int64) throws
    func release(_ bytes: Int64)
    func releaseAll()
}
```

`reserve` uses CostMeter.reserveQuery, preserving cost-first failure. Successful
reservations alone increase owned charge. A rejected spillable element is never
charged. A positive reservation precedes the work_mem comparison; spill triggers
only when residency exceeds the budget, not at equality. A state-map shrink
releases exactly the value's logical shrinkage. An I/O failure fails the query;
it never falls back to unbounded resident retention. Cleanup returns only this
owner's charge and closes every scratch file.

## Repeatable stage spool

```swift
final class RowSpool {
    init(config: SpillConfig)
    var count: Int64 { get }
    var isSpilled: Bool { get }
    var residentBytes: Int64 { get }
    func append(_ row: [SQLValue]) throws
    func cursor() throws -> RowSpoolCursor
    func takeOutput() throws -> RowSpoolCursor
    func releaseCharge()
    func close()
}
final class RowSpoolCursor {
    func next() throws -> [SQLValue]?
    func close()
}
```

A row reserves `MemorySizes.row(row)` before retention. With scratch and a
positive budget, account rejection or work_mem overflow spills the prior prefix
plus the new row, preserving source order; the whole resident charge is then
released. A permanently spilled spool reserves nothing further. A normal cursor
seals the append phase, borrows the spool and permits independent repeated passes; its lifetime cannot
extend an already-closed owner. `takeOutput` requires no active borrowers, transfers ownership exactly once and releases the
remaining charge when output completes/closes, matching final emission lifetime.
Operators may call releaseCharge at the specified stage boundary before final
cleanup; it does not silently re-admit retained rows.

## Keyed state and retained rows

```swift
final class SpillStateMap {
    init(config: SpillConfig)
    var isSpilled: Bool { get }
    var residentBytes: Int64 { get }
    func get(_ key: [SQLValue]) throws -> [SQLValue]?
    func put(_ key: [SQLValue], value: [SQLValue]) throws
    func insert(_ key: [SQLValue]) throws -> Bool
    func releaseCharge()
    func close()
}
final class HashRowTable {
    init(config: SpillConfig)
    var isSpilled: Bool { get }
    var residentBytes: Int64 { get }
    func append(hash: UInt64, row: [SQLValue]) throws
    func cursor(hash: UInt64) throws -> HashRowCursor
    func releaseCharge()
    func close()
}
final class HashRowCursor {
    func next() throws -> [SQLValue]?
    func close()
}
```

State keys use the existing canonical `ValueEqualityKey` (decimal scale, floating
zero/NaN, NULL and recursive equality); value rows use the exact private codec.
A new resident state entry reserves ENTRY + sum(value_bytes(key)) +
sum(value_bytes(value)); replacement reserves/releases only value growth/shrink.
Membership uses an empty value tuple. No public key iteration is provided:
first-occurrence/grouping-set output order belongs to a separate RowSpool.

HashRowTable reserves ENTRY + row_bytes(row), retains insertion order under each
full hash, and streams candidates one at a time. The SQL operator compares full
canonical keys and handles NULL admission; a hash collision never establishes
SQL equality. Hot keys remain a bounded linked-record replay, not an array of all
matching rows. Retained aggregate collections use the same interface keyed by
aggregate/group ordinal, preserving global fold order.

Both structures spill permanently after the exact threshold and reserve nothing
further. A private byte-key linear-hashing store has its bucket directory in
scratch and a fixed-size resident allocator directory. Lookups compare full keys;
splits scan one node at a time. Replacements reuse/move value blocks without a
chain of stale key versions. HashRowTable stores append-only forward row chains
with first/last pointers in that store. Its first cursor seals the build phase;
independent per-hash cursors then replay the immutable table without retaining
an uncharged pre-spill prefix. Scratch placement never determines output
order or metered cost; all error cleanup remains inside the query owner.

## Verification and ownership

Host agent owns SpillConfig/SpillCharge/SpillRowCodec/SpillRowFile/RowSpool/
SpillStateMap/HashRowTable/private hash storage and invariant tests. Parent owns
external sort and its exact stage integration; aggregate/dedup/join consumers
will be delegated against these interfaces. Narrow ExpressionContext,
StatementServices, SessionSettings and Session constructor hooks are coordinated
before publication. No Sources edits while the integrated gate runs.

Internal tests prove all value representations roundtrip, >64KiB records,
repeatable independent cursors, exact equality/rejection spill points, replacement
shrink/growth, forced hash collisions/hot keys, bounded resident metadata and
fault/early-close cleanup on native and sealed scratch. Shared ordinary/file and
forced-spill corpus asserts values/errors/costs; query-memory peak comparison is
separate. Tiny forced-spill fixtures do not establish a larger-than-RAM claim.

Initial verification: all five `SpillBoundaryTests` passed in the parent-owned
focused integration gate (native/sealed frames, exact representation, charges
and failure cleanup). The keyed primitives and five `SpillHashBoundaryTests`
then passed the integrated gate as well; all thirteen spill/sorter host tests passed.
The additional malformed free-list link case awaits the following focused gate.
No larger-than-RAM or complete operator acceptance is implied.
