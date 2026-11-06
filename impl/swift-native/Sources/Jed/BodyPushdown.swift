/// Rewrites already resolved bodies without re-resolving catalogs, callbacks,
/// CTE references or views. Every child is resolved once; the transformation
/// retains its original expression identities except the moved column reads.
internal enum BodyPushdown {
    static func admits(_ plan: SelectPlan, columns: Set<Int>, matchingSafe: Bool) -> Bool {
        var pending = [plan]
        while let plan = pending.popLast() {
            guard plan.limit == nil, !plan.offsetPresent, plan.distinctOn == nil, plan.withQuery == nil else { return false }
            if let compound = plan.compound {
                switch compound.body {
                case .values: return false
                case .nested(let child): pending.append(child)
                case .set(let operation, let all, let left, let right):
                    guard matchingSafe || operation == "union" && all else { return false }
                    for index in columns {
                        guard plan.columns.indices.contains(index),
                              widening(left.columns[index].type, plan.columns[index].type),
                              widening(right.columns[index].type, plan.columns[index].type) else { return false }
                    }
                    pending.append(right); pending.append(left)
                }
                continue
            }
            guard matchingSafe || !plan.distinct && plan.aggregation == nil && plan.window == nil else { return false }
            if let aggregate = plan.aggregation {
                guard aggregate.sets.count == 1, !aggregate.sets[0].isEmpty else { return false }
            }
            for index in columns {
                guard plan.projections.indices.contains(index), case .column(let source) = plan.projections[index].node else { return false }
                if let aggregate = plan.aggregation {
                    guard aggregate.keys.contains(where: { if case .column(let column) = $0.node { return column == source }; return false }) else { return false }
                }
                if let window = plan.window, !window.calls.allSatisfy({ $0.partition.contains(source) }) { return false }
            }
        }
        return true
    }
    static func apply(_ term: ResolvedExpression, to original: SelectPlan, settings: SessionSettings,
                      parameters: [SQLValue]? = nil, planningProbe: PlanningProbe? = nil) throws -> SelectPlan {
        let work = PushdownWork(parameters: parameters, planningProbe: planningProbe), result = PushdownValue(original)
        enqueue(term, to: original, settings: settings, work: work, result: result)
        try work.run()
        return result.value
    }
    static func enqueue(_ term: ResolvedExpression, to original: SelectPlan, settings: SessionSettings,
                        work: PushdownWork, result: PushdownValue<SelectPlan>) {
        work.append { work in try begin(term, original: original, settings: settings, work: work, result: result) }
    }
    private static func begin(_ term: ResolvedExpression, original: SelectPlan, settings: SessionSettings,
                              work: PushdownWork, result: PushdownValue<SelectPlan>) throws {
        if let compound = original.compound {
            switch compound.body {
            case .set(let operation, let all, let left, let right):
                let leftResult = PushdownValue(left)
                work.append { work in
                    let rightResult = PushdownValue(right)
                    work.append { _ in
                        var plan = original
                        plan.compound = CompoundQueryPlan(body: .set(operation, all, leftResult.value, rightResult.value), settings: compound.settings)
                        result.value = plan
                    }
                    enqueue(armTerm(term, arm: right, output: original), to: right,
                        settings: compound.settings, work: work, result: rightResult)
                }
                enqueue(armTerm(term, arm: left, output: original), to: left,
                    settings: compound.settings, work: work, result: leftResult)
            case .nested(let child):
                let childResult = PushdownValue(child)
                work.append { _ in
                    var plan = original
                    plan.compound = CompoundQueryPlan(body: .nested(childResult.value), settings: compound.settings)
                    result.value = plan
                }
                enqueue(term, to: child, settings: compound.settings, work: work, result: childResult)
            case .values: throw JedError("XX000", "VALUES cannot receive body pushdown")
            }
            return
        }
        var plan = original
        let local = PushdownSafety.rewrite(term) { _, index in original.projections[index] }
        let filter = PredicateOrdering.order(QueryEstimate.and(QueryEstimate.conjuncts(original.predicate) + [local])!, planningProbe: work.planningProbe)
        plan.completePredicate = QueryEstimate.and(QueryEstimate.conjuncts(original.completePredicate ?? original.predicate) + [local])
        plan.completePredicate = plan.completePredicate.map { PredicateOrdering.order($0, planningProbe: work.planningProbe) }
        if let relation = plan.relation, !PredicateContradiction.proves(plan.completePredicate, parameters: work.parameters, planningProbe: work.planningProbe) {
            let residual = PushdownValue<ResolvedExpression?>(nil), prepared = plan
            work.append { _ in
                var plan = prepared; plan.predicate = residual.value
                result.value = try finish(plan, original: original, local: local, settings: settings)
            }
            RelationPushdown.enqueueWhere(filter, into: relation, work: work, result: residual)
        } else {
            plan.predicate = filter
            result.value = try finish(plan, original: original, local: local, settings: settings)
        }
    }
    private static func finish(_ prepared: SelectPlan, original: SelectPlan, local: ResolvedExpression,
                               settings: SessionSettings) throws -> SelectPlan {
        var plan = prepared
        plan.touched = Set(original.touched).union(PushdownSafety.columns(local)).sorted()
        plan.scan = BaseScanPlan(predicate: plan.completePredicate, keys: plan.scan.keys,
            types: plan.store?.tree.types ?? [])
        if let table = plan.sourceTable, let store = plan.store, let snapshot = plan.sourceSnapshot, let domain = plan.sourceDomain {
            let keys = plan.scan.keys
            plan.scan = BaseScanPlan(predicate: plan.completePredicate, keys: keys, types: store.tree.types)
            plan.accessCandidates = try AccessInventory.resolve(table: table, store: store, snapshot: snapshot,
                domain: domain, predicate: plan.predicate, base: plan.scan, settings: settings,
                context: StatementServices(settings: settings, meter: CostMeter()).context(),
                orderOnly: plan.limit != nil && plan.aggregation == nil && plan.distinctOn == nil ? plan.order : [])
        }
        plan.chosenAccess = nil; plan.chosenJoin = nil; plan.chosenNWay = nil
        return plan
    }
    private static func armTerm(_ term: ResolvedExpression, arm: SelectPlan, output: SelectPlan) -> ResolvedExpression {
        PushdownSafety.rewrite(term) { original, index in
            var column = original; column.type = arm.columns[index].type
            if let from = column.type?.type, let to = output.columns[index].type,
               (from == .scalar(.f32) && to.type == .scalar(.f64)) || (integer(from) && to.type == .scalar(.decimal)) {
                var cast = original; cast.node = .cast(column, to, .explicit); return cast
            }
            return column
        }
    }
    private static func widening(_ from: TypeDescriptor?, _ to: TypeDescriptor?) -> Bool {
        guard let from else { return true }
        guard let to else { return false }
        if from.type == to.type { return true }
        if integer(from.type) && to.type == .scalar(.decimal) { return true }
        if from.type == .scalar(.f32) && to.type == .scalar(.f64) { return true }
        return wideningType(from.type, to.type)
    }
    private static func integer(_ type: SQLType) -> Bool {
        if case .scalar(let scalar) = type { return scalar.integerWidth != nil }; return false
    }
    private static func wideningType(_ from: SQLType, _ to: SQLType) -> Bool {
        if from == to { return true }
        switch (from, to) {
        case (.scalar(let a), .scalar(let b)):
            guard let x = a.integerWidth, let y = b.integerWidth else { return false }; return x <= y
        case (.array(let a), .array(let b)): return wideningType(a, b)
        case (.record(let a), .record(let b)):
            return a.count == b.count && zip(a, b).allSatisfy { $0.name == $1.name && wideningType($0.descriptor.type, $1.descriptor.type) }
        default: return false
        }
    }
}
