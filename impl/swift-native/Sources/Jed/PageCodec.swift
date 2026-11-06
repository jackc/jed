import JedCrypto

internal struct SealedPage: Sendable {
    let bytes: [UInt8]
    let tag: [UInt8]
}

/// Per-page seam. Logical pages include their 16-byte header and omit the
/// physical reserve. Entropy is explicit so the pager controls seal ordering.
internal protocol PageCodec: Sendable {
    var pageSize: Int { get }
    var capacity: Int { get }
    var reservedBytes: Int { get }
    var firstBodyPage: UInt32 { get }
    func sealBody(pageIndex: UInt32, plaintext: [UInt8], salt: [UInt8]) throws -> SealedPage
    func openBody(pageIndex: UInt32, sealed: [UInt8], expectedTag: [UInt8]?) throws -> [UInt8]
}

internal extension PageCodec {
    var capacity: Int { pageSize - 16 - reservedBytes }
}

internal enum PageEnvelope {
    static func validateSize(_ size: Int, encrypted: Bool, opening: Bool) throws {
        guard size >= 256, size <= 65_536, size & (size - 1) == 0 else {
            throw JedError(opening ? "XX001" : "22023", "invalid page size")
        }
        if encrypted, size < 1024 {
            throw JedError(opening ? "XX001" : "0A000", "encrypted page size must be at least 1024")
        }
    }

    static func readU32(_ bytes: [UInt8], at offset: Int) throws -> UInt32 {
        guard offset >= 0, offset <= bytes.count, bytes.count - offset >= 4 else {
            throw JedError.corruption("truncated page field")
        }
        var reader = ByteReader(bytes[offset..<(offset + 4)])
        return try reader.u32()
    }

    static func putU32(_ value: UInt32, into bytes: inout [UInt8], at offset: Int) throws {
        guard offset >= 0, offset <= bytes.count, bytes.count - offset >= 4 else {
            throw JedError.corruption("truncated page field")
        }
        var writer = ByteWriter(); writer.u32(value)
        bytes.replaceSubrange(offset..<(offset + 4), with: writer.bytes)
    }

    static func checksum(_ bytes: [UInt8], at offset: Int) throws -> UInt32 {
        guard offset >= 0, offset <= bytes.count, bytes.count - offset >= 4 else {
            throw JedError.corruption("truncated page checksum")
        }
        return Checksums.crc32(bytes.prefix(offset) + bytes.dropFirst(offset + 4))
    }

    static func validateHeader(_ bytes: [UInt8], encrypted: Bool) throws {
        guard bytes.count >= 16 else { throw JedError.corruption("truncated body page") }
        let type = bytes[0], flags = bytes[1]
        guard (1...(encrypted ? 7 : 8)).contains(Int(type)), bytes[2] == 0, bytes[3] == 0,
              type == 1 ? flags & ~1 == 0 : flags == 0 else {
            throw JedError.corruption("invalid body page header")
        }
        if type == 2 || type == 3 || type == 5 || type == 6 {
            guard try readU32(bytes, at: 8) == 0 else { throw JedError.corruption("invalid node successor") }
        }
    }
}

internal struct PlainPageCodec: PageCodec {
    let pageSize: Int
    let reservedBytes = 0
    let firstBodyPage: UInt32 = 2

    init(pageSize: Int, opening: Bool = false) throws {
        try PageEnvelope.validateSize(pageSize, encrypted: false, opening: opening)
        self.pageSize = pageSize
    }

    func sealBody(pageIndex: UInt32, plaintext: [UInt8], salt: [UInt8]) throws -> SealedPage {
        guard pageIndex >= firstBodyPage, plaintext.count == pageSize, salt.isEmpty else {
            throw JedError.corruption("invalid plaintext page envelope")
        }
        try PageEnvelope.validateHeader(plaintext, encrypted: false)
        var result = plaintext
        try PageEnvelope.putU32(PageEnvelope.checksum(result, at: 12), into: &result, at: 12)
        return SealedPage(bytes: result, tag: [])
    }

