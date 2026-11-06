/// Byte-format metadata and keyless recovery. Reads are one physical page at a
/// time; inherited tree pages are left to demand paging after root selection.
internal struct PageReference: Equatable, Sendable {
    let index: UInt32
    let tag: [UInt8]
    init(_ index: UInt32, tag: [UInt8] = []) { self.index = index; self.tag = tag }
}

internal struct ManifestEntry: Equatable, Sendable {
    let pageIndex: UInt32
    let digest: UInt64
}

internal struct MetaPage: Sendable {
    static let version: UInt16 = 50
    let pageSize: Int
    let encrypted: Bool
    let txid: UInt64
    let root: PageReference
    let pageCount: UInt32
    let freeList: PageReference
    let livePages: UInt32
    let manifestHead: UInt32
    let manifestEntries: UInt32
    let manifestPages: UInt32
    let manifestCRC: UInt64

    var firstBodyPage: UInt32 { encrypted ? 4 : 2 }
    var inlineStart: Int { encrypted ? 96 : 64 }
    var inlineEnd: Int { pageSize - (encrypted ? 40 : 0) }
    var inlineCapacity: Int { (inlineEnd - inlineStart) / 12 }
    var overflowCapacity: Int { (pageSize - 32) / 12 }

    static func decode(_ bytes: [UInt8]) throws -> MetaPage {
        try PageEnvelope.validateSize(bytes.count, encrypted: false, opening: true)
        var reader = ByteReader(bytes)
        guard Array(try reader.take(4)) == Array("JEDB".utf8), try reader.u16() == version else {
            throw JedError.corruption("invalid meta magic or format version")
        }
        let suite = try reader.u8(), reserve = try reader.u8()
        guard (suite == 0 && reserve == 0) || (suite == 1 && reserve == 40 && bytes.count >= 1024),
              try reader.u32() == UInt32(bytes.count) else {
            throw JedError.corruption("invalid meta page size or encryption variant")
        }
        let txid = try reader.u64(), rootIndex = try reader.u32(), pageCount = try reader.u32()
        let freeIndex = try reader.u32(), crc = try reader.u32()
        guard crc == (try PageEnvelope.checksum(bytes, at: 32)) else {
            throw JedError.corruption("invalid meta checksum")
        }
        let head = try reader.u32(), count = try reader.u32(), pages = try reader.u32()
        let chainCRC = try reader.u64(), live = try reader.u32()
        guard try reader.u32() == 0 else { throw JedError.corruption("invalid meta reserved field") }
        let rootTag = suite == 1 ? Array(try reader.take(16)) : []
        let freeTag = suite == 1 ? Array(try reader.take(16)) : []
        let result = MetaPage(pageSize: bytes.count, encrypted: suite == 1, txid: txid,
            root: PageReference(rootIndex, tag: rootTag), pageCount: pageCount,
            freeList: PageReference(freeIndex, tag: freeTag), livePages: live,
            manifestHead: head, manifestEntries: count, manifestPages: pages, manifestCRC: chainCRC)
        try result.validateShape()
        let usedEnd = result.inlineStart + min(Int(count), result.inlineCapacity) * 12
        guard bytes[usedEnd..<result.inlineEnd].allSatisfy({ $0 == 0 }) else {
            throw JedError.corruption("nonzero meta padding")
        }
        return result
    }

    func validateShape() throws {
        try PageEnvelope.validateSize(pageSize, encrypted: encrypted, opening: true)
        let first = firstBodyPage
        guard pageCount > first, root.index >= first, root.index < pageCount,
              freeList.index == 0 || (freeList.index >= first && freeList.index < pageCount),
              livePages <= pageCount - first else {
            throw JedError.corruption("invalid meta page bounds")
        }
        let body = UInt64(pageCount - first)
        let inline = min(UInt64(manifestEntries), UInt64(inlineCapacity))
        guard UInt64(manifestEntries) + UInt64(manifestPages) <= body,
              (manifestHead == 0) == (manifestPages == 0),
              manifestHead == 0 || (manifestHead >= first && manifestHead < pageCount),
              manifestPages != 0 || manifestCRC == 0,
              manifestEntries != 0 || manifestPages == 0,
              UInt64(manifestEntries) <= inline + UInt64(manifestPages) * UInt64(overflowCapacity) else {
            throw JedError.corruption("invalid manifest descriptor")
        }
        guard encrypted ? (root.tag.count == 16 && freeList.tag.count == 16) :
                            (root.tag.isEmpty && freeList.tag.isEmpty),
              freeList.index != 0 || freeList.tag.allSatisfy({ $0 == 0 }) else {
            throw JedError.corruption("invalid meta page-reference tag")
        }
    }

