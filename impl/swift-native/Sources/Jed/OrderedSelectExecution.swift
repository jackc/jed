internal struct ResolvedOrderTerm {
    let expression: ResolvedExpression
    let type: TypeDescriptor
    let descending: Bool
    let nullsFirst: Bool
    /// Authored expression keys retain a value slot even if resolution reduces
    /// an identity cast to a column read.
    var materialized = false
}

/// Blocking order keeps only the touched row layout. All ORDER BY expressions
/// precede decoration, and projection begins only after the final row window.
internal final class OrderedSelectExecution {
    private struct Entry {
        let row: [SQLValue]
        var keys: [SQLValue]
        let ordinal: Int
        var bytes: Int64 = 0
        var decorationBytes: Int64 = 0
    }
    private let plan: SelectPlan
    private let context: ExpressionContext
    private let hostBatch: HostBatch?
    private var entries: [Entry] = []
    private var reserved: Int64 = 0
    private var decorations: Int64 = 0
    private var started = false
    private var external: ExternalRowSorter?
    private let ahead = HostReadAhead<[SQLValue]>()
    private var boundedDistinctOn: BlockingDistinctOnExecution?
    private var position = 0
    private var emitted: Int64 = 0
    init(plan: SelectPlan, context: ExpressionContext) {
        self.plan = plan; self.context = context
        hostBatch = HostBatch(expressions: plan.projections, context: context)
    }

