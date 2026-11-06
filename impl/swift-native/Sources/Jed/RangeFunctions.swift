/// Pure range kernels over canonical values. Callers own operator_eval and
/// resolve the common subtype before entering this module.
internal enum RangeFunctions {
    static func canonicalize(_ input: SQLRange, element: SQLType) throws -> SQLRange {
        guard case .scalar(let scalar) = element,
              [.i32, .i64, .decimal, .timestamp, .timestamptz, .date].contains(scalar) else {
            throw JedError("42804", "invalid range subtype")
        }
        if input.isEmpty { return SQLRange(empty: true) }
        var lower = input.lower.flatMap { $0.isNull ? nil : $0 }
        var upper = input.upper.flatMap { $0.isNull ? nil : $0 }
        var lowerInclusive = lower != nil && input.lowerInclusive
        var upperInclusive = upper != nil && input.upperInclusive
        if let lower, let upper {
            let comparison = try ValueComparison.total(lower, upper, type: TypeDescriptor(element))
            guard comparison <= 0 else { throw JedError("22000", "range lower bound must be less than or equal to range upper bound") }
            if comparison == 0 && !(lowerInclusive && upperInclusive) { return SQLRange(empty: true) }
        }
        if scalar == .i32 || scalar == .i64 || scalar == .date {
            if let value = lower, !lowerInclusive, !infiniteDate(value) {
                lower = try successor(value, scalar: scalar); lowerInclusive = true
            }
            if let value = upper, upperInclusive, !infiniteDate(value) {
                upper = try successor(value, scalar: scalar); upperInclusive = false
            }
        }
        if let lower, let upper {
            let comparison = try ValueComparison.total(lower, upper, type: TypeDescriptor(element))
            if comparison > 0 || comparison == 0 && !(lowerInclusive && upperInclusive) { return SQLRange(empty: true) }
        }
        return SQLRange(lower: lower, upper: upper, lowerInclusive: lowerInclusive, upperInclusive: upperInclusive)
    }

    private static func infiniteDate(_ value: SQLValue) -> Bool {
        if case .date(let day) = value { return day == .min || day == .max }
        return false
    }
    private static func successor(_ value: SQLValue, scalar: ScalarType) throws -> SQLValue {
        switch value {
        case .integer(let number) where scalar == .i32 || scalar == .i64:
            guard number < Int64.max, scalar != .i32 || number < Int32.max else { throw JedError("22003", "integer out of range") }
            return .integer(number + 1)
        case .date(let day) where scalar == .date:
            guard day < Int32.max - 1 else { throw JedError("22003", "integer out of range") }
            return .date(day + 1)
        default: throw JedError("42804", "value does not match range subtype")
        }
    }

    static func construct(lower: SQLValue, upper: SQLValue, flags: SQLValue? = nil,
                          element: SQLType) throws -> SQLValue {
        var inclusiveLower = true, inclusiveUpper = false
        if let flags {
            guard !flags.isNull else { throw JedError("22000", "range constructor flags argument must not be null") }
            guard case .text(let text) = flags, text.utf8.count == 2,
                  text.utf8[0] == 91 || text.utf8[0] == 40,
                  text.utf8[1] == 93 || text.utf8[1] == 41 else { throw JedError("42601", "invalid range bound flags") }
            inclusiveLower = text.utf8[0] == 91; inclusiveUpper = text.utf8[1] == 93
        }
        return .range(try canonicalize(SQLRange(lower: lower.isNull ? nil : lower, upper: upper.isNull ? nil : upper,
                                               lowerInclusive: inclusiveLower, upperInclusive: inclusiveUpper), element: element))
    }

    static func call(name: String, args: [SQLValue], element: SQLType) throws -> SQLValue {
        if name == "range_merge", args.count == 2 { return try binary("range_merge", args[0], args[1], element: element) }
        guard args.count == 1 else { throw JedError("42883", "invalid range function arguments") }
        if args[0].isNull { return .null }
        guard case .range(let range) = args[0] else { throw JedError("42804", "expected range value") }
        switch name {
        case "lower": return range.lower ?? .null
        case "upper": return range.upper ?? .null
        case "isempty": return .boolean(range.isEmpty)
        case "lower_inc": return .boolean(range.lowerInclusive)
        case "upper_inc": return .boolean(range.upperInclusive)
        case "lower_inf": return .boolean(!range.isEmpty && range.lower == nil)
        case "upper_inf": return .boolean(!range.isEmpty && range.upper == nil)
        default: throw JedError("42883", "unknown range function \(name)")
        }
    }

