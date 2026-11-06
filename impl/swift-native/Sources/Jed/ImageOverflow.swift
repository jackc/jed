/// From-scratch chain emission retains only one slab of stored bytes. An
/// inherited chain needs its source references so authenticated successors can
/// be sealed backwards; its SQL value is never decoded or recompressed.
internal enum ImageOverflow {
    static func pointer(_ head: PageReference, stored: Int, raw: Int, compressed: Bool) throws -> [UInt8] {
        guard stored > 0, stored <= UInt32.max, raw >= 0, raw <= UInt32.max else { throw JedError.corruption("invalid image overflow length") }
        var writer = ByteWriter(); writer.u8(compressed ? 4 : 2); writer.u32(head.index); writer.u32(UInt32(stored))
        if compressed { writer.u32(UInt32(raw)) }
        writer.append(head.tag); return writer.bytes
    }

    static func fresh(_ bytes: [UInt8], allocator: inout PageAllocator, codec: any PageCodec,
                      entropy: KeyEntropy?, sink: (StoredPage) throws -> Void) throws -> PageReference {
        guard !bytes.isEmpty, bytes.count <= UInt32.max else { throw JedError.corruption("invalid fresh image overflow") }
        let slab = codec.capacity - (codec.reservedBytes == 0 ? 0 : 16)
        let count = (bytes.count + slab - 1) / slab
        let first = try allocate(count, allocator: &allocator)
        var next = PageSealer.nullReference(codec)
        for ordinal in (0..<count).reversed() {
            let lower = ordinal * slab, upper = min(bytes.count, lower + slab)
            let sealed = try PageSealer.seal(codec: codec, index: first + UInt32(ordinal), type: 4,
                count: UInt32(upper - lower), next: next, payload: Array(bytes[lower..<upper]), entropy: entropy)
            try sink(sealed.page); next = sealed.reference
        }
        return next
    }

    static func copy(_ head: PageReference, length: Int, pageCount: UInt32, pool: NodePool,
                     allocator: inout PageAllocator, codec: any PageCodec, entropy: KeyEntropy?,
                     sink: (StoredPage) throws -> Void) throws -> PageReference {
        let source = pool.codec, slab = source.capacity - (source.reservedBytes == 0 ? 0 : 16)
        guard source.pageSize == codec.pageSize, source.reservedBytes == codec.reservedBytes,
              length > 0, pageCount > source.firstBodyPage,
              UInt64(length) <= UInt64(pageCount - source.firstBodyPage) * UInt64(slab) else {
            throw JedError.corruption("invalid inherited image overflow")
        }
        let start = source.reservedBytes == 0 ? 16 : 32
        func read(_ reference: PageReference, remaining: Int) throws -> (bytes: [UInt8], count: Int, next: PageReference) {
            guard reference.index >= source.firstBodyPage, reference.index < pageCount else { throw JedError.corruption("image overflow exceeds source") }
            let bytes = try pool.readBody(reference)
            guard bytes.count == source.pageSize - source.reservedBytes, bytes[0] == 4,
                  try PageEnvelope.readU32(bytes, at: 12) == PageEnvelope.checksum(bytes, at: 12) else {
                throw JedError.corruption("invalid image overflow page")
            }
            try PageEnvelope.validateHeader(bytes, encrypted: source.reservedBytes != 0)
            let count = Int(try PageEnvelope.readU32(bytes, at: 4)), next = try PageEnvelope.readU32(bytes, at: 8)
            let tag = source.reservedBytes == 0 ? [] : Array(bytes[16..<32])
            guard count == min(slab, remaining), bytes[(start + count)...].allSatisfy({ $0 == 0 }),
                  (remaining == count) == (next == 0), next != 0 || tag.allSatisfy({ $0 == 0 }) else {
                throw JedError.corruption("invalid image overflow slab or terminator")
            }
            return (bytes, count, PageReference(next, tag: tag))
        }
        var references: [PageReference] = [], seen: Set<UInt32> = [], remaining = length, current = head
        while remaining > 0 {
            guard seen.insert(current.index).inserted else { throw JedError.corruption("cyclic image overflow") }
            let page = try read(current, remaining: remaining)
            references.append(current); current = page.next; remaining -= page.count
        }
        let first = try allocate(references.count, allocator: &allocator)
        var next = PageSealer.nullReference(codec)
        for ordinal in references.indices.reversed() {
            let page = try read(references[ordinal], remaining: length - ordinal * slab)
            // The immutable source pin covers both passes. A mutable or faulty
            // byte device still cannot silently change the recorded chain.
            let expected = ordinal + 1 == references.count ? PageSealer.nullReference(source) : references[ordinal + 1]
            guard page.next == expected else { throw JedError.corruption("image overflow changed during copying") }
            let sealed = try PageSealer.seal(codec: codec, index: first + UInt32(ordinal), type: 4,
                count: UInt32(page.count), next: next, payload: Array(page.bytes[start..<(start + page.count)]), entropy: entropy)
            try sink(sealed.page); next = sealed.reference
        }
        return next
    }

    private static func allocate(_ count: Int, allocator: inout PageAllocator) throws -> UInt32 {
        let first = try allocator.allocate()
        for ordinal in 1..<count {
            guard try allocator.allocate() == first + UInt32(ordinal) else { throw JedError.corruption("image allocator is not contiguous") }
        }
        return first
    }
}
