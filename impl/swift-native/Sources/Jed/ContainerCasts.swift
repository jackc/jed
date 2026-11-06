/// Open-type casts and store validation are derived recursively. The resolver
/// keeps ROW-constructor coercion separate from casting a dynamic record value.
internal enum ContainerCasts {
    static func cast(_ value: SQLValue, from source: TypeDescriptor, to target: TypeDescriptor,
                     mode: TypeRules.CastMode = .explicit, context: ScalarCasts.Context = .init(),
                     growth: Bool = true) throws -> SQLValue {
        if case .scalar(let scalar) = source.type, case .scalar = target.type {
            return try ScalarCasts.cast(value, from: scalar, to: target, mode: mode, context: context)
        }
        if source.type == target.type { return try store(value, to: target, explicit: mode == .explicit, context: context) }
        if case .range = source.type { throw JedError("0A000", "runtime casts from ranges are not supported") }
        if case .range = target.type { throw JedError("0A000", "runtime casts to ranges are not supported") }
        if case .scalar(.text) = target.type,
           TypeRules.fields(source.type) != nil || isArray(source.type) {
            guard mode == .explicit else { throw cannotCast(source.type, target.type) }
            if value.isNull { return .null }
            let text = try ContainerText.render(value, type: source, context: context, growth: growth)
            return try ScalarCasts.applyTypmod(.text(text), descriptor: target, explicit: true)
        }
        if source.type == .scalar(.text), TypeRules.fields(target.type) != nil || isArray(target.type) {
            guard mode == .explicit else { throw cannotCast(source.type, target.type) }
            if value.isNull { return .null }
            guard case .text(let text) = value else { throw cannotCast(source.type, target.type) }
            return try ContainerText.input(text, to: target, context: context)
        }
        if case .array(let oldElement) = source.type, case .array(let newElement) = target.type {
            guard case .scalar(let old) = oldElement, case .scalar(let new) = newElement,
                  ![.json, .jsonb, .jsonpath].contains(old), ![.json, .jsonb, .jsonpath].contains(new) else {
                throw JedError("0A000", "casts between arrays of these element types are not supported")
            }
            guard mode == .explicit, TypeRules.admitsCast(from: oldElement, to: newElement, mode: mode) else {
                throw cannotCast(source.type, target.type)
            }
            if value.isNull { return .null }
            guard case .array(let array) = value else { throw cannotCast(source.type, target.type) }
            let element = ContainerText.elementDescriptor(target, element: newElement)
            var values: [SQLValue] = []
            for item in array.elements {
                if old == .bytea && new == .text && !item.isNull {
                    // This cast is one array growth kernel, not one bytea render
                    // charge per element plus a second charge for the array.
                    values.append(try ScalarCasts.applyTypmod(.text(ScalarCasts.render(item, context: context)), descriptor: element, explicit: true))
                } else { values.append(try ScalarCasts.cast(item, from: old, to: element, context: context)) }
            }
            let output = SQLValue.array(try SQLArray(dimensions: array.dimensions, lowerBounds: array.lowerBounds, elements: values))
            if growth && old == .bytea && new == .text { context.meter?.charge(MemorySizes.payload(output)); try context.meter?.guardCost() }
            return output
        }
        throw cannotCast(source.type, target.type)
    }

    private static func isArray(_ type: SQLType) -> Bool { if case .array = type { return true }; return false }
    private static func cannotCast(_ source: SQLType, _ target: SQLType) -> JedError {
        JedError(TypeRules.fields(source) != nil || TypeRules.fields(target) != nil ? "42846" : "42804",
                 "cannot cast type \(source.name) to \(target.name)")
    }

