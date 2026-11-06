extension ExpressionResolver {
    func arrayConstructor(_ fields: [SQLExpression], expected: TypeDescriptor?, position: Int) throws -> ResolvedExpression {
        let hint: TypeDescriptor?
        if case .array(let element) = expected?.type { hint = TypeDescriptor(element) } else { hint = nil }
        var children = try fields.map { syntax in
            if case .array = syntax.kind { return try resolve(syntax, expected: expected) }
            if case .null = syntax.kind { return try resolve(syntax) }
            return try resolve(syntax, expected: hint)
        }
        var common: SQLType?
        for child in children { common = try TypeRules.common(common, child.type?.type) }
        if common == nil { common = hint?.type }
        guard let common else { throw JedError("42P18", "could not determine array element type") }
        let nested: Bool, element: SQLType
        if case .array(let subtype) = common { nested = true; element = subtype }
        else { nested = false; element = common }
        // Expression arrays may contain transient records/ranges. The stored
        // type descriptor has its own narrower element admission contract.
        for index in children.indices {
            let descriptor = TypeDescriptor(common)
            if children[index].type == nil { children[index] = try resolve(fields[index], expected: descriptor) }
            else if children[index].type?.type != common {
                let source = children[index]
                children[index] = ResolvedExpression(type: descriptor, node: .convert(source, descriptor), position: source.position)
            }
        }
        return ResolvedExpression(type: TypeDescriptor(.array(element)), node: .array(children, nested: nested), position: position)
    }

    func containerOperation(_ op: String, _ left: SQLExpression, _ right: SQLExpression, position: Int) throws -> ResolvedExpression? {
        guard ["||", "@>", "<@", "&&", "<<", ">>", "&<", "&>", "-|-", "+", "-", "*"].contains(op) else { return nil }
        var a = try resolve(left), b = try resolve(right)
        func rangeElement(_ type: SQLType?) -> SQLType? { if case .range(let element) = type { element } else { nil } }
        if let element = rangeElement(a.type?.type) ?? rangeElement(b.type?.type) {
            let ranges = ["&&", "<<", ">>", "&<", "&>", "-|-", "+", "-", "*"]
            guard ranges.contains(op) || op == "@>" || op == "<@" else { return nil }
            let range = TypeDescriptor(.range(element))
            if a.type == nil { a = try resolve(left, expected: range) }
            if b.type == nil { b = try resolve(right, expected: range) }
            if a.type?.type != range.type || b.type?.type != range.type {
                if op == "@>", a.type?.type == range.type {
                    b = try resolve(right, expected: TypeDescriptor(element))
                    guard b.type == nil || TypeRules.admitsCast(from: b.type!.type, to: element, mode: .assignment) else { throw JedError("42883", "range element has incompatible type") }
                } else if op == "<@", b.type?.type == range.type {
                    a = try resolve(left, expected: TypeDescriptor(element))
                    guard a.type == nil || TypeRules.admitsCast(from: a.type!.type, to: element, mode: .assignment) else { throw JedError("42883", "range element has incompatible type") }
                } else { throw JedError("42883", "range operands have incompatible types") }
            }
            let result = ["+", "-", "*"].contains(op) ? range : TypeDescriptor(.scalar(.boolean))
            return ResolvedExpression(type: result, node: .rangeOperation(op, a, b, element), position: position)
        }
        let leftArray: Bool = { if case .array = a.type?.type { true } else { false } }()
        let rightArray: Bool = { if case .array = b.type?.type { true } else { false } }()
        if a.type == nil && b.type == nil && ["@>", "<@", "&&"].contains(op) {
            return ResolvedExpression(type: TypeDescriptor(.scalar(.boolean)), node: .binary(op, a, b, TypeDescriptor(.scalar(.boolean))), position: position)
        }
        guard leftArray || rightArray else { return nil }
        if op == "||" {
            let name = leftArray && (rightArray || b.type == nil) || rightArray && a.type == nil ? "array_cat" : leftArray ? "array_append" : "array_prepend"
            let call = SQLCall(name: SQLText(validatedUTF8: Array(name.utf8)), arguments: [SQLArgument(value: left), SQLArgument(value: right)])
            guard let resolved = try ContainerFunctionResolver.resolve(call, resolve: { try self.resolve($0, expected: $1) }) else { throw JedError("XX000", "array concatenation catalog entry missing") }
            return ResolvedExpression(type: resolved.resultType, node: .call(resolved), position: position)
        }
        if ["@>", "<@", "&&"].contains(op) {
            let descriptor = leftArray ? a.type! : b.type!
            a = try resolve(left, expected: descriptor)
            b = try resolve(right, expected: descriptor)
            guard a.type?.type == b.type?.type else { throw JedError("42883", "array operands have incompatible types") }
            let row = FunctionSpecFact(category: "array", name: op, kind: "containment", metadata: ["result": .text("boolean"), "null": .text("propagates")])
            let function = ResolvedFunction(metadata: row, arguments: [a, b], resultType: TypeDescriptor(.scalar(.boolean)), evaluationOrder: [0, 1], constantArguments: [a, b].map(FunctionResolver.isConstant))
            return ResolvedExpression(type: function.resultType, node: .call(function), position: position)
        }
        return nil
    }
}
