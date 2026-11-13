extension ExpressionResolver {
    func arrayConstructor(_ fields: [SQLExpression], expected: TypeDescriptor?, position: Int,
                          operands: [Operand]? = nil) throws -> ResolvedExpression {
        func child(_ index: Int, expected: TypeDescriptor? = nil) throws -> ResolvedExpression {
            if let operands { return try operands[index].resolve(expected: expected) }
            return try resolve(fields[index], expected: expected)
        }
        let hint: TypeDescriptor?
        if case .array(let element) = expected?.type { hint = TypeDescriptor(element) } else { hint = nil }
        var children = try fields.indices.map { index in
            if case .array = fields[index].kind { return try child(index, expected: expected) }
            if case .null = fields[index].kind { return try child(index) }
            return try child(index, expected: hint)
        }
        var inferred: SQLType?
        for child in children { inferred = try ResolutionDiagnostics.common(inferred, child) }
        let common = inferred ?? hint?.type ?? .scalar(.text)
        let nested: Bool, element: SQLType
        if case .array(let subtype) = common { nested = true; element = subtype }
        else { nested = false; element = common }
        // Expression arrays may contain transient records/ranges. The stored
        // type descriptor has its own narrower element admission contract.
        for index in children.indices {
            let descriptor = TypeDescriptor(common)
            // Unknown parameters stay unresolved until an actual contextual
            // hint reaches them; the all-NULL text fallback is not a bind hint.
            if let sourceType = children[index].type, sourceType.type != common {
                let source = children[index]
                children[index] = ResolvedExpression(type: descriptor, node: .convert(source, descriptor), position: source.position)
            }
        }
        return ResolvedExpression(type: TypeDescriptor(.array(element)), node: .array(children, nested: nested), position: position)
    }

    func containerOperation(_ op: String, _ lhs: Operand, _ rhs: Operand, position: Int) throws -> ResolvedExpression? {
        guard ["||", "@>", "<@", "&&", "<<", ">>", "&<", "&>", "-|-", "+", "-", "*"].contains(op) else { return nil }
        let a = try lhs.resolve(), b = try rhs.resolve()
        return try finishContainerOperation(op, lhs, rhs, left: a, right: b, position: position)
    }

    // Resolve children before allocating the larger range/array dispatch frame.
    // Otherwise every arithmetic operand keeps that unrelated scratch frame on
    // the stack while descending to the shared expression-depth boundary.
    private func finishContainerOperation(_ op: String, _ lhs: Operand, _ rhs: Operand,
                                          left a: ResolvedExpression, right b: ResolvedExpression,
                                          position: Int) throws -> ResolvedExpression? {
        let left = lhs.syntax, right = rhs.syntax
        var a = a, b = b
        func rangeElement(_ type: SQLType?) -> SQLType? { if case .range(let element) = type { element } else { nil } }
        if let element = rangeElement(a.type?.type) ?? rangeElement(b.type?.type) {
            let ranges = ["&&", "<<", ">>", "&<", "&>", "-|-", "+", "-", "*"]
            guard ranges.contains(op) || op == "@>" || op == "<@" else { return nil }
            let range = TypeDescriptor(.range(element))
            if a.type == nil || FunctionResolver.bareString(left) { a = try lhs.resolve(expected: range) }
            if b.type == nil || FunctionResolver.bareString(right) { b = try rhs.resolve(expected: range) }
            if a.type?.type != range.type || b.type?.type != range.type {
                if op == "@>", a.type?.type == range.type {
                    b = try rhs.resolve(expected: TypeDescriptor(element))
                    guard b.type == nil || TypeRules.admitsCast(from: b.type!.type, to: element, mode: .assignment) else { throw JedError("42883", "range element has incompatible type") }
                } else if op == "<@", b.type?.type == range.type {
                    a = try lhs.resolve(expected: TypeDescriptor(element))
                    guard a.type == nil || TypeRules.admitsCast(from: a.type!.type, to: element, mode: .assignment) else { throw JedError("42883", "range element has incompatible type") }
                } else { throw JedError("42883", "range operands have incompatible types") }
            }
            let result = ["+", "-", "*"].contains(op) ? range : TypeDescriptor(.scalar(.boolean))
            return ResolvedExpression(type: result, node: .rangeOperation(op, a, b, element), position: position)
        }
        if case .array = a.type?.type, FunctionResolver.bareString(right) { b = try rhs.resolve(expected: a.type) }
        if case .array = b.type?.type, FunctionResolver.bareString(left) { a = try lhs.resolve(expected: b.type) }
        let leftArray: Bool = { if case .array = a.type?.type { true } else { false } }()
        let rightArray: Bool = { if case .array = b.type?.type { true } else { false } }()
        if a.type == nil && b.type == nil && ["@>", "<@", "&&"].contains(op) {
            return ResolvedExpression(type: TypeDescriptor(.scalar(.boolean)), node: .binary(op, a, b, TypeDescriptor(.scalar(.boolean))), position: position)
        }
        guard leftArray || rightArray else { return nil }
        if op == "||" {
            let name = leftArray && (rightArray || b.type == nil) || rightArray && a.type == nil ? "array_cat" : leftArray ? "array_append" : "array_prepend"
            let call = SQLCall(name: SQLText(validatedUTF8: Array(name.utf8)), arguments: [SQLArgument(value: left), SQLArgument(value: right)])
            guard let resolved = try ContainerFunctionResolver.resolve(call, originals: [a, b],
                resolve: callArguments(call, operands: [lhs, rhs])) else { throw JedError("XX000", "array concatenation catalog entry missing") }
            return ResolvedExpression(type: resolved.resultType, node: .call(resolved), position: position,
                collation: try ExpressionCollation.combine(resolved.arguments, storage: storageCollation))
        }
        if ["@>", "<@", "&&"].contains(op) {
            let descriptor = leftArray ? a.type! : b.type!
            a = try lhs.resolve(expected: descriptor)
            b = try rhs.resolve(expected: descriptor)
            guard a.type?.type == b.type?.type else { throw JedError("42883", "array operands have incompatible types") }
            let row = FunctionSpecFact(category: "array", name: op, kind: "containment", metadata: ["result": .text("boolean"), "null": .text("propagates")])
            let function = ResolvedFunction(metadata: row, arguments: [a, b], resultType: TypeDescriptor(.scalar(.boolean)), evaluationOrder: [0, 1], constantArguments: [a, b].map(FunctionResolver.isConstant))
            return ResolvedExpression(type: function.resultType, node: .call(function), position: position)
        }
        return nil
    }
}
