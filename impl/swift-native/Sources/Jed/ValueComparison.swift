internal enum ValueComparison {
    private static func order<T: Comparable>(_ left: T, _ right: T) -> Int { left == right ? 0 : left < right ? -1 : 1 }
    private static func bytes(_ left: [UInt8], _ right: [UInt8]) -> Int {
        left == right ? 0 : left.lexicographicallyPrecedes(right) ? -1 : 1
    }
    private static func float(_ value: SQLValue) -> Double? {
        switch value { case .f32(let bits): Double(Float(bitPattern: bits)); case .f64(let bits): Double(bitPattern: bits); default: nil }
    }

    /// Total order for sort/group/key/internal bounds. SQL row operators use
    /// the distinct three-valued entry point below. NULL sorts last recursively.
    static func total(_ left: SQLValue, _ right: SQLValue, type: TypeDescriptor,
                      collatedKey: KeyEncoding.CollatedKey? = nil, equality: Bool = false) throws -> Int {
        if left.isNull { return right.isNull ? 0 : 1 }
        if right.isNull { return -1 }
        if let a = float(left), let b = float(right) {
            if a.isNaN { return b.isNaN ? 0 : 1 }
            if b.isNaN { return -1 }
            return order(a, b)
        }
        switch (left, right) {
        case (.integer(let a), .integer(let b)): return order(a, b)
        case (.decimal(let a), .decimal(let b)): return a.compare(to: b)
        case (.integer(let a), .decimal(let b)): return SQLDecimal(a).compare(to: b)
        case (.decimal(let a), .integer(let b)): return a.compare(to: SQLDecimal(b))
        case (.boolean(let a), .boolean(let b)): return a == b ? 0 : a ? 1 : -1
        case (.text(let a), .text(let b)):
            if !equality, let collation = type.collation, collation.utf8 != [67] {
                guard let collatedKey else { throw JedError("XX002", "collation data is unavailable") }
                return try bytes(collatedKey(a, collation), collatedKey(b, collation))
            }
            return order(a, b)
        case (.bytea(let a), .bytea(let b)): return bytes(a, b)
        case (.uuid(let a), .uuid(let b)): return bytes(a.bytes, b.bytes)
        case (.date(let a), .date(let b)): return order(a, b)
        case (.timestamp(let a), .timestamp(let b)), (.timestamptz(let a), .timestamptz(let b)): return order(a, b)
        case (.interval(let a), .interval(let b)): return order(a.comparisonSpan, b.comparisonSpan)
        case (.jsonb(let a), .jsonb(let b)): return SQLJSON.compare(a, b)
        case (.composite(let a), .composite(let b)):
            guard let fields = TypeRules.fields(type.type), a.count == fields.count, b.count == fields.count else {
                throw JedError("42804", "composite value does not match comparison type")
            }
            for index in fields.indices {
                let result = try total(a[index], b[index], type: fields[index].descriptor, collatedKey: collatedKey, equality: equality)
                if result != 0 { return result }
            }
            return 0
        case (.array(let a), .array(let b)):
            guard case .array(let element) = type.type else { throw JedError("42804", "array value does not match comparison type") }
            var descriptor = TypeDescriptor(element); descriptor.collation = type.collation
            for (a, b) in zip(a.elements, b.elements) {
                let result = try total(a, b, type: descriptor, collatedKey: collatedKey, equality: equality)
                if result != 0 { return result }
            }
            let length = order(a.elements.count, b.elements.count)
            if length != 0 { return length }
            let dimensions = order(a.dimensions.count, b.dimensions.count)
            if dimensions != 0 { return dimensions }
            for index in a.dimensions.indices {
                let result = order(a.dimensions[index], b.dimensions[index])
                if result != 0 { return result }
                let bound = order(a.lowerBounds[index], b.lowerBounds[index])
                if bound != 0 { return bound }
            }
            return 0
        case (.range(let a), .range(let b)):
            guard case .range(let element) = type.type else { throw JedError("42804", "range value does not match comparison type") }
            if a.isEmpty { return b.isEmpty ? 0 : -1 }; if b.isEmpty { return 1 }
            let descriptor = TypeDescriptor(element)
            let lower = try bound(a.lower, inclusive: a.lowerInclusive, b.lower, inclusive: b.lowerInclusive, lower: true, type: descriptor, collatedKey: collatedKey)
            return lower == 0 ? try bound(a.upper, inclusive: a.upperInclusive, b.upper, inclusive: b.upperInclusive, lower: false, type: descriptor, collatedKey: collatedKey) : lower
        default: throw JedError("42804", "values do not match comparison type \(type.type.name)")
        }
    }

    static func bound(_ left: SQLValue?, inclusive leftInclusive: Bool,
                      _ right: SQLValue?, inclusive rightInclusive: Bool, lower: Bool,
                      type: TypeDescriptor, collatedKey: KeyEncoding.CollatedKey? = nil) throws -> Int {
        guard let left else { return right == nil ? 0 : lower ? -1 : 1 }
        guard let right else { return lower ? 1 : -1 }
        let result = try total(left, right, type: type, collatedKey: collatedKey)
        if result != 0 { return result }
        if leftInclusive == rightInclusive { return 0 }
        return leftInclusive == lower ? -1 : 1
    }

    static func sql(_ operation: String, _ left: SQLValue, _ right: SQLValue,
                    type: TypeDescriptor, collatedKey: KeyEncoding.CollatedKey? = nil) throws -> SQLValue {
        if left.isNull || right.isNull { return .null }
        if case .composite(let a) = left, case .composite(let b) = right {
            guard let fields = TypeRules.fields(type.type), a.count == fields.count, b.count == fields.count else {
                throw JedError("42804", "composite value does not match comparison type")
            }
            if operation == "=" || operation == "<>" {
                var unknown = false
                for index in fields.indices {
                    switch try sql("=", a[index], b[index], type: fields[index].descriptor, collatedKey: collatedKey) {
                    case .boolean(false): return .boolean(operation == "<>")
                    case .null: unknown = true
                    default: break
                    }
                }
                return unknown ? .null : .boolean(operation == "=")
            }
            for index in fields.indices {
                let less = try sql("<", a[index], b[index], type: fields[index].descriptor, collatedKey: collatedKey)
                if less.isNull { return .null }
                if case .boolean(true) = less { return .boolean(operation == "<" || operation == "<=") }
                let more = try sql(">", a[index], b[index], type: fields[index].descriptor, collatedKey: collatedKey)
                if more.isNull { return .null }
                if case .boolean(true) = more { return .boolean(operation == ">" || operation == ">=") }
            }
            return .boolean(operation == "<=" || operation == ">=")
        }
        let result = try total(left, right, type: type, collatedKey: collatedKey, equality: operation == "=" || operation == "<>")
        switch operation {
        case "=": return .boolean(result == 0)
        case "<>": return .boolean(result != 0)
        case "<": return .boolean(result < 0)
        case ">": return .boolean(result > 0)
        case "<=": return .boolean(result <= 0)
        case ">=": return .boolean(result >= 0)
        default: throw JedError("XX000", "unknown resolved comparison")
        }
    }

    static func isNull(_ value: SQLValue, negated: Bool) -> Bool {
        if value.isNull { return !negated }
        if case .composite(let fields) = value {
            return negated ? fields.allSatisfy({ !$0.isNull }) : fields.allSatisfy(\.isNull)
        }
        return negated
    }
}
