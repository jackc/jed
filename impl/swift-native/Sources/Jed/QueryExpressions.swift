/// Walks retained query plans without borrowing the native call stack. The two
/// modes keep their distinct historic visitation orders; WITH folds begin only
/// after the execution has installed its binding layer.
internal enum QueryExpressions {
    enum Work {
        case query(SelectPlan), relation(RelationPlan), compound(CompoundQueryPlan)
        case binding(CTEBinding), items([ResolvedExpression])
    }
    static func collect(_ root: Work, folding: Bool) -> [ResolvedExpression] {
        var pending = [root], result: [ResolvedExpression] = []
        while let next = pending.popLast() {
            switch next {
            case .items(let expressions): result.append(contentsOf: expressions)
            case .query(let plan):
                if let withQuery = plan.withQuery {
                    if !folding {
                        pending.append(.query(withQuery.body))
                        for binding in withQuery.bindings.reversed() where binding.references > 0 { pending.append(.binding(binding)) }
                    }
                    continue
                }
                result.append(contentsOf: plan.projections)
                if let predicate = plan.predicate { result.append(predicate) }
                if !folding { result.append(contentsOf: plan.order.map(\.expression)) }
                if let aggregate = plan.aggregation { result.append(contentsOf: folding ? aggregate.foldExpressions : aggregate.expressions) }
                if folding { result.append(contentsOf: plan.window?.expressions ?? []) }
                else if let window = plan.window { pending.append(.items(window.expressions)) }
                // The VALUES query wrapper follows the same derived-body
                // boundary as SELECT: its cell subqueries execute in that body.
                if let relation = plan.relation { pending.append(.relation(relation)) }
                if let compound = plan.compound { pending.append(.compound(compound)) }
            case .relation(let relation):
                switch relation.body {
                case .base, .cte, .system: break
                case .derived(let plan, _), .lateralDerived(let plan, _):
                    if !folding { pending.append(.query(plan)) }
                case .function(let function): result.append(contentsOf: function.expressions)
                case .jsonTable(let table): result.append(contentsOf: table.expressions)
                case .join(let left, let right, _, let predicate):
                    if let predicate { pending.append(.items([predicate])) }
                    pending.append(.relation(right)); pending.append(.relation(left))
                }
            case .compound(let compound):
                switch compound.body {
                case .set(_, _, let left, let right): pending.append(.query(right)); pending.append(.query(left))
                case .values(let rows, _): for row in rows { result.append(contentsOf: row) }
                case .nested(let child): pending.append(.query(child))
                }
            case .binding(let binding):
                if let recursive = binding.recursive {
                    if let extra = recursive.augmentation {
                        pending.append(.items(extra.anchor + extra.recursive + (extra.filter.map { [$0] } ?? [])))
                    }
                    pending.append(.query(recursive.term))
                }
                if let plan = binding.plan { pending.append(.query(plan)) }
                else if let schema = binding.mutationSchema { pending.append(.items(schema.expressions)) }
            }
        }
        return result
    }
}