    /// Constructs the full stored meta envelope with zero encrypted reserve.
    /// Encrypted callers seal it for the intended slot only after the manifest.
    func encode(inline: [ManifestEntry]) throws -> [UInt8] {
        try validateShape()
        guard inline.count == min(Int(manifestEntries), inlineCapacity) else {
            throw JedError.corruption("incorrect inline manifest length")
        }
        var writer = ByteWriter()
        writer.append("JEDB".utf8); writer.u16(Self.version)
        writer.u8(encrypted ? 1 : 0); writer.u8(encrypted ? 40 : 0)
        writer.u32(UInt32(pageSize)); writer.u64(txid); writer.u32(root.index)
        writer.u32(pageCount); writer.u32(freeList.index); writer.u32(0)
        writer.u32(manifestHead); writer.u32(manifestEntries); writer.u32(manifestPages)
        writer.u64(manifestCRC); writer.u32(livePages); writer.u32(0)
        if encrypted { writer.append(root.tag); writer.append(freeList.tag) }
        for item in inline { writer.u32(item.pageIndex); writer.u64(item.digest) }
        writer.append(repeatElement(UInt8(0), count: pageSize - writer.bytes.count))
        var bytes = writer.bytes
        try PageEnvelope.putU32(PageEnvelope.checksum(bytes, at: 32), into: &bytes, at: 32)
        return bytes
    }
}

internal struct ValidatedMeta: Sendable {
    let meta: MetaPage
    let slot: UInt32
    var floorSlot: UInt32
    let bytes: [UInt8]
    let entries: [ManifestEntry]
    let chain: [UInt32]
    var protectedPages: Set<UInt32> { Set(entries.map(\.pageIndex)).union(chain) }
}

/// Encodes a manifest after the allocator's monotone reservation has settled.
/// Extra reserved chain pages are retained and encoded as empty trailing pages.
internal struct CommitManifest: Sendable {
    let meta: MetaPage
    let inline: [ManifestEntry]
    let pages: [ManifestPageBytes]

    static func build(pageSize: Int, encrypted: Bool, txid: UInt64, root: PageReference,
                      pageCount: UInt32, freeList: PageReference, livePages: UInt32,
                      entries: [ManifestEntry], chain: [UInt32]) throws -> CommitManifest {
        try PageEnvelope.validateSize(pageSize, encrypted: encrypted, opening: true)
        let first: UInt32 = encrypted ? 4 : 2
        let inlineCapacity = (pageSize - (encrypted ? 136 : 64)) / 12
        let overflowCapacity = (pageSize - 32) / 12
        guard entries.count <= Int(UInt32.max), chain.count <= Int(UInt32.max),
              pageCount > first, entries.count + chain.count <= Int(pageCount - first),
              entries.count <= inlineCapacity + chain.count * overflowCapacity,
              !entries.isEmpty || chain.isEmpty else {
            throw JedError.corruption("invalid manifest allocation")
        }
        let chainIDs = Set(chain)
        guard chainIDs.count == chain.count,
              chain.allSatisfy({ $0 >= first && $0 < pageCount }) else {
            throw JedError.corruption("invalid manifest page reservation")
        }
        var previous: UInt32 = 0
        for entry in entries {
            guard entry.pageIndex > previous, entry.pageIndex >= first, entry.pageIndex < pageCount,
                  !chainIDs.contains(entry.pageIndex) else { throw JedError.corruption("invalid manifest entry") }
            previous = entry.pageIndex
        }
        guard entries.isEmpty || (entries.contains { $0.pageIndex == root.index } &&
            (freeList.index == 0 || entries.contains { $0.pageIndex == freeList.index })) else {
            throw JedError.corruption("manifest omits a published root")
        }
        var offset = min(entries.count, inlineCapacity), crc: UInt64 = 0
        var pages: [ManifestPageBytes] = []
        let codec = try PlainPageCodec(pageSize: pageSize)
        for (ordinal, index) in chain.enumerated() {
            let count = min(entries.count - offset, overflowCapacity)
            var writer = ByteWriter()
            writer.u8(8); writer.u8(0); writer.u16(0); writer.u32(UInt32(count))
            writer.u32(ordinal + 1 == chain.count ? 0 : chain[ordinal + 1]); writer.u32(0)
            writer.u64(txid); writer.u32(UInt32(ordinal)); writer.u32(0)
            for entry in entries[offset..<(offset + count)] {
                writer.u32(entry.pageIndex); writer.u64(entry.digest)
            }
            writer.append(repeatElement(UInt8(0), count: pageSize - writer.bytes.count))
            let bytes = try codec.sealBody(pageIndex: index, plaintext: writer.bytes, salt: []).bytes
            var prefix = ByteWriter(); prefix.u32(index)
            crc = Checksums.crc64Update(crc, prefix.bytes)
            crc = Checksums.crc64Update(crc, bytes)
            pages.append(ManifestPageBytes(pageIndex: index, bytes: bytes))
            offset += count
        }
        let meta = MetaPage(pageSize: pageSize, encrypted: encrypted, txid: txid, root: root,
            pageCount: pageCount, freeList: freeList, livePages: livePages,
            manifestHead: chain.first ?? 0, manifestEntries: UInt32(entries.count),
            manifestPages: UInt32(chain.count), manifestCRC: crc)
        try meta.validateShape()
        return CommitManifest(meta: meta, inline: Array(entries.prefix(inlineCapacity)), pages: pages)
    }
}

