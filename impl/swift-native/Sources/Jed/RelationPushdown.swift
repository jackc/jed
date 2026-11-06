/// ON is processed in join order, then WHERE. Runtime filters use local slots;
/// the explain annotation keeps the authored enclosing relation's slots.
internal enum RelationPushdown {
    private struct Leaf { let plan: RelationPlan; let offset: Int; let nullable: Bool }
    private static func leaves(_ relation: RelationPlan, offset: Int = 0, nullable: Bool = false) -> [Leaf] {
        var pending = [Leaf(plan: relation, offset: offset, nullable: nullable)], result: [Leaf] = []
        while let item = pending.popLast() {
            if case .join(let left, let right, let kind, _) = item.plan.body {
                pending.append(Leaf(plan: right, offset: item.offset + left.columns.count,
                    nullable: item.nullable || kind == "left" || kind == "full"))
                pending.append(Leaf(plan: left, offset: item.offset,
                    nullable: item.nullable || kind == "right" || kind == "full"))
            } else { result.append(item) }
        }
        return result
    }
    static func whereFilter(_ predicate: ResolvedExpression?, into relation: RelationPlan,
                            parameters: [SQLValue]? = nil, planningProbe: PlanningProbe? = nil) throws -> ResolvedExpression? {
        let work = PushdownWork(parameters: parameters, planningProbe: planningProbe), result = PushdownValue<ResolvedExpression?>(nil)
        enqueueWhere(predicate, into: relation, work: work, result: result)
        try work.run()
        return result.value
    }
    static func enqueueWhere(_ predicate: ResolvedExpression?, into relation: RelationPlan,
                             work: PushdownWork, result: PushdownValue<ResolvedExpression?>) {
        work.append { work in
            let inputs = leaves(relation)
            let state = Terms(QueryEstimate.conjuncts(predicate), inputs: inputs, scanAllowed: inputs.count > 1)
            work.append { _ in result.value = QueryEstimate.and(state.residual) }
            enqueueTerms(state, work: work)
        }
        enqueueOn(relation, work: work)
    }
    private static func enqueueOn(_ relation: RelationPlan, work: PushdownWork) {
        work.append { work in
            guard case .join(let left, let right, let kind, let original) = relation.body else { return }
            work.append { work in
                guard relation.pushedOn == nil else { return }
                let inputs = leaves(left) + leaves(right, offset: left.columns.count)
                let state = Terms(QueryEstimate.conjuncts(original.map { PredicateOrdering.order($0, planningProbe: work.planningProbe) }),
                    inputs: inputs, scanAllowed: true, kind: kind, leftCount: left.columns.count)
                work.append { _ in relation.pushedOn = state.residual }
                enqueueTerms(state, work: work)
            }
            enqueueOn(right, work: work); enqueueOn(left, work: work)
        }
    }
    private final class Terms {
        let terms: [ResolvedExpression]
        let inputs: [Leaf]
        let scanAllowed: Bool
        let kind: String?
        let leftCount: Int
        var position = 0
        var residual: [ResolvedExpression] = []
        init(_ terms: [ResolvedExpression], inputs: [Leaf], scanAllowed: Bool,
             kind: String? = nil, leftCount: Int = 0) {
            self.terms = terms; self.inputs = inputs; self.scanAllowed = scanAllowed
            self.kind = kind; self.leftCount = leftCount
        }
    }
    private static func enqueueTerms(_ state: Terms, work: PushdownWork) {
        work.append { work in try advance(state, work: work) }
    }
    private static func advance(_ state: Terms, work: PushdownWork) throws {
        while state.position < state.terms.count {
            let term = state.terms[state.position]; state.position += 1
            let columns = PushdownSafety.columns(term)
            guard state.kind != "full", !columns.isEmpty,
                  PushdownSafety.predicate(term, parameters: work.parameters, planningProbe: work.planningProbe), let target = state.inputs.first(where: { item in
                guard !item.nullable, item.plan.lateralColumns.isEmpty,
                      columns.allSatisfy({ $0 >= item.offset && $0 < item.offset + item.plan.columns.count }) else { return false }
                guard let kind = state.kind else { return true }
                let onRight = item.offset >= state.leftCount
                return kind == "inner" || kind == "cross" || kind == "left" && onRight || kind == "right" && !onRight
            }) else { state.residual.append(term); continue }
            let local = map(term) { $0 - target.offset }
            if case .derived(let body, let settings) = target.plan.body,
               BodyPushdown.admits(body, columns: PushdownSafety.columns(local), matchingSafe: PushdownSafety.preservesEquality(local)) {
                let result = PushdownValue(body)
                work.append { work in
                    target.plan.body = .derived(result.value, settings)
                    enqueueTerms(state, work: work)
                }
                BodyPushdown.enqueue(local, to: body, settings: settings, work: work, result: result)
                return
            }
            if case .cte(let reference) = target.plan.body, let body = reference.inlinePlan,
               BodyPushdown.admits(body, columns: PushdownSafety.columns(local), matchingSafe: PushdownSafety.preservesEquality(local)) {
                let result = PushdownValue(body)
                work.append { work in
                    var reference = reference; reference.inlinePlan = result.value
                    target.plan.body = .cte(reference)
                    enqueueTerms(state, work: work)
                }
                BodyPushdown.enqueue(local, to: body, settings: reference.binding.settings, work: work, result: result)
                return
            }
            guard state.scanAllowed else { state.residual.append(term); continue }
            target.plan.scanFilter = PredicateOrdering.order(QueryEstimate.and(QueryEstimate.conjuncts(target.plan.scanFilter) + [local])!, planningProbe: work.planningProbe)
            target.plan.scanDisplayFilter = PredicateOrdering.order(QueryEstimate.and(QueryEstimate.conjuncts(target.plan.scanDisplayFilter) + [term])!, planningProbe: work.planningProbe)
        }
    }
    static func map(_ expression: ResolvedExpression, column: (Int) -> Int) -> ResolvedExpression {
        PushdownSafety.rewrite(expression) { input, index in var result = input; result.node = .column(column(index)); return result }
    }
}
