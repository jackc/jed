/// Walk only self-delimiting key bytes to find the appended storage key. This
/// never reconstructs SQL values or assumes a fixed-width primary key.
internal enum IndexKeyBoundary {
    static func suffix(_ bytes: [UInt8], types: [TypeDescriptor], options: [UInt8]) throws -> [UInt8] {
        let split = try prefix(bytes, types: types, options: options)
        guard split.count < bytes.count else { throw JedError.corruption("index entry has no storage key") }
        return Array(bytes.dropFirst(split.count))
    }
    /// Integrity uniqueness compares the encoded prefix and skips NULL keys.
    /// Keep the ordinary suffix decoder's exact slot grammar in one place.
    static func prefix(_ bytes: [UInt8], types: [TypeDescriptor], options: [UInt8]) throws -> (count: Int, hasNull: Bool) {
        guard options.isEmpty || options.count == types.count else { throw JedError.corruption("invalid index ordering arity") }
        var offset = 0, hasNull = false
        for ordinal in types.indices {
            let option = options.isEmpty ? 0 : options[ordinal]
            let descending = option & 1 != 0, first = option & 2 != 0
            let present: UInt8 = descending ? (first ? 255 : 254) : (first ? 1 : 0)
            let absent: UInt8 = descending ? (first ? 254 : 255) : (first ? 0 : 1)
            guard offset < bytes.count else { throw JedError.corruption("truncated index presence slot") }
            let tag = bytes[offset]; offset += 1
            if tag == absent { hasNull = true; continue }
            guard tag == present else { throw JedError.corruption("invalid index presence slot") }
            try body(bytes, offset: &offset, type: types[ordinal], inverted: descending, depth: 0)
        }
        return (offset, hasNull)
    }
    private static func body(_ bytes: [UInt8], offset: inout Int, type: TypeDescriptor,
                             inverted: Bool, depth: Int) throws {
        func read(_ position: inout Int) throws -> UInt8 {
            guard position < bytes.count else { throw JedError.corruption("truncated index key") }
            defer { position += 1 }
            return inverted ? ~bytes[position] : bytes[position]
        }
        func skip(_ count: Int, _ position: inout Int) throws {
            guard count >= 0, count <= bytes.count - position else { throw JedError.corruption("truncated index key body") }
            position += count
        }
        func terminated(_ position: inout Int) throws {
            while true {
                if try read(&position) != 0 { continue }
                let next = try read(&position)
                if next == 1 { return }
                guard next == 255 else { throw JedError.corruption("invalid escaped index key") }
            }
        }
        if let width = ValueCodec.fixedWidth(type.type) { try skip(width, &offset); return }
        switch type.type {
        case .scalar(.text):
            if let collation = type.collation, collation.description != "C" {
                for _ in 0..<3 {
                    while true {
                        let first = try read(&offset), second = try read(&offset)
                        if first == 0 && second == 0 { break }
                    }
                }
            }
            try terminated(&offset)
        case .scalar(.bytea): try terminated(&offset)
        case .scalar(.decimal):
            let sign = try read(&offset)
            if sign == 4 { return }
            guard sign == 3 || sign == 5 else { throw JedError.corruption("invalid decimal index key") }
            try skip(4, &offset)
            let terminal: UInt8 = sign == 3 ? 255 : 0
            while true {
                let digit = try read(&offset)
                if digit == terminal { break }
                guard sign == 3 ? (155...254).contains(digit) : (1...100).contains(digit) else {
                    throw JedError.corruption("invalid decimal index digit")
                }
            }
        case .array(let element):
            var child = TypeDescriptor(element); child.collation = type.collation
            while true {
                let tag = try read(&offset)
                if tag == 0 { break }
                if tag == 2 { continue }
                guard tag == 1 else { throw JedError.corruption("invalid array index member") }
                try body(bytes, offset: &offset, type: child, inverted: inverted, depth: depth)
            }
            let dimensions = Int(try read(&offset))
            // The wire dimension count is u8. Input operations may impose
            // narrower limits, but valid stored arrays retain all 255 slots.
            try skip(dimensions * 8, &offset)
        case .composite(let composite):
            guard depth < 32 else { throw JedError.corruption("index key nesting exceeds type limit") }
            for field in composite.fields {
                let tag = try read(&offset)
                if tag == 1 { continue }
                guard tag == 0 else { throw JedError.corruption("invalid composite index member") }
                try body(bytes, offset: &offset, type: field.descriptor, inverted: inverted, depth: depth + 1)
            }
        case .range(let element):
            let tag = try read(&offset)
            if tag == 0 { return }
            guard tag == 1 else { throw JedError.corruption("invalid range index member") }
            let lower = try read(&offset)
            if lower == 1 {
                try body(bytes, offset: &offset, type: TypeDescriptor(element), inverted: inverted, depth: depth)
                guard try read(&offset) <= 1 else { throw JedError.corruption("invalid lower range index flag") }
            } else if lower != 0 { throw JedError.corruption("invalid lower range index bound") }
            let upper = try read(&offset)
            if upper == 1 {
                try body(bytes, offset: &offset, type: TypeDescriptor(element), inverted: inverted, depth: depth)
                guard try read(&offset) <= 1 else { throw JedError.corruption("invalid upper range index flag") }
            } else if upper != 2 { throw JedError.corruption("invalid upper range index bound") }
        default: throw JedError.corruption("type has no ordered index key body")
        }
    }
}
