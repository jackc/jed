/// A leaf cell is a borrowed slice of an immutable packed page. NULL is nil;
/// fixed-width bodies have no tag. No SQL value tree is constructed at fault.
internal struct StoredCell: Sendable {
    let bytes: ArraySlice<UInt8>?
    let type: TypeDescriptor
    var fixedWidth: Int? { ValueCodec.fixedWidth(type.type) }
    var storedSize: Int { fixedWidth ?? bytes?.count ?? 0 }
    var isNull: Bool { bytes == nil }

    func materialize(pager: Pager? = nil, pageCount: UInt32? = nil) throws -> SQLValue {
        guard let bytes else { return .null }
        if fixedWidth != nil { return try ValueCodec.decodeBody(Array(bytes), type: type) }
        var reader = ByteReader(bytes)
        let tag = try reader.u8()
        if tag == 0 { return try ValueCodec.decode(Array(bytes), type: type) }
        if tag == 3 {
            let raw = Int(try reader.u32()), compressed = Int(try reader.u16())
            let block = Array(try reader.take(compressed)); try reader.requireEnd()
            return try ValueCodec.decodeRawBody(LZ4.decompress(block, rawLength: raw), type: type)
        }
        guard tag == 2 || tag == 4, let pager else { throw JedError.corruption("unresolvable stored value") }
        let first = try reader.u32(), length = Int(try reader.u32())
        let rawLength = tag == 4 ? Int(try reader.u32()) : length
        let pointerTag = pager.codec.reservedBytes == 0 ? [] : Array(try reader.take(16))
        try reader.requireEnd()
        let content = try OverflowChain.read(PageReference(first, tag: pointerTag), length: length,
            pageCount: pageCount ?? pager.selected.meta.pageCount, codec: pager.codec, read: pager.readBody)
        let raw = tag == 4 ? try LZ4.decompress(content, rawLength: rawLength) : content
        return try ValueCodec.decodeRawBody(raw, type: type)
    }

    /// Storage facts support touched-set cost and kept-chain planning without
    /// reading or decompressing an external value.
    func largeFacts(encrypted: Bool) throws -> (raw: Int, stored: Int, external: PageReference?, compressed: Bool)? {
        guard fixedWidth == nil, let bytes else { return nil }
        var reader = ByteReader(bytes)
        switch try reader.u8() {
        case 0: return nil
        case 2, 4:
            let compressed = bytes.first == 4
            let first = try reader.u32(), stored = Int(try reader.u32())
            let raw = compressed ? Int(try reader.u32()) : stored
            let tag = encrypted ? Array(try reader.take(16)) : []
            try reader.requireEnd()
            return (raw, stored, PageReference(first, tag: tag), compressed)
        case 3:
            let raw = Int(try reader.u32()), stored = Int(try reader.u16())
            _ = try reader.take(stored); try reader.requireEnd()
            return (raw, stored, nil, true)
        default: throw JedError.corruption("invalid packed value tag")
        }
    }
}

internal struct PackedRecord: Sendable {
    let key: [UInt8]
    let cells: [StoredCell]
    let overflows: [Int: PendingOverflow]
    init(key: [UInt8], cells: [StoredCell], overflows: [Int: PendingOverflow] = [:]) {
        self.key = key; self.cells = cells; self.overflows = overflows
    }
    var weight: Int { cells.reduce(key.count) { $0 + $1.storedSize } }

    func materialize(column: Int, pager: Pager? = nil, pageCount: UInt32? = nil) throws -> SQLValue {
        if let overflow = overflows[column] {
            let raw = overflow.compressed ? try LZ4.decompress(overflow.bytes, rawLength: overflow.rawLength) : overflow.bytes
            return try ValueCodec.decodeRawBody(raw, type: cells[column].type)
        }
        return try cells[column].materialize(pager: pager, pageCount: pageCount)
    }

    func sealOverflow(allocator: inout PageAllocator, codec: any PageCodec,
                      entropy: KeyEntropy? = nil) throws -> (record: PackedRecord, pages: [StoredPage]) {
        var cells = cells, pages: [StoredPage] = []
        for column in cells.indices {
            guard let pending = overflows[column] else { continue }
            let chain = try OverflowChain.write(pending.bytes, allocator: &allocator, codec: codec, entropy: entropy)
            let pointer = try pending.pointer(chain.head)
            cells[column] = StoredCell(bytes: pointer[...], type: cells[column].type)
            pages.append(contentsOf: chain.pages)
        }
        return (PackedRecord(key: key, cells: cells), pages)
    }
}

internal struct PendingOverflow: Sendable {
    let bytes: [UInt8]
    let rawLength: Int
    let compressed: Bool
    func pointer(_ reference: PageReference) throws -> [UInt8] {
        guard bytes.count <= UInt32.max, rawLength >= 0, rawLength <= UInt32.max else {
            throw JedError("0A000", "overflow value exceeds the byte-format limit")
        }
        var writer = ByteWriter()
        writer.u8(compressed ? 4 : 2); writer.u32(reference.index); writer.u32(UInt32(bytes.count))
        if compressed { writer.u32(UInt32(rawLength)) }
        writer.append(reference.tag)
        return writer.bytes
    }
}

