/// Format v53's bounded, non-transitive (parent, slot) pointer overrides.
internal struct PageRemap: Equatable, Sendable {
    let parent: UInt32
    let slot: UInt16
    let target: PageReference

    static func width(_ encrypted: Bool) -> Int { encrypted ? 26 : 10 }
    static func capacity(pageSize: Int, encrypted: Bool) -> Int {
        (pageSize - (encrypted ? 136 : 64)) / 3 / width(encrypted)
    }
    static func reserve(pageSize: Int, encrypted: Bool) -> Int {
        capacity(pageSize: pageSize, encrypted: encrypted) * width(encrypted)
    }
    static func inlineCapacity(pageSize: Int, encrypted: Bool) -> Int {
        (pageSize - (encrypted ? 136 : 64) - reserve(pageSize: pageSize, encrypted: encrypted)) / 8
    }
    static func validate(_ entries: [PageRemap], pageSize: Int, encrypted: Bool, pageCount: UInt32) throws {
        guard entries.count <= capacity(pageSize: pageSize, encrypted: encrypted) else {
            throw JedError.corruption("too many remap entries")
        }
        var previous: PageRemap?, targets: Set<UInt32> = []
        let first: UInt32 = encrypted ? 4 : 2
        for entry in entries {
            guard entry.parent >= first, entry.parent < pageCount,
                  entry.target.index >= first, entry.target.index < pageCount,
                  entry.parent != entry.target.index, targets.insert(entry.target.index).inserted,
                  entry.target.tag.count == (encrypted ? 16 : 0),
                  previous.map({ $0.parent < entry.parent || ($0.parent == entry.parent && $0.slot < entry.slot) }) ?? true else {
                throw JedError.corruption("invalid remap order, bounds or target")
            }
            previous = entry
        }
    }
    static func decode(_ bytes: [UInt8], count: UInt32, meta: MetaPage) throws -> [PageRemap] {
        guard count <= capacity(pageSize: meta.pageSize, encrypted: meta.encrypted) else {
            throw JedError.corruption("too many remap entries")
        }
        var reader = ByteReader(bytes[meta.inlineEnd..<meta.remapEnd]), entries: [PageRemap] = []
        for _ in 0..<count {
            let parent = try reader.u32(), slot = try reader.u16(), page = try reader.u32()
            entries.append(PageRemap(parent: parent, slot: slot,
                target: PageReference(page, tag: meta.encrypted ? Array(try reader.take(16)) : [])))
        }
        guard try reader.take(reader.remaining).allSatisfy({ $0 == 0 }) else { throw JedError.corruption("nonzero remap padding") }
        return entries
    }
    static func resolve(_ children: [PageReference], parent: UInt32, entries: [PageRemap]) throws -> [PageReference] {
        var result = children
        for entry in entries where entry.parent == parent {
            guard Int(entry.slot) < result.count else { throw JedError.corruption("remap slot exceeds parent item count") }
            result[Int(entry.slot)] = entry.target
        }
        return result
    }
}

internal enum ManifestIdentity {
    static func digest(_ bytes: [UInt8], encrypted: Bool) throws -> UInt64 {
        UInt64(encrypted ? Checksums.crc32(bytes) : try PageEnvelope.readU32(bytes, at: 12))
    }
}

internal enum PageStamp {
    static func contains(_ bytes: [UInt8], floor: UInt64?, newest: UInt64) -> Bool {
        guard let floor, newest > floor, newest - floor < 65_536 else { return true }
        let stamp = UInt16(bytes[2]) << 8 | UInt16(bytes[3])
        let distance = stamp &- UInt16(truncatingIfNeeded: floor)
        return distance > 0 && UInt64(distance) <= newest - floor
    }
}

/// A commit-local codec view stamps pages before their checksum/encryption.
/// The underlying codec and every published reader remain immutable.
internal struct StampedPageCodec: PageCodec {
    let base: any PageCodec
    let txid: UInt64
    var pageSize: Int { base.pageSize }
    var reservedBytes: Int { base.reservedBytes }
    var firstBodyPage: UInt32 { base.firstBodyPage }
    func sealBody(pageIndex: UInt32, plaintext: [UInt8], salt: [UInt8]) throws -> SealedPage {
        var bytes = plaintext
        guard bytes.count >= 16 else { throw JedError.corruption("short stamped page") }
        bytes[2] = UInt8(truncatingIfNeeded: txid >> 8); bytes[3] = UInt8(truncatingIfNeeded: txid)
        return try base.sealBody(pageIndex: pageIndex, plaintext: bytes, salt: salt)
    }
    func openBody(pageIndex: UInt32, sealed: [UInt8], expectedTag: [UInt8]?) throws -> [UInt8] {
        try base.openBody(pageIndex: pageIndex, sealed: sealed, expectedTag: expectedTag)
    }
}
