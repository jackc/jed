internal struct CatalogChainWrite: Sendable {
    let root: PageReference
    let pages: [StoredPage]
}

internal enum CatalogChain {
    private struct Chunk {
        var bytes: [UInt8]
        var count: UInt32
        var continued: Bool
    }

    private static func layout(_ entries: [CatalogEntry], codec: any PageCodec) throws -> [Chunk] {
        let capacity = codec.capacity - (codec.reservedBytes == 0 ? 0 : 16)
        let encoded = try entries.map { try CatalogCodec.encode($0, encrypted: codec.reservedBytes != 0) }
        var chunks: [Chunk] = [], active = Chunk(bytes: [], count: 0, continued: false)
        for bytes in encoded {
            if bytes.count > capacity {
                if active.count > 0 { chunks.append(active); active = Chunk(bytes: [], count: 0, continued: false) }
                var offset = 0
                while offset < bytes.count {
                    let end = min(bytes.count, offset + capacity)
                    chunks.append(Chunk(bytes: Array(bytes[offset..<end]), count: offset == 0 ? 1 : 0, continued: end < bytes.count))
                    offset = end
                }
            } else {
                if active.bytes.count + bytes.count > capacity { chunks.append(active); active = Chunk(bytes: [], count: 0, continued: false) }
                active.bytes.append(contentsOf: bytes); active.count += 1
            }
        }
        if active.count > 0 || chunks.isEmpty { chunks.append(active) }
        return chunks
    }

    static func pageCount(_ entries: [CatalogEntry], codec: any PageCodec) throws -> Int {
        try layout(entries, codec: codec).count
    }

    /// Every entry is encoded before allocating or writing a page. Long entries
    /// begin and end on their own pages; ordinary entries pack greedily.
    static func write(_ entries: [CatalogEntry], allocator: inout PageAllocator,
                      codec: any PageCodec, entropy: KeyEntropy? = nil) throws -> CatalogChainWrite {
        let chunks = try layout(entries, codec: codec)
        var ids: [UInt32] = []
        for _ in chunks { ids.append(try allocator.allocate()) }
        var next = PageSealer.nullReference(codec), pages: [StoredPage] = []
        for ordinal in chunks.indices.reversed() {
            let chunk = chunks[ordinal]
            let sealed = try PageSealer.seal(codec: codec, index: ids[ordinal], type: 1, flags: chunk.continued ? 1 : 0,
                count: chunk.count, next: next, payload: chunk.bytes, entropy: entropy)
            pages.append(sealed.page); next = sealed.reference
        }
        return CatalogChainWrite(root: next, pages: pages.reversed())
    }

    /// Metadata is resident; only one sealed catalog page is handed to the
    /// image sink at a time. Nonces follow reverse chain order.
    static func writeImage(_ entries: [CatalogEntry], allocator: inout PageAllocator,
                           codec: any PageCodec, entropy: KeyEntropy? = nil,
                           sink: (StoredPage) throws -> Void) throws -> PageReference {
        let chunks = try layout(entries, codec: codec)
        var ids: [UInt32] = []
        for _ in chunks { ids.append(try allocator.allocate()) }
        var next = PageSealer.nullReference(codec)
        for ordinal in chunks.indices.reversed() {
            let chunk = chunks[ordinal]
            let sealed = try PageSealer.seal(codec: codec, index: ids[ordinal], type: 1, flags: chunk.continued ? 1 : 0,
                count: chunk.count, next: next, payload: chunk.bytes, entropy: entropy)
            try sink(sealed.page); next = sealed.reference
        }
        return next
    }

    /// Catalog metadata is resident, but each physical read remains bounded by
    /// one page. Only an explicitly continued entry accumulates across pages.
    static func read(root: PageReference, pageCount: UInt32, codec: any PageCodec,
                     read: (PageReference) throws -> [UInt8]) throws -> (entries: [CatalogEntry], pages: [UInt32]) {
        var current = root, seen: Set<UInt32> = [], pages: [UInt32] = [], entries: [CatalogEntry] = []
        var continuation: [UInt8]?
        let start = codec.reservedBytes == 0 ? 16 : 32
        while current.index != 0 {
            guard current.index >= codec.firstBodyPage, current.index < pageCount,
                  seen.insert(current.index).inserted else { throw JedError.corruption("invalid catalog chain link") }
            let page = try read(current)
            guard page.count == codec.pageSize - codec.reservedBytes, page[0] == 1,
                  try PageEnvelope.readU32(page, at: 12) == PageEnvelope.checksum(page, at: 12) else {
                throw JedError.corruption("invalid catalog page")
            }
            try PageEnvelope.validateHeader(page, encrypted: codec.reservedBytes != 0)
            let count = Int(try PageEnvelope.readU32(page, at: 4)), continued = page[1] != 0
            let next = try PageEnvelope.readU32(page, at: 8), tag = codec.reservedBytes == 0 ? [] : Array(page[16..<32])
            guard next != 0 || (tag.allSatisfy({ $0 == 0 }) && !continued) else {
                throw JedError.corruption("invalid catalog chain terminator")
            }
            pages.append(current.index)
            if continuation != nil {
                guard count == 0 else { throw JedError.corruption("catalog continuation declares another entry") }
                continuation!.append(contentsOf: page[start...])
                if !continued {
                    var reader = ByteReader(continuation!)
                    entries.append(try CatalogCodec.decode(&reader, encrypted: codec.reservedBytes != 0))
                    guard reader.remaining < codec.capacity - (start - 16),
                          try reader.take(reader.remaining).allSatisfy({ $0 == 0 }) else {
                        throw JedError.corruption("invalid catalog continuation padding")
                    }
                    continuation = nil
                }
            } else if continued {
                guard count == 1 else { throw JedError.corruption("catalog continuation does not begin with one entry") }
                continuation = Array(page[start...])
            } else {
                guard count <= page.count - start else { throw JedError.corruption("invalid catalog item count") }
                var reader = ByteReader(page[start...])
                for _ in 0..<count { entries.append(try CatalogCodec.decode(&reader, encrypted: codec.reservedBytes != 0)) }
                guard try reader.take(reader.remaining).allSatisfy({ $0 == 0 }) else { throw JedError.corruption("invalid catalog page padding") }
            }
            current = PageReference(next, tag: tag)
        }
        guard !pages.isEmpty, continuation == nil else { throw JedError.corruption("missing or incomplete catalog chain") }
        return (entries, pages)
    }
}