internal final class PackedLeaf: Sendable {
    private enum Region: Sendable {
        case fixed(bitmap: Range<Int>, values: Range<Int>, width: Int)
        case variable(offsets: [Int], values: Range<Int>)
    }
    let bytes: [UInt8]
    let types: [TypeDescriptor]
    let count: Int
    let payloadSize: Int
    private let keys: [Range<Int>]
    private let regions: [Region]

    convenience init(bytes: [UInt8], types: [TypeDescriptor], codec: any PageCodec) throws {
        try self.init(bytes: bytes, types: types, capacity: codec.capacity, encrypted: codec.reservedBytes != 0)
    }

    /// Dirty nodes stay packed even when no valid split exists. Pending writes
    /// retain these bytes until commit preflight rejects the physical page fit;
    /// the on-disk initializer above always uses the codec's exact capacity.
    static func pending(_ records: [PackedRecord], types: [TypeDescriptor], codec: any PageCodec) throws -> PackedLeaf {
        let total = records.reduce(overhead(records: records.count, types: types)) { $0 + $1.weight }
        guard total <= UInt32.max else { throw JedError("0A000", "pending leaf exceeds the byte-format limit") }
        let capacity = max(codec.capacity, total)
        let payload = try encodePayload(records, types: types, capacity: capacity)
        var writer = ByteWriter(); writer.u8(2); writer.u8(0); writer.u16(0)
        writer.u32(UInt32(records.count)); writer.u32(0); writer.u32(0); writer.append(payload)
        writer.append(repeatElement(UInt8(0), count: capacity - payload.count))
        var bytes = writer.bytes
        try PageEnvelope.putU32(PageEnvelope.checksum(bytes, at: 12), into: &bytes, at: 12)
        return try PackedLeaf(bytes: bytes, types: types, capacity: capacity, encrypted: codec.reservedBytes != 0)
    }

    convenience init(integrityBytes bytes: [UInt8], types: [TypeDescriptor], codec: any PageCodec) throws {
        try self.init(bytes: bytes, types: types, capacity: codec.capacity, encrypted: codec.reservedBytes != 0, integrity: true)
    }

    private init(bytes: [UInt8], types: [TypeDescriptor], capacity: Int, encrypted: Bool, integrity: Bool = false) throws {
        guard bytes.count == capacity + 16, bytes.count >= 16 else {
            throw JedError.corruption("invalid packed leaf size")
        }
        try PageEnvelope.validateHeader(bytes, encrypted: encrypted)
        guard bytes[0] == 2, try PageEnvelope.readU32(bytes, at: 12) == PageEnvelope.checksum(bytes, at: 12) else {
            throw JedError.corruption("invalid packed leaf header or checksum")
        }
        let count = Int(try PageEnvelope.readU32(bytes, at: 4))
        guard count > 0, count <= capacity / 4, types.count < capacity / 4 else {
            throw JedError.corruption("invalid packed leaf directory counts")
        }
        var reader = ByteReader(bytes[16...])
        var ends: [Int] = [], previous = 0
        for _ in 0..<count {
            let end = Int(try reader.u32())
            guard end >= previous, end <= capacity - 4 * count else {
                throw JedError.corruption("invalid key directory")
            }
            ends.append(end); previous = end
        }
        let keyStart = 16 + reader.offset
        _ = try reader.take(previous)
        var ranges: [Range<Int>] = [], start = keyStart
        for end in ends { ranges.append(start..<(keyStart + end)); start = keyStart + end }
        for index in 1..<count where !integrity {
            guard bytes[ranges[index - 1]].lexicographicallyPrecedes(bytes[ranges[index]]) else {
                throw JedError.corruption("leaf keys are not strictly increasing")
            }
        }
        var starts: [Int] = []
        for _ in 0...types.count { starts.append(Int(try reader.u32())) }
        guard starts.first == reader.offset, let contentEnd = starts.last, contentEnd <= capacity,
              zip(starts, starts.dropFirst()).allSatisfy({ $0 <= $1 }),
              bytes[(16 + contentEnd)...].allSatisfy({ $0 == 0 }) else {
            throw JedError.corruption("invalid column directory or leaf padding")
        }
        var regions: [Region] = []
        for column in types.indices {
            let range = (16 + starts[column])..<(16 + starts[column + 1])
            guard !range.isEmpty, bytes[range.lowerBound] == 0 else { throw JedError.corruption("invalid column region flags") }
            if let width = ValueCodec.fixedWidth(types[column].type) {
                let bitmapCount = (count + 7) / 8
                guard range.count == 1 + bitmapCount + count * width else { throw JedError.corruption("invalid fixed column extent") }
                let bitmap = (range.lowerBound + 1)..<(range.lowerBound + 1 + bitmapCount)
                let values = bitmap.upperBound..<range.upperBound
                if count % 8 != 0 {
                    guard bytes[bitmap.upperBound - 1] & (UInt8.max >> (count % 8)) == 0 else {
                        throw JedError.corruption("nonzero unused null-bitmap bits")
                    }
                }
                for row in 0..<count where bytes[bitmap.lowerBound + row / 8] & (0x80 >> (row % 8)) != 0 {
                    let start = values.lowerBound + row * width
                    guard bytes[start..<(start + width)].allSatisfy({ $0 == 0 }) else {
                        throw JedError.corruption("nonzero NULL fixed-width slot")
                    }
                }
                regions.append(.fixed(bitmap: bitmap, values: values, width: width))
            } else {
                guard range.count >= 1 + count * 4 else { throw JedError.corruption("truncated variable column directory") }
                var directory = ByteReader(bytes[(range.lowerBound + 1)..<range.upperBound])
                var offsets: [Int] = [], prior = 0
                let valueStart = range.lowerBound + 1 + count * 4
                for _ in 0..<count {
                    let end = Int(try directory.u32())
                    guard end >= prior, end <= range.upperBound - valueStart else {
                        throw JedError.corruption("invalid value directory")
                    }
                    if end > prior && !integrity {
                        let first = bytes[valueStart + prior]
                        guard first == 0 || first == 2 || first == 3 || first == 4 else {
                            throw JedError.corruption("invalid leaf value presence tag")
                        }
                    }
                    offsets.append(end); prior = end
                }
                guard prior == range.upperBound - valueStart else { throw JedError.corruption("unused variable column bytes") }
                regions.append(.variable(offsets: offsets, values: valueStart..<range.upperBound))
            }
        }
        self.bytes = bytes; self.types = types; self.count = count
        payloadSize = contentEnd; keys = ranges; self.regions = regions
    }

