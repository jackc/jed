internal struct FreeList: Sendable {
    let entries: [UInt32]
    let chain: [UInt32]

    static func load(meta: MetaPage, protected: Set<UInt32>,
                     read: (PageReference) throws -> [UInt8]) throws -> FreeList {
        var reference = meta.freeList, seen: Set<UInt32> = [], entries: [UInt32] = [], chain: [UInt32] = []
        var previous: UInt32 = 0
        let prefix = meta.encrypted ? 32 : 16
        let logicalSize = meta.pageSize - (meta.encrypted ? 40 : 0)
        while reference.index != 0 {
            guard reference.index >= meta.firstBodyPage, reference.index < meta.pageCount,
                  seen.insert(reference.index).inserted else { throw JedError.corruption("invalid free-list chain") }
            let page = try read(reference)
            guard page.count == logicalSize else { throw JedError.corruption("invalid free-list page length") }
            try PageEnvelope.validateHeader(page, encrypted: meta.encrypted)
            guard page[0] == 7, try PageEnvelope.readU32(page, at: 12) == PageEnvelope.checksum(page, at: 12) else {
                throw JedError.corruption("invalid free-list header or checksum")
            }
            let count = Int(try PageEnvelope.readU32(page, at: 4))
            guard count <= (logicalSize - prefix) / 4 else { throw JedError.corruption("invalid free-list count") }
            var reader = ByteReader(page[prefix...])
            for _ in 0..<count {
                let index = try reader.u32()
                guard index > previous, index >= meta.firstBodyPage, index < meta.pageCount,
                      !protected.contains(index) else { throw JedError.corruption("invalid free-list entry") }
                entries.append(index); previous = index
            }
            guard try reader.take(reader.remaining).allSatisfy({ $0 == 0 }) else {
                throw JedError.corruption("nonzero free-list padding")
            }
            chain.append(reference.index)
            let next = try PageEnvelope.readU32(page, at: 8)
            let tag = meta.encrypted ? Array(page[16..<32]) : []
            guard next != 0 || tag.allSatisfy({ $0 == 0 }) else { throw JedError.corruption("nonzero null successor tag") }
            reference = PageReference(next, tag: tag)
        }
        guard entries.allSatisfy({ !seen.contains($0) }) else { throw JedError.corruption("free-list chain lists itself") }
        return FreeList(entries: entries, chain: chain)
    }
}
