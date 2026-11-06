internal struct DomainImageSummary: Sendable {
    let root: PageReference
    let pageCount: UInt32
    let livePages: UInt32
}

/// Whole-image serialization is a page sink, not a database-sized byte array.
/// The caller holds a source snapshot pin and owns destination allocation,
/// barriers, atomic replacement, and cleanup after any sink failure. The sink
/// receives each page once, in sealing order; both meta slots arrive last.
internal enum DomainImage {
    static func write(_ snapshot: DomainSnapshot, txid: UInt64, codec target: (any PageCodec)? = nil,
                      keyBlock: KeyBlock? = nil, entropy: KeyEntropy? = nil,
                      sink: (StoredPage) throws -> Void) throws -> DomainImageSummary {
        let codec = target ?? snapshot.pool.codec
        guard codec.pageSize == snapshot.pool.codec.pageSize, codec.reservedBytes == snapshot.pool.codec.reservedBytes else {
            throw JedError("0A000", "image layout change requires rebuilding records first")
        }
        guard (codec.reservedBytes == 0) == (keyBlock == nil) else { throw JedError("22023", "image key block does not match its codec") }
        if codec.reservedBytes != 0, entropy == nil { throw JedError("58030", "encrypted image requires key entropy") }
        // This checks catalog lengths, references and independent store layouts
        // without sealing or emitting bytes, so metadata refusal is atomic.
        _ = try DomainCommitPlanner.layout(snapshot, highWater: codec.firstBodyPage, free: [])
        if let keyBlock {
            let bytes = try keyBlock.encode(pageSize: codec.pageSize)
            try sink(StoredPage(index: 2, bytes: bytes)); try sink(StoredPage(index: 3, bytes: bytes))
        }
        var allocation = try PageAllocator(highWater: codec.firstBodyPage, free: [], firstBody: codec.firstBodyPage)
        var image = snapshot.catalog
        for position in image.tables.indices.sorted(by: { image.tables[$0].name < image.tables[$1].name }) {
            var table = image.tables[position]
            let store = snapshot.tables[table.name]!
            table.rowCount = store.count!
            table.root = try store.tree.writeImage(allocator: &allocation, codec: codec, entropy: entropy, sink: sink)
            table.indexes.sort { $0.name < $1.name }
            for ordinal in table.indexes.indices {
                let index = table.indexes[ordinal]
                if index.kind == 2 {
                    table.indexes[ordinal].root = try snapshot.gistTrees[index.name]!.writeImage(
                        allocator: &allocation, codec: codec, entropy: entropy, sink: sink)
                } else {
                    table.indexes[ordinal].root = try snapshot.indexes[index.name]!.tree.writeImage(
                        allocator: &allocation, codec: codec, entropy: entropy, sink: sink)
                }
            }
            image.tables[position] = table
        }
        let root = try CatalogChain.writeImage(image.entries(), allocator: &allocation, codec: codec, entropy: entropy, sink: sink)
        let live = allocation.highWater - codec.firstBodyPage
        let manifest = try CommitManifest.build(pageSize: codec.pageSize, encrypted: codec.reservedBytes != 0,
            txid: txid, root: root, pageCount: allocation.highWater, freeList: PageSealer.nullReference(codec),
            livePages: live, entries: [], chain: [])
        let clear = try manifest.meta.encode(inline: [])
        for slot: UInt32 in 0...1 {
            let bytes: [UInt8]
            if let encrypted = codec as? EncryptedPageCodec {
                bytes = try encrypted.sealMeta(clear, slot: slot, nonce: EncryptionPolicy.entropy(24, from: entropy!))
            } else { bytes = clear }
            try sink(StoredPage(index: slot, bytes: bytes))
        }
        return DomainImageSummary(root: root, pageCount: allocation.highWater, livePages: live)
    }
}
