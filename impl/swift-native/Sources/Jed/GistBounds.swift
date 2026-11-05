/// GiST bounds are typed tuples. Range components use the lossless value codec;
/// scalar components use fixed-width ordered keys and need no collation context.
internal enum GistBound: Sendable {
    case range(SQLRange)
    case scalar(min: [UInt8], max: [UInt8])
}

internal enum GistStrategy: Sendable { case overlaps, contains, equal }
internal enum GistQuery: Sendable { case range(SQLRange), scalar([UInt8]) }

internal struct GistOpclass: Sendable {
    let type: TypeDescriptor
    let scalarWidth: Int?
    var isRange: Bool { scalarWidth == nil }

    init(type: TypeDescriptor) throws {
        self.type = type
        switch type.type {
        case .range(let element): _ = try ValueCodec.rangeElement(element); scalarWidth = nil
        case .scalar(.i16): scalarWidth = 2
        case .scalar(.i32), .scalar(.date): scalarWidth = 4
        case .scalar(.i64), .scalar(.timestamp), .scalar(.timestamptz): scalarWidth = 8
        case .scalar(.boolean): scalarWidth = 1
        case .scalar(.uuid): scalarWidth = 16
        case .scalar(.text), .scalar(.bytea), .scalar(.decimal), .scalar(.interval):
            throw JedError("0A000", "GiST operator class is not supported yet for \(type.type.name)")
        default: throw JedError("42704", "type \(type.type.name) has no default GiST operator class")
        }
    }

    func leafBound(_ value: SQLValue) throws -> GistBound? {
        if value.isNull { return nil }
        if isRange {
            guard case .range(let range) = value else { throw JedError("42804", "GiST range value has wrong type") }
            _ = try ValueCodec.encodeBody(value, type: type)
            return .range(range)
        }
        let key = try KeyEncoding.encode(value, type: type)
        return .scalar(min: key, max: key)
    }

    func query(_ value: SQLValue) throws -> GistQuery? {
        guard let bound = try leafBound(value) else { return nil }
        switch bound { case .range(let range): return .range(range); case .scalar(let key, _): return .scalar(key) }
    }

    private func validateKey(_ key: [UInt8]) throws {
        guard key.count == scalarWidth,
              type.type != .scalar(.boolean) || key[0] <= 1 else {
            throw JedError.corruption("invalid GiST scalar key")
        }
    }

    func validate(_ bound: GistBound, leaf: Bool = false) throws {
        switch bound {
        case .range(let range):
            guard case .range(let element) = type.type else { throw JedError.corruption("GiST bound kind mismatch") }
            try ValueCodec.validateRange(range, element: TypeDescriptor(element), stored: true)
        case .scalar(let min, let max):
            try validateKey(min); try validateKey(max)
            guard !max.lexicographicallyPrecedes(min), !leaf || min == max else {
                throw JedError.corruption("invalid GiST scalar bounds")
            }
        }
    }

    func encode(_ bound: GistBound) throws -> [UInt8] {
        try validate(bound)
        switch bound {
        case .range(let range): return try ValueCodec.encodeBody(.range(range), type: type)
        case .scalar(let min, let max):
            var writer = ByteWriter()
            writer.u16(UInt16(min.count)); writer.append(min)
            writer.u16(UInt16(max.count)); writer.append(max)
            return writer.bytes
        }
    }

    func decode(_ reader: inout ByteReader, leaf: Bool) throws -> GistBound {
        let result: GistBound
        if isRange {
            guard case .range(let range) = try ValueCodec.readBody(&reader, type: type) else {
                throw JedError.corruption("invalid GiST range body")
            }
            result = .range(range)
        } else {
            let minSize = Int(try reader.u16())
            let min = Array(try reader.take(minSize))
            let maxSize = Int(try reader.u16())
            let max = Array(try reader.take(maxSize))
            result = .scalar(min: min, max: max)
        }
        try validate(result, leaf: leaf)
        return result
    }

    static func bytesCompare(_ a: [UInt8], _ b: [UInt8]) -> Int {
        a == b ? 0 : a.lexicographicallyPrecedes(b) ? -1 : 1
    }

    func compare(_ a: GistBound, _ b: GistBound) throws -> Int {
        switch (a, b) {
        case (.range(let a), .range(let b)):
            return try ValueComparison.total(.range(a), .range(b), type: type)
        case (.scalar(let amin, let amax), .scalar(let bmin, let bmax)):
            let min = Self.bytesCompare(amin, bmin)
            return min == 0 ? Self.bytesCompare(amax, bmax) : min
        default: throw JedError.corruption("GiST bound kind mismatch")
        }
    }

