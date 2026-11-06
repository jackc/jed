extension SelectPlan {
    /// Candidate pricing and execution share one retained identity. Binding a
    /// parameter may change the winner; no row or callback is read to price it.
    func selectAccess(parameters: [SQLValue]?, context: ExpressionContext) throws -> SelectPlan {
        guard chosenAccess == nil, let candidates = accessCandidates, !candidates.isEmpty else { return self }
        var winner: SelectPlan?, best: Int64 = .max
        for candidate in candidates {
            var plan = self
            plan.chosenAccess = candidate
            if !candidate.tableOrder {
                plan.orderedByScan = aggregation == nil && distinctOn == nil && candidate.providesOrder(order)
                plan.descending = false
            }
            let source = try candidate.estimate(parameters: parameters, context: context)
            let total = try SelectEstimate.estimate(plan: plan, source: source, parameters: parameters, context: context)
            if winner == nil || total.cost < best { winner = plan; best = total.cost }
        }
        return winner ?? self
    }
}
