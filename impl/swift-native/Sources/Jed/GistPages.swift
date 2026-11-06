internal struct GistWrite: Sendable {
    let root: PageReference
    let pages: [StoredPage]
}

internal struct GistOpen: Sendable {
    let tree: GistTree
    /// Reachable pages in postorder, for reclamation and integrity accounting.
    let pages: [UInt32]
}

internal extension GistTree {
    /// The baseline GiST contract eagerly reads its complete, bounded-fanout
    /// tree. This deliberate access-method narrowing is not a pager cache rule.
    static func open(root: PageReference, pageCount: UInt32, opclasses: [GistOpclass],
                     codec: any PageCodec, read: (PageReference) throws -> [UInt8]) throws -> GistOpen {
        try GistTuple.validateCount(opclasses.count, opclasses)
        let tagLength = codec.reservedBytes == 0 ? 0 : 16
        guard root.tag.count == tagLength else { throw JedError.corruption("invalid GiST root tag") }
        if root.index == 0 {
            guard root.tag.allSatisfy({ $0 == 0 }) else { throw JedError.corruption("nonzero empty GiST root tag") }
            return GistOpen(tree: GistTree(opclasses: opclasses, root: nil), pages: [])
        }
        var visited: Set<UInt32> = [], pages: [UInt32] = [], leafKeys: [[UInt8]] = []
        func visit(_ reference: PageReference, depth: Int) throws {
            guard depth <= maximumDepth, reference.index >= codec.firstBodyPage,
                  reference.index < pageCount, reference.tag.count == tagLength,
                  visited.insert(reference.index).inserted else {
                throw JedError.corruption("invalid, repeated, or excessively deep GiST page")
            }
            let bytes = try read(reference)
            guard bytes.count == codec.pageSize - codec.reservedBytes else { throw JedError.corruption("invalid GiST page length") }
            try PageEnvelope.validateHeader(bytes, encrypted: tagLength != 0)
            guard bytes[0] == 5 || bytes[0] == 6,
                  try PageEnvelope.readU32(bytes, at: 12) == PageEnvelope.checksum(bytes, at: 12) else {
                throw JedError.corruption("invalid GiST page header or checksum")
            }
            let leaf = bytes[0] == 5, count = Int(try PageEnvelope.readU32(bytes, at: 4))
            guard count <= codec.capacity / (leaf ? 4 : 6 + tagLength) else {
                throw JedError.corruption("GiST item count overruns page")
            }
            var reader = ByteReader(bytes[16...]), children: [PageReference] = []
            for _ in 0..<count {
                let boundSize = Int(try reader.u16()), bound = Array(try reader.take(boundSize))
                if leaf {
                    let keySize = Int(try reader.u16()), key = Array(try reader.take(keySize))
                    leafKeys.append(bound + key)
                } else {
                    // Open reconstructs the canonical resident tree from leaf
                    // entries. Persisted routing-bound agreement belongs to the
                    // host integrity operation, which inspects the stored pages.
                    children.append(PageReference(try reader.u32(), tag: Array(try reader.take(tagLength))))
                }
            }
            for child in children { try visit(child, depth: depth + 1) }
            pages.append(reference.index)
        }
        try visit(root, depth: 1)
        // The same rebuild is used on open and after statement mutation. An
        // incidental persisted shape cannot change deterministic descent costs.
        let tree = try build(opclasses: opclasses, leafKeys: leafKeys)
        return GistOpen(tree: tree, pages: pages)
    }

    func serialize(allocator: inout PageAllocator, codec: any PageCodec,
                   entropy: KeyEntropy? = nil) throws -> GistWrite {
        var pages: [StoredPage] = []
        let root = try writeImage(allocator: &allocator, codec: codec, entropy: entropy) { pages.append($0) }
        return GistWrite(root: root, pages: pages)
    }

    func writeImage(allocator: inout PageAllocator, codec: any PageCodec,
                    entropy: KeyEntropy? = nil, sink: (StoredPage) throws -> Void) throws -> PageReference {
        guard let root else { return PageSealer.nullReference(codec) }
        var working = allocator
        func appendBound(_ bound: [GistBound], to writer: inout ByteWriter) throws {
            let bytes = try GistTuple.encode(bound, ops: opclasses)
            guard bytes.count <= UInt16.max else { throw JedError.corruption("GiST bound exceeds wire length") }
            writer.u16(UInt16(bytes.count)); writer.append(bytes)
        }
        func visit(_ node: GistNode) throws -> PageReference {
            guard node.entryCount > 0, node.entryCount <= Self.fanout else { throw JedError.corruption("invalid GiST fanout") }
            var writer = ByteWriter()
            if node.isLeaf {
                for entry in node.entries {
                    guard entry.storageKey.count <= UInt16.max else { throw JedError.corruption("GiST storage key exceeds wire length") }
                    try appendBound(entry.bound, to: &writer)
                    writer.u16(UInt16(entry.storageKey.count)); writer.append(entry.storageKey)
                }
            } else {
                var references: [PageReference] = []
                for child in node.children { references.append(try visit(child)) }
                for (child, reference) in zip(node.children, references) {
                    try appendBound(child.bound, to: &writer)
                    writer.u32(reference.index); writer.append(reference.tag)
                }
            }
            guard writer.bytes.count <= codec.capacity else { throw JedError.corruption("GiST node exceeds page capacity") }
            let index = try working.allocate()
            let sealed = try PageSealer.seal(codec: codec, index: index, type: node.isLeaf ? 5 : 6,
                count: UInt32(node.entryCount), payload: writer.bytes, entropy: entropy)
            try sink(sealed.page)
            return sealed.reference
        }
        let reference = try visit(root)
        allocator = working
        return reference
    }
}
