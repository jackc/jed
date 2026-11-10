/// A leaf cell is a borrowed slice of an immutable packed page. NULL is nil;
/// fixed-width bodies have no tag. No SQL value tree is constructed at fault.
internal struct StoredCell: Sendable {
    let bytes: ArraySlice<UInt8>?
    /// The fixed slot width in bytes, or 0 for a variable-width column. A
    /// cell carries no type descriptor: copying one per cell (and per record
    /// copy) cost more than the bytes it described. Decoding takes the
    /// column's descriptor from the owning tree.
    let width: UInt8
    init(bytes: ArraySlice<UInt8>?, width: Int) { self.bytes = bytes; self.width = UInt8(width) }
    init(bytes: ArraySlice<UInt8>?, type: TypeDescriptor) {
        self.init(bytes: bytes, width: ValueCodec.fixedWidth(type.type) ?? 0)
    }
    var fixedWidth: Int? { width == 0 ? nil : Int(width) }
    var storedSize: Int { width == 0 ? bytes?.count ?? 0 : Int(width) }
    var isNull: Bool { bytes == nil }

    func materialize(type: TypeDescriptor, pager: Pager? = nil, pageCount: UInt32? = nil) throws -> SQLValue {
        guard let bytes else { return .null }
        if fixedWidth != nil { return try ValueCodec.decodeBody(bytes, type: type) }
        var reader = ByteReader(bytes)
        let tag = try reader.u8()
        if tag == 0 { return try ValueCodec.decode(bytes, type: type) }
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
    /// The fields share one immutable box: a writer leaf holds its records in
    /// an array, and every edit of that leaf copies the array, so a record
    /// must cost one retain to copy rather than one per field.
    private final class Storage: @unchecked Sendable {
        let key: [UInt8]
        let cells: [StoredCell]
        let overflows: [Int: PendingOverflow]
        init(key: [UInt8], cells: [StoredCell], overflows: [Int: PendingOverflow]) {
            self.key = key
            self.cells = cells
            self.overflows = overflows
        }
    }
    private let storage: Storage
    var key: [UInt8] { storage.key }
    var cells: [StoredCell] { storage.cells }
    var overflows: [Int: PendingOverflow] { storage.overflows }
    init(key: [UInt8], cells: [StoredCell], overflows: [Int: PendingOverflow] = [:]) {
        storage = Storage(key: key, cells: cells, overflows: overflows)
    }
    var weight: Int { cells.reduce(key.count) { $0 + $1.storedSize } }

    func materialize(column: Int, type: TypeDescriptor, pager: Pager? = nil, pageCount: UInt32? = nil) throws -> SQLValue {
        if !overflows.isEmpty, let overflow = overflows[column] {
            let raw = overflow.compressed ? try LZ4.decompress(overflow.bytes, rawLength: overflow.rawLength) : overflow.bytes
            return try ValueCodec.decodeRawBody(raw, type: type)
        }
        return try cells[column].materialize(type: type, pager: pager, pageCount: pageCount)
    }

    func sealOverflow(allocator: inout PageAllocator, codec: any PageCodec,
                      entropy: KeyEntropy? = nil) throws -> (record: PackedRecord, pages: [StoredPage]) {
        var cells = cells, pages: [StoredPage] = []
        for column in cells.indices {
            guard let pending = overflows[column] else { continue }
            let chain = try OverflowChain.write(pending.bytes, allocator: &allocator, codec: codec, entropy: entropy)
            let pointer = try pending.pointer(chain.head)
            cells[column] = StoredCell(bytes: pointer[...], width: 0)
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
    /// Decode kinds resolved once per leaf. `other` uses the general codec.
    private enum Kind: UInt8, Sendable {
        case other, i16, i32, i64, timestamp, timestamptz, date, boolean, f32, f64, text, bytea
        init(_ type: SQLType) {
            guard case .scalar(let scalar) = type else { self = .other; return }
            switch scalar {
            case .i16: self = .i16
            case .i32: self = .i32
            case .i64: self = .i64
            case .timestamp: self = .timestamp
            case .timestamptz: self = .timestamptz
            case .date: self = .date
            case .boolean: self = .boolean
            case .f32: self = .f32
            case .f64: self = .f64
            case .text: self = .text
            case .bytea: self = .bytea
            default: self = .other
            }
        }
    }
    /// Plain integers only: reading a region retains nothing. A fixed column
    /// has width > 0 and a bitmap; a variable column's end offsets live in the
    /// shared `offsets` vector starting at `directory`.
    private struct Region: Sendable {
        let kind: Kind
        let width: Int
        let bitmap: Int
        let values: Int
        let directory: Int
    }
    let bytes: [UInt8]
    let types: [TypeDescriptor]
    let count: Int
    let payloadSize: Int
    private let keys: [Range<Int>]
    private let regions: [Region]
    private let offsets: [Int]

    convenience init(bytes: [UInt8], types: [TypeDescriptor], codec: any PageCodec) throws {
        try self.init(bytes: bytes, types: types, capacity: codec.capacity, encrypted: codec.reservedBytes != 0)
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
        var regions: [Region] = [], allOffsets: [Int] = []
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
                regions.append(Region(kind: Kind(types[column].type), width: width, bitmap: bitmap.lowerBound,
                    values: values.lowerBound, directory: 0))
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
                regions.append(Region(kind: Kind(types[column].type), width: 0, bitmap: 0,
                    values: valueStart, directory: allOffsets.count))
                allOffsets.append(contentsOf: offsets)
            }
        }
        self.bytes = bytes; self.types = types; self.count = count
        payloadSize = contentEnd; keys = ranges; self.regions = regions; offsets = allOffsets
    }

    func key(at row: Int) -> ArraySlice<UInt8> { bytes[keys[row]] }

    /// Three-way byte order of a stored key against a probe, without building
    /// a slice: negative when the stored key sorts first.
    func compareKey(at row: Int, with other: [UInt8]) -> Int {
        let range = keys[row], count = range.count, limit = min(count, other.count)
        var index = 0
        while index < limit {
            let a = bytes[range.lowerBound + index], b = other[index]
            if a != b { return a < b ? -1 : 1 }
            index += 1
        }
        return count == other.count ? 0 : (count < other.count ? -1 : 1)
    }

    /// The stored byte span of a cell, or nil for NULL.
    @inline(__always)
    private func span(row: Int, region: Region) -> Range<Int>? {
        if region.width > 0 {
            if bytes[region.bitmap + row / 8] & (0x80 >> (row % 8)) != 0 { return nil }
            let start = region.values + row * region.width
            return start..<(start + region.width)
        }
        let base = region.directory + row
        let start = region.values + (row == 0 ? 0 : offsets[base - 1]), end = region.values + offsets[base]
        return start == end ? nil : start..<end
    }

    func isNull(row: Int, column: Int) -> Bool { span(row: row, region: regions[column]) == nil }

    func cell(row: Int, column: Int) -> StoredCell {
        let range = span(row: row, region: regions[column])
        return StoredCell(bytes: range.map { bytes[$0] }, width: regions[column].width)
    }

    @inline(__always)
    private func be32(_ at: Int) -> UInt32 {
        UInt32(bytes[at]) << 24 | UInt32(bytes[at + 1]) << 16 | UInt32(bytes[at + 2]) << 8 | UInt32(bytes[at + 3])
    }
    @inline(__always)
    private func be64(_ at: Int) -> UInt64 { UInt64(be32(at)) << 32 | UInt64(be32(at + 4)) }

    /// Decode common scalar cells directly from the page. Returns nil when
    /// the general codec must run (other types, compressed or external
    /// values); results and errors match `cell(...).materialize()` exactly.
    func fastValue(row: Int, column: Int) throws -> SQLValue? {
        let region = regions[column]
        if region.kind == .other { return nil }
        guard let range = span(row: row, region: region) else { return .null }
        let at = range.lowerBound
        switch region.kind {
        case .i16: return .integer(Int64(Int16(bitPattern: (UInt16(bytes[at]) << 8 | UInt16(bytes[at + 1])) ^ 0x8000)))
        case .i32: return .integer(Int64(Int32(bitPattern: be32(at) ^ 0x8000_0000)))
        case .i64: return .integer(Int64(bitPattern: be64(at) ^ 0x8000_0000_0000_0000))
        case .timestamp: return .timestamp(Int64(bitPattern: be64(at) ^ 0x8000_0000_0000_0000))
        case .timestamptz: return .timestamptz(Int64(bitPattern: be64(at) ^ 0x8000_0000_0000_0000))
        case .date: return .date(Int32(bitPattern: be32(at) ^ 0x8000_0000))
        case .boolean:
            guard bytes[at] <= 1 else { throw JedError.corruption("invalid boolean body") }
            return .boolean(bytes[at] == 1)
        case .f32: return .f32(be32(at))
        case .f64: return .f64(be64(at))
        case .text, .bytea:
            // Inline tag 0, then a u16 length that must fill the span exactly.
            guard bytes[at] == 0, range.count >= 3 else { return nil }
            let length = Int(bytes[at + 1]) << 8 | Int(bytes[at + 2])
            guard length == range.count - 3 else { return nil }
            let body = Array(bytes[(at + 3)..<range.upperBound])
            if region.kind == .bytea { return .bytea(body) }
            do { return .text(try SQLText(utf8: body)) }
            catch { throw JedError.corruption("invalid UTF-8 in stored text") }
        case .other: return nil
        }
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
