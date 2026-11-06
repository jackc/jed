/// One execution-owned view of the current packed record. Values are decoded
/// at most once per column and record; callers retain the frame, not its row.
/// The stable expression getter avoids copying a context or capturing a new
/// owned row for every predicate. Decoded scratch is outside the query buffer
/// account, like the existing scalar storage-read scratch (memory.md §5.4).
internal final class PackedLaneRow {
    private let pager: Pager?
    private let slots: [Int]
    private var values: [SQLValue]
    private var stamps: [UInt64]
    private var generation: UInt64 = 0
    private var position: TreePosition?

    init(width: Int, touched: [Int], pager: Pager?) {
        self.pager = pager
        var slots = Array(repeating: -1, count: width)
        for (slot, column) in touched.enumerated() { slots[column] = slot }
        self.slots = slots
        values = Array(repeating: .null, count: touched.count)
        stamps = Array(repeating: 0, count: touched.count)
    }

    func begin(_ position: TreePosition) {
        if generation == .max {
            stamps = Array(repeating: 0, count: stamps.count)
            generation = 0
        }
        generation += 1
        self.position = position
    }

    func value(_ column: Int) throws -> SQLValue {
        guard let position, slots.indices.contains(column), slots[column] >= 0 else {
            throw JedError("XX000", "packed lane read is outside the touched columns")
        }
        let slot = slots[column]
        if stamps[slot] != generation {
            values[slot] = try position.materialize(column, pager: pager)
            stamps[slot] = generation
        }
        return values[slot]
    }

    func isNull(_ column: Int) throws -> Bool {
        guard let position, slots.indices.contains(column), slots[column] >= 0 else {
            throw JedError("XX000", "packed lane null test is outside the touched columns")
        }
        return position.cell(column).isNull
    }

    /// Pending overflow bodies belong to the writer and must use materialize.
    /// Other cells borrow immutable encoded slices. Both packed pages and
    /// writer record vectors retain this encoding; no SQL values are stored.
    func storedCell(_ column: Int) throws -> StoredCell? {
        guard let position, slots.indices.contains(column), slots[column] >= 0 else {
            throw JedError("XX000", "packed lane span read is outside the touched columns")
        }
        guard position.node.pending[position.row]?[column] == nil else { return nil }
        return position.cell(column)
    }

    /// Generic predicates keep the scalar lane's eager touched-column order.
    /// Encoded predicates prepare only the columns of a reached fallback.
    func prepare(_ columns: [Int]) throws {
        for column in columns { _ = try value(column) }
    }

    func close() {
        position = nil
        values.removeAll(keepingCapacity: false)
        stamps.removeAll(keepingCapacity: false)
    }
}

/// The existing columnar lane's full/contiguous-primary-key scan, driven by
/// retained leaf positions instead of complete PackedRecords or SQL rows.
/// Bounds, storage-unit preflight, and row checkpoints keep their old order.
internal final class PackedLaneInput {
    let row: PackedLaneRow
    private let meter: CostMeter
    private var cursor: ScanCursor

    init(plan: SelectPlan, context: ExpressionContext) throws {
        guard let store = plan.store else { throw JedError("XX000", "packed lane has no table") }
        let access: ScanAccess
        if try plan.scan.hasKnownContradiction(parameters: context.parameters, collatedKey: context.collatedKey) {
            access = .empty
        } else {
            access = try plan.chosenAccess?.bind(context) ?? plan.scan.bind(context)
        }
        try ScanCost.charge(store, touched: plan.touched, meter: context.meter, access: access)
        cursor = access.cursor(store, descending: plan.descending)
        meter = context.meter
        row = PackedLaneRow(width: store.tree.types.count, touched: plan.touched, pager: store.tree.pool?.pager)
    }

    func nextPosition() throws -> TreePosition? {
        guard let position = try cursor.nextPosition() else { return nil }
        try meter.step(); meter.charge(1)
        return position
    }

    func next() throws -> Bool {
        guard let position = try nextPosition() else { return false }
        row.begin(position)
        return true
    }

    func close() { cursor.close(); row.close() }
    deinit { close() }
}
