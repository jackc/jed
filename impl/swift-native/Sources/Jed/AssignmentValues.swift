/// Assignment uses the strict storage relation, independently of the broader
/// explicit-cast catalog. Literal adaptation happens before that relation.
internal enum AssignmentValues {
    static func admits(_ source: SQLType?, to target: SQLType) -> Bool {
        guard let source else { return true }
        if source == target { return true }
        switch (source, target) {
        case (.scalar(let a), .scalar(let b)):
            return a.integerWidth != nil && (b.integerWidth != nil || b == .decimal)
                || a == .f32 && b == .f64 || a == .jsonb && b == .json
        default: return false
        }
    }

    static func resolve(_ syntax: SQLExpression, to target: TypeDescriptor,
                        resolver: ExpressionResolver) throws -> ResolvedExpression {
        if case .parenthesized(let child) = syntax.kind { return try resolve(child, to: target, resolver: resolver) }
        if case .row(let children) = syntax.kind, case .composite(let composite) = target.type {
            guard children.count == composite.fields.count else { throw JedError("42846", "cannot cast type record to \(composite.name)").at(syntax.position) }
            do {
                let fields = try zip(children, composite.fields).map { try resolve($0, to: $1.descriptor, resolver: resolver) }
                let types = zip(fields, composite.fields).map { expression, field in
                    TypeField(name: field.name, descriptor: expression.type ?? field.descriptor)
                }
                return ResolvedExpression(type: TypeDescriptor(.record(types)), node: .row(fields), position: syntax.position)
            } catch let error as JedError where error.code == "42804" {
                throw JedError("42846", "cannot cast type record to \(composite.name)").at(syntax.position)
            }
        }
        if case .string(let text) = syntax.kind {
            switch target.type {
            case .composite, .array, .range:
                let value = try ContainerText.input(text, to: target, explicit: false, context: resolver.scalar)
                return ResolvedExpression(type: target, node: .constant(value), position: syntax.position)
            default: break
            }
        }
        // Modifiers are checked by store, not an explicit literal cast (which
        // would silently truncate a varchar before assignment can reject it).
        let expression = try resolver.resolve(syntax, expected: TypeDescriptor(target.type))
        guard admits(expression.type?.type, to: target.type) else {
            if case .record = expression.type?.type, case .composite = target.type {
                throw JedError("42846", "cannot cast type record to \(target.type.name)").at(syntax.position)
            }
            throw JedError("42804", "expression of type \(expression.type?.type.name ?? "unknown") cannot be assigned to \(target.type.name)").at(syntax.position)
        }
        return expression
    }

    static func store(_ value: SQLValue, expression: ResolvedExpression, to target: TypeDescriptor,
                      context: ExpressionContext) throws -> SQLValue {
        if value.isNull { return .null }
        if case .row(let children) = expression.node, case .composite(let values) = value,
           let fields = TypeRules.fields(target.type), fields.count == children.count, values.count == fields.count {
            return .composite(try fields.indices.map {
                if values[$0].isNull && fields[$0].notNull { throw JedError("23502", "null value in composite field \(fields[$0].name)") }
                return try store(values[$0], expression: children[$0], to: fields[$0].descriptor, context: context)
            })
        }
        guard let source = expression.type else { return .null }
        return try ContainerCasts.cast(value, from: source, to: target, mode: .assignment, context: context.scalar, growth: false)
    }

    /// INSERT's literal-field ROW/ARRAY constructors are the historical literal
    /// slot path: they incur no operator charge. General expressions still do.
    static func literalSlot(_ expression: ResolvedExpression) -> Bool {
        switch expression.node {
        case .constant, .parameter: return true
        case .row(let fields), .array(let fields, _): return fields.allSatisfy(literalSlot)
        default: return false
        }
    }
    static func evaluateSlot(_ expression: ResolvedExpression, context: ExpressionContext) throws -> SQLValue {
        if literalSlot(expression) {
            switch expression.node {
            case .row(let fields): return .composite(try fields.map { try evaluateSlot($0, context: context) })
            case .array(let fields, let nested):
                return try ArrayFunctions.construct(fields.map { try evaluateSlot($0, context: context) }, nested: nested, constant: true)
            default: break
            }
        }
        return try context.evaluate(expression)
    }
}
