/// Base rows are admitted before ON evaluation. A correlated inner owns one
/// complete candidate buffer per outer row, overlapping that row's outputs.
internal final class JoinAccessExecution {
    private let plan: SelectPlan
    private let join: JoinAccessPlan
    private let context: ExpressionContext
    private let projected: Bool
    private var rows: [[SQLValue]?] = []
    private var held: Int64 = 0
    private var position = 0
    private var started = false
    init(plan: SelectPlan, context: ExpressionContext, projected: Bool) {
        self.plan = plan; join = plan.chosenJoin!; self.context = context; self.projected = projected
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
            if let relation = plan.relation { context.executionProfile?.add(relation, .total, work) }
        }
        var needed = Set(plan.touched)
        StoredExpressionDependencies.walk(join.predicate.map { [$0] } ?? []) {
            if case .column(let column) = $0.node { needed.insert(column) }
        }
        func mask(_ relation: RelationPlan, offset: Int) -> [Int] {
            var result = Set(needed.filter { $0 >= offset && $0 < offset + relation.columns.count }.map { $0 - offset })
            StoredExpressionDependencies.walk(relation.scanFilter.map { [$0] } ?? []) {
                if case .column(let column) = $0.node { result.insert(column) }
            }
            return result.sorted()
        }
        let outerMask = mask(join.outer, offset: join.outerOffset), innerMask = mask(join.inner, offset: join.innerOffset)
        func gather(_ relation: RelationPlan, access: ScanCandidate, columns: [Int], current: ExpressionContext) throws -> [[SQLValue]] {
            try context.profile(relation, .total) {
                let cursor = try access.makeCursor(context: current, touched: columns)
                defer { cursor.close() }
                var result: [[SQLValue]] = []
                while let record = try cursor.next() {
                    try meter.step(); meter.charge(1)
                    var row = Array(repeating: SQLValue.null, count: relation.columns.count)
                    for column in columns { row[column] = try record.materialize(column: column, pager: access.store.tree.pool?.pager) }
                    if let filter = relation.scanFilter {
                        var filtered = current; filtered.column = { row[$0] }
                        if try !filtered.matches(filter) { continue }
                    }
                    try reserve(row); result.append(row)
                }
                return result
            }
        }
        let outer: [[SQLValue]], inner: [[SQLValue]]
        if !projected, join.swapped, !join.indexed {
            inner = try gather(join.inner, access: join.innerAccess, columns: innerMask, current: context)
            outer = try gather(join.outer, access: join.outerAccess, columns: outerMask, current: context)
        } else {
            outer = try gather(join.outer, access: join.outerAccess, columns: outerMask, current: context)
            inner = join.indexed ? [] : try gather(join.inner, access: join.innerAccess, columns: innerMask, current: context)
        }
        defer { for row in outer { release(row) }; for row in inner { release(row) } }
        let hash: HashJoinTable?
        if case .hash(let keys) = join.algorithm, !projected || plan.limit != 0 {
            hash = try HashJoinTable(rows: inner, keys: keys.map { (column: $0.inner, type: $0.type) }, context: context)
        } else { hash = nil }
        defer { hash?.close() }
        guard !projected || plan.limit != 0 else { return }
        var passed: Int64 = 0, emitted: Int64 = 0, stopped = false, matched = false
        func keep(_ a: [SQLValue], _ b: [SQLValue], checkingOn: Bool = true) throws {
            guard !stopped else { return }
            try meter.pollStep()
            let row = join.swapped ? b + a : a + b
            var current = context; current.column = { row[$0] }
            if checkingOn {
                if let predicate = join.predicate, try !current.matches(predicate) { return }
                // A WHERE rejection does not turn a matching pair into an
                // unmatched outer row. Only ON controls NULL extension.
                matched = true
            }
            if projected {
                if let predicate = plan.predicate,
                   try !context.profile(plan.profileIdentity, .filter, { try current.matches(predicate) }) { return }
                passed = saturatingAdd(passed, 1)
                if passed <= plan.offset { return }
                let output = try context.profile(plan.profileIdentity, .output) {
                    try meter.step()
                    if plan.productionCharge { meter.charge(1) }
                    return try plan.projections.map(current.evaluate)
                }
                try reserve(output); rows.append(output); emitted += 1
                if let limit = plan.limit, emitted >= limit { stopped = true }
            } else { try reserve(row); rows.append(row) }
        }
        for a in outer {
            if stopped { break }
            matched = false
            var indexedCandidates: [[SQLValue]] = []
            // An unmatched LEFT row is emitted while its per-probe input is
            // still owned, just like a matching row. Release after that step.
            defer { for row in indexedCandidates { release(row) } }
            if join.indexed {
                let placeholder = Array(repeating: SQLValue.null, count: join.inner.columns.count)
                let logical = join.swapped ? placeholder + a : a + placeholder
                var current = context; current.outerRows.insert({ logical[$0] }, at: 0)
                indexedCandidates = try gather(join.inner, access: join.innerAccess, columns: innerMask, current: current)
                for b in indexedCandidates { try keep(a, b); if stopped { break } }
            } else if let hash, case .hash(let keys) = join.algorithm {
                var candidates: [Int] = []
                try hash.probe(a, keys: keys.map { (column: $0.outer, type: $0.type) }) { candidates.append($0) }
                for index in candidates { try keep(a, inner[index]); if stopped { break } }
            } else {
                for b in inner { try keep(a, b); if stopped { break } }
            }
            if join.kind == "left", !matched, !stopped {
                try keep(a, Array(repeating: .null, count: join.inner.columns.count), checkingOn: false)
            }
        }
    }
    func close() { rows = []; context.meter.resources.query.release(held); held = 0 }
    deinit { close() }
}
