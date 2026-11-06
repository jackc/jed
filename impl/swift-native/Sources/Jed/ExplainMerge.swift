extension ExplainTree {
    func merge(_ plan: MergePreparation) throws -> ExplainNode {
        let source = try query(plan.source)
        let candidate = try plan.selectAccess(parameters: parameters, context: context)
        let target = try scan(candidate, touched: plan.touched)
        let model = PlanCardinality(table: plan.relation.table, snapshot: plan.read,
                                    parameters: parameters, collatedKey: context.collatedKey, planningProbe: context.planningProbe)
        let logical = try model.rows(plan.accessPredicate, input: plan.relation.table.rowCount)
        let matched = min(target.rows, logical)
        let rows = QueryEstimate.multiply(source.rows, matched)
        let keys = try plan.hashKeys(access: candidate, parameters: parameters, context: context)
        let loopCost: Int64
        var detail = ["left"]
        if let keys {
            detail.append("keys=\(keys.count)")
            let width = plan.hashWidth(keys)
            let build = QueryEstimate.multiply(target.rows, width)
            let probe = saturatingAdd(QueryEstimate.multiply(source.rows, width),
                QueryEstimate.multiply(rows, saturatingAdd(width, Int64(keys.count * 4))))
            loopCost = saturatingAdd(source.cost, saturatingAdd(target.cost,
                saturatingAdd(build, saturatingAdd(probe, try expressionCost([plan.combinedCondition], rows: rows)))))
        } else {
            let filtered = saturatingAdd(target.cost, try expressionCost([plan.accessPredicate], rows: target.rows))
            loopCost = saturatingAdd(source.cost, QueryEstimate.multiply(source.rows, filtered))
            target.rows = QueryEstimate.multiply(target.rows, source.rows)
            target.logicalRows = QueryEstimate.multiply(target.logicalRows, source.rows)
            target.cost = QueryEstimate.multiply(target.cost, source.rows)
        }
        detail.append(try condition(plan.combinedCondition, label: "on", compact: "on:conjuncts="))
        let loop = ExplainNode(keys == nil ? "Nested Loop" : "Hash Join", detail,
            rows: rows, cost: loopCost, logicalRows: QueryEstimate.multiply(source.rows, logical), children: [source, target])
        let profile = context.executionProfile
        let targetIdentity = plan.probeProfileIdentity, loopIdentity = plan.loopProfileIdentity
        target.actual = { profile?.value(targetIdentity, .total) ?? 0 }
        loop.actual = { profile?.value(loopIdentity, .total) ?? 0 }
        var children = [loop], cost = loopCost, affected = source.rows
        if plan.hasBySource {
            let access = try plan.selectUnmatchedAccess(parameters: parameters, context: context)
            var unmatched = try scan(access, touched: plan.touched)
            let identity = plan.unmatchedProfileIdentity
            unmatched.actual = { profile?.value(identity, .source) ?? 0 }
            if let filter = plan.targetFilter {
                let count = try model.rows(filter, input: plan.relation.table.rowCount)
                unmatched = ExplainNode("Filter", [try condition(filter, label: "filter", compact: "conjuncts=")],
                    rows: min(unmatched.rows, count),
                    cost: saturatingAdd(unmatched.cost, try expressionCost([filter], rows: unmatched.rows)),
                    logicalRows: count, children: [unmatched])
            }
            unmatched.actual = { profile?.value(identity, .total) ?? 0 }
            children.append(unmatched); cost = saturatingAdd(cost, unmatched.cost)
            affected = saturatingAdd(affected, unmatched.rows)
        }
        return ExplainNode("Merge " + plan.relation.table.name.description, ["when=\(plan.clauses.count)"],
            rows: affected, cost: cost, children: children)
    }
}
