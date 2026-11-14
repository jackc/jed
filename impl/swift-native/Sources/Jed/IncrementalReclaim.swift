internal struct FreePageGeneration: Sendable {
    let txid: UInt64
    var pages: Set<UInt32>
}

/// File-owner state only. A prepared copy is published with the commit, never
/// while planning it. Losing identity knowledge leaks pages until a rebuild.
internal struct IncrementalReclaim: Sendable {
    var known = true
    var tolerated: Int64 = 0
    var generations: [FreePageGeneration] = []
    var held: Set<UInt32> = []

    func newer(than watermark: UInt64) -> Set<UInt32> {
        generations.filter { $0.txid > watermark }.reduce(into: Set<UInt32>()) { $0.formUnion($1.pages) }
    }
    static func leaked(meta: MetaPage, free: Int, chain: Int, held: Int) -> Int64 {
        Int64(meta.pageCount) - Int64(meta.firstBodyPage) - Int64(meta.livePages) - Int64(free)
            - Int64(chain) - Int64(meta.manifestPages) - Int64(held)
    }
    static func due(meta: MetaPage, free: Int, chain: Int, held: Int) -> Bool {
        let leaked = leaked(meta: meta, free: free, chain: chain, held: held)
        return leaked > GeneratedSpec.compactMinimumPages && leaked * 16 > Int64(meta.livePages)
    }
}

internal extension PageReachability {
    /// Enumerate only replaced nodes and chains. Kept subtree roots stop the
    /// walk; retained interiors are removed later, after visiting their children.
    static func orphanIDs(_ snapshot: DomainSnapshot, kept: Set<UInt32>, keptChains: Set<UInt32>,
                          pageCount: UInt32) throws -> Set<UInt32> {
        var pages: Set<UInt32> = []
        let codec = snapshot.pool.codec, encrypted = codec.reservedBytes != 0
        let slab = codec.capacity - (encrypted ? 16 : 0)
        func add(_ index: UInt32) throws {
            guard index >= codec.firstBodyPage, index < pageCount, pages.insert(index).inserted else {
                throw JedError.corruption("invalid or repeated orphan page")
            }
        }
        func visit(_ child: TreeChild, tree: BPlusTree) throws {
            guard let reference = child.reference, !kept.contains(reference.index) else { return }
            try add(reference.index)
            if case .resident(let node) = child, !node.isLeaf {
                for child in node.children { try visit(child, tree: tree) }
                return
            }
            guard tree.types.contains(where: { LeafValue.width($0) == nil }) else { return }
            let node = try tree.resolve(child)
            for row in 0..<node.entryCount {
                for cell in node.record(at: row).cells {
                    guard let facts = try cell.largeFacts(encrypted: encrypted), var head = facts.external,
                          !keptChains.contains(head.index) else { continue }
                    var remaining = facts.stored
                    guard remaining > 0 else { throw JedError.corruption("empty orphan overflow") }
                    while remaining > 0 {
                        try add(head.index)
                        let bytes = try snapshot.pool.readBody(head)
                        guard bytes[0] == 4, try PageEnvelope.readU32(bytes, at: 4) == min(slab, remaining) else {
                            throw JedError.corruption("invalid orphan overflow slab")
                        }
                        remaining -= min(slab, remaining)
                        let next = try PageEnvelope.readU32(bytes, at: 8)
                        guard (remaining == 0) == (next == 0) else { throw JedError.corruption("invalid orphan overflow terminator") }
                        head = PageReference(next, tag: encrypted ? Array(bytes[16..<32]) : [])
                    }
                }
            }
        }
        for tree in InteriorDeferral.trees(snapshot) { if let root = tree.root { try visit(root, tree: tree) } }
        return pages
    }
}
