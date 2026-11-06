/// Simulation-only structural oracles. Recovery uses an isolated byte copy;
/// live checks walk the exact committed resident nodes owned by the process.
internal enum SimIntegrity {
    struct Image {
        let version: UInt64
        let livePages: UInt32
        let pageCount: UInt32
        let freePages: [UInt32]
        let reachable: Set<UInt32>
        let protectedPages: Set<UInt32>
        let pointerKinds: [String: [UInt32]]
    }

    static func image(bytes: [UInt8], credential: Credential?, environment: Environment) throws -> Image {
        let copy = MemoryBlockStore()
        try copy.write(at: 0, bytes: bytes)
        let storage = try StorageDomain.open(store: copy, catalogID: 0, credential: credential, readOnly: true,
            collatedKey: { try environment.unicode.key($0, collation: $1) },
            collationVerdict: { try CatalogEnvironment.verdict($0, environment: environment) },
            validateCatalog: { try CatalogEnvironment.validate($0, environment: environment) })
        let database = Database(core: DatabaseCore(storage: storage), environment: environment)
        defer { try? database.close() }
        let report = try database.integrityCheck(options: .init(maxFindings: 0))
        guard report.ok else {
            throw violation(report.findings.map { $0.kind + ": " + $0.detail }.joined(separator: "; "))
        }
        let selected = storage.pager.selected, meta = selected.meta
        let free = try FreeList.load(meta: meta, protected: selected.protectedPages, read: storage.pager.readBody)
        var walk = Walker(pager: storage.pager, pageCount: meta.pageCount, free: Set(free.entries))
        try walk.catalog(storage.committed.catalog, root: meta.root)
        try walk.trees(storage.committed)
        guard walk.reached.count == meta.livePages else { throw violation("live count differs from reachable pages") }
        guard Set(free.entries).count == free.entries.count,
              Set(free.entries).isDisjoint(with: selected.protectedPages) else { throw violation("free list overlaps manifest dependencies") }
        walk.kinds["free"] = free.chain.first.map { [$0] } ?? []
        walk.kinds["free_next"] = Array(free.chain.dropFirst())
        walk.kinds["manifest"] = selected.chain
        return Image(version: meta.txid, livePages: meta.livePages, pageCount: meta.pageCount,
            freePages: free.entries, reachable: walk.reached, protectedPages: selected.protectedPages,
            pointerKinds: walk.kinds.mapValues { $0.sorted() })
    }

    static func live(_ storage: StorageDomain) throws {
        let allocation = try storage.pager.allocationSnapshot(reuseAllowed: true)
        let snapshot = storage.committed
        var walk = Walker(pager: storage.pager, pageCount: allocation.highWater, free: Set(allocation.persistedFree))
        // Catalog/GiST pages have no retained TreeChild identity. Their root
        // references remain in the same committed catalog as the resident trees.
        try walk.catalog(snapshot.catalog, root: storage.pager.selected.meta.root)
        try walk.trees(snapshot)
        guard walk.reached.count == storage.livePages else { throw violation("live resident count differs from committed count") }
    }

    private static func violation(_ message: String) -> JedError {
        JedError.corruption("simulation integrity: " + message)
    }

    private struct Walker {
        let pager: Pager
        let pageCount: UInt32
        let free: Set<UInt32>
        var reached: Set<UInt32> = []
        var kinds: [String: [UInt32]] = [:]
        var encrypted: Bool { pager.codec.reservedBytes != 0 }

