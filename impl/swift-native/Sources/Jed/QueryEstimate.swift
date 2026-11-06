/// Exact, saturating planner arithmetic. Mechanical fractions and tie ranks
/// come from the frozen language-neutral estimator data, never host floats.
internal enum QueryEstimate {
    static func multiply(_ a: Int64, _ b: Int64) -> Int64 {
        guard a > 0, b > 0 else { return 0 }
        return a > Int64.max / b ? Int64.max : a * b
    }
    static func scale(_ n: Int64, numerator p: Int64, denominator q: Int64) -> Int64 {
        guard n > 0, p > 0 else { return 0 }
        let whole = multiply(n / q, p), tail = multiply(n % q, p)
        return saturatingAdd(whole, tail / q + (tail % q == 0 ? 0 : 1))
    }
    static func fraction(_ name: String, rows: Int64) -> Int64 {
        guard case .array(let entries) = GeneratedSpec.estimator["selectivity"] else { preconditionFailure("missing estimator data") }
        for case .table(let entry) in entries {
            if case .text(let id) = entry["id"], id == name, case .integer(let p) = entry["numerator"], case .integer(let q) = entry["denominator"] {
                return scale(rows, numerator: p, denominator: q)
            }
        }
        preconditionFailure("unknown estimator fraction")
    }
    static func expression(_ expression: ResolvedExpression?) -> Int64 {
        guard let expression else { return 0 }
        let own: Int64
        switch expression.node {
        case .constant, .column, .outerColumn, .parameter, .convert: own = 0
        case .call(let call): own = call.cost
        default: own = 1
        }
        return StoredExpressionDependencies.children(expression).reduce(own) { saturatingAdd($0, self.expression($1)) }
    }
    static func rows(_ predicate: ResolvedExpression?, input: Int64) -> Int64 {
        guard input > 0, let predicate else { return input }
        switch predicate.node {
        case .constant(.boolean(let value)): return value ? input : 0
        case .constant(.null): return 0
        case .binary("and", let a, let b, _): return rows(b, input: rows(a, input: input))
        case .binary("or", let a, let b, _): return min(input, saturatingAdd(rows(a, input: input), rows(b, input: input)))
        case .unary("not", let value): return input - rows(value, input: input)
        case .binary(let operation, let a, let b, _):
            if case .constant(.null) = a.node { return 0 }
            if case .constant(.null) = b.node { return 0 }
            if operation == "=" { return fraction("equality", rows: input) }
            if operation == "<>" { return input - fraction("equality", rows: input) }
            if ["<", "<=", ">", ">="].contains(operation) { return fraction("inequality", rows: input) }
            return fraction("opaque", rows: input)
        case .isNull(_, let negated):
            let count = fraction("null_test", rows: input); return negated ? input - count : count
        case .column: return fraction("boolean", rows: input)
        default: return fraction("opaque", rows: input)
        }
    }
    static func conjuncts(_ expression: ResolvedExpression?) -> [ResolvedExpression] {
        guard let expression else { return [] }
        var stack = [expression], result: [ResolvedExpression] = []
        while let value = stack.popLast() {
            if case .binary("and", let a, let b, _) = value.node { stack.append(b); stack.append(a) }
            else { result.append(value) }
        }
        return result
    }
    static func and(_ expressions: [ResolvedExpression]) -> ResolvedExpression? {
        guard var result = expressions.first else { return nil }
        for expression in expressions.dropFirst() {
            result = ResolvedExpression(type: TypeDescriptor(.scalar(.boolean)), node: .binary("and", result, expression, TypeDescriptor(.scalar(.boolean))), position: expression.position)
        }
        return result
    }
}
