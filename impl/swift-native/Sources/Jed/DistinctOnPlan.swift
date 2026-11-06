/// DISTINCT ON compares adjacent fully sorted rows and owns no hash table.
internal struct DistinctOnPlan {
    let count: Int
    let evaluationOrder: [Int]

    static func resolve(keys: [ResolvedOrderTerm], order: [ResolvedOrderTerm],
                        same: (ResolvedExpression, ResolvedExpression) -> Bool = ExpressionIdentity.same) throws -> (plan: Self, order: [ResolvedOrderTerm]) {
        var effective = order, matched: Set<Int> = [], prefix = 0, ended = false
        for term in order {
            let matches = keys.indices.filter { same(keys[$0].expression, term.expression) }
            if matches.isEmpty { ended = true }
            else {
                guard !ended else { throw JedError("42P10", "SELECT DISTINCT ON expressions must match initial ORDER BY expressions") }
                prefix += 1; matched.formUnion(matches)
            }
        }
        for index in keys.indices where !matched.contains(index) {
            guard !ended else { throw JedError("42P10", "SELECT DISTINCT ON expressions must match initial ORDER BY expressions") }
            if !effective.contains(where: { same($0.expression, keys[index].expression) }) {
                effective.append(keys[index]); prefix += 1
            }
        }
        var evaluation: [Int] = []
        for key in keys {
            if let index = effective.firstIndex(where: { same(key.expression, $0.expression) }), !evaluation.contains(index) { evaluation.append(index) }
        }
        evaluation += effective.indices.filter { !evaluation.contains($0) }
        return (Self(count: prefix, evaluationOrder: evaluation), effective)
    }
    func same(_ a: [SQLValue], _ b: [SQLValue], order: [ResolvedOrderTerm]) throws -> Bool {
        for index in 0..<count {
            var type = order[index].type
            if (type.collation?.description ?? "C") != "C", ExpressionCollation.collatable(type.type) { type = TypeDescriptor(.scalar(.bytea)) }
            if try ValueComparison.total(a[index], b[index], type: type) != 0 { return false }
        }
        return true
    }
}
