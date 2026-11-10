/// The selected join keeps its physical visitation order while every input,
/// intermediate, hash bucket and candidate sequence has a bounded owner.
internal enum BlockingJoinRows {
    private struct Input {
        let relation: RelationPlan
        let offset: Int
        var access: ScanCandidate?
        var bound: BaseScanPlan?
        var dynamic = false
        var lateral = false
    }
    private struct Key {
        let probe: Int
        let build: Int
        let type: TypeDescriptor
    }
    private struct Step {
        let input: Int
        let kind: String
        let predicates: [ResolvedExpression]
        var keys: [Key] = []
        var identity: QueryProfileIdentity?
    }
    private struct Layout {
        let relation: RelationPlan
        var inputs: [Input]
        let order: [Int]
        let steps: [Step]
    }

    static func eligible(_ plan: SelectPlan) -> Bool {
        if let physical = plan.chosenNWay { return physical.hasCostedIsland }
        if let join = plan.chosenJoin {
            if case .hash = join.algorithm { return true }
            return false
        }
        guard let relation = plan.relation,
              case .join(let left, let right, let kind, let predicate) = relation.body else { return false }
        if case .hash = JoinPlan.choose(left: left, right: right, kind: kind, predicate: predicate).algorithm { return true }
        return false
    }

    private static func layout(_ plan: SelectPlan) -> Layout? {
        guard let relation = plan.relation else { return nil }
        if let physical = plan.chosenNWay {
            var inputs = physical.inputs.map { Input(relation: $0.source, offset: $0.offset, lateral: $0.lateral) }
            for (position, ordinal) in physical.winner.order.enumerated() {
                let access = physical.winner.access[position]
                inputs[ordinal].access = access.candidate
                inputs[ordinal].dynamic = access.indexed || inputs[ordinal].lateral
            }
            let steps = physical.winner.steps.map { step -> Step in
                let keys: [Key]
                if case .hash(let source) = step.algorithm { keys = source.map { Key(probe: $0.outer, build: $0.inner, type: $0.type) } }
                else { keys = [] }
                return Step(input: step.input, kind: NWayAccessPlan.stepKind(step, edges: physical.edges),
                    predicates: step.onIndices.compactMap { physical.edges[$0].residual }, keys: keys, identity: step.identity)
            }
            return Layout(relation: relation, inputs: inputs, order: physical.winner.order, steps: steps)
        }
        if let join = plan.chosenJoin {
            let inputs = [Input(relation: join.left, offset: 0,
                                access: join.swapped ? join.innerAccess : join.outerAccess,
                                dynamic: join.swapped && join.indexed),
                          Input(relation: join.right, offset: join.left.columns.count,
                                access: join.swapped ? join.outerAccess : join.innerAccess,
                                dynamic: !join.swapped && join.indexed)]
            let keys: [Key]
            if case .hash(let source) = join.algorithm {
                keys = source.map { Key(probe: join.outerOffset + $0.outer, build: $0.inner, type: $0.type) }
            } else { keys = [] }
            let inner = join.swapped ? 0 : 1
            return Layout(relation: relation, inputs: inputs, order: [1 - inner, inner],
                steps: [Step(input: inner, kind: join.kind, predicates: join.predicate.map { [$0] } ?? [], keys: keys)])
        }
        guard case .join(let left, let right, let kind, let predicate) = relation.body else { return nil }
        let selected = JoinPlan.choose(left: left, right: right, kind: kind, predicate: predicate)
        var inputs = [Input(relation: left, offset: 0), Input(relation: right, offset: left.columns.count)]
        let inner = selected.swapped ? 0 : 1, outer = 1 - inner
        var keys: [Key] = []
        if relation.repeatsRight {
            inputs[1].lateral = true; inputs[1].dynamic = true
        } else {
            switch selected.algorithm {
            case .indexed(let bound): inputs[inner].bound = bound; inputs[inner].dynamic = true
            case .hash(let source):
                keys = source.map { Key(probe: inputs[outer].offset + (selected.swapped ? $0.right : $0.left),
                    build: selected.swapped ? $0.left : $0.right, type: $0.type) }
            case .nested: break
            }
        }
        return Layout(relation: relation, inputs: inputs, order: [outer, inner],
            steps: [Step(input: inner, kind: kind, predicates: relation.effectiveOn.map { [$0] } ?? [], keys: keys)])
    }

