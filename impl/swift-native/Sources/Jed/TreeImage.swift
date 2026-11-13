internal extension BPlusTree {
    /// A streaming from-scratch walk. The output owns no source nodes/cache and
    /// the walk retains no destination leaves or database-wide page collection.
    func writeImage(allocator: inout PageAllocator, codec target: any PageCodec,
                    entropy: KeyEntropy?, sink: (StoredPage) throws -> Void) throws -> PageReference {
        guard target.pageSize == codec.pageSize, target.reservedBytes == codec.reservedBytes else {
            throw JedError("0A000", "image layout change requires rebuilding records first")
        }
        func visit(_ child: TreeChild) throws -> PageReference {
            let node = try resolve(child)
            if !node.isLeaf {
                var children: [PageReference] = []
                for child in node.children { children.append(try visit(child)) }
                let payload = try PackedInterior.encodePayload(keys: node.keys, children: children, codec: target)
                let sealed = try PageSealer.seal(codec: target, index: allocator.allocate(), type: 3,
                    count: UInt32(node.entryCount), payload: payload, entropy: entropy)
                try sink(sealed.page); return sealed.reference
            }
            guard node.entryCount > 0 else { throw JedError.corruption("cannot export an empty leaf") }
            // The canonical image gives the leaf its id before its chains,
            // while the nonce stream still seals chains before that leaf.
            let index = try allocator.allocate()
            var records: [PackedRecord] = []
            for row in 0..<node.entryCount {
                let record = node.record(at: row)
                var cells = record.cells
                for column in cells.indices {
                    let head: PageReference, stored: Int, raw: Int, compressed: Bool
                    if let pending = record.overflows[column] {
                        head = try ImageOverflow.fresh(pending.bytes, allocator: &allocator, codec: target, entropy: entropy, sink: sink)
                        stored = pending.bytes.count; raw = pending.rawLength; compressed = pending.compressed
                    } else if let facts = try cells[column].largeFacts(encrypted: codec.reservedBytes != 0), let old = facts.external {
                        guard let pool else { throw JedError.corruption("inherited image overflow has no source") }
                        head = try ImageOverflow.copy(old, length: facts.stored,
                            pageCount: pool.pager?.selected.meta.pageCount ?? UInt32.max, pool: pool,
                            allocator: &allocator, codec: target, entropy: entropy, sink: sink)
                        stored = facts.stored; raw = facts.raw; compressed = facts.compressed
                    } else { continue }
                    let bytes = try ImageOverflow.pointer(head, stored: stored, raw: raw, compressed: compressed)
                    cells[column] = StoredCell(bytes: bytes[...], width: 0)
                }
                records.append(PackedRecord(key: record.key, cells: cells))
            }
            let payload = try PackedLeaf.encodePayload(records, types: types, capacity: target.capacity, layout: layout)
            let sealed = try PageSealer.seal(codec: target, index: index, type: 2,
                count: UInt32(node.entryCount), payload: payload, entropy: entropy)
            try sink(sealed.page); return sealed.reference
        }
        return try root.map(visit) ?? PageSealer.nullReference(target)
    }
}
