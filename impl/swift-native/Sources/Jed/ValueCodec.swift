/// The inline single-value format seam. Overflow and compression are handled
/// by the page layer; recursive values contain bare bodies, never pointers.
internal enum ValueCodec {
    static func encode(_ value: SQLValue, type: TypeDescriptor) throws -> [UInt8] {
        if value.isNull { return [1] }
        return [0] + (try encodeBody(value, type: type))
    }

    static func decode(_ bytes: [UInt8], type: TypeDescriptor) throws -> SQLValue {
        try decode(bytes[...], type: type)
    }

    static func decode(_ bytes: ArraySlice<UInt8>, type: TypeDescriptor) throws -> SQLValue {
        var reader = ByteReader(bytes)
        let tag = try reader.u8()
        let value: SQLValue
        switch tag {
        case 0: value = try readBody(&reader, type: type, depth: 0)
        case 1: value = .null
        default: throw JedError.corruption("invalid inline value presence tag")
        }
        try reader.requireEnd()
        return value
    }

    static func encodeBody(_ value: SQLValue, type: TypeDescriptor) throws -> [UInt8] {
        var writer = ByteWriter()
        try writeBody(value, type: type, depth: 0, into: &writer)
        return writer.bytes
    }

    static func decodeBody(_ bytes: [UInt8], type: TypeDescriptor) throws -> SQLValue {
        try decodeBody(bytes[...], type: type)
    }

    static func decodeBody(_ bytes: ArraySlice<UInt8>, type: TypeDescriptor) throws -> SQLValue {
        var reader = ByteReader(bytes)
        let value = try readBody(&reader, type: type, depth: 0)
        try reader.requireEnd()
        return value
    }

    /// Content payload for compression/overflow: top-level string/bytea values
    /// omit their inline u16 length. All other variable bodies are already
    /// self-describing and retain their ordinary body bytes.
    static func encodeRawBody(_ value: SQLValue, type: TypeDescriptor) throws -> [UInt8] {
        switch (type.type, value) {
        case (.scalar(.text), .text(let text)), (.scalar(.json), .json(let text)), (.scalar(.jsonpath), .jsonpath(let text)):
            return text.utf8
        case (.scalar(.bytea), .bytea(let bytes)): return bytes
        default: return try encodeBody(value, type: type)
        }
    }

    static func decodeRawBody(_ bytes: [UInt8], type: TypeDescriptor) throws -> SQLValue {
        switch type.type {
        case .scalar(.text), .scalar(.json), .scalar(.jsonpath):
            let text: SQLText
            do { text = try SQLText(utf8: bytes) } catch { throw JedError.corruption("invalid UTF-8 in stored text") }
            if type.type == .scalar(.text) { return .text(text) }
            return type.type == .scalar(.json) ? .json(text) : .jsonpath(text)
        case .scalar(.bytea): return .bytea(bytes)
        default: return try decodeBody(bytes, type: type)
        }
    }

    static func fixedWidth(_ type: SQLType) -> Int? {
        switch type {
        case .scalar(.boolean): 1
        case .scalar(.i16): 2
        case .scalar(.i32), .scalar(.date), .scalar(.f32): 4
        case .scalar(.i64), .scalar(.timestamp), .scalar(.timestamptz), .scalar(.f64): 8
        case .scalar(.uuid), .scalar(.interval): 16
        default: nil
        }
    }

    private static func mismatch() -> JedError { JedError("42804", "value does not match its static type") }

    static func arrayElement(_ type: SQLType) throws -> TypeDescriptor {
        switch type {
        case .scalar, .composite: return TypeDescriptor(type)
        default: throw JedError("0A000", "unsupported array element type")
        }
    }

    static func rangeElement(_ type: SQLType) throws -> TypeDescriptor {
        switch type {
        case .scalar(.i32), .scalar(.i64), .scalar(.decimal), .scalar(.date),
             .scalar(.timestamp), .scalar(.timestamptz): return TypeDescriptor(type)
        default: throw JedError("0A000", "unsupported range element type")
        }
    }