    /// A projected drive only streams the last step of an ordered LIMIT join.
    /// Other consumers receive raw logical slots for their subsequent stages.
    static func gather(_ plan: SelectPlan, context: ExpressionContext, projected: Bool = false) throws -> RowSpool? {
        guard let layout = layout(plan) else { return nil }
        let config = try BlockingRows.configuration(context), meter = context.meter
        let before = meter.accrued
        let filterBefore = context.executionProfile?.value(plan.profileIdentity, .filter) ?? 0
        let outputBefore = context.executionProfile?.value(plan.profileIdentity, .output) ?? 0
        defer {
            let filter = (context.executionProfile?.value(plan.profileIdentity, .filter) ?? filterBefore) - filterBefore
            let output = (context.executionProfile?.value(plan.profileIdentity, .output) ?? outputBefore) - outputBefore
            let work = max(0, meter.accrued - before - filter - output)
            context.executionProfile?.add(plan.profileIdentity, .source, work)
            context.executionProfile?.add(layout.relation, .total, work)
        }
        if try plan.scan.hasKnownContradiction(parameters: context.parameters, collatedKey: context.collatedKey) {
            return RowSpool(config: config)
        }
        var needed = Set(plan.touched)
        for step in layout.steps {
            StoredExpressionDependencies.walk(step.predicates) {
                if case .column(let column) = $0.node { needed.insert(column) }
            }
            for key in step.keys {
                needed.insert(key.probe); needed.insert(layout.inputs[step.input].offset + key.build)
            }
        }
        for input in layout.inputs {
            needed.formUnion(input.relation.lateralColumns.filter { $0 >= layout.relation.precedingWidth }
                .map { $0 - layout.relation.precedingWidth })
        }
        let masks = layout.inputs.map { input -> [Int] in
            var columns = Set(needed.filter { $0 >= input.offset && $0 < input.offset + input.relation.columns.count }
                .map { $0 - input.offset })
            columns.formUnion(input.relation.scanFilter.map(PushdownSafety.columns) ?? [])
            return columns.sorted()
        }
        var inputs = layout.inputs.map { _ in RowSpool(config: config) }
        defer { for input in inputs { input.close() } }
        let scanOrder = projected && inputs.count == 2 ? layout.order : Array(inputs.indices)
        for ordinal in scanOrder where !layout.inputs[ordinal].dynamic {
            inputs[ordinal] = try scan(layout.inputs[ordinal], mask: masks[ordinal], context: context, config: config)
        }
        func place(_ row: [SQLValue], ordinal: Int, into prefix: [SQLValue]? = nil) -> [SQLValue] {
            var result = prefix ?? Array(repeating: SQLValue.null, count: layout.relation.columns.count)
            let offset = layout.inputs[ordinal].offset
            for index in row.indices { result[offset + index] = row[index] }
            return result
        }
        var running = RowSpool(config: config)
        var escaped: RowSpool?
        defer { if running !== escaped { running.close() } }
        let driver = layout.order[0]
        do {
            let cursor = try inputs[driver].cursor(); defer { cursor.close() }
            while let row = try cursor.next() { try running.append(place(row, ordinal: driver)) }
        }
        inputs[driver].close()
        for (position, step) in layout.steps.enumerated() {
            let input = layout.inputs[step.input]
            let emitLeft = step.kind == "left" || step.kind == "full"
            let emitRight = step.kind == "right" || step.kind == "full"
            let final = projected && position == layout.steps.count - 1
            let hash: BoundedHashJoinTable?
            if !step.keys.isEmpty && (!final || inputs.count >= 3 || plan.limit != 0) {
                hash = try BoundedHashJoinTable(rows: inputs[step.input],
                    keys: step.keys.map { ($0.build, $0.type) }, config: config, context: context)
            } else { hash = nil }
            defer { hash?.close() }
            if hash != nil && !emitRight { inputs[step.input].close() }
            let matchedRight = SpillStateMap(config: config)
            defer { matchedRight.close() }
            let next = RowSpool(config: config)
            var keptNext = false
            defer { if !keptNext { next.close() } }
            var passed: Int64 = 0, stopped = final && plan.limit == 0
            func emit(_ row: [SQLValue]) throws {
                if !final { try next.append(row); return }
                var current = context; current.column = { row[$0] }
                if let predicate = plan.predicate,
                   try !context.profile(plan.profileIdentity, .filter, { try current.matches(predicate) }) { return }
                passed = saturatingAdd(passed, 1)
                if passed <= plan.offset { return }
                let value = try context.profile(plan.profileIdentity, .output) {
                    try meter.step()
                    if plan.productionCharge { meter.charge(1) }
                    return try plan.projections.map(current.evaluate)
                }
                try next.append(value)
                if let limit = plan.limit, next.count >= limit { stopped = true }
            }
            do {
                let cursor = try running.cursor(); defer { cursor.close() }
                while !stopped, let prefix = try cursor.next() {
                    var candidates: RowSpool?
                    defer { candidates?.close() }
                    if input.dynamic {
                        var current = context
                        if input.lateral {
                            current.column = { index in
                                if index < layout.relation.precedingWidth { return try context.column(index) }
                                return prefix[index - layout.relation.precedingWidth]
                            }
                            current.subqueries = SubqueryExecution()
                        } else { current.outerRows.insert({ prefix[$0] }, at: 0) }
                        candidates = try scan(input, mask: masks[step.input], context: current, config: config)
                    } else if let hash {
                        candidates = try hash.probe(prefix, keys: step.keys.map { ($0.probe, $0.type) })
                    }
                    let source = candidates ?? inputs[step.input]
                    var matched = false, ordinal: Int64 = 0
                    let right = try source.cursor(); defer { right.close() }
                    while !stopped, let row = try right.next() {
                        try meter.pollStep()
                        let index = ordinal; ordinal += 1
                        let combined = place(row, ordinal: step.input, into: prefix)
                        var current = context; current.column = { combined[$0] }
                        var passes = true
                        for predicate in step.predicates where passes { passes = try current.matches(predicate) }
                        if !passes { continue }
                        try emit(combined); matched = true
                        if emitRight { _ = try matchedRight.insert([.integer(index)]) }
                    }
                    if emitLeft && !matched && !stopped { try emit(prefix) }
                }
            }
            if emitRight && !stopped {
                let cursor = try inputs[step.input].cursor(); defer { cursor.close() }
                var ordinal: Int64 = 0
                while let row = try cursor.next() {
                    let exists = try matchedRight.get([.integer(ordinal)]) != nil; ordinal += 1
                    if !exists { try emit(place(row, ordinal: step.input)) }
                    if stopped { break }
                }
            }
            inputs[step.input].close(); running.close(); matchedRight.close(); hash?.close()
            running = next; keptNext = true
            if let identity = step.identity, !final { context.executionProfile?.add(identity, .total, max(0, meter.accrued - before)) }
        }
        escaped = running
        return running
    }

