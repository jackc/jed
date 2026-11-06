import Foundation

/// Handwritten protocol-v1 codec from spec/pageservice/messages.toml and README.
/// Frame limits are checked before body allocation; list counts are bounded by
/// bytes remaining before collection storage is reserved. Semantic state-machine
/// checks (pins, versions, reserved flag bits) belong to the owner/client.
internal enum PageServiceProtocol {
    static let version: UInt16 = 1
    static let maxFrameLength = 16_777_216
    static let maxReadPages = 1024
    static let maxInvalidationEntries = 65_536
    static let maxReusePages = 65_536
    static let windowCommits = 256
    static let endpointMaxBytes = 100
    static func violation(_ reason: String) -> JedError { JedError("08P01", "page-service protocol: \(reason)") }
    static func checkLength(_ length: UInt32) throws {
        guard length >= 5, length <= maxFrameLength else { throw violation("invalid frame length") }
    }
    static func readLimit(pageSize: Int) -> Int { min(maxReadPages, (maxFrameLength - 9) / pageSize) }
    static func sqlstate(_ value: String) -> Bool {
        value.utf8.count == 5 && value.utf8.allSatisfy { (48...57).contains($0) || (65...90).contains($0) }
    }
}

internal struct ServiceHello: Sendable {
    var minimum: UInt16 = PageServiceProtocol.version
    var maximum: UInt16 = PageServiceProtocol.version
    var format: UInt16 = MetaPage.version
    var flags: UInt8 = 0
    var clientName = "jed-swift"
    var credential: Credential?
}
internal struct ServiceHelloReply: Sendable {
    var protocolVersion: UInt16
    var format: UInt16
    var pageSize: UInt32
    var connection: UInt64
    var flags: UInt8
    var maxReadPages: UInt32
}
internal struct ServiceCachePoint: Sendable {
    var incarnation: [UInt8]
    var txid: UInt64
    static let cold = ServiceCachePoint(incarnation: [UInt8](repeating: 0, count: 16), txid: 0)
}
internal struct ServiceReadReply: Sendable {
    var point: ServiceCachePoint
    var invalidate: UInt8
    var indices: [UInt32]
    var meta: [UInt8]
}
internal struct ServiceWriteReply: Sendable {
    var read: ServiceReadReply
    var reuseGrant: Bool
    var pending: [ManifestEntry]
    var reuse: [UInt32]
}
internal struct ServiceCommitBegin: Sendable {
    var txid: UInt64
    var meta: [UInt8]
    var manifest: [StoredPage]
}
internal enum ServiceMessage: Sendable {
    case hello(ServiceHello)
    case beginRead(ServiceCachePoint)
    case readPages(txid: UInt64, indices: [UInt32])
    case endRead(UInt64)
    case beginWrite(ServiceCachePoint, timeout: UInt64, flags: UInt8)
    case commitBegin(ServiceCommitBegin)
    case commitPages([StoredPage])
    case commitEnd
    case abort
    case commitStatus(txid: UInt64, digest: UInt64)
    case requestOwnership
    case helloOK(ServiceHelloReply)
    case readBegun(ServiceReadReply)
    case pages([[UInt8]])
    case writeBegun(ServiceWriteReply)
    case committed(UInt64)
    case commitStatusReply(UInt8)
    case ownershipReply(UInt8)
    case notice(kind: UInt8, connection: UInt64)
    case error(code: String, message: String, detail: String, flags: UInt8)

    var code: UInt8 {
        switch self {
        case .hello: 0x01
        case .beginRead: 0x02
        case .readPages: 0x03
        case .endRead: 0x04
        case .beginWrite: 0x05
        case .commitBegin: 0x06
        case .commitPages: 0x07
        case .commitEnd: 0x08
        case .abort: 0x09
        case .commitStatus: 0x0a
        case .requestOwnership: 0x0b
        case .helloOK: 0x41
        case .readBegun: 0x42
        case .pages: 0x43
        case .writeBegun: 0x45
        case .committed: 0x48
        case .commitStatusReply: 0x4a
        case .ownershipReply: 0x4b
        case .notice: 0x7e
        case .error: 0x7f
        }
    }
}

internal struct ServiceFrame: Sendable {
    var request: UInt32
    var message: ServiceMessage