        mutating func add(_ reference: PageReference, kind: String) throws {
            guard reference.index >= pager.codec.firstBodyPage, reference.index < pageCount,
                  !free.contains(reference.index), reached.insert(reference.index).inserted else {
                throw violation("page is outside the live address space, free, or multiply owned")
            }
            kinds[kind, default: []].append(reference.index)
        }
        mutating func catalog(_ image: CatalogImage, root: PageReference) throws {
            var current = root, first = true
            while current.index != 0 {
                try add(current, kind: first ? "catalog" : "catalog_next"); first = false
                let bytes = try pager.readBody(current)
                guard bytes[0] == 1 else { throw violation("catalog pointer names a non-catalog page") }
                current = PageReference(try PageEnvelope.readU32(bytes, at: 8), tag: encrypted ? Array(bytes[16..<32]) : [])
            }
            for table in image.tables.sorted(by: { $0.name < $1.name }) {
                for index in table.indexes.sorted(by: { $0.name < $1.name }) where index.kind == 2 && index.root.index != 0 {
                    try gist(index.root, kind: "root", depth: 0)
                }
            }
        }
        mutating func gist(_ reference: PageReference, kind: String, depth: Int) throws {
            guard depth < GistTree.maximumDepth else { throw violation("excessive GiST depth") }
            try add(reference, kind: kind)
            let bytes = try pager.readBody(reference)
            guard bytes[0] == 5 || bytes[0] == 6 else { throw violation("GiST pointer names another page kind") }
            if bytes[0] == 5 { return }
            var reader = ByteReader(bytes[16...])
            let count = Int(try PageEnvelope.readU32(bytes, at: 4))
            guard count > 0, count <= GistTree.fanout else { throw violation("invalid GiST fanout") }
            for _ in 0..<count {
                let length = Int(try reader.u16()); _ = try reader.take(length)
                let child = PageReference(try reader.u32(), tag: Array(try reader.take(encrypted ? 16 : 0)))
                try gist(child, kind: "gist_child", depth: depth + 1)
            }
        }
        mutating func trees(_ snapshot: DomainSnapshot) throws {
            for name in snapshot.tables.keys.sorted() {
                let tree = snapshot.tables[name]!.tree
                if let root = tree.root { try node(root, tree: tree, kind: "root", depth: 0) }
            }
            for name in snapshot.indexes.keys.sorted() where snapshot.gistTrees[name] == nil {
                let tree = snapshot.indexes[name]!.tree
                if let root = tree.root { try node(root, tree: tree, kind: "root", depth: 0) }
            }
        }
        mutating func node(_ child: TreeChild, tree: BPlusTree, kind: String, depth: Int) throws {
            guard depth < BPlusTree.maximumDepth, let reference = child.reference else {
                throw violation("committed tree has an unpersisted or excessively deep node")
            }
            try add(reference, kind: kind)
            // Resolving a resident leaf must retain that leaf's own value
            // references. Re-reading its page here would miss stale residents.
            let resident = try tree.resolve(child)
            if resident.isLeaf {
                guard resident.pending.keys.allSatisfy({ $0 >= 0 && $0 < resident.entryCount }) else {
                    throw violation("resident overflow cache names an absent row")
                }
                for row in 0..<resident.entryCount {
                    let record = resident.record(at: row)
                    guard record.overflows.keys.allSatisfy({ record.cells.indices.contains($0) }) else {
                        throw violation("resident overflow cache names an absent column")
                    }
                    for (column, cell) in record.cells.enumerated() {
                        let retained = record.overflows[column]
                        if let facts = try cell.largeFacts(encrypted: encrypted), let head = facts.external {
                            // A committed root may keep its already-resident
                            // payload cache. Its sealed cell still owns a real
                            // chain, and that chain must equal the cached bytes.
                            if let retained {
                                guard retained.bytes.count == facts.stored, retained.rawLength == facts.raw,
                                      retained.compressed == facts.compressed else {
                                    throw violation("resident overflow cache metadata differs from its cell")
                                }
                            }
                            try overflow(head, length: facts.stored, compressed: facts.compressed, expected: retained?.bytes)
                        } else if retained != nil {
                            throw violation("resident overflow cache has no external cell")
                        }
                    }
                }
            } else {
                for next in resident.children { try node(next, tree: tree, kind: "child", depth: depth + 1) }
            }
        }
        mutating func overflow(_ head: PageReference, length: Int, compressed: Bool, expected: [UInt8]? = nil) throws {
            let slab = pager.codec.capacity - (encrypted ? 16 : 0), start = encrypted ? 32 : 16
            guard length > 0, UInt64(length) <= UInt64(pageCount) * UInt64(slab) else { throw violation("invalid overflow length") }
            var current = head, remaining = length, first = true
            while remaining > 0 {
                try add(current, kind: first ? (compressed ? "overflow_lz4" : "overflow") : "overflow_next")
                first = false
                let bytes = try pager.readBody(current)
                guard bytes[0] == 4 else { throw violation("overflow pointer names another page kind") }
                let count = Int(try PageEnvelope.readU32(bytes, at: 4)), next = try PageEnvelope.readU32(bytes, at: 8)
                guard count == min(remaining, slab), bytes[(start + count)...].allSatisfy({ $0 == 0 }) else {
                    throw violation("overflow slab length or padding is invalid")
                }
                if let expected, !bytes[start..<(start + count)].elementsEqual(expected[(length - remaining)..<(length - remaining + count)]) {
                    throw violation("resident overflow cache differs from its committed chain")
                }
                remaining -= count
                let tag = encrypted ? Array(bytes[16..<32]) : []
                guard (remaining == 0) == (next == 0), next != 0 || tag.allSatisfy({ $0 == 0 }) else {
                    throw violation("overflow termination differs from its value length")
                }
                current = PageReference(next, tag: tag)
            }
        }
    }

