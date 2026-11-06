extension ExplainTree {
    func nway(_ physical: NWayAccessPlan, plan: SelectPlan) throws -> ExplainNode {
        let estimates = try physical.estimate(plan: plan, parameters: parameters, context: context, tree: self)
        var needed = Set(plan.touched)
        StoredExpressionDependencies.walk(physical.edges.compactMap(\.residual)) {
            if case .column(let column) = $0.node { needed.insert(column) }
        }
        for input in physical.inputs {
            needed.formUnion(input.source.lateralColumns.filter { $0 >= physical.source.precedingWidth }.map { $0 - physical.source.precedingWidth })
        }
        func leaf(_ position: Int) throws -> ExplainNode {
            let ordinal = physical.winner.order[position], input = physical.inputs[ordinal]
            let access = physical.winner.access[position], estimate = estimates.inputs[position]
            var mask = Set(needed.filter { $0 >= input.offset && $0 < input.offset + input.source.columns.count }.map { $0 - input.offset })
            StoredExpressionDependencies.walk(input.source.scanFilter.map { [$0] } ?? []) {
                if case .column(let column) = $0.node { mask.insert(column) }
            }
            let node: ExplainNode
            if let candidate = access.candidate {
                node = try scan(candidate, touched: mask.sorted())
                if access.indexed, let bound = candidate.primary {
                    node.attributes = try scan(table: candidate.table, store: candidate.store, bound: bound,
                        touched: mask.sorted(), snapshot: candidate.snapshot, filter: nil, joining: true).attributes
                } else if access.indexed, !node.attributes.isEmpty { node.attributes[0] = "Index-nested-loop " + node.attributes[0] }
                if let filter = input.source.scanFilter {
                    node.attributes.append(try condition(input.source.scanDisplayFilter ?? filter, label: "filter", compact: "filter:conjuncts="))
                }
            } else {
                let child = try source(input.source, touched: mask, offset: 0)
                if input.lateral && position > 0 {
                    let calls = position == 1 ? estimates.inputs[0].rows : estimates.steps[position - 2].rows
                    func repeated(_ child: ExplainNode) -> ExplainNode {
                        let copy = ExplainNode(child.name, child.attributes, rows: QueryEstimate.multiply(child.rows, calls),
                            cost: QueryEstimate.multiply(child.cost, calls), logicalRows: QueryEstimate.multiply(child.logicalRows, calls),
                            children: child.children.map(repeated))
                        copy.actual = child.actual
                        return copy
                    }
                    node = repeated(child)
                } else { node = child }
            }
            node.rows = estimate.rows; node.logicalRows = estimate.logicalRows; node.cost = estimate.cost
            let profile = context.executionProfile
            node.actual = { profile?.value(input.source, .total) ?? 0 }
            return node
        }
        var node = try leaf(0)
        for (position, step) in physical.winner.steps.enumerated() {
            let estimate = estimates.steps[position], predicates = step.onIndices.compactMap { physical.edges[$0].residual }
            var label = "Nested Loop", detail = [NWayAccessPlan.stepKind(step, edges: physical.edges)]
            if case .hash(let keys) = step.algorithm { label = "Hash Join"; detail.append("keys=\(keys.count)") }
            if predicates.count == 1 {
                detail.append(try condition(predicates[0], label: "on", compact: "on:conjuncts="))
            } else if predicates.count > 1 {
                if options.verbose { detail.append("on=[" + (try predicates.map(ExplainExpression.render)).joined(separator: ", ") + "]") }
                else {
                    let count = predicates.reduce(0) { $0 + QueryEstimate.conjuncts($1).count }
                    detail.append("on:predicates=\(predicates.count),conjuncts=\(count)")
                }
            }
            node = ExplainNode(label, detail, rows: estimate.rows, cost: estimate.cost, logicalRows: estimate.logicalRows,
                children: [node, try leaf(position + 1)])
            let profile = context.executionProfile
            if position == physical.winner.steps.count - 1 { node.actual = { profile?.value(physical.source, .total) ?? 0 } }
            else { node.actual = { profile?.value(step.identity, .total) ?? 0 } }
        }
        return node
    }
}
