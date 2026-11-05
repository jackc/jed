/// Bounds-checked big-endian wire/format reader. The byte device and page codec
/// validate outer envelopes; every inner record still validates its own bounds.
internal struct ByteReader {
    let bytes: ArraySlice<UInt8>
    private(set) var offset = 0
    var remaining: Int { bytes.count - offset }

    init(_ bytes: [UInt8]) { self.bytes = bytes[...] }
    init(_ bytes: ArraySlice<UInt8>) { self.bytes = bytes }

    mutating func take(_ count: Int) throws -> ArraySlice<UInt8> {
        guard count >= 0 && count <= remaining else {
            throw JedError.corruption("truncated byte field")
        }
        let start = bytes.startIndex + offset
        offset += count
        return bytes[start..<(start + count)]
    }

    mutating func u8() throws -> UInt8 { try take(1).first! }
    mutating func u16() throws -> UInt16 {
        try take(2).reduce(UInt16(0)) { ($0 << 8) | UInt16($1) }
    }
    mutating func u32() throws -> UInt32 {
        try take(4).reduce(UInt32(0)) { ($0 << 8) | UInt32($1) }
    }
    mutating func u64() throws -> UInt64 {
        try take(8).reduce(UInt64(0)) { ($0 << 8) | UInt64($1) }
    }
    mutating func i16() throws -> Int16 { Int16(bitPattern: try u16()) }
    mutating func i32() throws -> Int32 { Int32(bitPattern: try u32()) }
    mutating func i64() throws -> Int64 { Int64(bitPattern: try u64()) }
    mutating func text(_ count: Int) throws -> SQLText {
        let raw = Array(try take(count))
        do { return try SQLText(utf8: raw) }
        catch { throw JedError.corruption("invalid UTF-8 in stored text") }
    }
    func requireEnd() throws {
        guard remaining == 0 else { throw JedError.corruption("trailing bytes") }
    }
}

internal struct ByteWriter {
    private(set) var bytes: [UInt8] = []
    mutating func append(_ bytes: some Sequence<UInt8>) { self.bytes.append(contentsOf: bytes) }
    mutating func u8(_ value: UInt8) { bytes.append(value) }
    mutating func u16(_ value: UInt16) {
        bytes.append(UInt8(truncatingIfNeeded: value >> 8))
        bytes.append(UInt8(truncatingIfNeeded: value))
    }
    mutating func u32(_ value: UInt32) {
        for shift in stride(from: 24, through: 0, by: -8) {
            bytes.append(UInt8(truncatingIfNeeded: value >> shift))
        }
    }
    mutating func u64(_ value: UInt64) {
        for shift in stride(from: 56, through: 0, by: -8) {
            bytes.append(UInt8(truncatingIfNeeded: value >> shift))
        }
    }
    mutating func i16(_ value: Int16) { u16(UInt16(bitPattern: value)) }
    mutating func i32(_ value: Int32) { u32(UInt32(bitPattern: value)) }
    mutating func i64(_ value: Int64) { u64(UInt64(bitPattern: value)) }
}

/// CRC-32/IEEE and CRC-64/ECMA-182 are format checksums, not cryptographic
/// primitives. Tables are computed from the pinned polynomials, not host APIs.
internal enum Checksums {
    private static let crc32Table: [UInt32] = (0..<256).map { index in
        var value = UInt32(index)
        for _ in 0..<8 { value = (value >> 1) ^ (value & 1 == 0 ? 0 : 0xedb88320) }
        return value
    }
    private static let crc64Table: [UInt64] = (0..<256).map { index in
        var value = UInt64(index) << 56
        for _ in 0..<8 {
            value = (value << 1) ^ (value & 0x8000000000000000 == 0 ? 0 : 0x42f0e1eba9ea3693)
        }
        return value
    }
    static func crc32(_ bytes: some Sequence<UInt8>) -> UInt32 {
        var crc: UInt32 = .max
        for byte in bytes { crc = (crc >> 8) ^ crc32Table[Int((crc ^ UInt32(byte)) & 255)] }
        return ~crc
    }
    static func crc64(_ bytes: some Sequence<UInt8>) -> UInt64 {
        crc64Update(0, bytes)
    }
    static func crc64Update(_ initial: UInt64, _ bytes: some Sequence<UInt8>) -> UInt64 {
        var crc = initial
        for byte in bytes { crc = (crc << 8) ^ crc64Table[Int((crc >> 56) ^ UInt64(byte))] }
        return crc
    }
}