    func encode(pageSize: Int = 0) throws -> [UInt8] {
        guard pageSize >= 0, pageSize <= UInt32.max else { throw PageServiceProtocol.violation("invalid connection page size") }
        var body = ServiceWireWriter(pageSize: pageSize)
        switch message {
        case .hello(let value):
            try body.u16(value.minimum); try body.u16(value.maximum); try body.u16(value.format)
            try body.u8(value.flags); try body.string(value.clientName)
            if let credential = value.credential { try body.append(credential.pageServiceBytes()) }
            else { try body.u8(0) }
        case .beginRead(let value): try body.point(value)
        case .readPages(let txid, let indices): try body.u64(txid); try body.indices(indices)
        case .endRead(let txid): try body.u64(txid)
        case .beginWrite(let value, let timeout, let flags):
            try body.point(value); try body.u64(timeout); try body.u8(flags)
        case .commitBegin(let value):
            try body.u64(value.txid); try body.page(value.meta); try body.indexedPages(value.manifest)
        case .commitPages(let pages): try body.indexedPages(pages)
        case .commitEnd, .abort, .requestOwnership: break
        case .commitStatus(let txid, let digest): try body.u64(txid); try body.u64(digest)
        case .helloOK(let value):
            try body.u16(value.protocolVersion); try body.u16(value.format); try body.u32(value.pageSize)
            try body.u64(value.connection); try body.u8(value.flags); try body.u32(value.maxReadPages)
        case .readBegun(let value): try body.read(value)
        case .pages(let pages): try body.pages(pages)
        case .writeBegun(let value):
            try body.point(value.read.point); try body.u8(value.read.invalidate); try body.indices(value.read.indices)
            try body.u8(value.reuseGrant ? 1 : 0); try body.page(value.read.meta)
            try body.digests(value.pending); try body.indices(value.reuse)
        case .committed(let txid): try body.u64(txid)
        case .commitStatusReply(let status), .ownershipReply(let status): try body.u8(status)
        case .notice(let kind, let connection): try body.u8(kind); try body.u64(connection)
        case .error(let code, let message, let detail, let flags):
            guard PageServiceProtocol.sqlstate(code) else { throw PageServiceProtocol.violation("invalid SQLSTATE") }
            try body.append(Array(code.utf8)); try body.string(message); try body.string(detail); try body.u8(flags)
        }
        var frame = ByteWriter(); frame.u32(UInt32(5 + body.bytes.count)); frame.u8(message.code)
        frame.u32(request); frame.append(body.bytes)
        return frame.bytes
    }

    static func decode(_ bytes: [UInt8], pageSize: Int = 0) throws -> ServiceFrame {
        guard pageSize >= 0, pageSize <= UInt32.max else { throw PageServiceProtocol.violation("invalid connection page size") }
        do {
            var input = ByteReader(bytes)
            let length = try input.u32(); try PageServiceProtocol.checkLength(length)
            guard input.remaining == length else { throw PageServiceProtocol.violation("frame length does not match its bytes") }
            let code = try input.u8(), request = try input.u32()
            var body = ServiceWireReader(reader: ByteReader(try input.take(input.remaining)), pageSize: pageSize)
            let message: ServiceMessage
            switch code {
            case 0x01:
                message = .hello(try ServiceHello(minimum: body.u16(), maximum: body.u16(), format: body.u16(),
                    flags: body.u8(), clientName: body.string(), credential: body.credential()))
            case 0x02: message = .beginRead(try body.point())
            case 0x03: message = try .readPages(txid: body.u64(), indices: body.indices())
            case 0x04: message = .endRead(try body.u64())
            case 0x05: message = try .beginWrite(body.point(), timeout: body.u64(), flags: body.u8())
            case 0x06:
                message = .commitBegin(try ServiceCommitBegin(txid: body.u64(), meta: body.page(), manifest: body.indexedPages()))
            case 0x07: message = .commitPages(try body.indexedPages())
            case 0x08: message = .commitEnd
            case 0x09: message = .abort
            case 0x0a: message = try .commitStatus(txid: body.u64(), digest: body.u64())
            case 0x0b: message = .requestOwnership
            case 0x41:
                message = .helloOK(try ServiceHelloReply(protocolVersion: body.u16(), format: body.u16(), pageSize: body.u32(),
                    connection: body.u64(), flags: body.u8(), maxReadPages: body.u32()))
            case 0x42: message = .readBegun(try body.read())
            case 0x43: message = .pages(try body.pages())
            case 0x45:
                let point = try body.point(), invalidation = try body.u8(), indices = try body.indices()
                let grant = try body.boolean(), meta = try body.page(), pending = try body.digests(), reuse = try body.indices()
                message = .writeBegun(ServiceWriteReply(read: ServiceReadReply(point: point, invalidate: invalidation,
                    indices: indices, meta: meta), reuseGrant: grant, pending: pending, reuse: reuse))
            case 0x48: message = .committed(try body.u64())
            case 0x4a: message = .commitStatusReply(try body.u8())
            case 0x4b: message = .ownershipReply(try body.u8())
            case 0x7e: message = try .notice(kind: body.u8(), connection: body.u64())
            case 0x7f:
                let state = String(decoding: try body.reader.take(5), as: UTF8.self)
                guard PageServiceProtocol.sqlstate(state) else { throw PageServiceProtocol.violation("invalid SQLSTATE") }
                message = try .error(code: state, message: body.string(), detail: body.string(), flags: body.u8())
            default: throw PageServiceProtocol.violation("unknown or reserved message code")
            }
            try body.reader.requireEnd()
            return ServiceFrame(request: request, message: message)
        } catch {
            if let error = error as? JedError, error.code == "08P01" { throw error }
            throw PageServiceProtocol.violation("truncated or invalid field")
        }
    }
}

