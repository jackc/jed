extension SelectPlan {
    /// Candidate pricing and execution share one retained identity. Binding a
    /// parameter may change the winner; no row or callback is read to price it.
    func selectAccess(parameters: [SQLValue]?, context: ExpressionContext) throws -> SelectPlan {
        if chosenJoin == nil, chosenNWay == nil,
           let candidate = try NWayAccessPlan.resolve(self, parameters: parameters, context: context) {
            var plan = self; plan.chosenNWay = candidate
            plan.orderedByScan = candidate.providesOrder; plan.descending = false
            return plan
        }
        if chosenJoin == nil, chosenNWay == nil, relation != nil,
           let candidates = try JoinAccessPlan.candidates(self, parameters: parameters, context: context) {
            var winner: SelectPlan?, best: Int64 = .max
            for candidate in candidates {
                var plan = self; plan.chosenJoin = candidate
                plan.orderedByScan = candidate.providesOrder(plan); plan.descending = false
                let source = try candidate.estimate(plan: plan, parameters: parameters, context: context).total
                let total = try SelectEstimate.estimate(plan: plan, source: source, parameters: parameters, context: context)
                if winner == nil || total.cost < best { winner = plan; best = total.cost }
            }
            return winner ?? self
        }
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
