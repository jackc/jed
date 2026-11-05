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
        var visited: Set<UInt32> = [], pages: [UInt32] = [], allEntries: [GistEntry] = []
        func visit(_ reference: PageReference, depth: Int) throws -> GistNode {
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
            guard count > 0, count <= fanout else { throw JedError.corruption("invalid GiST fanout") }
            var reader = ByteReader(bytes[16...]), entries: [GistEntry] = [], children: [GistNode] = []
            var storedBounds: [[GistBound]] = [], childReferences: [PageReference] = []
            for _ in 0..<count {
                let size = Int(try reader.u16())
                var boundReader = ByteReader(try reader.take(size))
                let bound = try GistTuple.decode(&boundReader, ops: opclasses, leaf: leaf)
                try boundReader.requireEnd()
                if leaf {
                    let keySize = Int(try reader.u16())
                    guard keySize > 0 else { throw JedError.corruption("empty GiST row key") }
                    let entry = GistEntry(bound: bound, storageKey: Array(try reader.take(keySize)))
                    if let previous = entries.last, try !entryLess(previous, entry, ops: opclasses) {
                        throw JedError.corruption("GiST leaf entries are not ordered")
                    }
                    entries.append(entry)
                } else {
                    let index = try reader.u32(), tag = Array(try reader.take(tagLength))
                    storedBounds.append(bound); childReferences.append(PageReference(index, tag: tag))
                }
            }
            guard try reader.take(reader.remaining).allSatisfy({ $0 == 0 }) else {
                throw JedError.corruption("nonzero GiST page padding")
            }
            let node: GistNode
            if leaf {
                allEntries.append(contentsOf: entries)
                node = try GistNode(entries: entries, opclasses: opclasses)
            } else {
                for (index, reference) in childReferences.enumerated() {
                    let child = try visit(reference, depth: depth + 1)
                    // Canonical pages carry the exact subtree union. Comparing
                    // values permits equivalent decimal representations while
                    // rejecting a narrowed bound that could hide matching rows.
                    guard try GistTuple.compare(storedBounds[index], child.bound, ops: opclasses) == 0 else {
                        throw JedError.corruption("GiST interior bound differs from its subtree union")
                    }
                    if let previous = children.last, try !childLess(previous, child, ops: opclasses) {
                        throw JedError.corruption("GiST child entries are not ordered")
                    }
                    children.append(child)
                }
                node = try GistNode(children: children, opclasses: opclasses)
            }
            pages.append(reference.index)
            return node
        }
        _ = try visit(root, depth: 1)
        // The same rebuild is used on open and after statement mutation. An
        // incidental persisted shape cannot change deterministic descent costs.
        let tree = try build(opclasses: opclasses, entries: allEntries)
        return GistOpen(tree: tree, pages: pages)
    }

    func serialize(allocator: inout PageAllocator, codec: any PageCodec,
                   entropy: KeyEntropy? = nil) throws -> GistWrite {
        guard let root else { return GistWrite(root: PageSealer.nullReference(codec), pages: []) }
        var working = allocator, pages: [StoredPage] = []
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
            pages.append(sealed.page)
            return sealed.reference
        }
        let reference = try visit(root)
        allocator = working
        return GistWrite(root: reference, pages: pages)
    }
}