    func openBody(pageIndex: UInt32, sealed: [UInt8], expectedTag: [UInt8]? = nil) throws -> [UInt8] {
        guard pageIndex >= firstBodyPage, sealed.count == pageSize, expectedTag?.isEmpty != false else {
            throw JedError.corruption("invalid plaintext page envelope")
        }
        try PageEnvelope.validateHeader(sealed, encrypted: false)
        guard try PageEnvelope.readU32(sealed, at: 12) == PageEnvelope.checksum(sealed, at: 12) else {
            throw JedError.corruption("invalid body page checksum")
        }
        return sealed
    }
}

/// Stored manifest-overflow bytes, already validated by manifest traversal. The
/// order supplied here is chain order, never page-number or map iteration order.
internal struct ManifestPageBytes: Sendable {
    let pageIndex: UInt32
    let bytes: [UInt8]
}

internal struct EncryptedPageCodec: PageCodec {
    let pageSize: Int
    let reservedBytes = 40
    let firstBodyPage: UInt32 = 4
    private let dataKey: [UInt8]
    private let pageCipher: XAES256GCM
    private let metaCipher: XAES256GCM

    init(pageSize: Int, dataKey: [UInt8], opening: Bool = false) throws {
        try PageEnvelope.validateSize(pageSize, encrypted: true, opening: opening)
        guard dataKey.count == 32 else { throw JedError("22023", "data key must contain 32 bytes") }
        self.pageSize = pageSize; self.dataKey = dataKey
        do {
            let pageKey = try JedCrypto.hkdfSHA256(inputKeyMaterial: dataKey, salt: [], info: Array("jed/v1/page".utf8))
            let metaKey = try JedCrypto.hkdfSHA256(inputKeyMaterial: dataKey, salt: [], info: Array("jed/v1/meta".utf8))
            pageCipher = try XAES256GCM(key: pageKey)
            metaCipher = try XAES256GCM(key: metaKey)
        } catch { throw JedError("58030", "page-key derivation failed") }
    }

    func rewrap(_ block: KeyBlock, credential: Credential, parameters: Argon2Parameters,
                entropy: KeyEntropy) throws -> KeyBlock {
        try block.rewrap(dataKey: dataKey, credential: credential, parameters: parameters, entropy: entropy)
    }

    private func bodyAAD(_ index: UInt32) -> [UInt8] {
        var writer = ByteWriter()
        writer.append("JEDB".utf8); writer.u8(1); writer.u8(1); writer.u32(index)
        return writer.bytes
    }

    func sealBody(pageIndex: UInt32, plaintext: [UInt8], salt: [UInt8]) throws -> SealedPage {
        guard pageIndex >= firstBodyPage, plaintext.count == pageSize - reservedBytes, salt.count == 24 else {
            throw JedError.corruption("invalid encrypted page envelope")
        }
        try PageEnvelope.validateHeader(plaintext, encrypted: true)
        var clear = plaintext
        try PageEnvelope.putU32(0, into: &clear, at: 12)
        do {
            let sealed = try pageCipher.seal(nonce: salt, plaintext: clear, aad: bodyAAD(pageIndex))
            let tag = Array(sealed.suffix(16))
            return SealedPage(bytes: Array(sealed.dropLast(16)) + salt + tag, tag: tag)
        } catch { throw JedError("58030", "body page sealing failed") }
    }

    func openBody(pageIndex: UInt32, sealed: [UInt8], expectedTag: [UInt8]? = nil) throws -> [UInt8] {
        guard pageIndex >= firstBodyPage, sealed.count == pageSize else {
            throw JedError.corruption("invalid encrypted page envelope")
        }
        let tag = Array(sealed.suffix(16))
        if let expectedTag {
            guard expectedTag.count == 16, expectedTag == tag else { throw JedError.corruption("body page tag does not match its pointer") }
        }
        let end = pageSize - reservedBytes
        var clear: [UInt8]
        do {
            clear = try pageCipher.open(nonce: Array(sealed[end..<(end + 24)]),
                                        sealed: Array(sealed.prefix(end)) + tag, aad: bodyAAD(pageIndex))
        } catch { throw JedError.corruption("body page authentication failed") }
        guard try PageEnvelope.readU32(clear, at: 12) == 0 else {
            throw JedError.corruption("encrypted body page has a nonzero checksum")
        }
        try PageEnvelope.validateHeader(clear, encrypted: true)
        // The common logical-page parser can use its ordinary CRC check. The
        // checksum is synthesized only after authentication and zero-field check.
        try PageEnvelope.putU32(PageEnvelope.checksum(clear, at: 12), into: &clear, at: 12)
        return clear
    }

