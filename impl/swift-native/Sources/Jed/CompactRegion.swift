/// Byte-only region selection. The same calculation measures writer leaves
/// and emits them, so split decisions cannot drift from the file bytes.
internal enum CompactRegion {
    static func unsigned(_ bytes: ArraySlice<UInt8>) -> UInt64 {
        bytes.reduce(0) { ($0 << 8) | UInt64($1) }
    }
    static func bytes(_ value: UInt64, width: Int) -> [UInt8] {
        (0..<width).map { UInt8(truncatingIfNeeded: value >> ((width - $0 - 1) * 8)) }
    }
    static func arrayWidth(_ type: SQLType) -> Int? {
        guard case .array(.scalar(let element)) = type else { return nil }
        return element.integerWidth
    }
    static func arrayBodies(_ cell: StoredCell, width: Int) -> [UInt64]? {
        guard let bytes = cell.bytes else { return [] }
        var reader = ByteReader(bytes)
        guard (try? reader.u8()) == 0, let dimensions = try? reader.u8(), (try? reader.u8()) == 0 else { return nil }
        if dimensions == 0 { return reader.remaining == 0 ? [] : nil }
        guard dimensions == 1, let count = try? reader.u32(), count > 0, count < 255,
              (try? reader.u32()) == 1, reader.remaining == Int(count) * width else { return nil }
        return (0..<Int(count)).map { _ in unsigned(try! reader.take(width)) }
    }
    static func encode(_ cells: [StoredCell], type: TypeDescriptor) -> [UInt8] {
        let n = cells.count
        var plain = ByteWriter(); plain.u8(0)
        if let width = LeafValue.width(type) {
            var bitmap = [UInt8](repeating: 0, count: (n + 7) / 8)
            var values: [UInt8] = []
            for (row, cell) in cells.enumerated() {
                if let bytes = cell.bytes { values.append(contentsOf: bytes) }
                else { bitmap[row / 8] |= 0x80 >> (row % 8); values += repeatElement(UInt8(0), count: width) }
            }
            plain.append(bitmap); plain.append(values)
            if LeafValue.integer(type) {
                let present = cells.compactMap { $0.bytes.map(unsigned) }
                let base = present.min() ?? 0, delta = (present.max() ?? 0) - base
                let bw = delta == 0 ? 0 : delta <= 255 ? 1 : delta <= 65535 ? 2 : delta <= UInt32.max ? 4 : 8
                if bw < width, n * (width - bw) > width + 1 {
                    var packed = ByteWriter(); packed.u8(1); packed.append(bitmap)
                    packed.append(bytes(base, width: width)); packed.u8(UInt8(bw))
                    for cell in cells { packed.append(bytes(cell.bytes.map { unsigned($0) - base } ?? 0, width: bw)) }
                    return packed.bytes
                }
            }
        } else {
            var values: [UInt8] = []
            for cell in cells {
                if let bytes = cell.bytes { values.append(contentsOf: bytes) }
                plain.u16(UInt16(truncatingIfNeeded: values.count))
            }
            plain.append(values)
            if LeafValue.bare(type.type) {
                var entries: [[UInt8]] = [], shared: [[UInt8]: Int] = [:], codes: [UInt8] = []
                for cell in cells {
                    let value = cell.bytes.map(Array.init) ?? []
                    let share = value.isEmpty || value.first == 5
                    if share, let code = shared[value] { codes.append(UInt8(code)); continue }
                    if entries.count == 256 { return plain.bytes }
                    let code = entries.count; entries.append(value); codes.append(UInt8(code))
                    if share { shared[value] = code }
                }
                var packed = ByteWriter(); packed.u8(2); packed.u16(UInt16(entries.count))
                var end = 0
                for entry in entries { end += entry.count; packed.u16(UInt16(truncatingIfNeeded: end)) }
                for entry in entries { packed.append(entry) }; packed.append(codes)
                if packed.bytes.count < plain.bytes.count { return packed.bytes }
            } else if let width = arrayWidth(type.type) {
                var counts: [UInt8] = [], bodies: [UInt64] = []
                for cell in cells {
                    guard let elements = arrayBodies(cell, width: width) else { return plain.bytes }
                    counts.append(cell.isNull ? 255 : UInt8(elements.count)); bodies += elements
                }
                let base = bodies.min() ?? 0, delta = (bodies.max() ?? 0) - base
                let bits = delta == 0 ? 0 : 64 - delta.leadingZeroBitCount
                var packed = ByteWriter(); packed.u8(4); packed.append(counts)
                packed.append(bytes(base, width: width)); packed.u8(UInt8(bits))
                var payload = [UInt8](repeating: 0, count: (bodies.count * bits + 7) / 8), at = 0
                for body in bodies {
                    let offset = body - base
                    for bit in (0..<bits).reversed() {
                        if (offset >> bit) & 1 != 0 { payload[at / 8] |= 0x80 >> (at % 8) }
                        at += 1
                    }
                }
                packed.append(payload)
                if packed.bytes.count < plain.bytes.count { return packed.bytes }
            }
        }
        return plain.bytes
    }
}
