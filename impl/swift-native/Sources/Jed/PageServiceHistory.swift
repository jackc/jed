/// Bounded publication metadata. DatabaseCore owns this value inside its pin
/// registry, so a remote begin sees one consistent meta/window/watermark state.
internal struct PageServiceHistory: Sendable {
    struct Written: Sendable { let txid: UInt64; let indices: [UInt32]? }
    struct Resolution: Sendable { let txid: UInt64; let digest: UInt64 }
    let incarnation: [UInt8]
    private(set) var windowStart: UInt64
    private var written: [Written] = []
    private var resolution: [Resolution]

    static func digest(_ selected: ValidatedMeta) -> UInt64 {
        if !selected.meta.encrypted { return Checksums.crc64(selected.bytes) }
        var bytes = selected.bytes
        bytes.replaceSubrange(32..<36, with: repeatElement(UInt8(0), count: 4))
        bytes.replaceSubrange((bytes.count - 40)..<bytes.count, with: repeatElement(UInt8(0), count: 40))
        return Checksums.crc64(bytes)
    }
    init(incarnation: [UInt8], initial: ValidatedMeta, predecessor: [UInt8]? = nil) throws {
        guard incarnation.count == 16, incarnation.contains(where: { $0 != 0 }) else {
            throw JedError("58030", "invalid page-service incarnation")
        }
        self.incarnation = incarnation; windowStart = initial.meta.txid; resolution = []
        if let predecessor, let meta = try? MetaPage.decode(predecessor), meta.txid < UInt64.max,
           meta.txid + 1 == initial.meta.txid {
            resolution.append(Resolution(txid: meta.txid, digest: try ServiceIncomingDelta.resolutionDigest(predecessor)))
        }
        resolution.append(Resolution(txid: initial.meta.txid, digest: try ServiceIncomingDelta.resolutionDigest(initial.bytes)))
    }
    /// The serializer/validator computes digest before the storage phase. This
    /// publication step allocates no page bytes and cannot reject a durable root.
    mutating func publish(txid: UInt64, digest: UInt64, indices: [UInt32], reset: Bool = false) {
        if reset { written.removeAll(keepingCapacity: true); windowStart = txid }
        else {
            // One too-large exact set always answers "all"; retaining its whole
            // index vector adds no information. The txid still ages normally.
            let values = indices.count > PageServiceProtocol.maxInvalidationEntries ? nil : indices
            written.append(Written(txid: txid, indices: values))
            if written.count > PageServiceProtocol.windowCommits {
                windowStart = written.removeFirst().txid
            }
        }
        resolution.append(Resolution(txid: txid, digest: digest))
        if resolution.count > PageServiceProtocol.windowCommits { resolution.removeFirst() }
    }
    func answer(_ point: ServiceCachePoint, selected: ValidatedMeta) -> ServiceReadReply {
        var indices: Set<UInt32> = []
        var all = point.incarnation != incarnation || point.txid == 0 || point.txid > selected.meta.txid || point.txid < windowStart
        if !all {
            for entry in written where entry.txid > point.txid && entry.txid <= selected.meta.txid {
                guard let values = entry.indices else { all = true; break }
                for index in values {
                    indices.insert(index)
                    if indices.count > PageServiceProtocol.maxInvalidationEntries { all = true; break }
                }
                if all { break }
            }
        }
        return ServiceReadReply(point: ServiceCachePoint(incarnation: incarnation, txid: selected.meta.txid),
            invalidate: all ? 1 : 0, indices: all ? [] : indices.sorted(), meta: selected.bytes)
    }
    func status(txid: UInt64, digest: UInt64, current: UInt64) -> UInt8 {
        if txid > current { return 1 }
        if let entry = resolution.first(where: { $0.txid == txid }) { return entry.digest == digest ? 0 : 1 }
        return 2
    }
}