    func key(at row: Int) -> ArraySlice<UInt8> { bytes[keys[row]] }

    func cell(row: Int, column: Int) -> StoredCell {
        let range: Range<Int>?
        switch regions[column] {
        case .fixed(let bitmap, let values, let width):
            let null = bytes[bitmap.lowerBound + row / 8] & (0x80 >> (row % 8)) != 0
            let start = values.lowerBound + row * width
            range = null ? nil : start..<(start + width)
        case .variable(let offsets, let values):
            let start = values.lowerBound + (row == 0 ? 0 : offsets[row - 1]), end = values.lowerBound + offsets[row]
            range = start == end ? nil : start..<end
        }
        return StoredCell(bytes: range.map { bytes[$0] }, type: types[column])
    }

    func record(at row: Int) -> PackedRecord {
        PackedRecord(key: Array(key(at: row)), cells: types.indices.map { cell(row: row, column: $0) })
    }

    static func overhead(records: Int, types: [TypeDescriptor]) -> Int {
        types.reduce(4 * records + 4 * (types.count + 1)) {
            $0 + (ValueCodec.fixedWidth($1.type) == nil ? 1 + 4 * records : 1 + (records + 7) / 8)
        }
    }

    static func encodePayload(_ records: [PackedRecord], types: [TypeDescriptor], capacity: Int) throws -> [UInt8] {
        guard !records.isEmpty, records.count <= capacity / 4,
              records.allSatisfy({ $0.cells.count == types.count }),
              zip(records, records.dropFirst()).allSatisfy({ $0.key.lexicographicallyPrecedes($1.key) }) else {
            throw JedError.corruption("invalid packed leaf records")
        }
        let total = records.reduce(overhead(records: records.count, types: types)) { $0 + $1.weight }
        guard total <= capacity else { throw JedError("0A000", "leaf payload exceeds page capacity") }
        var keyDirectory = ByteWriter(), keys = ByteWriter(), regions: [[UInt8]] = []
        for record in records { keys.append(record.key); keyDirectory.u32(UInt32(keys.bytes.count)) }
        for column in types.indices {
            var region = ByteWriter(); region.u8(0)
            if let width = ValueCodec.fixedWidth(types[column].type) {
                var bitmap = [UInt8](repeating: 0, count: (records.count + 7) / 8), values = ByteWriter()
                for (row, record) in records.enumerated() {
                    if let bytes = record.cells[column].bytes {
                        guard bytes.count == width else { throw JedError.corruption("invalid fixed-width cell") }
                        values.append(bytes)
                    } else {
                        bitmap[row / 8] |= 0x80 >> (row % 8)
                        values.append(repeatElement(UInt8(0), count: width))
                    }
                }
                region.append(bitmap); region.append(values.bytes)
            } else {
                var directory = ByteWriter(), values = ByteWriter()
                for record in records {
                    if let bytes = record.cells[column].bytes { values.append(bytes) }
                    directory.u32(UInt32(values.bytes.count))
                }
                region.append(directory.bytes); region.append(values.bytes)
            }
            regions.append(region.bytes)
        }
        var result = ByteWriter()
        result.append(keyDirectory.bytes); result.append(keys.bytes)
        var offset = result.bytes.count + 4 * (types.count + 1)
        result.u32(UInt32(offset))
        for region in regions { offset += region.count; result.u32(UInt32(offset)) }
        for region in regions { result.append(region) }
        return result.bytes
    }
}
