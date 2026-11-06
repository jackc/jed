internal enum StorageDomainKind: Sendable { case file, memory }

internal struct DomainCommitPolicy: Sendable {
    /// Pass the prospective new txid when the reader registry is empty.
    var oldestPinnedVersion: UInt64 = .max
    var canCompactCommitted = true
    var reclaimAllowed = true
    var storageLimit: Int64 = 0
    var domainName: SQLText = SQLText(validatedUTF8: Array("main".utf8))
}

internal struct PreparedDomainCommit: Sendable {
    let identity: StorageRevision
    let pages: PreparedPageCommit
    let snapshot: DomainSnapshot
    let catalogPages: [UInt32]
    let gistPages: [UInt32]
    let livePages: UInt32
    let freeGeneration: UInt64
    let liveAtCompaction: UInt32
    let written: Set<UInt32>
}

internal struct DomainStorageState: Sendable {
    var snapshot: DomainSnapshot
    var catalogPages: [UInt32]
    var gistPages: [UInt32]
    var livePages: UInt32
    var liveAtCompaction: UInt32
    var freeGeneration: UInt64
    var lastWritten: Set<UInt32>
    // Memory bootstrap/open has no last commit for Q4 repair; files use their loaded catalog size.
    var repairCatalogPages: Int
}

internal struct DomainBodyLayout {
    let catalog: CatalogImage
    let root: PageReference
    let allocation: PageAllocator
    let written: Set<UInt32>
    let catalogPages: [UInt32]
    let gistPages: [UInt32]
    let kept: Set<UInt32>
    let keptChains: Set<UInt32>
}

internal struct DomainBodyWrite {
    let snapshot: DomainSnapshot
    let root: PageReference
    let allocation: PageAllocator
    let pages: [StoredPage]
    let catalogPages: [UInt32]
    let gistPages: [UInt32]
    let kept: Set<UInt32>
    let keptChains: Set<UInt32>
}

