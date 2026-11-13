/// Immutable writer records retain encoded cell spans and pending overflow
/// bytes, not decoded SQL values. Physical PAX bytes are constructed only when
/// serializing a page; eligible inline spans remain available to read kernels.
/// A retained root can keep this form after commit, like its pending values.
internal final class RecordLeaf: Sendable {
    let records: RecordList
    let layout: LeafLayout
    let types: [TypeDescriptor]
    let pending: [Int: [Int: PendingOverflow]]
    let payloadSize: Int

    // Insert admission and packed reconstruction supply the declared tree
    // descriptors; split/merge and overflow sealing retain that canonical form.
    init(_ records: [PackedRecord], types: [TypeDescriptor], layout: LeafLayout = .index) throws {
        guard records.allSatisfy({ $0.cells.count == types.count }),
              zip(records, records.dropFirst()).allSatisfy({ $0.key.lexicographicallyPrecedes($1.key) }) else {
            throw JedError.corruption("invalid packed leaf records")
        }
        var canonical = records, pending: [Int: [Int: PendingOverflow]] = [:]
        for (row, record) in records.enumerated() {
            var normalized: [StoredCell]?
            for column in types.indices {
                let cell = record.cells[column]
                guard let bytes = cell.bytes else { continue }
                if let width = LeafValue.width(types[column]) {
                    guard bytes.count == width else { throw JedError.corruption("invalid fixed-width cell") }
                } else if let tag = bytes.first {
                    guard tag == 0 || tag == 2 || tag == 3 || tag == 4 || tag == 5 else {
                        throw JedError.corruption("invalid leaf value presence tag")
                    }
                } else {
                    // An empty variable span is the on-disk NULL representation.
                    // Preserve the normalization formerly supplied by packing.
                    if normalized == nil { normalized = record.cells }
                    normalized![column] = StoredCell(bytes: nil, width: 0)
                }
            }
            if let normalized {
                canonical[row] = PackedRecord(key: record.key, cells: normalized, overflows: record.overflows)
            }
            if !record.overflows.isEmpty { pending[row] = record.overflows }
        }
        let total = PackedLeaf.measure(canonical, types: types, layout: layout)
        guard total <= UInt32.max else { throw JedError("0A000", "pending leaf exceeds the byte-format limit") }
        self.records = RecordList(canonical); self.types = types; self.layout = layout; self.pending = pending; payloadSize = total
    }

    /// Records already admitted by the validating initializer (or read from a
    /// validated page), in key order, with their exact payload size.
    private init(admitted records: RecordList, types: [TypeDescriptor],
                 pending: [Int: [Int: PendingOverflow]], payloadSize: Int, layout: LeafLayout) {
        self.records = records; self.types = types; self.layout = layout; self.pending = pending; self.payloadSize = payloadSize
    }

    /// Validate and normalize one incoming record exactly as the full
    /// initializer does for every record.
    static func admit(_ record: PackedRecord, types: [TypeDescriptor]) throws -> PackedRecord {
        guard record.cells.count == types.count else {
            throw JedError("0A000", "record differs from its tree's type contract")
        }
        var normalized: [StoredCell]?
        for column in types.indices {
            let cell = record.cells[column]
            let width = LeafValue.width(types[column])
            guard Int(cell.width) == (width ?? 0) else {
                throw JedError("0A000", "record differs from its tree's type contract")
            }
            guard let bytes = cell.bytes else { continue }
            if let width {
                guard bytes.count == width else { throw JedError.corruption("invalid fixed-width cell") }
            } else if let tag = bytes.first {
                guard tag == 0 || tag == 2 || tag == 3 || tag == 4 || tag == 5 else {
                    throw JedError.corruption("invalid leaf value presence tag")
                }
            } else {
                if normalized == nil { normalized = record.cells }
                normalized![column] = StoredCell(bytes: nil, width: 0)
            }
        }
        guard let normalized else { return record }
        return PackedRecord(key: record.key, cells: normalized, overflows: record.overflows)
    }

    /// A validated clean page as writer records, without re-validating it.
    /// Retain its exact compact payload size; its accessor already reports an
    /// empty span as NULL.
    convenience init(adopting leaf: PackedLeaf, pending: [Int: [Int: PendingOverflow]]) {
        var records: [PackedRecord] = []
        records.reserveCapacity(leaf.count)
        for row in 0..<leaf.count {
            let record = leaf.record(at: row)
            if let overflows = pending[row] {
                records.append(PackedRecord(key: record.key, cells: record.cells, overflows: overflows))
            } else { records.append(record) }
        }
        self.init(admitted: RecordList(records), types: leaf.types, pending: pending, payloadSize: leaf.payloadSize, layout: leaf.layout)
    }

    private static func rekeyed(_ pending: [Int: [Int: PendingOverflow]], from row: Int, by delta: Int,
                                dropping: Int? = nil) -> [Int: [Int: PendingOverflow]] {
        if pending.isEmpty { return pending }
        var result: [Int: [Int: PendingOverflow]] = [:]
        for (index, value) in pending where index != dropping { result[index >= row ? index + delta : index] = value }
        return result
    }

    /// This leaf with one admitted record placed at `slot`: replacing the
    /// record there, or inserted before it. Existing records keep their admission,
    /// but compact region selection requires measuring the resulting leaf again.
    func placing(_ record: PackedRecord, at slot: Int, replacing: Bool) throws -> RecordLeaf {
        var edited = records, moved = pending
        if replacing {
            edited.replace(at: slot, with: record)
            if moved[slot] != nil { moved[slot] = nil }
        } else {
            edited.insert(record, at: slot)
            moved = Self.rekeyed(pending, from: slot, by: 1)
        }
        let total = PackedLeaf.measure(edited.flat, types: types, layout: layout)
        guard total <= UInt32.max else { throw JedError("0A000", "pending leaf exceeds the byte-format limit") }
        if !record.overflows.isEmpty { moved[slot] = record.overflows }
        return RecordLeaf(admitted: edited, types: types, pending: moved, payloadSize: total, layout: layout)
    }

    /// This leaf without the record at `slot`.
    func removing(at slot: Int) -> RecordLeaf {
        var edited = records
        _ = edited.remove(at: slot)
        let total = PackedLeaf.measure(edited.flat, types: types, layout: layout)
        return RecordLeaf(admitted: edited, types: types, pending: Self.rekeyed(pending, from: slot, by: -1, dropping: slot),
                          payloadSize: total, layout: layout)
    }
}