    /// A ROW constructor reaches this seam only after field-by-field resolution
    /// has admitted the assignment/explicit conversions. It is not a runtime
    /// record-to-nominal cast, which remains rejected by cast(_:from:to:).
    static func row(_ values: [SQLValue], from fields: [TypeDescriptor], to target: TypeDescriptor,
                    mode: TypeRules.CastMode = .assignment, context: ScalarCasts.Context = .init()) throws -> SQLValue {
        guard let targetFields = TypeRules.fields(target.type), values.count == targetFields.count, fields.count == values.count else {
            throw JedError("42846", "cannot cast type record to \(target.type.name)")
        }
        var output: [SQLValue] = []
        for index in values.indices {
            let value = values[index], source = fields[index], destination = targetFields[index].descriptor
            if value.isNull { output.append(.null); continue }
            do { output.append(try cast(value, from: source, to: destination, mode: mode, context: context)) }
            catch let error as JedError where error.code == "42804" || error.code == "0A000" {
                throw JedError("42846", "cannot cast type record to \(target.type.name)")
            }
        }
        return .composite(output)
    }

    /// Same-type assignment/binding still checks every scalar domain and field
    /// modifier. Defaults to assignment semantics: varchar excess is not silently
    /// truncated. A literal text source belongs in ContainerText.input instead.
    static func store(_ value: SQLValue, to target: TypeDescriptor, explicit: Bool = false,
                      context: ScalarCasts.Context = .init()) throws -> SQLValue {
        try store(value, to: target, explicit: explicit, context: context, depth: 0)
    }
    private static func store(_ value: SQLValue, to target: TypeDescriptor, explicit: Bool,
                              context: ScalarCasts.Context, depth: Int) throws -> SQLValue {
        guard depth <= 64 else { throw JedError("54001", "container type nesting limit exceeded") }
        if value.isNull { return .null }
        switch target.type {
        case .scalar(let scalar):
            let matches: Bool
            switch (scalar, value) {
            case (.i16, .integer), (.i32, .integer), (.i64, .integer), (.boolean, .boolean),
                 (.text, .text), (.decimal, .decimal), (.bytea, .bytea), (.uuid, .uuid),
                 (.date, .date), (.timestamp, .timestamp), (.timestamptz, .timestamptz),
                 (.interval, .interval), (.f32, .f32), (.f64, .f64),
                 (.json, .json), (.jsonb, .jsonb), (.jsonpath, .jsonpath): matches = true
            default: matches = false
            }
            guard matches else { throw JedError("42804", "value does not match its static type \(scalar.name)") }
            if case .integer(let number) = value, scalar.integerWidth != nil {
                return .integer(try ScalarCasts.checkedInteger(number, type: scalar))
            }
            return try ScalarCasts.applyTypmod(value, descriptor: target, explicit: explicit)
        case .composite, .record:
            guard case .composite(let values) = value, let fields = TypeRules.fields(target.type), fields.count == values.count else {
                throw JedError("42846", "composite value does not match \(target.type.name)")
            }
            var output: [SQLValue] = []
            for (value, field) in zip(values, fields) {
                if value.isNull && field.notNull { throw JedError("23502", "null value in composite field \(field.name)") }
                output.append(try store(value, to: field.descriptor, explicit: explicit, context: context, depth: depth + 1))
            }
            return .composite(output)
        case .array(let element):
            guard case .array(let array) = value else { throw JedError("42804", "expected array value") }
            let descriptor = ContainerText.elementDescriptor(target, element: element)
            var output: [SQLValue] = []
            for value in array.elements { output.append(try store(value, to: descriptor, explicit: explicit, context: context, depth: depth + 1)) }
            return .array(try SQLArray(dimensions: array.dimensions, lowerBounds: array.lowerBounds, elements: output))
        case .range(let element):
            guard case .range(let range) = value else { throw JedError("42804", "expected range value") }
            let descriptor = TypeDescriptor(element)
            let lower = try range.lower.map { try store($0, to: descriptor, explicit: explicit, context: context, depth: depth + 1) }
            let upper = try range.upper.map { try store($0, to: descriptor, explicit: explicit, context: context, depth: depth + 1) }
            return .range(try RangeFunctions.canonicalize(SQLRange(empty: range.isEmpty, lower: lower, upper: upper,
                                                                   lowerInclusive: range.lowerInclusive, upperInclusive: range.upperInclusive), element: element))
        }
    }
}
