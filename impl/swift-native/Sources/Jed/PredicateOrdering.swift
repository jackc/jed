/// Filter qual ordering is distinct from value-expression evaluation. Only a
/// clause's outer AND chain is reordered; OR/NOT/CASE and WHEN remain authored.
internal enum PredicateOrdering {
    struct Key {
        let correlated: Bool
        let operators: Int64
    }
    static func key(_ expression: ResolvedExpression, parameters: [SQLValue]? = nil,
                    folded: SubqueryExecution? = nil, planningProbe: PlanningProbe? = nil) -> Key {
        let known = PlanCardinality(table: nil, snapshot: nil, parameters: parameters, planningProbe: planningProbe)
        var pending = [expression], correlated = false, operators: Int64 = 0
        while let item = pending.popLast() {
            var own: Int64 = 1
            switch item.node {
            case .constant, .column, .outerColumn, .parameter, .convert, .dateInput, .currentDate, .currentTimestamp:
                own = 0
            case .quantified(_, _, _, let array, _):
                if case .array(let value) = known.known(array) { own = saturatingAdd(own, Int64(value.elements.count)) }
                else if case .array(let values, _) = array.node { own = saturatingAdd(own, Int64(values.count)) }
            case .subquery(let query):
                correlated = correlated || query.correlated
                if !query.correlated, case .membership = query.kind,
                   let rows = folded?.values(query) { own = saturatingAdd(own, Int64(rows.count)) }
            default: break
            }
            operators = saturatingAdd(operators, own)
            pending.append(contentsOf: StoredExpressionDependencies.children(item))
        }
        return Key(correlated: correlated, operators: operators)
    }
    static func order(_ expression: ResolvedExpression, parameters: [SQLValue]? = nil,
                      folded: SubqueryExecution? = nil, planningProbe: PlanningProbe? = nil) -> ResolvedExpression {
        let terms = QueryEstimate.conjuncts(expression)
        guard terms.count > 1 else { return expression }
        let keys = terms.map { key($0, parameters: parameters, folded: folded, planningProbe: planningProbe) }
        func before(_ a: Int, _ b: Int) -> Bool {
            if keys[a].correlated != keys[b].correlated { return !keys[a].correlated }
            if keys[a].operators != keys[b].operators { return keys[a].operators < keys[b].operators }
            return a < b
        }
        guard terms.indices.dropFirst().contains(where: { before($0, $0 - 1) }) else { return expression }
        return QueryEstimate.and(terms.indices.sorted(by: before).map { terms[$0] })!
    }
}