private struct ServiceWireReader {
    var reader: ByteReader
    let pageSize: Int
    mutating func u8() throws -> UInt8 { try reader.u8() }
    mutating func u16() throws -> UInt16 { try reader.u16() }
    mutating func u32() throws -> UInt32 { try reader.u32() }
    mutating func u64() throws -> UInt64 { try reader.u64() }
    mutating func boolean() throws -> Bool {
        let value = try u8()
        guard value <= 1 else { throw PageServiceProtocol.violation("invalid boolean") }
        return value == 1
    }
    mutating func string() throws -> String {
        let count = try u32(), bytes = try reader.take(Int(count))
        guard let value = String(bytes: bytes, encoding: .utf8) else { throw PageServiceProtocol.violation("invalid UTF-8") }
        return value
    }
    mutating func point() throws -> ServiceCachePoint {
        try ServiceCachePoint(incarnation: Array(reader.take(16)), txid: u64())
    }
    mutating func credential() throws -> Credential? {
        switch try u8() {
        case 0: return nil
        case 1: return .passphrase(try string())
        case 2: return try .hostKey(Array(reader.take(32)))
        default: throw PageServiceProtocol.violation("invalid credential kind")
        }
    }
    mutating func count(itemBytes: Int) throws -> Int {
        let count = Int(try u32())
        guard count == 0 || itemBytes > 0 && count <= reader.remaining / itemBytes else {
            throw PageServiceProtocol.violation("list count exceeds its available bytes")
        }
        return count
    }
    mutating func indices() throws -> [UInt32] {
        let count = try count(itemBytes: 4)
        var values: [UInt32] = []; values.reserveCapacity(count)
        for _ in 0..<count { values.append(try u32()) }
        return values
    }
    mutating func digests() throws -> [ManifestEntry] {
        let count = try count(itemBytes: 12)
        var values: [ManifestEntry] = []; values.reserveCapacity(count)
        for _ in 0..<count { values.append(try ManifestEntry(pageIndex: u32(), digest: u64())) }
        return values
    }
    mutating func page() throws -> [UInt8] {
        guard pageSize > 0 else { throw PageServiceProtocol.violation("page field before negotiation") }
        return Array(try reader.take(pageSize))
    }
    mutating func pages() throws -> [[UInt8]] {
        let count = try count(itemBytes: pageSize)
        var values: [[UInt8]] = []; values.reserveCapacity(count)
        for _ in 0..<count { values.append(try page()) }
        return values
    }
    mutating func indexedPages() throws -> [StoredPage] {
        let count = try count(itemBytes: pageSize > 0 ? pageSize + 4 : 0)
        var values: [StoredPage] = []; values.reserveCapacity(count)
        for _ in 0..<count { values.append(try StoredPage(index: u32(), bytes: page())) }
        return values
    }
    mutating func read() throws -> ServiceReadReply {
        try ServiceReadReply(point: point(), invalidate: u8(), indices: indices(), meta: page())
    }
}

