extension ExplainTree {
    func mutation(_ preparation: MutationExecutor.Preparation) throws -> ExplainNode {
        let node: ExplainNode
        switch preparation {
        case .insert(let plan):
            let child: ExplainNode
            if let source = plan.sourcePlan { child = try query(source) }
            else { child = ExplainNode("Values", ["rows=\(plan.slots.count)"], rows: Int64(plan.slots.count), cost: 0) }
            var attributes: [String] = []
            if let conflict = plan.conflict { attributes.append(conflict.doNothing ? "on conflict do nothing" : "on conflict do update") }
            node = ExplainNode("Insert " + plan.relation.table.name.description, attributes,
                rows: child.rows, cost: child.cost, children: [child])
        case .mutation(let plan):
            var child = try scan(table: plan.relation.table, store: plan.relation.store, bound: plan.scan,
                touched: plan.touched.sorted(), snapshot: plan.read, filter: nil)
            let profile = context.executionProfile, identity = plan.profileIdentity
            child.actual = { profile?.value(identity, .source) ?? 0 }
            if let predicate = plan.accessPredicate {
                let model = PlanCardinality(table: plan.relation.table, snapshot: plan.read,
                                           parameters: parameters, collatedKey: context.collatedKey)
                let count = try model.rows(predicate, input: plan.relation.table.rowCount)
                var attributes = [try condition(predicate, label: "filter", compact: "conjuncts=")]
                if plan.scan.contradiction { attributes.append("contradiction") }
                child = ExplainNode("Filter", attributes, rows: count,
                    cost: saturatingAdd(child.cost, try expressionCost([predicate], rows: child.rows)), children: [child])
                child.actual = { profile?.cumulative(identity, through: .filter) ?? 0 }
            }
            node = ExplainNode((plan.deleting ? "Delete " : "Update ") + plan.relation.table.name.description,
                plan.deleting ? [] : ["sets=\(plan.changes.count)"], rows: child.rows, cost: child.cost, children: [child])
        }
        let profile = context.executionProfile, identity = preparation.profileIdentity
        node.actual = { profile?.value(identity, .total) ?? 0 }
        return node
    }
}