    private func validateMetaEnvelope(_ page: [UInt8], slot: UInt32) throws {
        guard slot <= 1, page.count == pageSize else { throw JedError.corruption("invalid meta envelope") }
        guard Array(page.prefix(4)) == Array("JEDB".utf8), page[4] == 0, page[5] == 50,
              page[6] == 1, page[7] == 40, try PageEnvelope.readU32(page, at: 8) == UInt32(pageSize),
              page[60..<64].allSatisfy({ $0 == 0 }) else {
            throw JedError.corruption("invalid encrypted meta header")
        }
    }

    private func metaAAD(_ page: [UInt8], slot: UInt32, manifest: [ManifestPageBytes]) throws -> [UInt8] {
        try validateMetaEnvelope(page, slot: slot)
        var clear = page
        try PageEnvelope.putU32(0, into: &clear, at: 32)
        clear.replaceSubrange((pageSize - reservedBytes)..<pageSize, with: repeatElement(UInt8(0), count: reservedBytes))
        var writer = ByteWriter()
        writer.append("JEDB".utf8); writer.u8(1); writer.u8(2); writer.u32(slot); writer.append(clear)
        var seen: Set<UInt32> = []
        for item in manifest {
            guard item.pageIndex >= firstBodyPage, item.bytes.count == pageSize, seen.insert(item.pageIndex).inserted else {
                throw JedError.corruption("invalid manifest page authentication input")
            }
            writer.u32(item.pageIndex); writer.append(item.bytes)
        }
        return writer.bytes
    }

    /// Full structural/manifest validation and keyless slot selection belong to
    /// the pager. This method seals that already constructed candidate.
    func sealMeta(_ page: [UInt8], slot: UInt32, manifest: [ManifestPageBytes] = [], nonce: [UInt8]) throws -> [UInt8] {
        guard nonce.count == 24 else { throw JedError("22023", "meta nonce must contain 24 bytes") }
        let aad = try metaAAD(page, slot: slot, manifest: manifest)
        let tag: [UInt8]
        do { tag = try metaCipher.seal(nonce: nonce, plaintext: [], aad: aad) }
        catch { throw JedError("58030", "meta page sealing failed") }
        var result = page
        result.replaceSubrange((pageSize - reservedBytes)..<pageSize, with: nonce + tag)
        try PageEnvelope.putU32(PageEnvelope.checksum(result, at: 32), into: &result, at: 32)
        return result
    }

    /// Authenticate the one selected slot. Callers must not retry an older
    /// candidate after failure: CRC-valid tampering is corruption.
    func verifyMeta(_ page: [UInt8], slot: UInt32, manifest: [ManifestPageBytes] = []) throws {
        let aad = try metaAAD(page, slot: slot, manifest: manifest)
        guard try PageEnvelope.readU32(page, at: 32) == PageEnvelope.checksum(page, at: 32) else {
            throw JedError.corruption("invalid meta checksum")
        }
        let start = pageSize - reservedBytes
        do {
            _ = try metaCipher.open(nonce: Array(page[start..<(start + 24)]), sealed: Array(page.suffix(16)), aad: aad)
        } catch { throw JedError.corruption("meta page authentication failed") }
    }

    /// The page service transmits no slot ordinal, so its separate protocol
    /// permits either ordinal. This is not file recovery candidate fallback.
    func verifyServedMeta(_ page: [UInt8], manifest: [ManifestPageBytes] = [], staleHeldKey: Bool) throws {
        if (try? verifyMeta(page, slot: 0, manifest: manifest)) != nil { return }
        if (try? verifyMeta(page, slot: 1, manifest: manifest)) != nil { return }
        throw JedError(staleHeldKey ? "28P04" : "08P01", staleHeldKey ? "held data key is stale" : "invalid served meta authentication")
    }
}
