/// Materialized inputs stay live while left-deep intermediate rows are built.
/// The ordered LIMIT lane streams only the final step into its result buffer.
internal final class NWayAccessExecution {
    private let plan: SelectPlan
    private let physical: NWayAccessPlan
    private let context: ExpressionContext
    private let projected: Bool
    private var rows: [[SQLValue]?] = []
    private var held: Int64 = 0
    private var position = 0
    private var started = false
    init(plan: SelectPlan, context: ExpressionContext, projected: Bool) {
        self.plan = plan; physical = plan.chosenNWay!; self.context = context; self.projected = projected
    }
    private func reserve(_ row: [SQLValue]) throws {
        let bytes = MemorySizes.row(row); try context.meter.reserveQuery(bytes); held = saturatingAdd(held, bytes)
    }
    private func release(_ row: [SQLValue]) {
        let bytes = MemorySizes.row(row); context.meter.resources.query.release(bytes); held -= bytes
    }
    func next(retaining: Bool = false) throws -> [SQLValue]? {
        if !started { started = true; try prepare() }
        guard position < rows.count, let row = rows[position] else { return nil }
        rows[position] = nil; position += 1
        if !retaining { release(row) }
        return row
    }
    func discardRetained(_ row: [SQLValue]) { release(row) }
    var hostBatchWindow: (index: Int, count: Int)? { !projected && position > 0 ? (position - 1, rows.count) : nil }
    func hostBatchValue(row: Int, column: Int, current: [SQLValue]) -> SQLValue? {
        let values = row == position - 1 ? current : rows.indices.contains(row) ? rows[row] : nil
        return values.flatMap { $0.indices.contains(column) ? $0[column] : nil }
    }
    func releaseCharge() { context.meter.resources.query.release(held); held = 0 }