    private static func bound(_ left: SQLRange, _ right: SQLRange, lower: Bool, element: SQLType) throws -> Int {
        try ValueComparison.bound(lower ? left.lower : left.upper, inclusive: lower ? left.lowerInclusive : left.upperInclusive,
                                  lower ? right.lower : right.upper, inclusive: lower ? right.lowerInclusive : right.upperInclusive,
                                  lower: lower, type: TypeDescriptor(element))
    }
    private static func contains(_ range: SQLRange, value: SQLValue, element: SQLType) throws -> Bool {
        if range.isEmpty { return false }
        if let lower = range.lower {
            let c = try ValueComparison.total(value, lower, type: TypeDescriptor(element))
            if c < 0 || c == 0 && !range.lowerInclusive { return false }
        }
        if let upper = range.upper {
            let c = try ValueComparison.total(value, upper, type: TypeDescriptor(element))
            if c > 0 || c == 0 && !range.upperInclusive { return false }
        }
        return true
    }
    private static func before(_ left: SQLRange, _ right: SQLRange, element: SQLType) throws -> Bool {
        guard !left.isEmpty, !right.isEmpty, let upper = left.upper, let lower = right.lower else { return false }
        let c = try ValueComparison.total(upper, lower, type: TypeDescriptor(element))
        return c < 0 || c == 0 && !(left.upperInclusive && right.lowerInclusive)
    }
    private static func touches(_ left: SQLRange, _ right: SQLRange, element: SQLType) throws -> Bool {
        guard let upper = left.upper, let lower = right.lower else { return false }
        return try ValueComparison.total(upper, lower, type: TypeDescriptor(element)) == 0 && left.upperInclusive != right.lowerInclusive
    }

    static func binary(_ operation: String, _ left: SQLValue, _ right: SQLValue, element: SQLType) throws -> SQLValue {
        if left.isNull || right.isNull { return .null }
        if operation == "<@" { return try binary("@>", right, left, element: element) }
        if operation == "@>", case .range(let range) = left {
            if case .range(let other) = right {
                if other.isEmpty { return .boolean(true) }; if range.isEmpty { return .boolean(false) }
                return .boolean(try bound(range, other, lower: true, element: element) <= 0 && bound(range, other, lower: false, element: element) >= 0)
            }
            return .boolean(try contains(range, value: right, element: element))
        }
        guard case .range(let a) = left, case .range(let b) = right else { throw JedError("42804", "expected range operands") }
        let empty = a.isEmpty || b.isEmpty
        switch operation {
        case "<<": return .boolean(try before(a, b, element: element))
        case ">>": return .boolean(try before(b, a, element: element))
        case "&<": return .boolean(try !empty && bound(a, b, lower: false, element: element) <= 0)
        case "&>": return .boolean(try !empty && bound(a, b, lower: true, element: element) >= 0)
        case "-|-": return .boolean(try !empty && (touches(a, b, element: element) || touches(b, a, element: element)))
        default: break
        }
        let overlaps = try !empty && !before(a, b, element: element) && !before(b, a, element: element)
        if operation == "&&" { return .boolean(overlaps) }
        if operation == "+" || operation == "range_merge" {
            if a.isEmpty { return right }; if b.isEmpty { return left }
            if operation == "+", !overlaps {
                guard try touches(a, b, element: element) || touches(b, a, element: element) else {
                    throw JedError("22000", "result of range union would not be contiguous")
                }
            }
            let lower = try bound(a, b, lower: true, element: element) <= 0 ? a : b
            let upper = try bound(a, b, lower: false, element: element) >= 0 ? a : b
            return .range(SQLRange(lower: lower.lower, upper: upper.upper, lowerInclusive: lower.lowerInclusive, upperInclusive: upper.upperInclusive))
        }
        if operation == "*" {
            if !overlaps { return .range(SQLRange(empty: true)) }
            let lower = try bound(a, b, lower: true, element: element) >= 0 ? a : b
            let upper = try bound(a, b, lower: false, element: element) <= 0 ? a : b
            return .range(SQLRange(lower: lower.lower, upper: upper.upper, lowerInclusive: lower.lowerInclusive, upperInclusive: upper.upperInclusive))
        }
        if operation == "-" {
            if !overlaps { return left }
            let lower = try bound(a, b, lower: true, element: element)
            let upper = try bound(a, b, lower: false, element: element)
            if lower >= 0 && upper <= 0 { return .range(SQLRange(empty: true)) }
            guard lower >= 0 || upper <= 0 else { throw JedError("22000", "result of range difference would not be contiguous") }
            let result = lower >= 0
                ? SQLRange(lower: b.upper, upper: a.upper, lowerInclusive: !b.upperInclusive, upperInclusive: a.upperInclusive)
                : SQLRange(lower: a.lower, upper: b.lower, lowerInclusive: a.lowerInclusive, upperInclusive: !b.lowerInclusive)
            return .range(try canonicalize(result, element: element))
        }
        throw JedError("42883", "unknown range operator \(operation)")
    }
}