/// Handwritten structural preflight and serialization compose the same existing
/// tree/chain primitives. A dry layout draws no entropy and performs no writes.
internal enum DomainCommitPlanner {
    static func layout(_ snapshot: DomainSnapshot, highWater: UInt32, free: [UInt32]) throws -> DomainBodyLayout {
        let codec = snapshot.pool.codec, slab = codec.capacity - (codec.reservedBytes == 0 ? 0 : 16)
        var allocation = try PageAllocator(highWater: highWater, free: free, firstBody: codec.firstBodyPage)
        var written: Set<UInt32> = [], kept: Set<UInt32> = [], chains: Set<UInt32> = [], gistPages: [UInt32] = []
        let zero = PageSealer.nullReference(codec)
        func allocate() throws -> PageReference {
            let page = try allocation.allocate(); written.insert(page)
            return PageReference(page, tag: zero.tag)
        }
        func visitTree(_ child: TreeChild, source: BPlusTree) throws -> PageReference {
            if let reference = child.reference { kept.insert(reference.index); return reference }
            let node = try source.resolve(child)
            for child in node.children { _ = try visitTree(child, source: source) }
            if node.isLeaf {
                guard node.entryCount > 0 else { throw JedError.corruption("cannot plan an empty leaf") }
                for row in 0..<node.entryCount {
                    let record = node.record(at: row)
                    for column in record.cells.indices {
                        if let fresh = record.overflows[column] {
                            guard !fresh.bytes.isEmpty else { throw JedError.corruption("empty fresh overflow") }
                            for _ in 0..<((fresh.bytes.count + slab - 1) / slab) { _ = try allocate() }
                        } else if let facts = try record.cells[column].largeFacts(encrypted: codec.reservedBytes != 0), let head = facts.external {
                            chains.insert(head.index)
                        }
                    }
                }
            }
            return try allocate()
        }
        func gist(_ node: GistNode, ops: [GistOpclass]) throws -> PageReference {
            var bytes = 0
            if node.isLeaf {
                for entry in node.entries {
                    let bound = try GistTuple.encode(entry.bound, ops: ops)
                    guard bound.count <= UInt16.max, entry.storageKey.count <= UInt16.max else { throw JedError.corruption("oversized GiST entry") }
                    bytes += 4 + bound.count + entry.storageKey.count
                }
            } else {
                for child in node.children {
                    _ = try gist(child, ops: ops)
                    bytes += 2 + (try GistTuple.encode(child.bound, ops: ops)).count + 4 + zero.tag.count
                }
            }
            guard node.entryCount > 0, node.entryCount <= GistTree.fanout, bytes <= codec.capacity else {
                throw JedError.corruption("GiST node exceeds page capacity")
            }
            let reference = try allocate(); gistPages.append(reference.index); return reference
        }
        var image = snapshot.catalog
        try image.validateReferences(catalogID: snapshot.catalogID, borrowedTypes: snapshot.borrowedTypes, borrowedReferences: snapshot.borrowedReferences)
        guard Set(image.tables.map(\.name)) == Set(snapshot.tables.keys) else { throw JedError.corruption("catalog and table stores disagree") }
        var names: Set<SQLText> = []
        for position in image.tables.indices.sorted(by: { image.tables[$0].name < image.tables[$1].name }) {
            var table = image.tables[position]
            guard let store = snapshot.tables[table.name], let count = store.count, count >= 0 else { throw JedError.corruption("table has no exact cardinality") }
            let types = try snapshot.tableTypes(table)
            guard types == store.tree.types, store.tree.pool === snapshot.pool else { throw JedError.corruption("table row layout disagrees with catalog") }
            table.rowCount = count
            table.root = try store.tree.root.map { try visitTree($0, source: store.tree) } ?? zero
            table.indexes.sort { $0.name < $1.name }
            for ordinal in table.indexes.indices {
                let index = table.indexes[ordinal]
                guard names.insert(index.name).inserted, let store = snapshot.indexes[index.name], store.tree.types.isEmpty else {
                    throw JedError.corruption("missing or duplicate index store")
                }
                if index.kind == 2 {
                    guard let value = snapshot.gistTrees[index.name], Int64(value.count) == store.count else { throw JedError.corruption("GiST leaf store disagrees with canonical tree") }
                    table.indexes[ordinal].root = try value.root.map { try gist($0, ops: value.opclasses) } ?? zero
                } else {
                    guard store.tree.pool === snapshot.pool else { throw JedError.corruption("index store belongs to another storage domain") }
                    table.indexes[ordinal].root = try store.tree.root.map { try visitTree($0, source: store.tree) } ?? zero
                }
            }
            image.tables[position] = table
        }
        guard names == Set(snapshot.indexes.keys), Set(snapshot.gistTrees.keys) == Set(image.tables.flatMap { $0.indexes.filter { $0.kind == 2 }.map(\.name) }) else {
            throw JedError.corruption("orphaned index store")
        }
        // Validate all catalog strings and metadata before the first page seal.
        let count = try CatalogChain.pageCount(image.entries(), codec: codec)
        var catalogPages: [UInt32] = []
        for _ in 0..<count { catalogPages.append(try allocate().index) }
        return DomainBodyLayout(catalog: image, root: PageReference(catalogPages[0], tag: zero.tag),
            allocation: allocation, written: written, catalogPages: catalogPages, gistPages: gistPages, kept: kept, keptChains: chains)
    }