    private func prepare() throws {
        let meter = context.meter, before = meter.accrued
        if try plan.scan.hasKnownContradiction(parameters: context.parameters, collatedKey: context.collatedKey) { return }
        let oldFilter = context.executionProfile?.value(plan.profileIdentity, .filter) ?? 0
        let oldOutput = context.executionProfile?.value(plan.profileIdentity, .output) ?? 0
        defer {
            let filter = (context.executionProfile?.value(plan.profileIdentity, .filter) ?? oldFilter) - oldFilter
            let output = (context.executionProfile?.value(plan.profileIdentity, .output) ?? oldOutput) - oldOutput
            let work = max(0, meter.accrued - before - filter - output)
            if projected { context.executionProfile?.add(plan.profileIdentity, .source, work) }
            context.executionProfile?.add(physical.source, .total, work)
        }
        var needed = Set(plan.touched)
        StoredExpressionDependencies.walk(physical.edges.compactMap(\.residual)) {
            if case .column(let column) = $0.node { needed.insert(column) }
        }
        for input in physical.inputs {
            needed.formUnion(input.source.lateralColumns.filter { $0 >= physical.source.precedingWidth }.map { $0 - physical.source.precedingWidth })
        }
        let masks = physical.inputs.map { input -> [Int] in
            var result = Set(needed.filter { $0 >= input.offset && $0 < input.offset + input.source.columns.count }.map { $0 - input.offset })
            StoredExpressionDependencies.walk(input.source.scanFilter.map { [$0] } ?? []) {
                if case .column(let column) = $0.node { result.insert(column) }
            }
            return result.sorted()
        }
        let winner = physical.winner
        var accesses = Array<NWayAccessPlan.Access?>(repeating: nil, count: physical.inputs.count)
        for (position, ordinal) in winner.order.enumerated() { accesses[ordinal] = winner.access[position] }
        func gather(_ ordinal: Int, current: ExpressionContext) throws -> [[SQLValue]] {
            let input = physical.inputs[ordinal]
            guard let access = accesses[ordinal]!.candidate else {
                let execution = RelationExecution(plan: input.source, context: current, touched: Set(masks[ordinal]))
                defer { execution.close() }
                var result: [[SQLValue]] = []
                // Transfer each already-admitted source row into this owner's
                // buffer; do not overlap a duplicate complete relation charge.
                while let row = try execution.next() { try reserve(row); result.append(row) }
                return result
            }
            return try context.profile(input.source, .total) {
                let cursor = try access.makeCursor(context: current, touched: masks[ordinal])
                defer { cursor.close() }
                var result: [[SQLValue]] = []
                while let record = try cursor.next() {
                    try meter.step(); meter.charge(1)
                    var row = Array(repeating: SQLValue.null, count: input.source.columns.count)
                    for column in masks[ordinal] {
                        row[column] = try record.materialize(column: column, type: access.store.tree.types[column], pager: access.store.tree.pool?.pager)
                    }
                    if let filter = input.source.scanFilter {
                        var filtered = current; filtered.column = { row[$0] }
                        if try !filtered.matches(filter) { continue }
                    }
                    try reserve(row); result.append(row)
                }
                return result
            }
        }
        var materialized = Array(repeating: [[SQLValue]](), count: physical.inputs.count)
        // Eager ordinary inputs preserve authored acquisition order even when
        // their eventual join order is different.
        for ordinal in physical.inputs.indices where !accesses[ordinal]!.indexed && !physical.inputs[ordinal].lateral {
            materialized[ordinal] = try gather(ordinal, current: context)
        }
        defer { for input in materialized { for row in input { release(row) } } }
        func combined(_ prefix: [SQLValue]?, _ row: [SQLValue], ordinal: Int) -> [SQLValue] {
            let offset = physical.inputs[ordinal].offset
            let width = physical.hasCostedIsland ? physical.source.columns.count : offset + row.count
            var result = prefix ?? []
            result.append(contentsOf: repeatElement(SQLValue.null, count: width - result.count))
            for index in row.indices { result[offset + index] = row[index] }
            return result
        }
        let driver = winner.order[0]
        var running: [[SQLValue]] = []
        if physical.hasCostedIsland {
            for row in materialized[driver] {
                let value = combined(nil, row, ordinal: driver); try reserve(value); running.append(value)
            }
        } else {
            running = materialized[driver]; materialized[driver] = []
        }
        defer { for row in running { release(row) } }
        var passed: Int64 = 0, emitted: Int64 = 0, stopped = false
        for (position, step) in winner.steps.enumerated() {
            let ordinal = step.input, access = accesses[ordinal]!
            let kind = NWayAccessPlan.stepKind(step, edges: physical.edges)
            let emitLeft = kind == "left" || kind == "full", emitRight = kind == "right" || kind == "full"
            let finalProjection = projected && position == winner.steps.count - 1
            let hash: HashJoinTable?
            if case .hash(let keys) = step.algorithm {
                hash = try HashJoinTable(rows: materialized[ordinal],
                    keys: keys.map { (column: $0.inner, type: $0.type) }, context: context)
            } else { hash = nil }
            defer { hash?.close() }
            var output: [[SQLValue]] = []
            var matchedRight = Array(repeating: false, count: materialized[ordinal].count)
            func emit(_ value: [SQLValue]) throws {
                var current = context; current.column = { value[$0] }
                if finalProjection {
                    if let predicate = plan.predicate,
                       try !context.profile(plan.profileIdentity, .filter, { try current.matches(predicate) }) { return }
                    passed = saturatingAdd(passed, 1)
                    if passed <= plan.offset { return }
                    let result = try context.profile(plan.profileIdentity, .output) {
                        try meter.step()
                        if plan.productionCharge { meter.charge(1) }
                        return try plan.projections.map { try current.evaluate($0) }
                    }
                    try reserve(result); rows.append(result); emitted += 1
                    if let limit = plan.limit, emitted >= limit { stopped = true }
                } else { try reserve(value); output.append(value) }
            }
            func keep(_ prefix: [SQLValue], _ row: [SQLValue]) throws -> Bool {
                guard !stopped else { return false }
                try meter.pollStep()
                let value = combined(prefix, row, ordinal: ordinal)
                var current = context; current.column = { value[$0] }
                for index in step.onIndices {
                    if let predicate = physical.edges[index].residual, try !current.matches(predicate) { return false }
                }
                try emit(value); return true
            }
            if !finalProjection || plan.limit != 0 {
                for prefix in running {
                    if stopped { break }
                    var matched = false
                    if access.indexed || physical.inputs[ordinal].lateral {
                        var current = context
                        if access.indexed { current.outerRows.insert({ prefix[$0] }, at: 0) }
                        else {
                            current.column = { index in
                                if index < self.physical.source.precedingWidth { return try self.context.column(index) }
                                return prefix[index - self.physical.source.precedingWidth]
                            }
                            current.subqueries = SubqueryExecution()
                        }
                        let candidates = try gather(ordinal, current: current)
                        defer { for row in candidates { release(row) } }
                        for row in candidates { if try keep(prefix, row) { matched = true }; if stopped { break } }
                    } else if let hash, case .hash(let keys) = step.algorithm {
                        var positions: [Int] = []
                        // A bucket is completely verified before its candidate
                        // rows enter ON, including the ordered LIMIT lane.
                        try hash.probe(prefix, keys: keys.map { (column: $0.outer, type: $0.type) }) { positions.append($0) }
                        for index in positions {
                            if try keep(prefix, materialized[ordinal][index]) { matched = true; matchedRight[index] = true }
                            if stopped { break }
                        }
                    } else {
                        for (index, row) in materialized[ordinal].enumerated() {
                            if try keep(prefix, row) { matched = true; matchedRight[index] = true }
                            if stopped { break }
                        }
                    }
                    if emitLeft && !matched {
                        try emit(combined(prefix, Array(repeating: .null,
                            count: physical.inputs[ordinal].source.columns.count), ordinal: ordinal))
                    }
                }
            }
            if emitRight {
                for (index, row) in materialized[ordinal].enumerated() where !matchedRight[index] {
                    try emit(combined(nil, row, ordinal: ordinal))
                }
            }
            for row in running { release(row) }; running = output
            if !finalProjection { context.executionProfile?.add(step.identity, .total, max(0, meter.accrued - before)) }
        }
        if !projected { rows = running.map { Optional($0) }; running = [] }
    }
    func close() { rows = []; context.meter.resources.query.release(held); held = 0 }
    deinit { close() }
}