    /// Eager upstream producers complete Q1 admission before any spool write.
    /// Direct ordinary PK/full scans append to the spool as they scan.
    private static func scan(_ input: Input, mask: [Int], context: ExpressionContext,
                             config: SpillConfig) throws -> RowSpool {
        let result = RowSpool(config: config)
        do {
            guard case .base(let store) = input.relation.body else {
                let execution = RelationExecution(plan: input.relation, context: context, touched: Set(mask))
                defer { execution.close() }
                let first = try execution.next(retaining: true)
                execution.releaseCharge()
                if let first { try result.append(first) }
                while let row = try execution.next(retaining: true) { try result.append(row) }
                return result
            }
            try context.profile(input.relation, .total) {
                let eager: Bool
                if input.dynamic { eager = true }
                else if let candidate = input.access {
                    switch candidate.path { case .primary, .full: eager = false; default: eager = true }
                } else { eager = false }
                var rows: [[SQLValue]] = [], held: Int64 = 0
                defer { context.meter.resources.query.release(held) }
                func accept(_ record: PackedRecord) throws {
                    try context.meter.step(); context.meter.charge(1)
                    var row = Array(repeating: SQLValue.null, count: input.relation.columns.count)
                    for column in mask { row[column] = try record.materialize(column: column, type: store.tree.types[column], pager: store.tree.pool?.pager) }
                    if let predicate = input.relation.scanFilter {
                        var current = context; current.column = { row[$0] }
                        if try !current.matches(predicate) { return }
                    }
                    if eager {
                        let bytes = MemorySizes.row(row); try context.meter.reserveQuery(bytes)
                        held = saturatingAdd(held, bytes); rows.append(row)
                    } else { try result.append(row) }
                }
                if let access = input.access {
                    let cursor = try access.makeCursor(context: context, touched: mask)
                    defer { cursor.close() }
                    while let record = try cursor.next() { try accept(record) }
                } else {
                    let scan = input.bound ?? BaseScanPlan(predicate: input.relation.scanFilter,
                        keys: input.relation.allowsIndexAccess ? input.relation.baseTable?.primaryKey ?? [] : [], types: store.tree.types)
                    let access = try scan.bind(context)
                    try ScanCost.charge(store, touched: mask, meter: context.meter, access: access)
                    var cursor = access.cursor(store); defer { cursor.close() }
                    while let record = try cursor.next() { try accept(record) }
                }
                if eager {
                    context.meter.resources.query.release(held); held = 0
                    for row in rows { try result.append(row) }
                }
            }
            return result
        } catch { result.close(); throw error }
    }
}