    private static func writeShort(_ bytes: [UInt8], into writer: inout ByteWriter) throws {
        guard bytes.count <= Int(UInt16.max) else { throw JedError("0A000", "value byte length exceeds the inline format limit") }
        writer.u16(UInt16(bytes.count)); writer.append(bytes)
    }

    private static func bitmap(_ values: [SQLValue]) -> [UInt8] {
        var bytes = [UInt8](repeating: 0, count: (values.count + 7) / 8)
        for (index, value) in values.enumerated() where value.isNull { bytes[index / 8] |= 0x80 >> (index % 8) }
        return bytes
    }

    private static func validateBitmap(_ bytes: [UInt8], count: Int) throws {
        if count % 8 != 0, let last = bytes.last {
            guard last & (UInt8.max >> (count % 8)) == 0 else { throw JedError.corruption("nonzero unused null-bitmap bits") }
        }
    }

    private static func writeBody(_ value: SQLValue, type: TypeDescriptor, depth: Int, into writer: inout ByteWriter) throws {
        switch (type.type, value) {
        case (.scalar(.i16), .integer(let number)):
            guard let number = Int16(exactly: number) else { throw JedError("22003", "integer out of range") }
            writer.u16(UInt16(bitPattern: number) ^ 0x8000)
        case (.scalar(.i32), .integer(let number)):
            guard let number = Int32(exactly: number) else { throw JedError("22003", "integer out of range") }
            writer.u32(UInt32(bitPattern: number) ^ 0x8000_0000)
        case (.scalar(.i64), .integer(let number)), (.scalar(.timestamp), .timestamp(let number)),
             (.scalar(.timestamptz), .timestamptz(let number)):
            writer.u64(UInt64(bitPattern: number) ^ 0x8000_0000_0000_0000)
        case (.scalar(.date), .date(let day)): writer.u32(UInt32(bitPattern: day) ^ 0x8000_0000)
        case (.scalar(.boolean), .boolean(let value)): writer.u8(value ? 1 : 0)
        case (.scalar(.text), .text(let text)), (.scalar(.json), .json(let text)), (.scalar(.jsonpath), .jsonpath(let text)):
            // SQL input validation/typmod coercion precedes storage. In
            // particular, json is stored verbatim and is reparsed at structural use.
            try writeShort(text.utf8, into: &writer)
        case (.scalar(.bytea), .bytea(let bytes)): try writeShort(bytes, into: &writer)
        case (.scalar(.uuid), .uuid(let uuid)): writer.append(uuid.bytes)
        case (.scalar(.decimal), .decimal(let decimal)): writer.append(try decimal.encodedBody())
        case (.scalar(.interval), .interval(let interval)):
            writer.i32(interval.months); writer.i32(interval.days); writer.i64(interval.microseconds)
        case (.scalar(.f32), .f32(let bits)): writer.u32(canonicalFloat32(bits))
        case (.scalar(.f64), .f64(let bits)): writer.u64(canonicalFloat64(bits))
        case (.scalar(.jsonb), .jsonb(let node)): try writeJSON(node, depth: 0, into: &writer)
        case (.composite(let composite), .composite(let fields)):
            guard depth < 32 else { throw JedError("54001", "composite nesting depth exceeds the maximum of 32") }
            guard !composite.fields.isEmpty, composite.fields.count <= Int(UInt16.max), fields.count == composite.fields.count else { throw mismatch() }
            writer.append(bitmap(fields))
            for (field, value) in zip(composite.fields, fields) where !value.isNull {
                try writeBody(value, type: field.descriptor, depth: depth + 1, into: &writer)
            }
        case (.array(let elementType), .array(let array)):
            let element = try arrayElement(elementType)
            writer.u8(UInt8(array.dimensions.count))
            let hasNulls = array.elements.contains(where: \.isNull)
            writer.u8(hasNulls ? 1 : 0)
            for (dimension, lower) in zip(array.dimensions, array.lowerBounds) { writer.u32(UInt32(dimension)); writer.i32(lower) }
            if hasNulls { writer.append(bitmap(array.elements)) }
            for value in array.elements where !value.isNull { try writeBody(value, type: element, depth: depth, into: &writer) }
        case (.range(let elementType), .range(let range)):
            let element = try rangeElement(elementType)
            try validateRange(range, element: element, stored: false)
            if range.isEmpty { writer.u8(1); return }
            var flags: UInt8 = 0
            if range.lower == nil { flags |= 2 }; if range.upper == nil { flags |= 4 }
            if range.lowerInclusive { flags |= 8 }; if range.upperInclusive { flags |= 16 }
            writer.u8(flags)
            if let lower = range.lower { try writeBody(lower, type: element, depth: depth, into: &writer) }
            if let upper = range.upper { try writeBody(upper, type: element, depth: depth, into: &writer) }
        case (.record, _): throw JedError("0A000", "anonymous records cannot be stored")
        default: throw mismatch()
        }
    }