    private static func checkRows(_ snapshot: DomainSnapshot, pager: Pager, pageCount: UInt32, environment: Environment) throws {
        let collated: KeyEncoding.CollatedKey = { try environment.unicode.key($0, collation: $1) }
        for table in snapshot.catalog.tables.sorted(by: { $0.name < $1.name }) {
            guard let store = snapshot.tables[table.name] else { throw violation("catalog table has no store") }
            // These are the plain column B-tree indexes required by §8.3.
            // Expression/partial indexes retain their separate SQL oracle.
            let indexes = table.indexes.filter { index in
                index.kind == 0 && index.predicate == nil && index.keys.allSatisfy { if case .column = $0 { return true }; return false }
            }.sorted { $0.name < $1.name }
            var expected = Array(repeating: [[UInt8]](), count: indexes.count)
            var cursor = store.cursor(); defer { cursor.close() }
            var previous: [UInt8]?, count: Int64 = 0
            while let record = try cursor.next() {
                guard previous.map({ $0.lexicographicallyPrecedes(record.key) }) ?? true else { throw violation("table storage keys are not strictly increasing") }
                previous = record.key; count += 1
                let values = try record.cells.indices.map { try record.materialize(column: $0, pager: pager, pageCount: pageCount) }
                if !table.primaryKey.isEmpty {
                    var key: [UInt8] = []
                    for column in table.primaryKey {
                        key += try KeyEncoding.encode(values[column], type: store.tree.types[column], collatedKey: collated)
                    }
                    guard key == record.key else { throw violation("row primary key differs from storage key") }
                }
                for ordinal in indexes.indices {
                    let index = indexes[ordinal]
                    var key: [UInt8] = []
                    for (position, item) in index.keys.enumerated() {
                        guard case .column(let column) = item else { throw violation("non-column key in plain index check") }
                        let option = index.keyOptions.isEmpty ? 0 : index.keyOptions[position]
                        key += try KeyEncoding.encodeSlot(values[column], type: store.tree.types[column],
                            descending: option & 1 != 0, nullsFirst: option & 2 != 0, collatedKey: collated)
                    }
                    expected[ordinal].append(key + record.key)
                }
            }
            guard count == table.rowCount else { throw violation("table row count differs from its leaf total") }
            for ordinal in indexes.indices {
                guard let indexStore = snapshot.indexes[indexes[ordinal].name] else { throw violation("catalog index has no store") }
                expected[ordinal].sort { $0.lexicographicallyPrecedes($1) }
                var indexCursor = indexStore.cursor(); defer { indexCursor.close() }
                var position = 0
                while let record = try indexCursor.next() {
                    guard position < expected[ordinal].count, record.key == expected[ordinal][position] else {
                        throw violation("plain column index differs from table-derived entries")
                    }
                    position += 1
                }
                guard position == expected[ordinal].count else { throw violation("plain column index omits table-derived entries") }
            }
        }
    }
}
