/// An aggregate over enclosing-query columns alone belongs to that enclosing
/// level in PostgreSQL. Jed deliberately rejects that shape instead of folding
/// it once per inner row. Direct ordered-set arguments are not fold inputs.
internal enum AggregateScope {
    static func rejectOuterOnly(_ expressions: [ResolvedExpression], position: Int) throws {
        var pending = expressions
        var outer = false
        while let expression = pending.popLast() {
            switch expression.node {
            case .column, .subquery: return
            case .outerColumn: outer = true
            default: break
            }
            pending.append(contentsOf: StoredExpressionDependencies.children(expression))
        }
        if outer {
            throw JedError("42803", "an aggregate whose arguments reference only outer-query columns is not supported").at(position)
        }
    }
}
