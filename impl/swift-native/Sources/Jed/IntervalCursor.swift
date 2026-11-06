/// A bounded interval producer retains only encoded bounds and one tree cursor.
/// Eager consumers admit the complete access block before residual evaluation;
/// streaming LIMIT consumers pay each interval only when they start it, then
/// secondary table fetches as they pull entries. Row/filter charges belong to
/// the relation or mutation owner, exactly as for an ordinary AccessCursor.
internal final class IntervalCursor {
    private let plan: IntervalPlan
    private let table: TableStore
    private let index: TableStore?
    private let context: ExpressionContext
    private let touched: [Int]
    private let intervals: [ScanInterval]
    private let descending: Bool
    private let incremental: Bool
    private var position = 0
    private var cursor: TreeCursor?
    private var closed = false

    init(plan: IntervalPlan, table: TableStore, index: TableStore?, context: ExpressionContext,
         touched: [Int], access: ScanAccess, descending: Bool = false, incremental: Bool = false) throws {
        self.plan = plan; self.table = table; self.index = index; self.context = context
        self.touched = touched; self.descending = descending; self.incremental = incremental
        let intervals = access.isEmpty ? [] : access.intervals ?? [ScanInterval(lower: access.lower, upper: access.upper)]
        self.intervals = descending ? Array(intervals.reversed()) : intervals
        if !incremental {
            var units: Int64 = 0
            for interval in self.intervals {
                units = saturatingAdd(units, try accessUnits(interval, fetch: true))
            }
            context.meter.charge(units); try context.meter.guardCost()
        }
    }
    private func accessUnits(_ interval: ScanInterval, fetch: Bool) throws -> Int64 {
        let access = ScanAccess(lower: interval.lower, upper: interval.upper, isEmpty: false)
        guard let index, let definition = plan.index else {
            return try ScanCost.units(table, touched: touched, meter: context.meter, access: access)
        }
        var units = access.pages(index.tree)
        if fetch {
            var scan = access.cursor(index); defer { scan.close() }
            while let entry = try scan.next() {
                try context.meter.pollStep()
                let key = try IndexKeyBoundary.suffix(entry.key, types: definition.types, options: definition.index.keyOptions)
                units = saturatingAdd(units, try ScanCost.point(key, store: table, touched: touched).units)
            }
        }
        return units
    }
    /// Enter exactly one interval. A speculative host window may read only
    /// after this phase, so filling it cannot start or charge a later interval.
    private func beginInterval() throws -> Bool {
        guard !closed else { return false }
        if cursor != nil { return true }
        guard position < intervals.count else { close(); return false }
        let interval = intervals[position]; position += 1
        if incremental {
            context.meter.charge(try accessUnits(interval, fetch: false)); try context.meter.guardCost()
        }
        cursor = (index ?? table).cursor(lower: interval.lower, upper: interval.upper, descending: descending)
        return true
    }
    /// Nil is the current interval boundary, not permission to advance it.
    private func nextInInterval() throws -> PackedRecord? {
        guard !closed, cursor != nil else { return nil }
        guard let record = try cursor?.next() else { cursor?.close(); cursor = nil; return nil }
        return record
    }
    func beginHostBatchInterval() throws -> Bool {
        guard index == nil else { throw JedError("XX000", "secondary interval cannot supply unmetered host rows") }
        do { return try beginInterval() } catch { close(); throw error }
    }
    func nextHostBatchRecord() throws -> PackedRecord? {
        guard index == nil else { throw JedError("XX000", "secondary interval cannot supply unmetered host rows") }
        do { return try nextInInterval() } catch { close(); throw error }
    }
    func next() throws -> PackedRecord? {
        guard !closed else { return nil }
        do {
            while try beginInterval() {
                guard let record = try nextInInterval() else { continue }
                guard let definition = plan.index else { return record }
                let key = try IndexKeyBoundary.suffix(record.key, types: definition.types, options: definition.index.keyOptions)
                if incremental {
                    let point = try ScanCost.point(key, store: table, touched: touched)
                    context.meter.charge(point.units); try context.meter.guardCost()
                    return point.record
                }
                guard let row = try table.get(key) else { throw JedError.corruption("interval index entry refers to a missing table row") }
                return row
            }
            return nil
        } catch { close(); throw error }
    }
    func close() { cursor?.close(); cursor = nil; position = intervals.count; closed = true }
    deinit { close() }
}