/// Hash collisions are fully checked into a per-probe spool before ON begins.
/// Duplicate-heavy buckets never become an unbounded candidate vector.
internal final class BoundedHashJoinTable {
    private let entries: HashRowTable
    private let config: SpillConfig
    private let context: ExpressionContext
    init(rows: RowSpool, keys: [(column: Int, type: TypeDescriptor)], config: SpillConfig,
         context: ExpressionContext) throws {
        self.config = config; self.context = context; entries = HashRowTable(config: config)
        do {
            let cursor = try rows.cursor(); defer { cursor.close() }
            while let row = try cursor.next() {
                try context.meter.pollStep()
                guard let key = try HashJoinTable.encode(row, keys: keys, context: context) else { continue }
                try entries.append(hash: HashJoinTable.hash(key), row: [.bytea(key), .composite(row)])
            }
        } catch { entries.close(); throw error }
    }
    func probe(_ row: [SQLValue], keys: [(column: Int, type: TypeDescriptor)]) throws -> RowSpool {
        let result = RowSpool(config: config)
        do {
            guard let key = try HashJoinTable.encode(row, keys: keys, context: context) else { return result }
            let cursor = try entries.cursor(hash: HashJoinTable.hash(key)); defer { cursor.close() }
            while let entry = try cursor.next() {
                guard entry.count == 2, case .bytea(let stored) = entry[0], case .composite(let value) = entry[1] else {
                    throw JedError("58030", "invalid private hash build row")
                }
                try context.meter.step(); context.meter.charge(Int64(max(1, min(key.count, stored.count))))
                if stored == key { try result.append(value) }
            }
            return result
        } catch { result.close(); throw error }
    }
    func close() { entries.close() }
    deinit { close() }
}
