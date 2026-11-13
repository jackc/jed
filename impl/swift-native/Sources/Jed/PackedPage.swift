/// A leaf cell is a borrowed slice of an immutable packed page. NULL is nil;
/// fixed-width bodies have no tag. No SQL value tree is constructed at fault.
internal struct StoredCell: Sendable {
    let bytes: ArraySlice<UInt8>?
    /// The fixed slot width in bytes, or 0 for a variable-width column. A
    /// cell carries no type descriptor: copying one per cell (and per record
    /// copy) cost more than the bytes it described. Decoding takes the
    /// column's descriptor from the owning tree.
    let width: UInt8
    let keyDerived: Bool
    init(bytes: ArraySlice<UInt8>?, width: Int, keyDerived: Bool = false) { self.bytes = bytes; self.width = UInt8(width); self.keyDerived = keyDerived }
    init(bytes: ArraySlice<UInt8>?, type: TypeDescriptor) {
        self.init(bytes: bytes, width: LeafValue.width(type) ?? 0)
    }
    var fixedWidth: Int? { width == 0 ? nil : Int(width) }
    var storedSize: Int { keyDerived ? 0 : width == 0 ? bytes?.count ?? 0 : Int(width) }
    var isNull: Bool { bytes == nil }

    func materialize(type: TypeDescriptor, pager: Pager? = nil, pageCount: UInt32? = nil) throws -> SQLValue {
        guard let bytes else { return .null }
        if fixedWidth != nil { return try LeafValue.decode(bytes, type: type) }
        var reader = ByteReader(bytes)
        let tag = try reader.u8()
        if tag == 5, LeafValue.bare(type.type) { return try ValueCodec.decodeRawBody(Array(bytes.dropFirst()), type: type) }
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
        case 0, 5: return nil
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
    private enum Region: Sendable {
        case key(Int, Int)
        case fixed(bitmap: Int, values: Int, width: Int)
        case spans([Range<Int>])
        case base(bitmap: Int, values: Int, width: Int, base: UInt64, stride: Int)
        case arrays(counts: [Int], offsets: [Int], values: Int, width: Int, base: UInt64, bits: Int)
    }
    let bytes: [UInt8]
    let types: [TypeDescriptor]
    let layout: LeafLayout
    let count: Int
    let payloadSize: Int
    private let prefix: [UInt8]
    private let keys: [Range<Int>]
    private let regions: [Region]

    convenience init(bytes: [UInt8], types: [TypeDescriptor], codec: any PageCodec, layout: LeafLayout = .index) throws {
        try self.init(bytes: bytes, types: types, capacity: codec.capacity, encrypted: codec.reservedBytes != 0, layout: layout)
    }
    convenience init(integrityBytes bytes: [UInt8], types: [TypeDescriptor], codec: any PageCodec, layout: LeafLayout = .index) throws {
        try self.init(bytes: bytes, types: types, capacity: codec.capacity, encrypted: codec.reservedBytes != 0, layout: layout, integrity: true)
    }
    private init(bytes: [UInt8], types: [TypeDescriptor], capacity: Int, encrypted: Bool,
                 layout: LeafLayout, integrity: Bool = false) throws {
        guard bytes.count == capacity + 16, bytes.count >= 16 else { throw JedError.corruption("invalid packed leaf size") }
        try PageEnvelope.validateHeader(bytes, encrypted: encrypted)
        guard bytes[0] == 2, try PageEnvelope.readU32(bytes, at: 12) == PageEnvelope.checksum(bytes, at: 12) else {
            throw JedError.corruption("invalid packed leaf header or checksum")
        }
        let n = Int(try PageEnvelope.readU32(bytes, at: 4))
        guard n > 0, n <= capacity, types.count < capacity / 2 else { throw JedError.corruption("invalid leaf counts") }
        var reader = ByteReader(bytes[16...])
        let flags = try reader.u8()
        guard flags & ~3 == 0 else { throw JedError.corruption("invalid leaf flags") }
        let fixed = flags & 1 != 0 ? Int(try reader.u16()) : nil
        if let fixed, fixed == 0 { throw JedError.corruption("zero key stride") }
        let prefixLength = flags & 2 != 0 ? Int(try reader.u16()) : 0
        guard flags & 2 == 0 || prefixLength > 0,
              fixed.map({ prefixLength < $0 }) ?? true else { throw JedError.corruption("invalid key prefix") }
        let prefix = Array(try reader.take(prefixLength))
        var ends: [Int] = []
        if let fixed { ends = (1...n).map { $0 * (fixed - prefixLength) } }
        else {
            var prior = 0
            for _ in 0..<n {
                let end = Int(try reader.u16())
                guard end >= prior else { throw JedError.corruption("invalid key directory") }
                ends.append(end); prior = end
            }
        }
        let keyStart = 16 + reader.offset
        _ = try reader.take(ends.last!)
        var keys: [Range<Int>] = [], prior = 0
        for end in ends { keys.append((keyStart + prior)..<(keyStart + end)); prior = end }
        for row in 1..<n where !integrity {
            guard bytes[keys[row - 1]].lexicographicallyPrecedes(bytes[keys[row]]) else { throw JedError.corruption("leaf keys are not strictly increasing") }
        }
        var starts: [Int] = []
        for _ in 0...types.count { starts.append(Int(try reader.u16())) }
        guard starts.first == reader.offset, let end = starts.last, end <= capacity,
              zip(starts, starts.dropFirst()).allSatisfy({ $0 <= $1 }),
              bytes[(16 + end)...].allSatisfy({ $0 == 0 }) else { throw JedError.corruption("invalid column directory or leaf padding") }
        var regions: [Region] = []
        func null(_ bitmap: Int, _ row: Int) -> Bool { bytes[bitmap + row / 8] & (0x80 >> (row % 8)) != 0 }
        func bitmap(_ cursor: inout ByteReader, start: Int) throws -> Int {
            let at = start + cursor.offset, length = (n + 7) / 8
            let bits = try cursor.take(length)
            if n % 8 != 0, bits.last! & (UInt8.max >> (n % 8)) != 0 { throw JedError.corruption("invalid null bitmap padding") }
            return at
        }
        func spans(_ cursor: inout ByteReader, count: Int, start: Int, tail: Int = 0) throws -> [Range<Int>] {
            var ends: [Int] = [], previous = 0
            for _ in 0..<count {
                let value = Int(try cursor.u16())
                guard value >= previous else { throw JedError.corruption("invalid value directory") }
                ends.append(value); previous = value
            }
            guard previous == cursor.remaining - tail else { throw JedError.corruption("invalid value region extent") }
            let at = start + cursor.offset
            _ = try cursor.take(previous)
            var result: [Range<Int>] = []; previous = 0
            for end in ends { result.append((at + previous)..<(at + end)); previous = end }
            return result
        }
        func validateSpans(_ ranges: [Range<Int>], type: TypeDescriptor) throws {
            for range in ranges where !range.isEmpty && !integrity {
                let tag = bytes[range.lowerBound]
                guard tag == 2 || tag == 3 || tag == 4 || (LeafValue.bare(type.type) ? tag == 5 : tag == 0) else {
                    throw JedError.corruption("invalid leaf value presence tag")
                }
            }
        }
        for column in types.indices {
            let start = 16 + starts[column], stop = 16 + starts[column + 1], type = types[column]
            if start == stop {
                guard let offset = layout.keyOffsets[column], let width = LeafValue.width(type),
                      keys.allSatisfy({ prefixLength + $0.count >= offset + width }) else { throw JedError.corruption("invalid elided column") }
                regions.append(.key(offset, width)); continue
            }
            if layout.keyOffsets[column] != nil { throw JedError.corruption("stored elided column") }
            var region = ByteReader(bytes[start..<stop])
            switch try region.u8() {
            case 0:
                if let width = LeafValue.width(type) {
                    let bitmap = try bitmap(&region, start: start), values = start + region.offset
                    guard region.remaining == n * width else { throw JedError.corruption("invalid fixed column extent") }
                    for row in 0..<n where null(bitmap, row) {
                        guard bytes[(values + row * width)..<(values + (row + 1) * width)].allSatisfy({ $0 == 0 }) else { throw JedError.corruption("nonzero NULL slot") }
                    }
                    regions.append(.fixed(bitmap: bitmap, values: values, width: width))
                } else {
                    let ranges = try spans(&region, count: n, start: start)
                    try validateSpans(ranges, type: type); regions.append(.spans(ranges))
                }
            case 1:
                guard LeafValue.integer(type), let width = LeafValue.width(type) else { throw JedError.corruption("invalid base-offset column") }
                let bitmap = try bitmap(&region, start: start)
                let base = CompactRegion.unsigned(try region.take(width)), stride = Int(try region.u8()), values = start + region.offset
                guard [0, 1, 2, 4].contains(stride), stride < width, region.remaining == n * stride else { throw JedError.corruption("invalid offset width") }
                let maximum = width == 8 ? UInt64.max : (UInt64(1) << (width * 8)) - 1
                for row in 0..<n {
                    let delta = CompactRegion.unsigned(try region.take(stride))
                    guard delta <= maximum - base, !null(bitmap, row) || delta == 0 else { throw JedError.corruption("invalid column offset") }
                }
                regions.append(.base(bitmap: bitmap, values: values, width: width, base: base, stride: stride))
            case 2:
                guard LeafValue.bare(type.type) else { throw JedError.corruption("invalid dictionary column") }
                let count = Int(try region.u16())
                guard count > 0, count <= 256 else { throw JedError.corruption("invalid dictionary count") }
                let entries = try spans(&region, count: count, start: start, tail: n)
                try validateSpans(entries, type: type)
                var ranges: [Range<Int>] = []
                for _ in 0..<n {
                    let code = Int(try region.u8()); guard code < count else { throw JedError.corruption("invalid dictionary code") }
                    ranges.append(entries[code])
                }
                regions.append(.spans(ranges))
            case 4:
                guard let width = CompactRegion.arrayWidth(type.type) else { throw JedError.corruption("invalid packed array column") }
                let counts = try region.take(n).map { $0 == 255 ? -1 : Int($0) }
                var offsets: [Int] = [], total = 0
                for count in counts { offsets.append(total); total += max(0, count) }
                let base = CompactRegion.unsigned(try region.take(width)), bits = Int(try region.u8()), values = start + region.offset
                guard bits <= width * 8, region.remaining == (total * bits + 7) / 8 else { throw JedError.corruption("invalid packed array extent") }
                let data = try region.take(region.remaining)
                if total * bits % 8 != 0, data.last! & (UInt8.max >> (total * bits % 8)) != 0 { throw JedError.corruption("invalid packed array padding") }
                let maximum = width == 8 ? UInt64.max : (UInt64(1) << (width * 8)) - 1
                for index in 0..<total {
                    let delta = Self.bits(bytes, at: values * 8 + index * bits, count: bits)
                    guard delta <= maximum - base else { throw JedError.corruption("packed array element overflows") }
                }
                regions.append(.arrays(counts: counts, offsets: offsets, values: values, width: width, base: base, bits: bits))
            default: throw JedError.corruption("invalid column region kind")
            }
        }
        self.bytes = bytes; self.types = types; self.layout = layout; count = n; payloadSize = end
        self.keys = keys; self.prefix = prefix; self.regions = regions
    }
    private static func bits(_ bytes: [UInt8], at: Int, count: Int) -> UInt64 {
        var value: UInt64 = 0
        for bit in at..<(at + count) { value = (value << 1) | UInt64((bytes[bit / 8] >> (7 - bit % 8)) & 1) }
        return value
    }
    func key(at row: Int) -> ArraySlice<UInt8> {
        prefix.isEmpty ? bytes[keys[row]] : (prefix + bytes[keys[row]])[...]
    }
    func compareKey(at row: Int, with other: [UInt8]) -> Int {
        let suffix = keys[row], length = prefix.count + suffix.count
        for index in 0..<min(length, other.count) {
            let value = index < prefix.count ? prefix[index] : bytes[suffix.lowerBound + index - prefix.count]
            if value != other[index] { return value < other[index] ? -1 : 1 }
        }
        return length == other.count ? 0 : length < other.count ? -1 : 1
    }
    private func null(_ bitmap: Int, _ row: Int) -> Bool { bytes[bitmap + row / 8] & (0x80 >> (row % 8)) != 0 }
    func isNull(row: Int, column: Int) -> Bool {
        switch regions[column] {
        case .key: false
        case .fixed(let bitmap, _, _), .base(let bitmap, _, _, _, _): null(bitmap, row)
        case .spans(let spans): spans[row].isEmpty
        case .arrays(let counts, _, _, _, _, _): counts[row] < 0
        }
    }
    func cell(row: Int, column: Int) -> StoredCell {
        let width = LeafValue.width(types[column]) ?? 0
        if isNull(row: row, column: column) { return StoredCell(bytes: nil, width: width) }
        switch regions[column] {
        case .key(let offset, let width):
            let key = key(at: row), start = key.startIndex + offset
            return StoredCell(bytes: key[start..<(start + width)], width: width, keyDerived: true)
        case .fixed(_, let values, let width):
            return StoredCell(bytes: bytes[(values + row * width)..<(values + (row + 1) * width)], width: width)
        case .spans(let spans): return StoredCell(bytes: bytes[spans[row]], width: 0)
        case .base(_, let values, let width, let base, let stride):
            let value = base + CompactRegion.unsigned(bytes[(values + row * stride)..<(values + (row + 1) * stride)])
            return StoredCell(bytes: CompactRegion.bytes(value, width: width)[...], width: width)
        case .arrays(let counts, let offsets, let values, let width, let base, let bits):
            var writer = ByteWriter(); writer.u8(0); writer.u8(counts[row] == 0 ? 0 : 1); writer.u8(0)
            if counts[row] > 0 {
                writer.u32(UInt32(counts[row])); writer.u32(1)
                for index in offsets[row]..<(offsets[row] + counts[row]) {
                    let value = base + Self.bits(bytes, at: values * 8 + index * bits, count: bits)
                    writer.append(CompactRegion.bytes(value, width: width))
                }
            }
            return StoredCell(bytes: writer.bytes[...], width: 0)
        }
    }
    /// Read fixed scalar bodies without allocating a reconstructed key or
    /// offset cell. Region bounds and widths were checked at page admission.
    private func word(row: Int, column: Int) -> UInt64? {
        guard !isNull(row: row, column: column) else { return nil }
        switch regions[column] {
        case .fixed(_, let start, let width) where width <= 8:
            return CompactRegion.unsigned(bytes[(start + row * width)..<(start + (row + 1) * width)])
        case .base(_, let start, _, let base, let stride):
            return base + CompactRegion.unsigned(bytes[(start + row * stride)..<(start + (row + 1) * stride)])
        case .key(let offset, let width) where width <= 8:
            var value: UInt64 = 0
            for index in offset..<(offset + width) {
                let byte = index < prefix.count ? prefix[index] : bytes[keys[row].lowerBound + index - prefix.count]
                value = value << 8 | UInt64(byte)
            }
            return value
        default: return nil
        }
    }
    func fastValue(row: Int, column: Int) throws -> SQLValue? {
        if isNull(row: row, column: column) { return .null }
        if case .scalar(let scalar) = types[column].type, let value = word(row: row, column: column) {
            switch scalar {
            case .i16: return .integer(Int64(Int16(bitPattern: UInt16(value) ^ 0x8000)))
            case .i32: return .integer(Int64(Int32(bitPattern: UInt32(value) ^ 0x8000_0000)))
            case .i64: return .integer(Int64(bitPattern: value ^ 0x8000_0000_0000_0000))
            case .date: return .date(Int32(bitPattern: UInt32(value) ^ 0x8000_0000))
            case .timestamp: return .timestamp(Int64(bitPattern: value ^ 0x8000_0000_0000_0000))
            case .timestamptz: return .timestamptz(Int64(bitPattern: value ^ 0x8000_0000_0000_0000))
            case .boolean:
                guard value <= 1 else { throw JedError.corruption("invalid boolean body") }
                return .boolean(value == 1)
            case .f32: return .f32(UInt32(value))
            case .f64: return .f64(value)
            default: break
            }
        }
        let cell = cell(row: row, column: column)
        if cell.fixedWidth != nil || cell.bytes?.first == 0 || cell.bytes?.first == 5 { return try cell.materialize(type: types[column]) }
        return nil
    }
    func fastInteger(row: Int, column: Int) -> Int64? {
        guard case .scalar(let scalar) = types[column].type, scalar.integerWidth != nil,
              let value = word(row: row, column: column) else { return nil }
        switch scalar {
        case .i16: return Int64(Int16(bitPattern: UInt16(value) ^ 0x8000))
        case .i32: return Int64(Int32(bitPattern: UInt32(value) ^ 0x8000_0000))
        default: return Int64(bitPattern: value ^ 0x8000_0000_0000_0000)
        }
    }
    enum ShortBody { case null, inline(Range<Int>), other }
    func shortBody(row: Int, column: Int) -> ShortBody {
        if isNull(row: row, column: column) { return .null }
        guard case .spans(let spans) = regions[column] else { return .other }
        let span = spans[row]
        if bytes[span.lowerBound] == 5 { return .inline((span.lowerBound + 1)..<span.upperBound) }
        return .other
    }
    func taggedBody(row: Int, column: Int) -> ShortBody {
        if isNull(row: row, column: column) { return .null }
        guard case .spans(let spans) = regions[column] else { return .other }
        let span = spans[row]
        guard bytes[span.lowerBound] == 0 else { return .other }
        return .inline((span.lowerBound + 1)..<span.upperBound)
    }
    func hasNoLargeFacts(row: Int, column: Int) -> Bool {
        let cell = cell(row: row, column: column)
        return cell.isNull || cell.fixedWidth != nil || cell.bytes?.first == 0 || cell.bytes?.first == 5
    }
    func record(at row: Int) -> PackedRecord {
        PackedRecord(key: Array(key(at: row)), cells: types.indices.map { cell(row: row, column: $0) })
    }
    static func overhead(records: Int, types: [TypeDescriptor], layout: LeafLayout = .index) -> Int {
        var size = 1 + (layout.fixedKeyLength == nil ? 2 * records : 2) + 2 * (types.count + 1)
        for column in types.indices where layout.keyOffsets[column] == nil {
            size += LeafValue.width(types[column]) == nil ? 1 + 2 * records : 1 + (records + 7) / 8
        }
        return size
    }
    static func measure(_ records: [PackedRecord], types: [TypeDescriptor], layout: LeafLayout = .index) -> Int {
        payload(records, types: types, layout: layout).count
    }
    private static func payload(_ records: [PackedRecord], types: [TypeDescriptor], layout: LeafLayout) -> [UInt8] {
        var prefix = 0
        if let first = records.first?.key, let last = records.last?.key, records.count >= 2 {
            while prefix < min(first.count, last.count, 65535), first[prefix] == last[prefix] { prefix += 1 }
            if records.count * prefix <= prefix + 2 { prefix = 0 }
        }
        var writer = ByteWriter(); writer.u8((layout.fixedKeyLength == nil ? 0 : 1) | (prefix > 0 ? 2 : 0))
        if let width = layout.fixedKeyLength { writer.u16(UInt16(width)) }
        if prefix > 0 { writer.u16(UInt16(prefix)); writer.append(records[0].key.prefix(prefix)) }
        if layout.fixedKeyLength == nil {
            var end = 0
            for record in records { end += record.key.count - prefix; writer.u16(UInt16(truncatingIfNeeded: end)) }
        }
        for record in records { writer.append(record.key.dropFirst(prefix)) }
        let regions = types.indices.map { column -> [UInt8] in
            if layout.keyOffsets[column] != nil { return [] }
            return CompactRegion.encode(records.map { $0.cells[column] }, type: types[column])
        }
        var offset = writer.bytes.count + 2 * (types.count + 1)
        writer.u16(UInt16(truncatingIfNeeded: offset))
        for region in regions { offset += region.count; writer.u16(UInt16(truncatingIfNeeded: offset)) }
        for region in regions { writer.append(region) }
        return writer.bytes
    }
    static func encodePayload(_ records: [PackedRecord], types: [TypeDescriptor], capacity: Int, layout: LeafLayout = .index) throws -> [UInt8] {
        guard !records.isEmpty, records.allSatisfy({ $0.cells.count == types.count }),
              zip(records, records.dropFirst()).allSatisfy({ $0.key.lexicographicallyPrecedes($1.key) }),
              layout.fixedKeyLength.map({ width in records.allSatisfy { $0.key.count == width } }) ?? true else { throw JedError.corruption("invalid packed leaf records") }
        let bytes = payload(records, types: types, layout: layout)
        guard bytes.count <= capacity else { throw JedError("0A000", "leaf payload exceeds page capacity") }
        return bytes
    }
}
