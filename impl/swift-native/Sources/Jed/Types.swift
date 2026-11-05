/// Stable scalar codes from spec/fileformat/format.md. Containers are open types
/// and never become members of this enum.
public enum ScalarType: UInt8, CaseIterable, Sendable {
    case i16 = 1, i32 = 2, i64 = 3, text = 4, boolean = 5, decimal = 6
    case bytea = 7, uuid = 8, timestamp = 9, timestamptz = 10, interval = 11
    case f64 = 12, f32 = 13, date = 16, json = 18, jsonb = 19, jsonpath = 20

    public var name: String {
        switch self {
        case .i16: "i16"
        case .i32: "i32"
        case .i64: "i64"
        case .text: "text"
        case .boolean: "boolean"
        case .decimal: "decimal"
        case .bytea: "bytea"
        case .uuid: "uuid"
        case .timestamp: "timestamp"
        case .timestamptz: "timestamptz"
        case .interval: "interval"
        case .f32: "f32"
        case .f64: "f64"
        case .date: "date"
        case .json: "json"
        case .jsonb: "jsonb"
        case .jsonpath: "jsonpath"
        }
    }

    public var integerWidth: Int? {
        switch self {
        case .i16: 2
        case .i32: 4
        case .i64: 8
        default: nil
        }
    }

    internal func admits(_ integer: Int64) -> Bool {
        switch self {
        case .i16: integer >= Int16.min && integer <= Int16.max
        case .i32: integer >= Int32.min && integer <= Int32.max
        case .i64: true
        default: false
        }
    }
}

public struct TypeField: Sendable, Equatable {
    public let name: SQLText
    public let descriptor: TypeDescriptor
    public let notNull: Bool
    public init(name: SQLText, descriptor: TypeDescriptor, notNull: Bool = false) {
        self.name = name
        self.descriptor = descriptor
        self.notNull = notNull
    }
}

/// Nominal identity is scoped to a database catalog, not the field list.
public struct CompositeType: Sendable, Equatable {
    public let catalogID: UInt64
    public let id: UInt32
    public let name: SQLText
    public let fields: [TypeField]
    public init(catalogID: UInt64, id: UInt32, name: SQLText, fields: [TypeField]) {
        self.catalogID = catalogID
        self.id = id
        self.name = name
        self.fields = fields
    }
    public static func == (lhs: CompositeType, rhs: CompositeType) -> Bool {
        lhs.catalogID == rhs.catalogID && lhs.id == rhs.id
    }
}

public indirect enum SQLType: Sendable, Equatable {
    case scalar(ScalarType)
    case composite(CompositeType)
    case array(SQLType)
    case range(SQLType)
    case record([TypeField])

    public var name: String {
        switch self {
        case .scalar(let type): type.name
        case .composite(let type): type.name.description
        case .array(let element): element.name + "[]"
        case .record: "record"
        case .range(let element):
            switch element {
            case .scalar(.i32): "i32range"
            case .scalar(.i64): "i64range"
            case .scalar(.decimal): "numrange"
            case .scalar(.timestamp): "tsrange"
            case .scalar(.timestamptz): "tstzrange"
            case .scalar(.date): "daterange"
            default: "range<\(element.name)>"
            }
        }
    }
}

public struct DecimalTypmod: Sendable, Equatable {
    public let precision: Int
    public let scale: Int
    public init(precision: Int, scale: Int) throws {
        guard (1...1000).contains(precision), (0...precision).contains(scale) else {
            throw JedError("22023", "invalid decimal precision or scale")
        }
        self.precision = precision
        self.scale = scale
    }
}

public struct TemporalTypmod: Sendable, Equatable {
    public let fields: UInt8
    public let precision: UInt8?
    public init(fields: UInt8 = 0, precision: UInt8? = nil) throws {
        guard fields <= 13, precision == nil || precision! <= 6 else {
            throw JedError("22023", "invalid temporal type modifier")
        }
        self.fields = fields
        self.precision = precision
    }
}

public struct TypeDescriptor: Sendable, Equatable {
    public let type: SQLType
    public var decimal: DecimalTypmod?
    public var textLimit: Int?
    public var temporal: TemporalTypmod?
    public var collation: SQLText?
    public init(_ type: SQLType) { self.type = type }
}
