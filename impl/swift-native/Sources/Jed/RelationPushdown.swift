/// Scan pushdown retains the expression's local column ordinals and leaves
/// nullable or correlated inputs behind their semantic fence.
internal enum RelationPushdown {
    static func whereFilter(_ predicate: ResolvedExpression?, into relation: RelationPlan) -> ResolvedExpression? {
        var leaves: [(plan: RelationPlan, offset: Int, nullable: Bool)] = []
        func visit(_ input: RelationPlan, offset: Int, nullable: Bool) {
            if case .join(let a, let b, let kind, _) = input.body {
                visit(a, offset: offset, nullable: nullable || kind == "right" || kind == "full")
                visit(b, offset: offset + a.columns.count, nullable: nullable || kind == "left" || kind == "full")
            } else { leaves.append((input, offset, nullable)) }
        }
        visit(relation, offset: 0, nullable: false)
        guard leaves.count > 1 else { return predicate }
        var residual: [ResolvedExpression] = []
        for conjunct in QueryEstimate.conjuncts(predicate) {
            var columns: Set<Int> = []
            StoredExpressionDependencies.walk([conjunct]) { if case .column(let index) = $0.node { columns.insert(index) } }
            guard !columns.isEmpty, safe(conjunct), let leaf = leaves.first(where: { item in
                !item.nullable && item.plan.lateralColumns.isEmpty && columns.allSatisfy { $0 >= item.offset && $0 < item.offset + item.plan.columns.count }
            }), case .base = leaf.plan.body else { residual.append(conjunct); continue }
            let mapped = map(conjunct) { $0 - leaf.offset }
            leaf.plan.scanFilter = QueryEstimate.and(QueryEstimate.conjuncts(leaf.plan.scanFilter) + [mapped])
        }
        return QueryEstimate.and(residual)
    }
    private static func value(_ expression: ResolvedExpression) -> Bool {
        switch expression.node { case .column, .constant, .parameter: true; default: false }
    }
    private static func safe(_ expression: ResolvedExpression) -> Bool {
        switch expression.node {
        case .binary(let operation, let a, let b, let type):
            if operation == "and" || operation == "or" { return safe(a) && safe(b) }
            guard ["=", "<>", "!=", "<", "<=", ">", ">="].contains(operation), value(a), value(b) else { return false }
            return ["=", "<>", "!="].contains(operation) || (type.collation?.description ?? "C") == "C"
        case .unary("not", let child): return safe(child)
        case .isNull(let child, _): return value(child)
        case .distinct(let a, let b, _, _): return value(a) && value(b)
        default: return value(expression)
        }
    }
    static func map(_ expression: ResolvedExpression, column: (Int) -> Int) -> ResolvedExpression {
        var result = expression
        switch expression.node {
        case .column(let index): result.node = .column(column(index))
        case .binary(let op, let a, let b, let type): result.node = .binary(op, map(a, column: column), map(b, column: column), type)
        case .unary(let op, let child): result.node = .unary(op, map(child, column: column))
        case .isNull(let child, let negated): result.node = .isNull(map(child, column: column), negated)
        case .distinct(let a, let b, let type, let negated): result.node = .distinct(map(a, column: column), map(b, column: column), type, negated)
        default: break
        }
        return result
    }
}