/// Separates a host's error from structural corruption. Even if a host elects
/// to report XX001 itself, recovery must propagate it rather than try a fallback.
private struct RecoveryReadFailure: Error { let underlying: any Error }

internal enum MetaRecovery {
    static func open(_ store: any BlockStore) throws -> ValidatedMeta {
        let length = try store.size()
        guard length >= 12 else { throw JedError.corruption("file is shorter than a meta header") }
        let header = try store.read(at: 0, count: 12)
        let size = Int(try PageEnvelope.readU32(header, at: 8))
        try PageEnvelope.validateSize(size, encrypted: false, opening: true)
        return try select(pageSize: size, fileBytes: length) { index in
            try store.read(at: UInt64(index) * UInt64(size), count: size)
        }
    }

    static func select(pageSize: Int, fileBytes: UInt64,
                       read: (UInt32) throws -> [UInt8]) throws -> ValidatedMeta {
        try PageEnvelope.validateSize(pageSize, encrypted: false, opening: true)
        var candidates: [(UInt32, MetaPage, [UInt8])] = []
        var declaredVariant: Bool?
        for slot: UInt32 in 0...1 {
            guard UInt64(slot + 1) * UInt64(pageSize) <= fileBytes else { continue }
            let bytes = try read(slot)
            // Variant discovery uses CRC-valid envelopes before interpreting
            // credentials. A damaged slot cannot silently reinterpret the file.
            if bytes.count == pageSize,
               try PageEnvelope.readU32(bytes, at: 32) == PageEnvelope.checksum(bytes, at: 32) {
                let encrypted: Bool
                if bytes[6] == 0 && bytes[7] == 0 { encrypted = false }
                else if bytes[6] == 1 && bytes[7] == 40 { encrypted = true }
                else { throw JedError.corruption("unknown encryption variant") }
                guard declaredVariant == nil || declaredVariant == encrypted else {
                    throw JedError.corruption("meta slots disagree about encryption variant")
                }
                declaredVariant = encrypted
            }
            if let meta = try? MetaPage.decode(bytes), meta.pageSize == pageSize,
               UInt64(meta.pageCount) * UInt64(pageSize) <= fileBytes {
                candidates.append((slot, meta, bytes))
            }
        }
        candidates.sort { $0.1.txid == $1.1.txid ? $0.0 < $1.0 : $0.1.txid > $1.1.txid }
        var rejected: Set<UInt32> = []
        func validate(_ meta: MetaPage, _ bytes: [UInt8]) throws -> (entries: [ManifestEntry], chain: [UInt32]) {
            try validateManifest(meta, bytes: bytes) { index in
                do { return try read(index) }
                catch { throw RecoveryReadFailure(underlying: error) }
            }
        }
        for (slot, meta, bytes) in candidates {
            do {
                let validated = try validate(meta, bytes)
                var tied = false
                for (otherSlot, otherMeta, otherBytes) in candidates
                    where otherSlot != slot && otherMeta.txid == meta.txid && !rejected.contains(otherSlot) {
                    do { _ = try validate(otherMeta, otherBytes); tied = true }
                    catch let error as RecoveryReadFailure { throw error }
                    catch let error as JedError where error.code == "XX001" { rejected.insert(otherSlot) }
                }
                return ValidatedMeta(meta: meta, slot: slot, floorSlot: tied ? UInt32(meta.txid & 1) : slot,
                    bytes: bytes, entries: validated.entries, chain: validated.chain)
            } catch let error as RecoveryReadFailure { throw error.underlying }
              catch let error as JedError where error.code == "XX001" { rejected.insert(slot); continue }
        }
        throw JedError.corruption("neither meta slot has a complete generation")
    }