    func next() throws -> [SQLValue]? {
        if plan.distinctOn != nil, context.spill?.canSpill == true,
           try !plan.scan.hasKnownContradiction(parameters: context.parameters, collatedKey: context.collatedKey) {
            if boundedDistinctOn == nil { boundedDistinctOn = BlockingDistinctOnExecution(plan: plan, context: context) }
            return try boundedDistinctOn?.next()
        }
        if !started { try prepare(); started = true }
        if let external {
            let available = external.count - min(external.count, plan.offset)
            let remaining = min(available, plan.limit ?? Int64.max) - emitted
            guard remaining > 0 else { return nil }
            let row: [SQLValue], at: Int?
            if hostBatch != nil {
                let cap = HostReadAhead<[SQLValue]>.cap(meter: context.meter,
                    unit: plan.productionCharge ? 1 : 0, remaining: remaining)
                guard let index = try ahead.next(cap: cap, pull: { try external.next() }) else { return nil }
                at = index; row = ahead.rows[index]
                if index == 0 { hostBatch?.reset() }
            } else {
                guard let value = try external.next() else { return nil }
                row = value; at = nil
            }
            emitted += 1
            if plan.productionCharge { context.meter.charge(1); try context.meter.guardCost() }
            var current = context; current.column = { row[$0] }; current.hostBatch = hostBatch
            current.hostBatchColumn = { row.indices.contains($0) ? row[$0] : nil }
            if let at {
                try hostBatch?.setRow(at, count: ahead.rows.count, context: current) { source, column in
                    let values = self.ahead.rows[source]
                    return values.indices.contains(column) ? values[column] : nil
                }
            }
            return try plan.projections.map { try current.evaluate($0) }
        }
        guard position < entries.count, plan.limit == nil || emitted < plan.limit! else { return nil }
        let at = position
        let end = position + Int(min(Int64(entries.count - position), plan.limit.map { $0 - emitted } ?? Int64.max))
        let row = entries[position].row; position += 1; emitted += 1
        if plan.productionCharge { context.meter.charge(1); try context.meter.guardCost() }
        var current = context; current.column = { row[$0] }; current.hostBatch = hostBatch
        current.hostBatchColumn = { row.indices.contains($0) ? row[$0] : nil }
        try hostBatch?.setRow(at, count: end, context: current) { source, column in
            let values = self.entries[source].row
            return values.indices.contains(column) ? values[column] : nil
        }
        return try plan.projections.map { try current.evaluate($0) }
    }
    private func reserve(_ bytes: Int64) throws {
        try context.meter.reserveQuery(bytes); reserved = saturatingAdd(reserved, bytes)
    }
    private func prepare() throws {
        try context.profile(plan.profileIdentity, .sort, excluding: [.source, .filter]) { try prepareBody() }
    }
    private func prepareBody() throws {
        let meter = context.meter
        let streamable: Bool
        if let candidate = plan.chosenAccess {
            switch candidate.path { case .primary, .full: streamable = true; default: streamable = false }
        } else { streamable = plan.scan.pointSets.isEmpty }
        let direct = !plan.materializeSource && streamable && plan.distinctOn == nil && plan.store != nil && plan.relation == nil
            && plan.order.allSatisfy { !plan.materializesOrderKey($0) }
        if direct && !plan.order.contains(where: { Self.collated($0.type) }) {
            if let count = topKCount(), admitsDirectTopK(count) { try prepareTopK(count); return }
            try prepareExternal(); return
        }
        var sourcePlan = plan
        if !direct, plan.store != nil, plan.relation == nil { sourcePlan.materializeSource = true }
        let input = SelectInput(plan: sourcePlan, context: context); defer { input.close() }
        while let row = try input.next() {
            var current = context; current.column = { row[$0] }
            if let predicate = plan.predicate, !input.predicateApplied, try !context.profile(plan.profileIdentity, .filter, { try current.matches(predicate) }) { continue }
            try reserve(MemorySizes.row(row))
            entries.append(Entry(row: row, keys: [], ordinal: entries.count, bytes: MemorySizes.row(row)))
        }
        input.close()
        // Expression keys are evaluated for the complete pre-sort input before
        // any key decoration, even for a LIMIT that will emit no rows.
        for index in entries.indices {
            let row = entries[index].row
            var current = context; current.column = { row[$0] }
            entries[index].keys = Array(repeating: .null, count: plan.order.count)
            for ordinal in plan.keyEvaluationOrder {
                let term = plan.order[ordinal]
                let key = try current.evaluate(term.expression)
                if plan.materializesOrderKey(term) {
                    let bytes = MemorySizes.value(key); try reserve(bytes); entries[index].bytes = saturatingAdd(entries[index].bytes, bytes)
                }
                entries[index].keys[ordinal] = key
            }
        }
        if !plan.distinct, plan.distinctOn == nil, plan.aggregation == nil, plan.window == nil,
           let count = topKCount() {
            try selectMaterializedTopK(count); return
        }
        for index in entries.indices {
            for ordinal in plan.keyEvaluationOrder {
                let value = entries[index].keys[ordinal], type = plan.order[ordinal].type
                if let name = type.collation, name.description != "C", !value.isNull,
                   ExpressionCollation.collatable(type.type) {
                    let units: Int64
                    switch value {
                    case .text(let text): units = Int64(text.scalarCount)
                    case .array(let array): units = array.elements.reduce(0) { total, element in
                        if case .text(let text) = element { return saturatingAdd(total, Int64(text.scalarCount)) }; return total
                    }
                    default: units = 0
                    }
                    if units > 0 { meter.charge(units); try meter.guardCost() }
                    let key = try KeyEncoding.encode(value, type: type, collatedKey: context.collatedKey)
                    let bytes = saturatingAdd(GeneratedSpec.memoryValue, Int64(key.count))
                    try reserve(bytes); decorations = saturatingAdd(decorations, bytes)
                    entries[index].keys[ordinal] = .bytea(key)
                } else {
                    let units = MemorySizes.payload(value)
                    if units > 0 { meter.charge(units); try meter.guardCost() }
                }
            }
        }
        try entries.sort { left, right in
            for ordinal in plan.order.indices {
                let a = left.keys[ordinal], b = right.keys[ordinal], term = plan.order[ordinal]
                if a.isNull != b.isNull { return a.isNull == term.nullsFirst }
                var type = term.type
                if type.collation?.description != nil && type.collation?.description != "C" && ExpressionCollation.collatable(type.type) {
                    type = TypeDescriptor(.scalar(.bytea))
                }
                let order = try ValueComparison.total(a, b, type: type)
                if order != 0 { return term.descending ? order > 0 : order < 0 }
            }
            return left.ordinal < right.ordinal
        }
        if let distinct = plan.distinctOn {
            var kept: [Entry] = []
            for entry in entries {
                if let previous = kept.last, try distinct.same(previous.keys, entry.keys, order: plan.order) {
                    let bytes = entryBytes(entry); context.meter.resources.query.release(bytes); reserved -= bytes
                } else { kept.append(entry) }
            }
            entries = kept
        }
        for index in entries.indices { entries[index].keys = [] }
        context.meter.resources.query.release(decorations); reserved -= decorations; decorations = 0
        position = Int(min(Int64(entries.count), plan.offset))
    }
    private func topKCount() -> Int64? {
        guard let limit = plan.limit else { return nil }
        if limit == 0 { return 0 }
        let (count, overflow) = plan.offset.addingReportingOverflow(limit)
        return overflow ? nil : count
    }
    private func admitsDirectTopK(_ count: Int64) -> Bool {
        guard let config = context.spill, config.canSpill, let store = plan.store else { return true }
        for column in plan.touched {
            guard case .scalar = store.tree.types[column].type,
                  ValueCodec.fixedWidth(store.tree.types[column].type) != nil else { return false }
        }
        let bytes = QueryEstimate.multiply(count, saturatingAdd(8, QueryEstimate.multiply(40, Int64(store.tree.types.count))))
        return bytes <= config.workMemoryBytes
    }
    private func prepareExternal() throws {
        let config = context.spill
        let terms = plan.order
        let sorter = ExternalRowSorter(workMemoryBytes: config?.workMemoryBytes ?? 0,
            host: config?.host, meter: context.meter) { left, right in
                for term in terms {
                    guard case .column(let index) = term.expression.node else { throw JedError("XX000", "direct sort requires column keys") }
                    let a = left[index], b = right[index]
                    if a.isNull != b.isNull { return a.isNull == term.nullsFirst ? -1 : 1 }
                    let order = try ValueComparison.total(a, b, type: term.type)
                    if order != 0 { return term.descending ? -order : order }
                }
                return 0
            }
        external = sorter
        let input = SelectInput(plan: plan, context: context); defer { input.close() }
        while let row = try input.next() {
            var current = context; current.column = { row[$0] }
            if let predicate = plan.predicate, !input.predicateApplied,
               try !context.profile(plan.profileIdentity, .filter, { try current.matches(predicate) }) { continue }
            for term in terms {
                guard case .column(let index) = term.expression.node else { throw JedError("XX000", "direct sort requires column keys") }
                let units = MemorySizes.payload(row[index])
                if units > 0 { context.meter.charge(units); try context.meter.guardCost() }
            }
            try sorter.append(row)
        }
        try sorter.finish()
        var skipped: Int64 = 0
        while skipped < min(sorter.count, plan.offset) { _ = try sorter.next(); skipped += 1 }
    }
    private func before(_ left: Entry, _ right: Entry) throws -> Bool {
        for index in plan.order.indices {
            let term = plan.order[index], a = left.keys[index], b = right.keys[index]
            if a.isNull != b.isNull { return a.isNull == term.nullsFirst }
            let type = Self.collated(term.type) ? TypeDescriptor(.scalar(.bytea)) : term.type
            let order = try ValueComparison.total(a, b, type: type)
            if order != 0 { return term.descending ? order > 0 : order < 0 }
        }
        return left.ordinal < right.ordinal
    }
    private static func collated(_ type: TypeDescriptor) -> Bool {
        guard let name = type.collation, name.description != "C" else { return false }
        return ExpressionCollation.collatable(type.type)
    }
    /// The eager survivor lane first collects all rows and expression keys, then releases
    /// that buffer's logical charge before top-K selection. Only retained
    /// decorations are charged during selection; surviving rows are reserved
    /// again after decoration release (memory.md §§6.4, 6.8).
    private func selectMaterializedTopK(_ count: Int64) throws {
        var pending = entries.map(Optional.some); entries = []
        context.meter.resources.query.release(reserved); reserved = 0
        for index in pending.indices {
            var entry = pending[index]!; pending[index] = nil
            for ordinal in plan.keyEvaluationOrder {
                let value = entry.keys[ordinal], type = plan.order[ordinal].type
                if Self.collated(type), !value.isNull {
                    let units: Int64
                    switch value {
                    case .text(let text): units = Int64(text.scalarCount)
                    case .array(let array): units = array.elements.reduce(0) { n, value in
                        if case .text(let text) = value { return saturatingAdd(n, Int64(text.scalarCount)) }; return n
                    }
                    default: units = 0
                    }
                    if units > 0 { context.meter.charge(units); try context.meter.guardCost() }
                    let key = try KeyEncoding.encode(value, type: type, collatedKey: context.collatedKey)
                    entry.keys[ordinal] = .bytea(key)
                    entry.decorationBytes = saturatingAdd(entry.decorationBytes, saturatingAdd(GeneratedSpec.memoryValue, Int64(key.count)))
                } else {
                    let units = MemorySizes.payload(value)
                    if units > 0 { context.meter.charge(units); try context.meter.guardCost() }
                }
            }
            // LIMIT 0 still builds every key and retains every decoration error.
            guard count > 0 else { continue }
            if Int64(entries.count) < count {
                try reserve(entry.decorationBytes); decorations = saturatingAdd(decorations, entry.decorationBytes)
                entries.append(entry); try raiseWorst(entries.count - 1); continue
            }
            guard try before(entry, entries[0]) else { continue }
            try reserve(entry.decorationBytes); decorations = saturatingAdd(decorations, entry.decorationBytes)
            let bytes = entries[0].decorationBytes
            context.meter.resources.query.release(bytes); reserved -= bytes; decorations -= bytes
            entries[0] = entry; try lowerWorst()
        }
        try entries.sort(by: before)
        for index in entries.indices { entries[index].keys = []; entries[index].decorationBytes = 0 }
        context.meter.resources.query.release(decorations); reserved -= decorations; decorations = 0
        for entry in entries { try reserve(entry.bytes) }
        position = Int(min(Int64(entries.count), plan.offset))
    }
    private func entryBytes(_ entry: Entry) -> Int64 {
        if entry.bytes > 0 { return entry.bytes }
        return zip(plan.order, entry.keys).reduce(MemorySizes.row(entry.row)) { total, pair in
            if !plan.materializesOrderKey(pair.0) { return total }
            return saturatingAdd(total, MemorySizes.value(pair.1))
        }
    }
    private func prepareTopK(_ count: Int64) throws {
        let input = SelectInput(plan: plan, context: context); defer { input.close() }
        var ordinal = 0
        while let row = try input.next() {
            var current = context; current.column = { row[$0] }
            if let predicate = plan.predicate, !input.predicateApplied, try !context.profile(plan.profileIdentity, .filter, { try current.matches(predicate) }) { continue }
            let keys = try plan.order.map { try current.evaluate($0.expression) }
            for key in keys {
                let units = MemorySizes.payload(key)
                if units > 0 { context.meter.charge(units); try context.meter.guardCost() }
            }
            let entry = Entry(row: row, keys: keys, ordinal: ordinal); ordinal += 1
            guard count > 0 else { continue }
            if Int64(entries.count) < count {
                try reserve(entryBytes(entry)); entries.append(entry); try raiseWorst(entries.count - 1); continue
            }
            guard try before(entry, entries[0]) else { continue }
            // Admission precedes eviction; both rows are momentarily live.
            try reserve(entryBytes(entry))
            let bytes = entryBytes(entries[0]); context.meter.resources.query.release(bytes); reserved -= bytes
            entries[0] = entry; try lowerWorst()
        }
        try entries.sort(by: before)
        for index in entries.indices { entries[index].keys = [] }
        position = Int(min(Int64(entries.count), plan.offset))
    }
    // Max heap under the stable ORDER BY relation: the least desirable retained
    // row stays at zero, so each input performs at most logarithmic comparisons.
    private func raiseWorst(_ start: Int) throws {
        var child = start
        while child > 0 {
            let parent = (child - 1) / 2
            guard try before(entries[parent], entries[child]) else { return }
            entries.swapAt(parent, child); child = parent
        }
    }
    private func lowerWorst() throws {
        var parent = 0
        while parent < entries.count / 2 {
            let left = parent * 2 + 1, right = left + 1
            var child = left
            if right < entries.count, try before(entries[left], entries[right]) { child = right }
            guard try before(entries[parent], entries[child]) else { return }
            entries.swapAt(parent, child); parent = child
        }
    }
    func close() {
        hostBatch?.reset(); ahead.reset()
        boundedDistinctOn?.close(); boundedDistinctOn = nil
        external?.close(); external = nil; entries = []; context.meter.resources.query.release(reserved); reserved = 0; decorations = 0 }
    deinit { close() }
}
