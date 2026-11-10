/// Opaque revision identity. It is never serialized or exposed in SQL.
internal final class StorageRevision: Sendable {}

/// A store owns a packed tree and only the accounting metadata of pending
/// versions. Statement, savepoint, and cursor copies share immutable tree nodes.
internal struct TableStore: Sendable {
    private(set) var tree: BPlusTree
    private(set) var nextRowID: Int64
    private(set) var pendingBytes: Int64 = 0
    private var pendingKeys: [[UInt8]: Int64] = [:]
    private(set) var revision = StorageRevision()

    init(tree: BPlusTree, nextRowID: Int64 = 0) {
        self.tree = tree; self.nextRowID = nextRowID
    }
    var count: Int64? { tree.count }
    func get(_ key: [UInt8]) throws -> PackedRecord? { try tree.get(key) }
    func cursor(lower: TreeBound? = nil, upper: TreeBound? = nil, descending: Bool = false) -> TreeCursor {
        TreeCursor(tree, lower: lower, upper: upper, descending: descending)
    }
    static func footprint(_ record: PackedRecord, encrypted: Bool) throws -> Int64 {
        var bytes = Int64(record.key.count)
        for column in record.cells.indices {
            let cell = record.cells[column]
            bytes = saturatingAdd(bytes, Int64(cell.storedSize))
            if let pending = record.overflows[column] { bytes = saturatingAdd(bytes, Int64(pending.bytes.count)) }
            else if let facts = try cell.largeFacts(encrypted: encrypted), facts.external != nil {
                bytes = saturatingAdd(bytes, Int64(facts.stored))
            }
        }
        return bytes
    }
    /// Never throws, so callers apply it only after every fallible step.
    private mutating func stage(_ key: [UInt8], bytes: Int64, net: Bool) {
        let old = net ? pendingKeys.updateValue(bytes, forKey: key) ?? 0 : 0
        pendingBytes = saturatingAdd(pendingBytes, bytes) - old
    }
    private mutating func unstage(_ key: [UInt8]) {
        if let bytes = pendingKeys.removeValue(forKey: key) { pendingBytes -= bytes }
    }
    /// The caller checks SQL uniqueness and constraints; this storage primitive
    /// replaces a key atomically and reports the prior packed version.
    /// Fallible work runs against a copy of the tree alone. Copying the whole
    /// store instead would alias `pendingKeys` and force a full dictionary
    /// copy on every staged key.
    @discardableResult mutating func put(_ record: PackedRecord, netStaging: Bool) throws -> PackedRecord? {
        var edited = tree
        let old = try edited.insert(record)
        let bytes = try Self.footprint(record, encrypted: tree.codec.reservedBytes != 0)
        tree = edited
        stage(record.key, bytes: bytes, net: netStaging)
        revision = StorageRevision()
        return old
    }
    @discardableResult mutating func remove(_ key: [UInt8]) throws -> PackedRecord? {
        var edited = tree
        guard let old = try edited.remove(key) else { return nil }
        tree = edited
        unstage(key); revision = StorageRevision()
        return old
    }
    /// GIN edits replace one run with zero or more runs. Tracking those physical
    /// records preserves Q3's exact index-version measure without a posting scan.
    @discardableResult mutating func editGIN(term: [UInt8], key: [UInt8], insert: Bool,
                                            netStaging: Bool) throws -> Bool {
        var index = try GinIndex(tree: tree)
        let edited = try insert ? index.insert(term: term, key: key) : index.remove(term: term, key: key)
        guard edited else { return false }
        let encrypted = tree.codec.reservedBytes != 0
        var added: [([UInt8], Int64)] = []
        for key in index.lastChanges.inserted {
            added.append((key, try Self.footprint(PackedRecord(key: key, cells: []), encrypted: encrypted)))
        }
        tree = index.tree
        for old in index.lastChanges.removed { unstage(old) }
        for (key, bytes) in added { stage(key, bytes: bytes, net: netStaging) }
        revision = StorageRevision()
        return true
    }
    mutating func allocateRowID() throws -> Int64 {
        guard nextRowID < Int64.max else { throw JedError("54000", "synthetic row identifier exhausted") }
        let result = nextRowID; nextRowID += 1; revision = StorageRevision(); return result
    }
    mutating func forgetPendingKeys() { pendingKeys.removeAll(keepingCapacity: false) }
    mutating func clearPending() { pendingBytes = 0; forgetPendingKeys() }
    mutating func installPersistedTree(_ tree: BPlusTree) { self.tree = tree; clearPending() }
    func truncated() -> TableStore {
        TableStore(tree: BPlusTree(codec: tree.codec, types: tree.types, pool: tree.pool), nextRowID: nextRowID)
    }
}
