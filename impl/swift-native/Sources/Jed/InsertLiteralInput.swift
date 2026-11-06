/// Only source-list string literals that remain untyped may take an INSERT
/// target's input conversion. Sorting/grouping by that output first makes it
/// text, as do DISTINCT and query forms with a unified output type.
internal enum InsertLiteralInput {
    static func columns(_ query: SQLQuery) -> Set<Int> {
        var body = query, order = query.orderBy.map(\.expression)
        while true {
            switch body.body {
            case .nested(let child), .with(_, let child):
                body = child; order += child.orderBy.map(\.expression)
            default: return selected(body, order: order)
            }
        }
    }

    private static func unwrapped(_ expression: SQLExpression) -> SQLExpression {
        var expression = expression
        while case .parenthesized(let child) = expression.kind { expression = child }
        return expression
    }

    private static func selected(_ query: SQLQuery, order: [SQLExpression]) -> Set<Int> {
        guard case .select(let select) = query.body, !select.distinct, select.distinctOn.isEmpty else { return [] }
        var keys = order, groups = select.grouping
        while let group = groups.popLast() {
            switch group {
            case .set(let expressions): keys += expressions
            case .rollup(let children), .cube(let children), .sets(let children): groups += children
            }
        }
        return Set(select.items.enumerated().compactMap { index, item in
            guard let source = item.expression, case .string = unwrapped(source).kind else { return nil }
            for key in keys {
                switch unwrapped(key).kind {
                case .number(let spelling) where Int(spelling) == index + 1: return nil
                case .column(nil, let name) where name == item.alias: return nil
                default: break
                }
            }
            return index
        })
    }
}
