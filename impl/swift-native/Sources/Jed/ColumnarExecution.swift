/// Packed-table lanes retain only the values they gather. Cost ceilings keep
/// the scalar drive so the established abort schedule is unchanged.
internal enum ColumnarExecution {
    static func enabled(_ plan: SelectPlan, _ context: ExpressionContext) -> Bool {
        plan.store != nil && plan.relation == nil && plan.scan.pointSets.isEmpty && !plan.scan.contradiction
            && plan.chosenAccess?.intervals == nil && plan.chosenAccess?.orderedDefinition == nil
            && plan.chosenAccess?.gin == nil && plan.chosenAccess?.gist == nil
            && context.meter.limit == 0 && context.meter.lifetimeLimit == 0 && context.executionProfile == nil
    }
    static func projection(_ plan: SelectPlan, _ context: ExpressionContext) -> Bool {
        enabled(plan, context) && !plan.materializeSource && plan.aggregation == nil && plan.window == nil
            && !plan.distinct && plan.order.isEmpty && plan.limit == nil && plan.offset == 0
            && !plan.projections.isEmpty && plan.projections.allSatisfy { if case .column = $0.node { return true }; return false }
    }
    static func aggregation(_ plan: SelectPlan, _ aggregate: AggregateQueryPlan, _ context: ExpressionContext) -> Bool {
        guard enabled(plan, context), !plan.distinct, plan.productionCharge, plan.window == nil, plan.order.isEmpty,
              aggregate.having == nil, aggregate.sets.count == 1, aggregate.grouping.isEmpty,
              aggregate.keys.count <= 1, aggregate.sets[0].count == aggregate.keys.count,
              !aggregate.calls.isEmpty else { return false }
        for key in aggregate.keys {
            guard case .column = key.node, case .scalar(let scalar) = key.type?.type, scalar.integerWidth != nil else { return false }
        }
        return aggregate.calls.allSatisfy { call in
            guard !call.distinct, call.filter == nil, call.order.isEmpty, call.direct.isEmpty, call.plan.withinGroup.isEmpty else { return false }
            if call.plan.name == "count_star" { return call.arguments.isEmpty }
            guard call.arguments.count == 1, case .column = call.arguments[0].node else { return false }
            if ["count", "min", "max"].contains(call.plan.name) { return true }
            guard let type = call.arguments[0].type?.type else { return false }
            if call.plan.name == "sum" { return [.scalar(.i16), .scalar(.i32), .scalar(.f32), .scalar(.f64)].contains(type) }
            return call.plan.name == "avg" && [.scalar(.f32), .scalar(.f64)].contains(type)
        }
    }

    /// The existing lane admits packed operands, filters, and its one integer
    /// group key. Only sources with no materialization owner use the direct
    /// reader; every other lane keeps its original admission schedule.
    static func packedAggregation(_ plan: SelectPlan, _ aggregate: AggregateQueryPlan, _ context: ExpressionContext) -> Bool {
        aggregation(plan, aggregate, context) && !plan.materializeSource
            && plan.chosenJoin == nil && plan.chosenNWay == nil
    }
}

internal final class ColumnarProjectionExecution {
    private let plan: SelectPlan
    private let context: ExpressionContext
    private var lanes: [[SQLValue]] = []
    private var outputLanes: [Int] = []
    private var position = 0
    private var count = 0
    private var reserved: Int64 = 0
    private var started = false
    init(plan: SelectPlan, context: ExpressionContext) { self.plan = plan; self.context = context }
    func next() throws -> [SQLValue]? {
        if !started { try prepare(); started = true }
        guard position < count else { return nil }
        if plan.productionCharge { context.meter.charge(1); try context.meter.guardCost() }
        let row = outputLanes.map { lanes[$0][position] }; position += 1
        return row
    }
    private func prepare() throws {
        let columns = Array(Set(plan.projections.compactMap { expression -> Int? in
            if case .column(let index) = expression.node { return index }; return nil
        })).sorted()
        outputLanes = plan.projections.map { expression in
            if case .column(let index) = expression.node { return columns.firstIndex(of: index)! }
            preconditionFailure("columnar projection contains an expression")
        }
        lanes = Array(repeating: [], count: columns.count)
        let input = try PackedLaneInput(plan: plan, context: context); defer { input.close() }
        let row = input.row
        var current = context; current.column = { try row.value($0) }
        let filter = PackedLaneFilter(predicate: plan.predicate, types: plan.store!.tree.types,
            touched: plan.touched, context: current)
        while try input.next() {
            guard try filter.keep(row, context: current) else { continue }
            for (lane, column) in columns.enumerated() { lanes[lane].append(try row.value(column)) }
            count += 1
        }
        let bytes = lanes.reduce(Int64(0)) { total, lane in lane.reduce(total) { saturatingAdd($0, MemorySizes.value($1)) } }
        try context.meter.reserveQuery(bytes); reserved = bytes
    }
    func close() { lanes = []; context.meter.resources.query.release(reserved); reserved = 0 }
    deinit { close() }
}