    /// NaN payloads canonicalize at storage; the sign of zero stays intact.
    static func canonicalFloat32(_ bits: UInt32) -> UInt32 {
        bits & 0x7fff_ffff > 0x7f80_0000 ? 0x7fc0_0000 : bits
    }
    static func canonicalFloat64(_ bits: UInt64) -> UInt64 {
        bits & 0x7fff_ffff_ffff_ffff > 0x7ff0_0000_0000_0000 ? 0x7ff8_0000_0000_0000 : bits
    }

    private static func readDecimal(_ reader: inout ByteReader) throws -> SQLDecimal {
        var writer = ByteWriter()
        writer.u8(try reader.u8()); writer.u16(try reader.u16())
        let count = try reader.u16(); writer.u16(count)
        writer.append(try reader.take(Int(count) * 2))
        return try SQLDecimal.decodeBody(writer.bytes)
    }

    internal static func readBody(_ reader: inout ByteReader, type: TypeDescriptor, depth: Int = 0) throws -> SQLValue {
        switch type.type {
        case .scalar(let scalar):
            switch scalar {
            case .i16: return .integer(Int64(Int16(bitPattern: try reader.u16() ^ 0x8000)))
            case .i32: return .integer(Int64(Int32(bitPattern: try reader.u32() ^ 0x8000_0000)))
            case .i64: return .integer(Int64(bitPattern: try reader.u64() ^ 0x8000_0000_0000_0000))
            case .timestamp: return .timestamp(Int64(bitPattern: try reader.u64() ^ 0x8000_0000_0000_0000))
            case .timestamptz: return .timestamptz(Int64(bitPattern: try reader.u64() ^ 0x8000_0000_0000_0000))
            case .date: return .date(Int32(bitPattern: try reader.u32() ^ 0x8000_0000))
            case .boolean:
                let byte = try reader.u8()
                guard byte <= 1 else { throw JedError.corruption("invalid boolean body") }
                return .boolean(byte == 1)
            case .text, .json, .jsonpath:
                let count = Int(try reader.u16()); let text = try reader.text(count)
                return scalar == .text ? .text(text) : scalar == .json ? .json(text) : .jsonpath(text)
            case .bytea: let count = Int(try reader.u16()); return .bytea(Array(try reader.take(count)))
            case .uuid: return .uuid(try SQLUUID(bytes: Array(reader.take(16))))
            case .decimal: return .decimal(try readDecimal(&reader))
            case .interval: return .interval(SQLInterval(months: try reader.i32(), days: try reader.i32(), microseconds: try reader.i64()))
            case .f32: return .f32(try reader.u32())
            case .f64: return .f64(try reader.u64())
            case .jsonb: return .jsonb(try readJSON(&reader, depth: 0))
            }
        case .composite(let composite):
            guard depth < 32, !composite.fields.isEmpty, composite.fields.count <= Int(UInt16.max) else {
                throw JedError.corruption("invalid stored composite descriptor")
            }
            let bits = Array(try reader.take((composite.fields.count + 7) / 8))
            try validateBitmap(bits, count: composite.fields.count)
            var values: [SQLValue] = []
            for (index, field) in composite.fields.enumerated() {
                values.append(bits[index / 8] & (0x80 >> (index % 8)) != 0
                    ? .null : try readBody(&reader, type: field.descriptor, depth: depth + 1))
            }
            return .composite(values)
        case .array(let elementType):
            let element: TypeDescriptor
            do { element = try arrayElement(elementType) } catch { throw JedError.corruption("invalid array element descriptor") }
            let dimensions = Int(try reader.u8()), flags = try reader.u8()
            guard flags <= 1, dimensions != 0 || flags == 0 else { throw JedError.corruption("invalid array header") }
            var lengths: [Int] = [], lowerBounds: [Int32] = []
            var count = dimensions == 0 ? 0 : 1
            for _ in 0..<dimensions {
                let length = Int(try reader.u32()), lower = try reader.i32()
                guard length > 0, length <= 134_217_727, count <= 134_217_727 / length,
                      Int64(lower) + Int64(length) - 1 <= Int32.max else { throw JedError.corruption("invalid array shape") }
                count *= length; lengths.append(length); lowerBounds.append(lower)
            }
            let bits = flags == 1 ? Array(try reader.take((count + 7) / 8)) : []
            try validateBitmap(bits, count: count)
            var nullCount = 0
            for byte in bits { nullCount += byte.nonzeroBitCount }
            // Every admitted element body occupies at least one byte. Check the
            // input before allocating a result array from an untrusted count.
            guard count - nullCount <= reader.remaining else { throw JedError.corruption("truncated array elements") }
            var values: [SQLValue] = []
            for index in 0..<count {
                let isNull = !bits.isEmpty && bits[index / 8] & (0x80 >> (index % 8)) != 0
                values.append(isNull ? .null : try readBody(&reader, type: element, depth: depth))
            }
            return .array(try SQLArray(dimensions: lengths, lowerBounds: lowerBounds, elements: values))
        case .range(let elementType):
            let element: TypeDescriptor
            do { element = try rangeElement(elementType) } catch { throw JedError.corruption("invalid range element descriptor") }
            let flags = try reader.u8()
            guard flags & 0xe0 == 0, flags & 1 == 0 || flags == 1,
                  flags & 2 == 0 || flags & 8 == 0, flags & 4 == 0 || flags & 16 == 0 else { throw JedError.corruption("invalid range flags") }
            if flags == 1 { return .range(SQLRange(empty: true)) }
            let lower = flags & 2 == 0 ? try readBody(&reader, type: element, depth: depth) : nil
            let upper = flags & 4 == 0 ? try readBody(&reader, type: element, depth: depth) : nil
            let range = SQLRange(lower: lower, upper: upper, lowerInclusive: flags & 8 != 0, upperInclusive: flags & 16 != 0)
            try validateRange(range, element: element, stored: true)
            return .range(range)
        case .record: throw JedError.corruption("anonymous record in stored type")
        }
    }

