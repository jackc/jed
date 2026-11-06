/// Immutable writer records retain encoded cell spans and pending overflow
/// bytes, not decoded SQL values. Physical PAX bytes are constructed only when
/// serializing a page; eligible inline spans remain available to read kernels.
/// A retained root can keep this form after commit, like its pending values.
internal final class RecordLeaf: Sendable {
    let records: [PackedRecord]
    let pending: [Int: [Int: PendingOverflow]]
    let payloadSize: Int

    // Insert admission and packed reconstruction supply the declared tree
    // descriptors; split/merge and overflow sealing retain that canonical form.
    init(_ records: [PackedRecord], types: [TypeDescriptor]) throws {
        let total = records.reduce(PackedLeaf.overhead(records: records.count, types: types)) { $0 + $1.weight }
        guard total <= UInt32.max else { throw JedError("0A000", "pending leaf exceeds the byte-format limit") }
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
                if let width = ValueCodec.fixedWidth(types[column].type) {
                    guard bytes.count == width else { throw JedError.corruption("invalid fixed-width cell") }
                } else if let tag = bytes.first {
                    guard tag == 0 || tag == 2 || tag == 3 || tag == 4 else {
                        throw JedError.corruption("invalid leaf value presence tag")
                    }
                } else {
                    // An empty variable span is the on-disk NULL representation.
                    // Preserve the normalization formerly supplied by packing.
                    if normalized == nil { normalized = record.cells }
                    normalized![column] = StoredCell(bytes: nil, type: types[column])
                }
            }
            if let normalized {
                canonical[row] = PackedRecord(key: record.key, cells: normalized, overflows: record.overflows)
            }
            if !record.overflows.isEmpty { pending[row] = record.overflows }
        }
        self.records = canonical; self.pending = pending; payloadSize = total
    }
}