private struct ServiceWireWriter {
    private var output = ByteWriter()
    var bytes: [UInt8] { output.bytes }
    private(set) var pageSize: Int
    init(pageSize: Int) { self.pageSize = pageSize }
    mutating func append(_ bytes: [UInt8]) throws {
        guard bytes.count <= PageServiceProtocol.maxFrameLength - 5 - output.bytes.count else {
            throw PageServiceProtocol.violation("encoded frame exceeds maximum length")
        }
        output.append(bytes)
    }
    mutating func u8(_ value: UInt8) throws { try append([value]) }
    mutating func u16(_ value: UInt16) throws { var bytes = ByteWriter(); bytes.u16(value); try append(bytes.bytes) }
    mutating func u32(_ value: UInt32) throws { var bytes = ByteWriter(); bytes.u32(value); try append(bytes.bytes) }
    mutating func u64(_ value: UInt64) throws { var bytes = ByteWriter(); bytes.u64(value); try append(bytes.bytes) }
    mutating func string(_ value: String) throws {
        guard value.utf8.count <= PageServiceProtocol.maxFrameLength - 9 - output.bytes.count else {
            throw PageServiceProtocol.violation("encoded string exceeds frame limit")
        }
        try u32(UInt32(value.utf8.count)); try append(Array(value.utf8))
    }
    mutating func point(_ value: ServiceCachePoint) throws {
        guard value.incarnation.count == 16 else { throw PageServiceProtocol.violation("invalid incarnation length") }
        try append(value.incarnation); try u64(value.txid)
    }
    mutating func listCount(_ count: Int, bytesPerItem: Int) throws {
        guard count >= 0, count <= UInt32.max, count == 0 || bytesPerItem > 0 &&
            count <= (PageServiceProtocol.maxFrameLength - 9 - output.bytes.count) / bytesPerItem else {
            throw PageServiceProtocol.violation("encoded list exceeds frame limit")
        }
        try u32(UInt32(count))
    }
    mutating func indices(_ values: [UInt32]) throws {
        try listCount(values.count, bytesPerItem: 4)
        for value in values { try u32(value) }
    }
    mutating func digests(_ values: [ManifestEntry]) throws {
        try listCount(values.count, bytesPerItem: 12)
        for value in values { try u32(value.pageIndex); try u64(value.digest) }
    }
    mutating func page(_ bytes: [UInt8]) throws {
        if pageSize == 0 { pageSize = bytes.count }
        guard pageSize > 0, bytes.count == pageSize else { throw PageServiceProtocol.violation("inconsistent page size") }
        try append(bytes)
    }
    mutating func pages(_ values: [[UInt8]]) throws {
        if pageSize == 0, let first = values.first { pageSize = first.count }
        try listCount(values.count, bytesPerItem: pageSize)
        for value in values { try page(value) }
    }
    mutating func indexedPages(_ values: [StoredPage]) throws {
        if pageSize == 0, let first = values.first { pageSize = first.bytes.count }
        try listCount(values.count, bytesPerItem: pageSize > 0 ? pageSize + 4 : 0)
        for value in values { try u32(value.index); try page(value.bytes) }
    }
    mutating func read(_ value: ServiceReadReply) throws {
        try point(value.point); try u8(value.invalidate); try indices(value.indices); try page(value.meta)
    }
}

internal struct ServiceOwnerRecord: Sendable {
    var serving: Bool
    var uid: UInt32
    var incarnation: [UInt8]
    var pid: UInt32
    var endpoint: String

    func encode() throws -> [UInt8] {
        guard incarnation.count == 16, endpoint.utf8.count <= UInt32.max else {
            throw PageServiceProtocol.violation("invalid owner record")
        }
        var bytes = ByteWriter(); bytes.append("JEDOWNER".utf8); bytes.u16(1); bytes.u8(serving ? 1 : 0)
        bytes.u32(uid); bytes.append(incarnation); bytes.u32(pid)
        bytes.u32(UInt32(endpoint.utf8.count)); bytes.append(endpoint.utf8)
        bytes.u32(Checksums.crc32(bytes.bytes)); return bytes.bytes
    }
    static func decode(_ bytes: [UInt8]) -> ServiceOwnerRecord? {
        do {
            var reader = ByteReader(bytes)
            guard Array(try reader.take(8)) == Array("JEDOWNER".utf8), try reader.u16() == 1 else { return nil }
            let flags = try reader.u8()
            guard flags & ~UInt8(1) == 0 else { return nil }
            let uid = try reader.u32(), incarnation = Array(try reader.take(16)), pid = try reader.u32()
            let length = try reader.u32()
            guard let endpoint = String(bytes: try reader.take(Int(length)), encoding: .utf8),
                  try reader.u32() == Checksums.crc32(bytes.dropLast(4)), reader.remaining == 0 else { return nil }
            return ServiceOwnerRecord(serving: flags & 1 != 0, uid: uid, incarnation: incarnation, pid: pid, endpoint: endpoint)
        } catch { return nil }
    }
}
