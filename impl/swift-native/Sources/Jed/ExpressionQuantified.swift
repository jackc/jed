extension ExpressionResolver {
    func quantifiedExpression(_ operation: String, _ operand: SQLExpression, all: Bool,
                              source: SQLExpression, position: Int) throws -> ResolvedExpression {
        let lhs = Operand(operand, resolver: self), rhs = Operand(source, resolver: self)
        var value = try lhs.resolve()
        func constructor(_ syntax: SQLExpression) -> Bool {
            switch syntax.kind {
            case .array: true
            case .parenthesized(let child): constructor(child)
            default: false
            }
        }
        let hint: TypeDescriptor?
        switch value.type?.type {
        case .scalar(let scalar): hint = TypeDescriptor(.array(.scalar(scalar)))
        case .composite(let composite): hint = TypeDescriptor(.array(.composite(composite)))
        default: hint = nil
        }
        let scalarHint: TypeDescriptor?
        if case .scalar = value.type?.type { scalarHint = hint } else { scalarHint = nil }
        var array: ResolvedExpression
        do { array = try rhs.resolve() }
        catch let error as JedError where error.code == "42P18" && constructor(source) && !adaptable(operand) && scalarHint != nil {
            // An empty/parameter-only constructor has no first-pass element
            // type. A concrete scalar sibling supplies its declared context.
            array = try rhs.resolve(expected: scalarHint)
        }
        if FunctionResolver.bareParameter(source), let hint {
            array = try rhs.resolve(expected: hint)
        } else if constructor(source), !adaptable(operand), let hint = scalarHint {
            array = try rhs.resolve(expected: hint)
        }
        guard array.type != nil else { throw JedError("42P18", "could not determine ANY/ALL array type") }
        guard case .array(let element) = array.type?.type else { throw JedError("42809", "ANY/ALL requires an array") }
        if adaptable(operand), case .scalar = element { value = try lhs.resolve(expected: TypeDescriptor(element)) }
        do { try TypeRules.comparable(value.type?.type, element) }
        catch { throw JedError("42883", "ANY/ALL comparison operator does not exist") }
        let compared = try TypeRules.common(value.type?.type, element) ?? element
        return ResolvedExpression(type: TypeDescriptor(.scalar(.boolean)),
            node: .quantified(operation, all: all, value, array, TypeDescriptor(compared)), position: position)
    }
}
