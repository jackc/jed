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
    private mutating func stage(_ record: PackedRecord, net: Bool) throws {
        let bytes = try Self.footprint(record, encrypted: tree.codec.reservedBytes != 0)
        let old = net ? pendingKeys.updateValue(bytes, forKey: record.key) ?? 0 : 0
        pendingBytes = saturatingAdd(pendingBytes, bytes) - old
    }
    private mutating func unstage(_ key: [UInt8]) {
        if let bytes = pendingKeys.removeValue(forKey: key) { pendingBytes -= bytes }
    }
    /// The caller checks SQL uniqueness and constraints; this storage primitive
    /// replaces a key atomically and reports the prior packed version.
    @discardableResult mutating func put(_ record: PackedRecord, netStaging: Bool) throws -> PackedRecord? {
        var changed = self
        let old = try changed.tree.insert(record)
        try changed.stage(record, net: netStaging)
        changed.revision = StorageRevision(); self = changed
        return old
    }
    @discardableResult mutating func remove(_ key: [UInt8]) throws -> PackedRecord? {
        var changed = self
        guard let old = try changed.tree.remove(key) else { return nil }
        changed.unstage(key); changed.revision = StorageRevision(); self = changed
        return old
    }
    /// GIN edits replace one run with zero or more runs. Tracking those physical
    /// records preserves Q3's exact index-version measure without a posting scan.
    @discardableResult mutating func editGIN(term: [UInt8], key: [UInt8], insert: Bool,
                                            netStaging: Bool) throws -> Bool {
        var changed = self, index = try GinIndex(tree: tree)
        let edited = try insert ? index.insert(term: term, key: key) : index.remove(term: term, key: key)
        guard edited else { return false }
        changed.tree = index.tree
        for old in index.lastChanges.removed { changed.unstage(old) }
        for added in index.lastChanges.inserted { try changed.stage(PackedRecord(key: added, cells: []), net: netStaging) }
        changed.revision = StorageRevision(); self = changed
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
