/// Private rebuild storage. The caller owns its byte device and cleanup, and
/// supplies a throwaway codec/entropy stream independent of the final image.
/// No durable metadata or barriers are emitted; this address space is never
/// reopened. A native host may supply a bounded memory-to-scratch-file device.
internal struct ScratchDomain {
    let pager: Pager
    let pool: NodePool
    private let root: PageReference
    private let catalogPages: [UInt32]
    private let entropy: KeyEntropy?
    private let spill: SpillableScratchStore?
    private var lastReclaimedLive: UInt32
    private(set) var stages = 0

    static func create(store: any BlockStore, codec: any PageCodec, cacheBytes: Int,
                       entropy: KeyEntropy? = nil) throws -> ScratchDomain {
        guard cacheBytes >= 0 else { throw JedError("22023", "negative scratch cache size") }
        guard try store.size() == 0 else { throw JedError("55006", "scratch storage must be empty") }
        var allocation = try PageAllocator(highWater: codec.firstBodyPage, free: [], firstBody: codec.firstBodyPage)
        let catalog = try CatalogChain.write([], allocator: &allocation, codec: codec, entropy: entropy)
        let pager = try Pager.scratch(store: store, codec: codec, root: catalog.root,
            pageCount: allocation.highWater, body: catalog.pages)
        return try ScratchDomain(pager: pager, pool: NodePool(pager: pager, cacheBytes: cacheBytes),
            root: catalog.root, catalogPages: catalog.pages.map(\.index), entropy: entropy,
            spill: store as? SpillableScratchStore,
            lastReclaimedLive: UInt32(catalog.pages.count))
    }

    /// Every tree, including a GiST index's leaf-key store, is an ordinary
    /// scratch B-tree here. The canonical GiST is rebuilt after the final stage.
    mutating func stage(_ snapshot: inout DomainSnapshot) throws {
        guard snapshot.pool === pool, snapshot.gistTrees.isEmpty else {
            throw JedError.corruption("invalid rebuild scratch snapshot")
        }
        let current = try pager.allocationSnapshot(reuseAllowed: true)
        var allocation = try PageAllocator(highWater: current.highWater, free: current.reusableFree,
            firstBody: pool.codec.firstBodyPage)
        var tables = snapshot.tables, indexes = snapshot.indexes, pages: [StoredPage] = []
        for name in tables.keys.sorted() {
            let written = try tables[name]!.tree.serialize(allocator: &allocation, entropy: entropy)
            pages.append(contentsOf: written.pages); tables[name]!.installPersistedTree(written.tree)
        }
        for name in indexes.keys.sorted() {
            let written = try indexes[name]!.tree.serialize(allocator: &allocation, entropy: entropy)
            pages.append(contentsOf: written.pages); indexes[name]!.installPersistedTree(written.tree)
        }
        let plan = try pager.prepareMemory(body: pages, root: root, pageCount: allocation.highWater,
            freePages: Array(allocation.remaining), reuseAllowed: true)
        pool.invalidate(Set(pages.map(\.index)))
        let staged = try pager.stageMemory(plan)
        try spill?.spillIfNeeded(committedBytes: UInt64(allocation.highWater) * UInt64(pool.codec.pageSize))
        try pager.adoptMemory(staged)
        snapshot.finishCommit(txid: plan.manifest.meta.txid, catalog: snapshot.catalog, tables: tables, indexes: indexes)
        stages += 1
        // This private generation has no reader pins. Reclamation only changes
        // its free vector; the current packed roots remain intact.
        if DomainCommitPlanner.reclaimDue(pageCount: allocation.highWater,
            free: allocation.remaining.count, live: lastReclaimedLive) {
            let reached = try PageReachability.collect(snapshot, catalogPages: catalogPages, gistPages: [],
                pageCount: allocation.highWater, read: pager.readBody)
            let free = (pool.codec.firstBodyPage..<allocation.highWater).filter { !reached.contains($0) }
            try pager.replaceMemoryFree(free)
            lastReclaimedLive = UInt32(reached.count)
        }
    }
}
