/// Values carry their exact representation; resolved expressions and column
/// descriptors carry static types. SQL comparison is explicit: synthesized
/// Equatable would get decimals, floats, intervals and NULL wrong.
/// Keep scalar payloads and collection handles inline. Boxing the whole enum
/// would allocate even an integer decoded from a resident page. Only large or
/// recursively embedded payloads are indirect, so scalar row slots stay small.
public enum SQLValue: Sendable {
    case null
    case integer(Int64)
    case boolean(Bool)
    case text(SQLText)
    indirect case decimal(SQLDecimal)
    case bytea([UInt8])
    case uuid(SQLUUID)
    case timestamp(Int64)
    case timestamptz(Int64)
    case date(Int32)
    indirect case interval(SQLInterval)
    case f32(UInt32)
    case f64(UInt64)
    case composite([SQLValue])
    indirect case array(SQLArray)
    indirect case range(SQLRange)
    case json(SQLText)
    indirect case jsonb(SQLJSONValue)
    case jsonpath(SQLText)

    public var isNull: Bool {
        if case .null = self { return true }
        return false
    }
}

public struct SQLInterval: Sendable, Equatable {
    public let months: Int32
    public let days: Int32
    public let microseconds: Int64
    public init(months: Int32 = 0, days: Int32 = 0, microseconds: Int64 = 0) {
        self.months = months
        self.days = days
        self.microseconds = microseconds
    }
    /// PostgreSQL's comparison span; field equality is only representation
    /// equality. Swift's standard Int128 keeps this exact without a dependency.
    internal var comparisonSpan: Int128 {
        (Int128(months) * 30 + Int128(days)) * 86_400_000_000 + Int128(microseconds)
    }
}

public struct SQLUUID: Sendable, Equatable {
    public let bytes: [UInt8]
    public init(bytes: [UInt8]) throws {
        guard bytes.count == 16 else { throw JedError("22P02", "invalid UUID length") }
        self.bytes = bytes
    }
}

public struct SQLArray: Sendable {
    public let dimensions: [Int]
    public let lowerBounds: [Int32]
    public let elements: [SQLValue]

    public init(elements: [SQLValue]) throws {
        try self.init(dimensions: elements.isEmpty ? [] : [elements.count],
                      lowerBounds: elements.isEmpty ? [] : [1], elements: elements)
    }

    public init(dimensions: [Int], lowerBounds: [Int32], elements: [SQLValue]) throws {
        guard dimensions.count == lowerBounds.count else {
            throw JedError("2202E", "array dimensions and lower bounds do not match")
        }
        // MAXDIM=6 is a literal-input rule in the frozen baseline. Constructors
        // admit deeper shapes; the representation itself is bounded by ndim u8.
        guard dimensions.count <= 255 else {
            throw JedError("54000", "number of array dimensions exceeds the wire format maximum (255)")
        }
        var count = dimensions.isEmpty ? 0 : 1
        for (index, dimension) in dimensions.enumerated() {
            guard dimension > 0 && dimension <= 134_217_727,
                  count <= 134_217_727 / dimension else {
                throw JedError("54000", "array size exceeds the maximum allowed (134217727)")
            }
            // array_prepend may produce an upper bound of Int32.max. Other
            // builders impose their own stricter next-subscript check.
            guard Int64(lowerBounds[index]) + Int64(dimension) - 1 <= Int32.max else {
                throw JedError("54000", "array lower bound is too large: \(lowerBounds[index])")
            }
            count *= dimension
        }
        guard count == elements.count else {
            throw JedError("2202E", "array dimensions do not match element count")
        }
        self.dimensions = dimensions
        self.lowerBounds = lowerBounds
        self.elements = elements
    }
}

/// Range constructors in the evaluator canonicalize discrete endpoints. A host
/// value is revalidated and canonicalized against its resolved element type at bind.
public struct SQLRange: Sendable {
    public let isEmpty: Bool
    public let lower: SQLValue?
    public let upper: SQLValue?
    public let lowerInclusive: Bool
    public let upperInclusive: Bool

    public init(empty: Bool = false, lower: SQLValue? = nil, upper: SQLValue? = nil,
                lowerInclusive: Bool = true, upperInclusive: Bool = false) {
        self.isEmpty = empty
        self.lower = empty ? nil : lower
        self.upper = empty ? nil : upper
        self.lowerInclusive = !empty && lower != nil && lowerInclusive
        self.upperInclusive = !empty && upper != nil && upperInclusive
    }
}

public struct SQLJSONMember: Sendable {
    public let key: SQLText
    public let value: SQLJSONValue
    public init(key: SQLText, value: SQLJSONValue) { self.key = key; self.value = value }
}

/// jsonb's exact-number tree. Object order and duplicate elimination belong to
/// the handwritten JSON parser/canonicalizer, never a host dictionary encoder.
public enum SQLJSONValue: Sendable {
    case null
    case boolean(Bool)
    case string(SQLText)
    indirect case number(SQLDecimal)
    case array([SQLJSONValue])
    case object([SQLJSONMember])
}
