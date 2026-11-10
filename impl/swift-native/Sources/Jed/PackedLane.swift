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
    private var node: TreeNode?
    /// The clean packed page of `node`, when values can decode in place.
    private var packed: PackedLeaf?
    private var index = 0

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
        // Rows arrive leaf by leaf; swap the retained node only when it changes.
        if node !== position.node {
            node = position.node
            packed = position.node.pending.isEmpty ? position.node.leaf : nil
        }
        index = position.row
    }

    func value(_ column: Int) throws -> SQLValue {
        guard let node, column >= 0, column < slots.count, slots[column] >= 0 else {
            throw JedError("XX000", "packed lane read is outside the touched columns")
        }
        let slot = slots[column]
        if stamps[slot] != generation {
            if let packed, let value = try packed.fastValue(row: index, column: column) { values[slot] = value }
            else { values[slot] = try TreePosition(node: node, row: index).materialize(column, pager: pager) }
            stamps[slot] = generation
        }
        return values[slot]
    }

    func isNull(_ column: Int) throws -> Bool {
        guard let node, column >= 0, column < slots.count, slots[column] >= 0 else {
            throw JedError("XX000", "packed lane null test is outside the touched columns")
        }
        return packed?.isNull(row: index, column: column) ?? node.isNull(row: index, column: column)
    }

    /// The current row on a clean packed page, for predicates that read its
    /// bytes in place; nil on a writer leaf or one with pending overflow.
    func packedPosition() -> (leaf: PackedLeaf, row: Int)? {
        guard let packed else { return nil }
        return (packed, index)
    }

    /// Pending overflow bodies belong to the writer and must use materialize.
    /// Other cells borrow immutable encoded slices. Both packed pages and
    /// writer record vectors retain this encoding; no SQL values are stored.
    func storedCell(_ column: Int) throws -> StoredCell? {
        guard let node, column >= 0, column < slots.count, slots[column] >= 0 else {
            throw JedError("XX000", "packed lane span read is outside the touched columns")
        }
        if !node.pending.isEmpty, node.pending[index]?[column] != nil { return nil }
        return node.cell(row: index, column: column)
    }

    /// Generic predicates keep the scalar lane's eager touched-column order.
    /// Encoded predicates prepare only the columns of a reached fallback.
    func prepare(_ columns: [Int]) throws {
        for column in columns { _ = try value(column) }
    }

    func close() {
        node = nil; packed = nil
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
    private struct Point { var position: TreePosition? }
    private var point: Point?

    init(plan: SelectPlan, context: ExpressionContext) throws {
        guard let store = plan.store else { throw JedError("XX000", "packed lane has no table") }
        let access: ScanAccess
        if try plan.scan.hasKnownContradiction(parameters: context.parameters, collatedKey: context.collatedKey) {
            access = .empty
        } else {
            access = try plan.chosenAccess?.bind(context) ?? plan.scan.bind(context)
        }
        meter = context.meter
        if let key = access.pointKey {
            // One key: the cost preflight's lookup also serves the scan.
            let found = try ScanCost.chargePoint(store, key: key,
                variable: ScanCost.variableColumns(plan.touched, types: store.tree.types), meter: context.meter)
            point = Point(position: found)
            cursor = ScanAccess.empty.cursor(store)
        } else {
            try ScanCost.charge(store, touched: plan.touched, meter: context.meter, access: access)
            cursor = access.cursor(store, descending: plan.descending)
        }
        row = PackedLaneRow(width: store.tree.types.count, touched: plan.touched, pager: store.tree.pool?.pager)
    }

    /// The row of a single-key access, yielded once.
    private func takePoint() throws -> TreePosition? {
        guard let held = point else { return nil }
        point = Point(position: nil)
        return held.position
    }

    func nextPosition() throws -> TreePosition? {
        if point != nil {
            guard let position = try takePoint() else { return nil }
            try meter.step(); meter.charge(1)
            return position
        }
        guard let position = try cursor.nextPosition() else { return nil }
        try meter.step(); meter.charge(1)
        return position
    }

    /// Leaf-at-a-time form of `nextPosition`. The caller admits each row of
    /// the run with `admitRow` before using it, keeping the per-row order of
    /// cancellation polls and charges.
    func nextRun() throws -> TreeRun? {
        if point != nil {
            return try takePoint().map { TreeRun(node: $0.node, first: $0.row, count: 1, descending: false) }
        }
        return try cursor.nextRun()
    }
    @inline(__always) func admitRow() throws { try meter.step(); meter.charge(1) }

    func next() throws -> Bool {
        guard let position = try nextPosition() else { return false }
        row.begin(position)
        return true
    }

    func close() { cursor.close(); row.close(); point = nil }
    deinit { close() }
}
