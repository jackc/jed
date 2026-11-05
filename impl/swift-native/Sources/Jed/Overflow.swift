internal struct OverflowWrite: Sendable {
    let head: PageReference
    let pages: [StoredPage]
}

internal enum OverflowChain {
    static func write(_ payload: [UInt8], allocator: inout PageAllocator,
                      codec: any PageCodec, entropy: KeyEntropy? = nil) throws -> OverflowWrite {
        guard !payload.isEmpty, payload.count <= UInt32.max else {
            throw JedError("0A000", "overflow value exceeds the byte-format limit")
        }
        let capacity = codec.capacity - (codec.reservedBytes == 0 ? 0 : 16)
        let count = (payload.count + capacity - 1) / capacity
        var ids: [UInt32] = []
        for _ in 0..<count { ids.append(try allocator.allocate()) }
        var pages: [StoredPage] = []
        var next = PageSealer.nullReference(codec)
        for ordinal in ids.indices.reversed() {
            let lower = ordinal * capacity, upper = min(payload.count, lower + capacity)
            let sealed = try PageSealer.seal(codec: codec, index: ids[ordinal], type: 4,
                count: UInt32(upper - lower), next: next, payload: Array(payload[lower..<upper]), entropy: entropy)
            pages.append(sealed.page); next = sealed.reference
        }
        return OverflowWrite(head: next, pages: pages.reversed())
    }

    /// Bounded page traversal; the declared value length never causes an eager
    /// reservation. Every slab and successor must agree with that length.
    static func read(_ reference: PageReference, length: Int, pageCount: UInt32,
                     codec: any PageCodec, read: (PageReference) throws -> [UInt8]) throws -> [UInt8] {
        let capacity = codec.capacity - (codec.reservedBytes == 0 ? 0 : 16)
        guard length > 0, pageCount > codec.firstBodyPage,
              UInt64(length) <= UInt64(pageCount - codec.firstBodyPage) * UInt64(capacity) else {
            throw JedError.corruption("invalid overflow value length")
        }
        var output: [UInt8] = [], seen: Set<UInt32> = []
        var current = reference
        let start = codec.reservedBytes == 0 ? 16 : 32
        while output.count < length {
            guard current.index >= codec.firstBodyPage, current.index < pageCount,
                  seen.insert(current.index).inserted else { throw JedError.corruption("invalid overflow chain link") }
            let bytes = try read(current)
            guard bytes.count == codec.pageSize - codec.reservedBytes, bytes[0] == 4,
                  try PageEnvelope.readU32(bytes, at: 12) == PageEnvelope.checksum(bytes, at: 12) else {
                throw JedError.corruption("invalid overflow page")
            }
            try PageEnvelope.validateHeader(bytes, encrypted: codec.reservedBytes != 0)
            let count = Int(try PageEnvelope.readU32(bytes, at: 4)), next = try PageEnvelope.readU32(bytes, at: 8)
            guard count == min(capacity, length - output.count),
                  bytes[(start + count)...].allSatisfy({ $0 == 0 }) else {
                throw JedError.corruption("invalid overflow slab extent or padding")
            }
            output.append(contentsOf: bytes[start..<(start + count)])
            let tag = codec.reservedBytes == 0 ? [] : Array(bytes[16..<32])
            guard (output.count == length) == (next == 0), next != 0 || tag.allSatisfy({ $0 == 0 }) else {
                throw JedError.corruption("overflow terminator disagrees with value length")
            }
            current = PageReference(next, tag: tag)
        }
        return output
    }
}
