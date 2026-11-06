/// Host binding is unmetered admission, before touching any row. It has no
/// statement clock: text input follows type input, never special date words.
internal enum ParameterBinding {
    static func finalize(_ resolver: ExpressionResolver) throws -> [TypeDescriptor] {
        let count = resolver.parameterUses.max() ?? 0
        if count == 0 { return [] }
        return try (1...count).map { index in
            guard let type = resolver.parameters[index] else {
                throw JedError("42P18", "could not determine data type of parameter $\(index)")
            }
            return type
        }
    }
    static func bind(_ supplied: [SQLValue], types: [TypeDescriptor], timezone: SQLTimeZone) throws -> [SQLValue] {
        guard supplied.count == types.count else {
            throw JedError("42601", "bind parameter count mismatch: statement expects \(types.count), got \(supplied.count)")
        }
        return try zip(supplied, types).map { try value($0, to: $1, context: .init(timezone: timezone), depth: 0) }
    }
    private static func value(_ value: SQLValue, to type: TypeDescriptor, context: ScalarCasts.Context, depth: Int) throws -> SQLValue {
        guard depth <= 64 else { throw JedError("54001", "container parameter nesting limit exceeded") }
        if value.isNull { return .null }
        if case .text(let text) = value, type.type != .scalar(.text) {
            if case .scalar(let target) = type.type,
               ![.bytea, .uuid, .date, .timestamp, .timestamptz, .interval, .f32, .f64, .json, .jsonb, .jsonpath].contains(target) {
                throw JedError("42804", "text parameter does not match type \(target.name)")
            }
            return try ContainerText.input(text, to: type, explicit: false, context: context)
        }
        switch type.type {
        case .scalar(let target):
            let source: ScalarType
            switch value {
            case .integer: source = .i64
            case .text: source = .text
            case .boolean: source = .boolean
            case .decimal: source = .decimal
            case .bytea: source = .bytea
            case .uuid: source = .uuid
            case .date: source = .date
            case .timestamp: source = .timestamp
            case .timestamptz: source = .timestamptz
            case .interval: source = .interval
            case .f32: source = .f32
            case .f64: source = .f64
            case .json: source = .json
            case .jsonb: source = .jsonb
            case .jsonpath: source = .jsonpath
            default: throw JedError("42804", "parameter does not match type \(target.name)")
            }
            // Host admission is the value-storage contract, not the explicit
            // cast lattice. In particular decimal/text do not bind as integers,
            // while a supplied exact number may adapt to a floating parameter.
            if source == target { return try ContainerCasts.store(value, to: type, context: context) }
            if case .integer(let number) = value {
                if target.integerWidth != nil { return .integer(try ScalarCasts.checkedInteger(number, type: target)) }
                if target == .decimal { return try ScalarCasts.applyTypmod(.decimal(SQLDecimal(number)), descriptor: type, explicit: false) }
                if target == .f32 || target == .f64 { return try FloatText.fromDecimal(SQLDecimal(number), type: target) }
            }
            if case .decimal(let number) = value, target == .f32 || target == .f64 {
                return try FloatText.fromDecimal(number, type: target)
            }
            if source == .f32 && target == .f64 || source == .jsonb && target == .json {
                return try ScalarCasts.cast(value, from: source, to: type, mode: .assignment, context: context)
            }
            throw JedError("42804", "parameter does not match type \(target.name)")
        case .array(let element):
            guard case .array(let array) = value else { throw JedError("42804", "expected array parameter") }
            let elementType = ContainerText.elementDescriptor(type, element: element)
            let items = try array.elements.map { try self.value($0, to: elementType, context: context, depth: depth + 1) }
            return .array(try SQLArray(dimensions: array.dimensions, lowerBounds: array.lowerBounds, elements: items))
        case .composite(let composite):
            guard case .composite(let fields) = value, fields.count == composite.fields.count else {
                throw JedError("42804", "parameter does not match composite field count")
            }
            return .composite(try zip(fields, composite.fields).map { value, field in
                if value.isNull && field.notNull { throw JedError("23502", "null value in composite field \(field.name)") }
                return try self.value(value, to: field.descriptor, context: context, depth: depth + 1)
            })
        case .range(let element):
            guard case .range(let range) = value else { throw JedError("42804", "expected range parameter") }
            let lower = try range.lower.map { try self.value($0, to: TypeDescriptor(element), context: context, depth: depth + 1) }
            let upper = try range.upper.map { try self.value($0, to: TypeDescriptor(element), context: context, depth: depth + 1) }
            return .range(try RangeFunctions.canonicalize(SQLRange(empty: range.isEmpty, lower: lower, upper: upper,
                lowerInclusive: range.lowerInclusive, upperInclusive: range.upperInclusive), element: element))
        case .record: throw JedError("42P18", "anonymous record cannot type a parameter")
        }
    }
}
