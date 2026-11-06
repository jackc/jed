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
    private var bounded: BlockingDistinctExecution?
    private var seen: Set<[UInt8]> = []
    private var seenBytes: Int64 = 0
    private var spillSeen: SpillStateMap?
    private var skipped: Int64 = 0
    private var emitted: Int64 = 0
    init(plan: SelectPlan, context: ExpressionContext) { self.plan = plan; self.context = context }
    func openCursor() throws {
        guard plan.orderedByScan else { return }
        ensureStreaming()
        try streaming!.openCursor()
    }
    private func reserve(_ bytes: Int64) throws {
        try context.meter.reserveQuery(bytes); reserved = saturatingAdd(reserved, bytes)
    }
    private func release(_ bytes: Int64) { context.meter.resources.query.release(bytes); reserved -= bytes }
    private func ensureStreaming() {
        guard streaming == nil else { return }
        let execution = SelectExecution(plan: inputPlan(), context: context)
        execution.hostBatchRemaining = { [weak self] in
            guard let self, let limit = self.plan.limit else { return nil }
            return saturatingAdd(self.plan.offset - self.skipped, limit - self.emitted)
        }
        streaming = execution
    }
    func next() throws -> [SQLValue]? {
        if plan.orderedByScan {
            guard buffersStreamingOutput else { return try stream() }
            if !started { try prepareStreamingOutput(); started = true }
            guard position < rows.count, let row = rows[position] else { return nil }
            rows[position] = nil; position += 1; release(MemorySizes.row(row))
            return row
        }
        if context.spill?.canSpill == true,
           try !plan.scan.hasKnownContradiction(parameters: context.parameters, collatedKey: context.collatedKey) {
            if bounded == nil { bounded = BlockingDistinctExecution(plan: plan, context: context) }
            return try bounded?.next()
        }
        if !started { try prepare(); started = true }
        guard position < rows.count, let row = rows[position] else { return nil }
        if plan.productionCharge { context.meter.charge(1); try context.meter.guardCost() }
        rows[position] = nil; position += 1; release(MemorySizes.row(row)); return row
    }
    private func inputPlan() -> SelectPlan {
        var input = SelectPlan(columns: plan.columns, projections: plan.projections, predicate: plan.predicate,
            store: plan.store, touched: plan.touched, parameters: plan.parameters, limit: nil, offset: 0, order: plan.order,
            orderedByScan: plan.orderedByScan, descending: plan.descending, scan: plan.scan, aggregation: plan.aggregation,
            relation: plan.relation, productionCharge: false, window: plan.window,
            sourceDomain: plan.sourceDomain, sourceSnapshot: plan.sourceSnapshot, sourceTable: plan.sourceTable,
            profileIdentity: plan.profileIdentity, profilesRoot: false)
        input.accessCandidates = plan.accessCandidates; input.chosenAccess = plan.chosenAccess
        input.chosenJoin = plan.chosenJoin; input.chosenNWay = plan.chosenNWay; input.completePredicate = plan.completePredicate
        input.materializeSource = !plan.orderedByScan
        return input
    }
    private func stream() throws -> [SQLValue]? {
        if plan.limit.map({ emitted >= $0 }) == true { return nil }
        ensureStreaming()
        while let row = try streaming?.next() {
            for value in row {
                let units = MemorySizes.payload(value)
                if units > 0 { context.meter.charge(units); try context.meter.guardCost() }
            }
            if let config = context.spill, config.canSpill {
                if spillSeen == nil { spillSeen = SpillStateMap(config: config) }
                guard try spillSeen!.insert(row) else { continue }
            } else {
                let key = try ValueEqualityKey.encode(row)
                if seen.contains(key) { continue }
                let bytes = row.reduce(GeneratedSpec.memoryEntry) { saturatingAdd($0, MemorySizes.value($1)) }
                try reserve(bytes); seenBytes = saturatingAdd(seenBytes, bytes)
                seen.insert(key)
            }
            if skipped < plan.offset { skipped += 1; continue }
            if plan.productionCharge { context.meter.charge(1); try context.meter.guardCost() }
            emitted += 1; return row
        }
        return nil
    }
    private var buffersStreamingOutput: Bool {
        guard let access = plan.chosenAccess else { return false }
        switch access.path {
        case .primary, .full: return false
        default: return true
        }
    }
    private func prepareStreamingOutput() throws {
        // Generalized scan execution completes its final survivors before the
        // public cursor yields. The dedup table overlaps that result buffer.
        while let row = try stream() {
            try reserve(MemorySizes.row(row)); rows.append(row)
        }
        streaming?.close(); streaming = nil
        spillSeen?.close(); spillSeen = nil
        seen = []; release(seenBytes); seenBytes = 0
    }
    private func prepare() throws {
        try context.profile(plan.profileIdentity, .distinct, excluding: [.source, .folded, .filter, .aggregate, .window, .sort]) { try prepareBody() }
    }
    private func prepareBody() throws {
        let meter = context.meter
        let execution = SelectExecution(plan: inputPlan(), context: context); defer { execution.close() }
        var seen: Set<[UInt8]> = [], output: [[SQLValue]] = [], entries: Int64 = 0
        while let row = try execution.next() {
            for value in row {
                let units = MemorySizes.payload(value)
                if units > 0 { meter.charge(units); try meter.guardCost() }
            }
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
    func close() { spillSeen?.close(); spillSeen = nil; bounded?.close(); bounded = nil; streaming?.close(); streaming = nil; rows = []; seen = []; seenBytes = 0; context.meter.resources.query.release(reserved); reserved = 0 }
    deinit { close() }
}