    static func write(_ snapshot: DomainSnapshot, highWater: UInt32, free: [UInt32],
                      entropy: KeyEntropy?) throws -> DomainBodyWrite {
        let codec = snapshot.pool.codec
        var allocation = try PageAllocator(highWater: highWater, free: free, firstBody: codec.firstBodyPage)
        var image = snapshot.catalog, tables = snapshot.tables, indexes = snapshot.indexes
        var pages: [StoredPage] = [], kept: Set<UInt32> = [], chains: Set<UInt32> = [], gistPages: [UInt32] = []
        func append(_ result: TreeWrite) {
            pages.append(contentsOf: result.pages); kept.formUnion(result.keptRoots)
            chains.formUnion(result.keptOverflowHeads.map(\.index))
        }
        for position in image.tables.indices.sorted(by: { image.tables[$0].name < image.tables[$1].name }) {
            var table = image.tables[position], store = tables[image.tables[position].name]!
            let result = try store.tree.serialize(allocator: &allocation, entropy: entropy)
            append(result); store.installPersistedTree(result.tree); tables[table.name] = store
            table.root = result.root; table.rowCount = store.count!
            table.indexes.sort { $0.name < $1.name }
            for ordinal in table.indexes.indices {
                let name = table.indexes[ordinal].name
                if table.indexes[ordinal].kind == 2 {
                    let result = try snapshot.gistTrees[name]!.serialize(allocator: &allocation, codec: codec, entropy: entropy)
                    pages.append(contentsOf: result.pages); gistPages.append(contentsOf: result.pages.map(\.index))
                    table.indexes[ordinal].root = result.root
                    indexes[name]!.clearPending()
                } else {
                    let result = try indexes[name]!.tree.serialize(allocator: &allocation, entropy: entropy)
                    append(result); indexes[name]!.installPersistedTree(result.tree); table.indexes[ordinal].root = result.root
                }
            }
            image.tables[position] = table
        }
        let catalog = try CatalogChain.write(image.entries(), allocator: &allocation, codec: codec, entropy: entropy)
        pages.append(contentsOf: catalog.pages)
        var cleaned = snapshot
        guard snapshot.txid < UInt64.max else { throw JedError.corruption("transaction id exhausted") }
        cleaned.finishCommit(txid: snapshot.txid + 1, catalog: image, tables: tables, indexes: indexes)
        return DomainBodyWrite(snapshot: cleaned, root: catalog.root, allocation: allocation, pages: pages,
            catalogPages: catalog.pages.map(\.index), gistPages: gistPages, kept: kept, keptChains: chains)
    }

    static func live(after layout: DomainBodyLayout, previous: DomainStorageState, pageCount: UInt32) throws -> UInt32 {
        let orphaned = try PageReachability.orphaned(previous.snapshot, kept: layout.kept,
            keptChains: layout.keptChains, pageCount: pageCount)
        let before = UInt64(previous.livePages) + UInt64(layout.written.count)
        let removed = UInt64(previous.catalogPages.count) + UInt64(previous.gistPages.count) + orphaned
        guard removed <= before, before - removed <= UInt32.max else { throw JedError.corruption("live page count out of range") }
        return UInt32(before - removed)
    }
    static func reclaimDue(pageCount: UInt32, free: Int, live: UInt32) -> Bool {
        let unlisted = Int64(pageCount) - Int64(free)
        return unlisted > GeneratedSpec.compactMinimumPages && unlisted > GeneratedSpec.compactGrowth * Int64(live)
    }
    static func admit(layout: DomainBodyLayout, live: UInt32, previous: DomainStorageState,
                      pageCount: UInt32, kind: StorageDomainKind, policy: DomainCommitPolicy) throws -> Bool {
        let measured = kind == .file ? live : layout.allocation.highWater
        let before = kind == .file ? previous.livePages : pageCount
        if policy.storageLimit <= 0 || measured <= before || UInt64(measured) * UInt64(previous.snapshot.pool.codec.pageSize) <= UInt64(policy.storageLimit) { return true }
        return false
    }
    static func refusal(_ policy: DomainCommitPolicy) -> JedError {
        JedError("54P06", "storage of database \"\(policy.domainName)\" exceeded the limit of \(policy.storageLimit) bytes")
    }
}
