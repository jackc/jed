/// Schema facts used only by the packed table leaf. Index keys never elide
/// columns or use a fixed stride; scalar codecs keep their independent forms.
internal struct LeafLayout: Sendable, Equatable {
    var fixedKeyLength: Int? = nil
    var keyOffsets: [Int: Int] = [:]
    static let index = LeafLayout()
    static func table(types: [TypeDescriptor], primaryKey: [Int]) -> LeafLayout {
        var result = LeafLayout()
        if primaryKey.isEmpty { result.fixedKeyLength = 8; return result }
        var offset = 0
        for column in primaryKey {
            guard types.indices.contains(column) else { return result }
            switch types[column].type {
            case .scalar(.i16), .scalar(.i32), .scalar(.i64), .scalar(.date),
                 .scalar(.timestamp), .scalar(.timestamptz), .scalar(.boolean), .scalar(.uuid):
                result.keyOffsets[column] = offset
                offset += ValueCodec.fixedWidth(types[column].type)!
            default: break
            }
            if result.keyOffsets[column] == nil { break }
        }
        var width = 0
        for column in primaryKey {
            guard let size = ValueCodec.fixedWidth(types[column].type) else { return result }
            // Interval's order key is an i128, like its stored body size.
            width += size
        }
        if width <= 65535 { result.fixedKeyLength = width }
        return result
    }
}

internal enum LeafValue {
    static func width(_ type: TypeDescriptor) -> Int? {
        if type.type == .scalar(.decimal), let mod = type.decimal, mod.precision <= 18 {
            return mod.precision <= 4 ? 2 : mod.precision <= 9 ? 4 : 8
        }
        return ValueCodec.fixedWidth(type.type)
    }
    static func bare(_ type: SQLType) -> Bool {
        type == .scalar(.text) || type == .scalar(.bytea) || type == .scalar(.json)
    }
    static func integer(_ type: TypeDescriptor) -> Bool {
        switch type.type {
        case .scalar(.i16), .scalar(.i32), .scalar(.i64), .scalar(.date), .scalar(.timestamp), .scalar(.timestamptz): true
        case .scalar(.decimal): width(type) != nil
        default: false
        }
    }
    static func encode(_ value: SQLValue, type: TypeDescriptor) throws -> [UInt8] {
        if type.type == .scalar(.decimal), let width = width(type), let mod = type.decimal {
            guard case .decimal(let value) = value else { throw JedError("42804", "expected decimal slot") }
            let scaled = value.rescaled(to: mod.scale, round: true)
            guard let magnitude = UInt64(scaled.magnitude.description), magnitude <= UInt64(Int64.max) else {
                throw JedError("22003", "decimal slot overflow")
            }
            let integer = scaled.negative ? -Int64(magnitude) : Int64(magnitude)
            let scalar: ScalarType = width == 2 ? .i16 : width == 4 ? .i32 : .i64
            return try ValueCodec.encodeBody(.integer(integer), type: TypeDescriptor(.scalar(scalar)))
        }
        return try ValueCodec.encodeRawBody(value, type: type)
    }
    static func decode(_ bytes: ArraySlice<UInt8>, type: TypeDescriptor) throws -> SQLValue {
        if type.type == .scalar(.decimal), let width = width(type), let mod = type.decimal {
            let scalar: ScalarType = width == 2 ? .i16 : width == 4 ? .i32 : .i64
            guard case .integer(let integer) = try ValueCodec.decodeBody(bytes, type: TypeDescriptor(.scalar(scalar))) else {
                throw JedError.corruption("invalid decimal slot")
            }
            return .decimal(SQLDecimal(negative: integer < 0, magnitude: BigMagnitude(integer.magnitude), scale: mod.scale))
        }
        return try ValueCodec.decodeBody(bytes, type: type)
    }
}
