/// Bare key bodies are prefix-free within one resolved type. Concatenation
/// composes them; nullable/ordered slots add their own presence framing.
internal enum KeyEncoding {
    typealias CollatedKey = (_ text: SQLText, _ collation: SQLText) throws -> [UInt8]

    static func encode(_ value: SQLValue, type: TypeDescriptor, descending: Bool = false,
                       collatedKey: CollatedKey? = nil) throws -> [UInt8] {
        try validateType(type.type, depth: 0)
        guard !value.isNull else { throw JedError("23502", "NULL has no bare key body") }
        let body = try encodeBody(value, type: type, depth: 0, collatedKey: collatedKey)
        return descending ? body.map { ~$0 } : body
    }

    static func encodeSlot(_ value: SQLValue, type: TypeDescriptor, descending: Bool = false,
                           nullsFirst: Bool? = nil, collatedKey: CollatedKey? = nil) throws -> [UInt8] {
        try validateType(type.type, depth: 0)
        let body = value.isNull ? nil : try encodeBody(value, type: type, depth: 0, collatedKey: collatedKey)
        return slot(body: body, descending: descending, nullsFirst: nullsFirst ?? descending)
    }

    static func slot(body: [UInt8]?, descending: Bool, nullsFirst: Bool) -> [UInt8] {
        let present: UInt8 = descending ? (nullsFirst ? 255 : 254) : (nullsFirst ? 1 : 0)
        let absent: UInt8 = descending ? (nullsFirst ? 254 : 255) : (nullsFirst ? 0 : 1)
        guard let body else { return [absent] }
        return [present] + (descending ? body.map { ~$0 } : body)
    }

    static func validateType(_ type: SQLType, depth: Int = 0) throws {
        switch type {
        case .scalar(.json), .scalar(.jsonb), .scalar(.jsonpath), .record:
            throw JedError("0A000", "type has no persisted ordered-key encoding")
        case .scalar: return
        case .range(let element): _ = try ValueCodec.rangeElement(element)
        case .array(let element):
            guard case .scalar = element else { throw JedError("0A000", "array element has no ordered-key encoding") }
            try validateType(element, depth: depth)
        case .composite(let composite):
            guard depth < 32 else { throw JedError("54001", "composite nesting depth exceeds the maximum of 32") }
            guard !composite.fields.isEmpty else { throw JedError("0A000", "empty composite type has no stored encoding") }
            for field in composite.fields { try validateType(field.descriptor.type, depth: depth + 1) }
        }
    }

    private static func terminated(_ bytes: [UInt8]) -> [UInt8] {
        var result: [UInt8] = []
        for byte in bytes {
            result.append(byte)
            if byte == 0 { result.append(255) }
        }
        result.append(contentsOf: [0, 1])
        return result
    }

    private static func encodeBody(_ value: SQLValue, type: TypeDescriptor, depth: Int,
                                   collatedKey: CollatedKey?) throws -> [UInt8] {
        var writer = ByteWriter()
        switch (type.type, value) {
        case (.scalar(.text), .text(let text)):
            if let collation = type.collation, collation.utf8 != [67] {
                guard let collatedKey else { throw JedError("XX002", "required collation is not loaded") }
                return try collatedKey(text, collation)
            }
            return terminated(text.utf8)
        case (.scalar(.bytea), .bytea(let bytes)): return terminated(bytes)
        case (.scalar(.decimal), .decimal(let value)): return try value.keyBytes()
        case (.scalar(.f32), .f32(let raw)):
            var bits = ValueCodec.canonicalFloat32(raw)
            if bits & 0x7fff_ffff == 0 { bits = 0 }
            writer.u32(bits & 0x8000_0000 == 0 ? bits ^ 0x8000_0000 : ~bits)
        case (.scalar(.f64), .f64(let raw)):
            var bits = ValueCodec.canonicalFloat64(raw)
            if bits & 0x7fff_ffff_ffff_ffff == 0 { bits = 0 }
            writer.u64(bits & 0x8000_0000_0000_0000 == 0 ? bits ^ 0x8000_0000_0000_0000 : ~bits)
        case (.scalar(.interval), .interval(let interval)):
            let bits = UInt128(bitPattern: interval.comparisonSpan) ^ (UInt128(1) << 127)
            for shift in stride(from: 120, through: 0, by: -8) { writer.u8(UInt8(truncatingIfNeeded: bits >> shift)) }
        case (.array(let elementType), .array(let array)):
            var element = TypeDescriptor(elementType)
            element.collation = type.collation
            for value in array.elements {
                if value.isNull { writer.u8(2) }
                else {
                    writer.u8(1)
                    writer.append(try encodeBody(value, type: element, depth: depth, collatedKey: collatedKey))
                }
            }
            writer.u8(0); writer.u8(UInt8(array.dimensions.count))
            for (length, lower) in zip(array.dimensions, array.lowerBounds) {
                writer.u32(UInt32(length)); writer.u32(UInt32(bitPattern: lower) ^ 0x8000_0000)
            }
        case (.composite(let composite), .composite(let fields)):
            guard depth < 32, fields.count == composite.fields.count else { throw JedError("42804", "composite key field count does not match its type") }
            for (field, value) in zip(composite.fields, fields) {
                if value.isNull { writer.u8(1) }
                else {
                    writer.u8(0)
                    writer.append(try encodeBody(value, type: field.descriptor, depth: depth + 1, collatedKey: collatedKey))
                }
            }
        case (.range(let elementType), .range(let range)):
            let element = try ValueCodec.rangeElement(elementType)
            try ValueCodec.validateRange(range, element: element, stored: false)
            if range.isEmpty { return [0] }
            writer.u8(1)
            if let lower = range.lower {
                writer.u8(1); writer.append(try encodeBody(lower, type: element, depth: depth, collatedKey: collatedKey))
                writer.u8(range.lowerInclusive ? 0 : 1)
            } else { writer.u8(0) }
            if let upper = range.upper {
                writer.u8(1); writer.append(try encodeBody(upper, type: element, depth: depth, collatedKey: collatedKey))
                writer.u8(range.upperInclusive ? 1 : 0)
            } else { writer.u8(2) }
        case (.scalar, _): return try ValueCodec.encodeBody(value, type: type)
        default: throw JedError("42804", "key value does not match its static type")
        }
        return writer.bytes
    }
}