    static func validateManifest(_ meta: MetaPage, bytes: [UInt8], verifyBodies: Bool = true,
                                 read: (UInt32) throws -> [UInt8]) throws -> (entries: [ManifestEntry], chain: [UInt32]) {
        var entries: [ManifestEntry] = []
        var previous: UInt32 = 0
        func consume(_ data: ArraySlice<UInt8>, count: Int) throws {
            var reader = ByteReader(data)
            for _ in 0..<count {
                let index = try reader.u32(), digest = try reader.u64()
                guard index >= meta.firstBodyPage, index < meta.pageCount, index > previous else {
                    throw JedError.corruption("invalid manifest entry order or bounds")
                }
                previous = index
                entries.append(ManifestEntry(pageIndex: index, digest: digest))
            }
        }
        let inlineCount = min(Int(meta.manifestEntries), meta.inlineCapacity)
        try consume(bytes[meta.inlineStart..<(meta.inlineStart + 12 * inlineCount)], count: inlineCount)
        var chain: [UInt32] = [], seen: Set<UInt32> = []
        var next = meta.manifestHead, chainCRC: UInt64 = 0
        for ordinal in 0..<meta.manifestPages {
            guard next >= meta.firstBodyPage, next < meta.pageCount, seen.insert(next).inserted else {
                throw JedError.corruption("invalid manifest chain link")
            }
            let index = next, page = try read(index)
            guard page.count == meta.pageSize else { throw JedError.corruption("short manifest page") }
            try PageEnvelope.validateHeader(page, encrypted: false)
            guard page[0] == 8, try PageEnvelope.readU32(page, at: 12) == PageEnvelope.checksum(page, at: 12) else {
                throw JedError.corruption("invalid manifest page header or checksum")
            }
            var reader = ByteReader(page)
            _ = try reader.take(4)
            let count = Int(try reader.u32()); next = try reader.u32()
            _ = try reader.u32()
            guard try reader.u64() == meta.txid, try reader.u32() == ordinal, try reader.u32() == 0,
                  count == min(meta.overflowCapacity, Int(meta.manifestEntries) - entries.count),
                  (ordinal + 1 == meta.manifestPages) == (next == 0) else {
                throw JedError.corruption("invalid manifest chain descriptor")
            }
            try consume(try reader.take(count * 12), count: count)
            guard try reader.take(reader.remaining).allSatisfy({ $0 == 0 }) else {
                throw JedError.corruption("nonzero manifest padding")
            }
            var prefix = ByteWriter(); prefix.u32(index)
            chainCRC = Checksums.crc64Update(chainCRC, prefix.bytes)
            chainCRC = Checksums.crc64Update(chainCRC, page)
            chain.append(index)
        }
        guard entries.count == Int(meta.manifestEntries), chainCRC == meta.manifestCRC,
              entries.allSatisfy({ !seen.contains($0.pageIndex) }),
              entries.isEmpty || (entries.contains { $0.pageIndex == meta.root.index } &&
                (meta.freeList.index == 0 || entries.contains { $0.pageIndex == meta.freeList.index })) else {
            throw JedError.corruption("manifest identities do not match the meta")
        }
        // The service validates manifest structure before receiving body pages;
        // each arriving body then receives its identity check before its write.
        if !verifyBodies { return (entries, chain) }
        for entry in entries {
            let page = try read(entry.pageIndex)
            guard page.count == meta.pageSize, Checksums.crc64(page) == entry.digest else {
                throw JedError.corruption("manifest body identity mismatch")
            }
            if !meta.encrypted {
                try PageEnvelope.validateHeader(page, encrypted: false)
                guard page[0] <= 7, try PageEnvelope.readU32(page, at: 12) == PageEnvelope.checksum(page, at: 12) else {
                    throw JedError.corruption("invalid manifest body header or checksum")
                }
            }
        }
        return (entries, chain)
    }
}