    private func boundCompare(_ a: SQLRange, _ b: SQLRange, lower: Bool) throws -> Int {
        guard case .range(let element) = type.type else { throw JedError.corruption("GiST bound kind mismatch") }
        return try ValueComparison.bound(lower ? a.lower : a.upper,
            inclusive: lower ? a.lowerInclusive : a.upperInclusive,
            lower ? b.lower : b.upper, inclusive: lower ? b.lowerInclusive : b.upperInclusive,
            lower: lower, type: TypeDescriptor(element))
    }

    func union(_ a: GistBound, _ b: GistBound) throws -> GistBound {
        switch (a, b) {
        case (.range(let a), .range(let b)):
            if a.isEmpty { return .range(b) }; if b.isEmpty { return .range(a) }
            // Equal endpoints retain the right operand's representation. In
            // numrange, equal values may have different display scales, so
            // this choice also fixes the union's value-codec penalty bytes.
            let low = try boundCompare(a, b, lower: true) < 0 ? a : b
            let high = try boundCompare(a, b, lower: false) > 0 ? a : b
            return .range(SQLRange(lower: low.lower, upper: high.upper,
                lowerInclusive: low.lowerInclusive, upperInclusive: high.upperInclusive))
        case (.scalar(let amin, let amax), .scalar(let bmin, let bmax)):
            return .scalar(min: amin.lexicographicallyPrecedes(bmin) ? amin : bmin,
                           max: amax.lexicographicallyPrecedes(bmax) ? bmax : amax)
        default: throw JedError.corruption("GiST bound kind mismatch")
        }
    }

    private func overlaps(_ a: SQLRange, _ b: SQLRange) throws -> Bool {
        if a.isEmpty || b.isEmpty { return false }
        guard case .range(let element) = type.type else { throw JedError.corruption("GiST bound kind mismatch") }
        for (upper, upperInclusive, lower, lowerInclusive) in [
            (a.upper, a.upperInclusive, b.lower, b.lowerInclusive),
            (b.upper, b.upperInclusive, a.lower, a.lowerInclusive)
        ] {
            if let upper, let lower {
                let order = try ValueComparison.total(upper, lower, type: TypeDescriptor(element))
                if order < 0 || (order == 0 && !(upperInclusive && lowerInclusive)) { return false }
            }
        }
        return true
    }

    func matches(_ bound: GistBound, query: GistQuery, strategy: GistStrategy, leaf: Bool) throws -> Bool {
        switch (bound, query, strategy) {
        case (.range(let bound), .range(let query), .overlaps): return try overlaps(bound, query)
        case (.range(let bound), .range(let query), .contains):
            if !leaf { return try overlaps(bound, query) }
            if query.isEmpty { return true }; if bound.isEmpty { return false }
            return try boundCompare(bound, query, lower: true) <= 0 && boundCompare(bound, query, lower: false) >= 0
        case (.scalar(let min, let max), .scalar(let key), .equal):
            try validateKey(key)
            return leaf ? min == key : !key.lexicographicallyPrecedes(min) && !max.lexicographicallyPrecedes(key)
        default: throw JedError("0A000", "GiST strategy does not match its operator class")
        }
    }
}

internal enum GistTuple {
    static func validateCount(_ count: Int, _ ops: [GistOpclass]) throws {
        guard !ops.isEmpty, count == ops.count else { throw JedError.corruption("GiST tuple width mismatch") }
    }
    static func encode(_ bound: [GistBound], ops: [GistOpclass]) throws -> [UInt8] {
        try validateCount(bound.count, ops)
        var bytes: [UInt8] = []
        for (op, value) in zip(ops, bound) { bytes += try op.encode(value) }
        return bytes
    }
    static func decode(_ reader: inout ByteReader, ops: [GistOpclass], leaf: Bool) throws -> [GistBound] {
        try validateCount(ops.count, ops)
        return try ops.map { try $0.decode(&reader, leaf: leaf) }
    }
    static func compare(_ a: [GistBound], _ b: [GistBound], ops: [GistOpclass]) throws -> Int {
        try validateCount(a.count, ops); try validateCount(b.count, ops)
        for index in ops.indices {
            let order = try ops[index].compare(a[index], b[index])
            if order != 0 { return order }
        }
        return 0
    }
    static func union(_ a: [GistBound], _ b: [GistBound], ops: [GistOpclass]) throws -> [GistBound] {
        try validateCount(a.count, ops); try validateCount(b.count, ops)
        return try ops.indices.map { try ops[$0].union(a[$0], b[$0]) }
    }
}
