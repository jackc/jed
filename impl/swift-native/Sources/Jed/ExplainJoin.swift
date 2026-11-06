extension ExplainTree {
    func join(_ physical: JoinAccessPlan, plan: SelectPlan) throws -> ExplainNode {
        let estimate = try physical.estimate(plan: plan, parameters: parameters, context: context)
        var needed = Set(plan.touched)
        StoredExpressionDependencies.walk(physical.predicate.map { [$0] } ?? []) {
            if case .column(let column) = $0.node { needed.insert(column) }
        }
        func node(_ relation: RelationPlan, access: ScanCandidate, offset: Int,
                  estimate: PlanEstimate, indexed: Bool) throws -> ExplainNode {
            var local = Set(needed.filter { $0 >= offset && $0 < offset + relation.columns.count }.map { $0 - offset })
            StoredExpressionDependencies.walk(relation.scanFilter.map { [$0] } ?? []) {
                if case .column(let column) = $0.node { local.insert(column) }
            }
            let result = try scan(access, touched: local.sorted())
            if indexed, let bound = access.primary {
                result.attributes = try scan(table: access.table, store: access.store, bound: bound,
                    touched: local.sorted(), snapshot: access.snapshot, filter: nil, joining: true).attributes
            } else if indexed, !result.attributes.isEmpty {
                result.attributes[0] = "Index-nested-loop " + result.attributes[0]
            }
            result.rows = estimate.rows; result.logicalRows = estimate.logicalRows; result.cost = estimate.cost
            if let filter = relation.scanFilter { result.attributes.append(try condition(relation.scanDisplayFilter ?? filter, label: "filter", compact: "filter:conjuncts=")) }
            let profile = context.executionProfile
            result.actual = { profile?.value(relation, .total) ?? 0 }
            return result
        }
        let outer = try node(physical.outer, access: physical.outerAccess, offset: physical.outerOffset,
            estimate: estimate.outer, indexed: false)
        let inner = try node(physical.inner, access: physical.innerAccess, offset: physical.innerOffset,
            estimate: estimate.inner, indexed: physical.indexed)
        var label = "Nested Loop", detail = [physical.kind]
        if case .hash(let keys) = physical.algorithm { label = "Hash Join"; detail.append("keys=\(keys.count)") }
        if let predicate = physical.predicate { detail.append(try condition(predicate, label: "on", compact: "on:conjuncts=")) }
        let node = ExplainNode(label, detail, rows: estimate.total.rows, cost: estimate.total.cost,
            logicalRows: estimate.total.logicalRows, children: [outer, inner])
        let profile = context.executionProfile
        if let relation = plan.relation { node.actual = { profile?.value(relation, .total) ?? 0 } }
        return node
    }
}
