/// Lossy jsonb_ops extraction. Every returned tree is a necessary condition for
/// the original predicate; the executor must still recheck the complete path.
internal enum GinJSONPath {
    static func extract(_ program: JSONPathProgram, match: Bool) throws -> GinQueryTree? {
        if match {
            guard case .predicate(let predicate) = program.expression else { return nil }
            return try predicateTree(predicate, path: [], strict: program.strict, negated: false)
        }
        if case .predicate = program.expression { return nil }
        return try walk(program.expression, path: [], scalar: nil, strict: program.strict)
    }

    private static func predicateTree(_ predicate: JSONPathPredicate, path: [JSONPathStep],
                                      strict: Bool, negated: Bool) throws -> GinQueryTree? {
        switch predicate {
        case .and(let left, let right), .or(let left, let right):
            let a = try predicateTree(left, path: path, strict: strict, negated: negated)
            let b = try predicateTree(right, path: path, strict: strict, negated: negated)
            let conjunction: Bool
            if case .and = predicate { conjunction = !negated } else { conjunction = negated }
            if conjunction { return GinQueryTree.all([a, b].compactMap { $0 }).normalized() }
            guard let a, let b else { return nil }
            return GinQueryTree.any([a, b]).normalized()
        case .not(let child): return try predicateTree(child, path: path, strict: strict, negated: !negated)
        case .exists(let expression):
            return negated ? nil : try walk(expression, path: path, scalar: nil, strict: strict)
        case .compare(let operation, let left, let right):
            guard operation == "==", !negated else { return nil }
            if case .literal(let scalar) = left { return try walk(right, path: path, scalar: scalar, strict: strict) }
            if case .literal(let scalar) = right { return try walk(left, path: path, scalar: scalar, strict: strict) }
            return nil
        default: return nil
        }
    }

    private static func walk(_ expression: JSONPathExpression, path inherited: [JSONPathStep],
                             scalar: SQLJSONValue?, strict: Bool) throws -> GinQueryTree? {
        let base: JSONPathExpression, steps: [JSONPathStep]
        if case .path(let start, let accessors) = expression { base = start; steps = accessors }
        else { base = expression; steps = [] }
        var path: [JSONPathStep]
        switch base { case .root: path = []; case .current: path = inherited; default: return nil }
        var nodes: [GinQueryTree] = []
        for step in steps {
            switch step {
            case .member, .members, .any, .elements, .subscripts: path.append(step)
            case .filter(let predicate):
                if let node = try predicateTree(predicate, path: path, strict: strict, negated: false) { nodes.append(node) }
            case .method:
                // Keys accumulated before an unsupported step are insufficient:
                // only filters that already tested the prefix remain necessary.
                return GinQueryTree.all(nodes).normalized()
            }
        }
        if let scalar {
            for step in path.reversed() {
                if case .member(let key) = step { nodes.append(.term(GinTerms.jsonKey(key))) }
            }
            if case .string = scalar {
                let key = try GinQueryTree.term(GinTerms.jsonScalar(scalar, asKey: true))
                let value = try GinQueryTree.term(GinTerms.jsonScalar(scalar, asKey: false))
                if !strict || path.isEmpty { nodes.append(.any([key, value])) }
                else {
                    switch path.last {
                    case .elements?, .subscripts?: nodes.append(key)
                    case .any?: nodes.append(.any([key, value]))
                    default: nodes.append(value)
                    }
                }
            } else { nodes.append(.term(try GinTerms.jsonScalar(scalar, asKey: false))) }
        }
        return GinQueryTree.all(nodes).normalized()
    }
}
