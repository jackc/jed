/// A service writer fixes this authorization at begin_write. A later reader
/// pins the same or newer generation, so it cannot invalidate the reuse grant.
internal struct ServiceDeltaBase: Sendable {
    let txid: UInt64
    let pageCount: UInt32
    let codec: any PageCodec
    let granted: Set<UInt32>
    let pending: [UInt32: UInt64]
    func legal(_ page: UInt32, below count: UInt32) -> Bool {
        page >= codec.firstBodyPage && page < count && (page >= pageCount || granted.contains(page))
    }
}

/// Validation state retains only manifest metadata and received indices. Body
/// pages stream to their safe destinations after check B; they are not buffered
/// as a whole commit. Protocol refusal cannot overwrite a reachable generation.
internal struct ServiceIncomingDelta {
    let meta: MetaPage
    let bytes: [UInt8]
    let manifest: [ManifestPageBytes]
    let entries: [ManifestEntry]
    let protected: Set<UInt32>
    let written: [UInt32]
    private let expected: [UInt32: UInt64]
    private var received: Set<UInt32> = []

    init(base: ServiceDeltaBase, begin: ServiceCommitBegin) throws {
        do {
            guard base.txid < UInt64.max, begin.txid == base.txid + 1,
                  begin.meta.count == base.codec.pageSize else {
                throw PageServiceProtocol.violation("commit does not follow the pinned generation")
            }
            let meta = try MetaPage.decode(begin.meta)
            guard meta.txid == begin.txid, meta.pageCount >= base.pageCount,
                  meta.encrypted == (base.codec.reservedBytes != 0), meta.manifestEntries > 0,
                  begin.manifest.count == meta.manifestPages else {
                throw PageServiceProtocol.violation("commit metadata does not match its base")
            }
            var supplied: [UInt32: [UInt8]] = [:]
            for page in begin.manifest {
                guard page.bytes.count == meta.pageSize, base.legal(page.index, below: meta.pageCount),
                      supplied.updateValue(page.bytes, forKey: page.index) == nil else {
                    throw PageServiceProtocol.violation("illegal manifest page")
                }
            }
            let parsed = try MetaRecovery.validateManifest(meta, bytes: begin.meta, verifyBodies: false) { index in
                guard let bytes = supplied[index] else { throw PageServiceProtocol.violation("missing manifest page") }
                return bytes
            }
            guard parsed.chain == begin.manifest.map(\.index) else {
                throw PageServiceProtocol.violation("manifest pages are not in chain order")
            }
            var expected: [UInt32: UInt64] = [:], carried = 0
            for entry in parsed.entries {
                if let digest = base.pending[entry.pageIndex] {
                    guard entry.digest == digest else { throw PageServiceProtocol.violation("pending-page identity changed") }
                    carried += 1
                } else {
                    guard base.legal(entry.pageIndex, below: meta.pageCount) else {
                        throw PageServiceProtocol.violation("body page is not beyond high-water or granted free")
                    }
                    expected[entry.pageIndex] = entry.digest
                }
            }
            guard carried == base.pending.count else { throw PageServiceProtocol.violation("commit omits pending pages") }
            let manifest = begin.manifest.map { ManifestPageBytes(pageIndex: $0.index, bytes: $0.bytes) }
            if let encrypted = base.codec as? EncryptedPageCodec {
                let first = (try? encrypted.verifyMeta(begin.meta, slot: 0, manifest: manifest)) != nil
                let second = first || (try? encrypted.verifyMeta(begin.meta, slot: 1, manifest: manifest)) != nil
                guard second else { throw PageServiceProtocol.violation("commit meta does not authenticate under the held key") }
            }
            self.meta = meta; bytes = begin.meta; self.manifest = manifest; entries = parsed.entries
            self.expected = expected; protected = Set(parsed.entries.map(\.pageIndex)).union(parsed.chain)
            written = Set(expected.keys).union(parsed.chain).sorted()
        } catch {
            if let error = error as? JedError, error.code == "08P01" { throw error }
            throw PageServiceProtocol.violation("malformed commit metadata or manifest")
        }
    }

    /// Check before writing. A carried pending page may never arrive again.
    mutating func receive(_ page: StoredPage) throws {
        guard let digest = expected[page.index], !received.contains(page.index),
              page.bytes.count == meta.pageSize, Checksums.crc64(page.bytes) == digest else {
            throw PageServiceProtocol.violation("duplicate, unlisted or mismatched body page")
        }
        if !meta.encrypted {
            guard (1...7).contains(page.bytes[0]),
                  try PageEnvelope.readU32(page.bytes, at: 12) == PageEnvelope.checksum(page.bytes, at: 12) else {
                throw PageServiceProtocol.violation("body type or checksum is invalid")
            }
        }
        received.insert(page.index)
    }
    func complete() throws {
        guard received.count == expected.count else { throw PageServiceProtocol.violation("commit ended before every listed page arrived") }
    }
    var ownEntries: [ManifestEntry] {
        expected.keys.sorted().map { ManifestEntry(pageIndex: $0, digest: expected[$0]!) }
    }

    /// Slot authentication changes at the owner; resolution identifies the
    /// logical meta with its checksum and authentication reserve zeroed.
    static func resolutionDigest(_ bytes: [UInt8]) throws -> UInt64 {
        let meta = try MetaPage.decode(bytes)
        guard meta.encrypted else { return Checksums.crc64(bytes) }
        var logical = bytes
        logical.replaceSubrange(32..<36, with: repeatElement(UInt8(0), count: 4))
        logical.replaceSubrange((logical.count - 40)..<logical.count, with: repeatElement(UInt8(0), count: 40))
        return Checksums.crc64(logical)
    }
}
