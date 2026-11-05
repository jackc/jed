/// Validated record-free routing page. Encrypted child pointers include their
/// bound tags; separator bytes remain logical keys, never ciphertext.
internal struct PackedInterior: Sendable {
    let keys: [[UInt8]]
    let children: [PageReference]
    let payloadSize: Int

    init(bytes: [UInt8], codec: any PageCodec, pageCount: UInt32) throws {
        guard bytes.count == codec.pageSize - codec.reservedBytes else {
            throw JedError.corruption("invalid interior page size")
        }
        try PageEnvelope.validateHeader(bytes, encrypted: codec.reservedBytes != 0)
        guard bytes[0] == 3,
              try PageEnvelope.readU32(bytes, at: 12) == PageEnvelope.checksum(bytes, at: 12) else {
            throw JedError.corruption("invalid interior page header or checksum")
        }
        let count = Int(try PageEnvelope.readU32(bytes, at: 4))
        let width = Self.pointerWidth(codec)
        guard count <= (codec.capacity - width) / (width + 4) else {
            throw JedError.corruption("interior directory exceeds page")
        }
        var reader = ByteReader(bytes[16...]), children: [PageReference] = []
        var seen: Set<UInt32> = []
        for _ in 0...count {
            let index = try reader.u32()
            guard index >= codec.firstBodyPage, index < pageCount, seen.insert(index).inserted else {
                throw JedError.corruption("invalid or repeated interior child")
            }
            let tag = width == 4 ? [] : Array(try reader.take(16))
            children.append(PageReference(index, tag: tag))
        }
        var ends: [Int] = [], previous = 0
        let available = codec.capacity - reader.offset - 4 * count
        for _ in 0..<count {
            let end = Int(try reader.u32())
            guard end >= previous, end <= available else {
                throw JedError.corruption("invalid separator directory")
            }
            ends.append(end); previous = end
        }
        var keys: [[UInt8]] = []
        previous = 0
        for end in ends {
            let key = Array(try reader.take(end - previous))
            guard keys.last.map({ $0.lexicographicallyPrecedes(key) }) ?? true else {
                throw JedError.corruption("interior keys are not strictly ordered")
            }
            keys.append(key); previous = end
        }
        payloadSize = reader.offset
        guard try reader.take(reader.remaining).allSatisfy({ $0 == 0 }) else {
            throw JedError.corruption("nonzero interior padding")
        }
        self.keys = keys; self.children = children
    }

    static func pointerWidth(_ codec: any PageCodec) -> Int { codec.reservedBytes == 0 ? 4 : 20 }

    static func payloadSize(keys: [[UInt8]], codec: any PageCodec) -> Int {
        keys.reduce(pointerWidth(codec) * (keys.count + 1) + 4 * keys.count) { $0 + $1.count }
    }

    static func encodePayload(keys: [[UInt8]], children: [PageReference], codec: any PageCodec) throws -> [UInt8] {
        guard children.count == keys.count + 1,
              zip(keys, keys.dropFirst()).allSatisfy({ $0.lexicographicallyPrecedes($1) }),
              payloadSize(keys: keys, codec: codec) <= codec.capacity else {
            throw JedError.corruption("invalid interior shape or page capacity")
        }
        let tagLength = pointerWidth(codec) - 4
        var writer = ByteWriter(), end = 0
        for child in children {
            guard child.index >= codec.firstBodyPage, child.tag.count == tagLength else {
                throw JedError.corruption("invalid interior child reference")
            }
            writer.u32(child.index); writer.append(child.tag)
        }
        for key in keys { end += key.count; writer.u32(UInt32(end)) }
        for key in keys { writer.append(key) }
        return writer.bytes
    }
}