    /// The evaluator normalizes ranges before this boundary. Rejecting an
    /// unnormalized host value keeps every persisted body and key canonical.
    static func validateRange(_ range: SQLRange, element: TypeDescriptor, stored: Bool) throws {
        let invalid = JedError(stored ? "XX001" : "22000", "range is not in canonical form")
        if range.isEmpty { return }
        if range.lower?.isNull == true || range.upper?.isNull == true { throw invalid }
        if let lower = range.lower, let upper = range.upper {
            let a = try KeyEncoding.encode(lower, type: element), b = try KeyEncoding.encode(upper, type: element)
            if b.lexicographicallyPrecedes(a) || (a == b && !(range.lowerInclusive && range.upperInclusive)) { throw invalid }
        }
        switch element.type {
        case .scalar(.i32), .scalar(.i64):
            if range.lower != nil && !range.lowerInclusive || range.upper != nil && range.upperInclusive { throw invalid }
        case .scalar(.date):
            func finite(_ bound: SQLValue?) -> Bool {
                if let bound, case .date(let day) = bound { return day != Int32.min && day != Int32.max }
                return false
            }
            if finite(range.lower) && !range.lowerInclusive || finite(range.upper) && range.upperInclusive { throw invalid }
        default: break
        }
    }

    private static func writeVarint(_ count: Int, into writer: inout ByteWriter) {
        var value = UInt64(count)
        repeat {
            let byte = UInt8(value & 127); value >>= 7
            writer.u8(byte | (value == 0 ? 0 : 128))
        } while value != 0
    }

