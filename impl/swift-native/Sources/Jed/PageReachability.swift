/// Reachability is maintenance work, never SQL scan cost. Incremental accounting
/// skips shared subtrees; periodic reclamation walks packed bytes one page at a
/// time and never materializes SQL values or whole overflow payloads.
internal enum PageReachability {
    static func orphaned(_ snapshot: DomainSnapshot, kept: Set<UInt32>, keptChains: Set<UInt32>,
                         pageCount: UInt32) throws -> UInt64 {
        let codec = snapshot.pool.codec, slab = codec.capacity - (codec.reservedBytes == 0 ? 0 : 16)
        func count(_ child: TreeChild, tree: BPlusTree) throws -> UInt64 {
            guard let reference = child.reference, !kept.contains(reference.index) else { return 0 }
            if case .resident(let node) = child, !node.isLeaf {
                var total: UInt64 = 1
                for child in node.children { total += try count(child, tree: tree) }
                return total
            }
            guard tree.types.contains(where: { ValueCodec.fixedWidth($0.type) == nil }) else { return 1 }
            let leaf = try tree.resolve(child)
            var total: UInt64 = 1
            for row in 0..<leaf.entryCount {
                for cell in leaf.record(at: row).cells {
                    if let facts = try cell.largeFacts(encrypted: codec.reservedBytes != 0), let head = facts.external,
                       !keptChains.contains(head.index) {
                        guard facts.stored > 0, head.index >= codec.firstBodyPage, head.index < pageCount,
                              UInt64(facts.stored) <= UInt64(pageCount) * UInt64(slab) else {
                            throw JedError.corruption("invalid orphaned overflow pointer")
                        }
                        total += (UInt64(facts.stored) + UInt64(slab) - 1) / UInt64(slab)
                    }
                }
            }
            return total
        }
        var total: UInt64 = 0
        for name in snapshot.tables.keys.sorted() {
            let tree = snapshot.tables[name]!.tree
            if let root = tree.root { total += try count(root, tree: tree) }
        }
        for name in snapshot.indexes.keys.sorted() where snapshot.gistTrees[name] == nil {
            let tree = snapshot.indexes[name]!.tree
            if let root = tree.root { total += try count(root, tree: tree) }
        }
        return total
    }

    static func collect(_ snapshot: DomainSnapshot, catalogPages: [UInt32], gistPages: [UInt32],
                        pageCount: UInt32, readAllPages: Bool = false, read: (PageReference) throws -> [UInt8]) throws -> Set<UInt32> {
        let codec = snapshot.pool.codec, encrypted = codec.reservedBytes != 0
        let start = encrypted ? 32 : 16, slab = codec.capacity - (encrypted ? 16 : 0)
        var reached: Set<UInt32> = []
        func add(_ page: UInt32) throws {
            guard page >= codec.firstBodyPage, page < pageCount, reached.insert(page).inserted else {
                throw JedError.corruption("invalid or multiply referenced live page")
            }
        }
        func overflow(_ head: PageReference, length: Int) throws {
            guard length > 0, UInt64(length) <= UInt64(pageCount) * UInt64(slab) else {
                throw JedError.corruption("invalid live overflow length")
            }
            var current = head, remaining = length
            while remaining > 0 {
                try add(current.index)
                let bytes = try read(current)
                guard bytes.count == codec.pageSize - codec.reservedBytes, bytes[0] == 4,
                      try PageEnvelope.readU32(bytes, at: 12) == PageEnvelope.checksum(bytes, at: 12) else {
                    throw JedError.corruption("invalid live overflow page")
                }
                try PageEnvelope.validateHeader(bytes, encrypted: encrypted)
                let count = Int(try PageEnvelope.readU32(bytes, at: 4)), next = try PageEnvelope.readU32(bytes, at: 8)
                guard count == min(slab, remaining), bytes[(start + count)...].allSatisfy({ $0 == 0 }) else {
                    throw JedError.corruption("invalid live overflow slab")
                }
                remaining -= count
                let tag = encrypted ? Array(bytes[16..<32]) : []
                guard (remaining == 0) == (next == 0), next != 0 || tag.allSatisfy({ $0 == 0 }) else {
                    throw JedError.corruption("invalid live overflow terminator")
                }
                current = PageReference(next, tag: tag)
            }
        }
        func tree(_ child: TreeChild, types: [TypeDescriptor]) throws {
            guard let reference = child.reference else { throw JedError.corruption("unpersisted node in live-page walk") }
            try add(reference.index)
            let checked = readAllPages ? try read(reference) : nil
            if case .resident(let node) = child, !node.isLeaf {
                for child in node.children { try tree(child, types: types) }
                return
            }
            guard types.contains(where: { ValueCodec.fixedWidth($0.type) == nil }) else { return }
            let leaf = try PackedLeaf(bytes: checked ?? read(reference), types: types, codec: codec)
            for row in 0..<leaf.count {
                for column in types.indices {
                    if let facts = try leaf.cell(row: row, column: column).largeFacts(encrypted: encrypted), let head = facts.external {
                        try overflow(head, length: facts.stored)
                    }
                }
            }
        }
        for page in catalogPages { try add(page) }
        for page in gistPages { try add(page) }
        for name in snapshot.tables.keys.sorted() {
            let store = snapshot.tables[name]!
            if let root = store.tree.root { try tree(root, types: store.tree.types) }
        }
        for name in snapshot.indexes.keys.sorted() where snapshot.gistTrees[name] == nil {
            if let root = snapshot.indexes[name]!.tree.root { try tree(root, types: []) }
        }
        return reached
    }
}
