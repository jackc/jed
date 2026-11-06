/// DISTINCT projects before keying and charges row_produced only for the
/// surviving window. Its hash set is separately held Q2 operator state.
internal final class DistinctExecution {
    private let plan: SelectPlan
    private let context: ExpressionContext
    private var rows: [[SQLValue]?] = []
    private var reserved: Int64 = 0
    private var position = 0
    private var started = false
    private var streaming: SelectExecution?
    private var seen: Set<[UInt8]> = []
    private var skipped: Int64 = 0
    private var emitted: Int64 = 0
    init(plan: SelectPlan, context: ExpressionContext) { self.plan = plan; self.context = context }
    private func reserve(_ bytes: Int64) throws {
        try context.meter.reserveQuery(bytes); reserved = saturatingAdd(reserved, bytes)
    }
    private func release(_ bytes: Int64) { context.meter.resources.query.release(bytes); reserved -= bytes }
    func next() throws -> [SQLValue]? {
        if plan.orderedByScan { return try stream() }
        if !started { try prepare(); started = true }
        guard position < rows.count, let row = rows[position] else { return nil }
        if plan.productionCharge { context.meter.charge(1); try context.meter.guardCost() }
        rows[position] = nil; position += 1; release(MemorySizes.row(row)); return row
    }
    private func inputPlan() -> SelectPlan {
        SelectPlan(columns: plan.columns, projections: plan.projections, predicate: plan.predicate,
            store: plan.store, touched: plan.touched, parameters: plan.parameters, limit: nil, offset: 0, order: plan.order,
            orderedByScan: plan.orderedByScan, descending: plan.descending, scan: plan.scan, aggregation: plan.aggregation,
            relation: plan.relation, productionCharge: false)
    }
    private func stream() throws -> [SQLValue]? {
        if plan.limit.map({ emitted >= $0 }) == true { return nil }
        if streaming == nil { streaming = SelectExecution(plan: inputPlan(), context: context) }
        while let row = try streaming?.next() {
            for value in row { context.meter.charge(MemorySizes.payload(value)); try context.meter.guardCost() }
            let key = try ValueEqualityKey.encode(row)
            if seen.contains(key) { continue }
            try reserve(row.reduce(GeneratedSpec.memoryEntry) { saturatingAdd($0, MemorySizes.value($1)) })
            seen.insert(key)
            if skipped < plan.offset { skipped += 1; continue }
            if plan.productionCharge { context.meter.charge(1); try context.meter.guardCost() }
            emitted += 1; return row
        }
        return nil
    }
    private func prepare() throws {
        let meter = context.meter
        let execution = SelectExecution(plan: inputPlan(), context: context); defer { execution.close() }
        var seen: Set<[UInt8]> = [], output: [[SQLValue]] = [], entries: Int64 = 0
        while let row = try execution.next() {
            for value in row { meter.charge(MemorySizes.payload(value)); try meter.guardCost() }
            let key = try ValueEqualityKey.encode(row)
            if seen.contains(key) { continue }
            let bytes = row.reduce(GeneratedSpec.memoryEntry) { saturatingAdd($0, MemorySizes.value($1)) }
            try reserve(bytes); entries = saturatingAdd(entries, bytes)
            try reserve(MemorySizes.row(row)); seen.insert(key); output.append(row)
        }
        execution.close(); seen = []; release(entries)
        let start = Int(min(Int64(output.count), plan.offset))
        let end = start + Int(min(Int64(output.count - start), plan.limit ?? Int64.max))
        for row in output[..<start] { release(MemorySizes.row(row)) }
        for row in output[end...] { release(MemorySizes.row(row)) }
        rows = output[start..<end].map(Optional.some)
    }
    func close() { streaming?.close(); streaming = nil; rows = []; seen = []; context.meter.resources.query.release(reserved); reserved = 0 }
    deinit { close() }
}