    private static func readVarint(_ reader: inout ByteReader) throws -> Int {
        var value: UInt64 = 0
        for shift in stride(from: 0, through: 63, by: 7) {
            let byte = try reader.u8()
            guard shift < 63 || byte <= 1 else { throw JedError.corruption("jsonb varint overflow") }
            value |= UInt64(byte & 127) << shift
            if byte < 128 {
                guard value <= UInt64(Int.max) else { throw JedError.corruption("jsonb length overflow") }
                return Int(value)
            }
        }
        throw JedError.corruption("jsonb varint overflow")
    }

    private static func keyLess(_ a: SQLText, _ b: SQLText) -> Bool {
        a.utf8.count == b.utf8.count ? a.utf8.lexicographicallyPrecedes(b.utf8) : a.utf8.count < b.utf8.count
    }

    private static func writeJSON(_ node: SQLJSONValue, depth: Int, into writer: inout ByteWriter) throws {
        switch node {
        case .null: writer.u8(0)
        case .boolean(let value): writer.u8(value ? 2 : 1)
        case .number(let number): writer.u8(3); writer.append(try number.encodedBody())
        case .string(let text): writer.u8(4); writeVarint(text.utf8.count, into: &writer); writer.append(text.utf8)
        case .array(let elements):
            guard depth < 256 else { throw JedError("54001", "json nesting depth exceeds the maximum of 256") }
            writer.u8(6); writeVarint(elements.count, into: &writer)
            for value in elements { try writeJSON(value, depth: depth + 1, into: &writer) }
        case .object(let members):
            guard depth < 256 else { throw JedError("54001", "json nesting depth exceeds the maximum of 256") }
            // Tie on original position makes last-wins independent of the
            // sorting algorithm's stability and avoids host string equivalence.
            let ordered = members.enumerated().sorted {
                $0.element.key == $1.element.key ? $0.offset > $1.offset : keyLess($0.element.key, $1.element.key)
            }
            var unique: [SQLJSONMember] = []
            for item in ordered where unique.last?.key != item.element.key { unique.append(item.element) }
            writer.u8(7); writeVarint(unique.count, into: &writer)
            for member in unique {
                try writeJSON(.string(member.key), depth: depth + 1, into: &writer)
                try writeJSON(member.value, depth: depth + 1, into: &writer)
            }
        }
    }

    private static func readJSON(_ reader: inout ByteReader, depth: Int) throws -> SQLJSONValue {
        let tag = try reader.u8()
        switch tag {
        case 0: return .null
        case 1: return .boolean(false)
        case 2: return .boolean(true)
        case 3: return .number(try readDecimal(&reader))
        case 4:
            let count = try readVarint(&reader)
            return .string(try reader.text(count))
        case 6, 7:
            guard depth < 256 else { throw JedError.corruption("jsonb nesting depth exceeds the maximum of 256") }
            let count = try readVarint(&reader)
            guard count <= reader.remaining / (tag == 7 ? 3 : 1) else { throw JedError.corruption("truncated jsonb container") }
            if tag == 6 {
                var values: [SQLJSONValue] = []
                for _ in 0..<count { values.append(try readJSON(&reader, depth: depth + 1)) }
                return .array(values)
            }
            var members: [SQLJSONMember] = []
            for _ in 0..<count {
                guard try reader.u8() == 4 else { throw JedError.corruption("invalid jsonb object key tag") }
                let length = try readVarint(&reader), key = try reader.text(length)
                if let previous = members.last, !keyLess(previous.key, key) { throw JedError.corruption("jsonb object keys are not canonical") }
                members.append(SQLJSONMember(key: key, value: try readJSON(&reader, depth: depth + 1)))
            }
            return .object(members)
        default: throw JedError.corruption("invalid or reserved jsonb node tag")
        }
    }
}
