/// The previous iteration is the only self-reference input. Result copies and
/// the seen table have distinct logical owners even when Swift shares storage.
internal final class RecursiveCTEExecution {
    private let binding: CTEBinding
    private let anchorPlan: SelectPlan
    private let recursive: RecursiveCTEPlan
    private let context: ExpressionContext
    private var reserved: Int64 = 0
    init(binding: CTEBinding, anchor: SelectPlan, recursive: RecursiveCTEPlan, context: ExpressionContext) {
        self.binding = binding; self.anchorPlan = anchor; self.recursive = recursive; self.context = context
    }
    private func reserve(_ bytes: Int64) throws {
        try context.meter.reserveQuery(bytes); reserved = saturatingAdd(reserved, bytes)
    }
    private func release(_ bytes: Int64) {
        context.meter.resources.query.release(bytes); reserved -= bytes
    }
    private func measure(_ rows: [[SQLValue]]) -> Int64 {
        rows.reduce(0) { saturatingAdd($0, MemorySizes.row($1)) }
    }
    private func collect(_ plan: SelectPlan) throws -> [[SQLValue]] {
        var current = context; current.patterns = PatternExecution(); current.subqueries = SubqueryExecution()
        current.meter = CostMeter(limit: context.meter.limit, lifetime: context.meter.lifetime,
            resources: context.meter.resources, cancellation: context.meter.cancellation)
        current.scalar.meter = current.meter
        current.sequences?.install(into: &current)
        StatementServices(settings: binding.settings, meter: current.meter).install(into: &current)
        let execution = SelectExecution(plan: plan, context: current)
        defer { execution.close() }
        var result: [[SQLValue]] = []
        while let row = try execution.next() {
            let bytes = MemorySizes.row(row)
            try current.meter.reserveQuery(bytes); reserved = saturatingAdd(reserved, bytes)
            result.append(row)
        }
        try current.meter.guardCost()
        execution.close()
        context.meter.absorbChildCost(current.meter.accrued)
        try context.meter.guardCost()
        return result
    }
    private func adjust(from before: Int64, to after: Int64) throws {
        if after >= before { try reserve(after - before) } else { release(before - after) }
    }
    private func augment(_ rows: [[SQLValue]], anchor: Bool) throws -> [[SQLValue]] {
        guard let columns = recursive.augmentation else { return rows }
        let before = measure(rows), expressions = anchor ? columns.anchor : columns.recursive
        var output: [[SQLValue]] = []
        for row in rows {
            try context.meter.step()
            var current = context
            current.column = { index in
                guard row.indices.contains(index) else { throw JedError("XX000", "recursive augmentation column is outside its row") }
                return row[index]
            }
            if !anchor, let predicate = columns.filter, !(try current.matches(predicate)) { continue }
            let values = try expressions.map { try current.evaluate($0) }
            output.append(Array(row.prefix(anchorPlan.columns.count)) + values)
        }
        try adjust(from: before, to: measure(output))
        try context.meter.guardCost()
        return output
    }
    func run(priorBufferBytes: Int64, working: ([[SQLValue]]) -> Void) throws -> (rows: [[SQLValue]], bytes: Int64) {
        var result: [[SQLValue]] = [], next: [[SQLValue]] = [], active: [[SQLValue]] = []
        var resultBytes: Int64 = 0, seenBytes: Int64 = 0
        var seen: Set<[UInt8]> = []
        func retain(_ rows: [[SQLValue]]) throws {
            next = []
            for row in rows {
                if !recursive.all {
                    // Frozen implementations guard one combined row-key charge.
                    // This preserves their observable abort total as well as the
                    // successful total; cost.md's per-key wording is stricter.
                    let units = row.reduce(Int64(0)) { saturatingAdd($0, MemorySizes.payload($1)) }
                    if units > 0 { context.meter.charge(units); try context.meter.guardCost() }
                    let key = try ValueEqualityKey.encode(row)
                    if seen.contains(key) { release(MemorySizes.row(row)); continue }
                    let charge = row.reduce(GeneratedSpec.memoryEntry) { saturatingAdd($0, MemorySizes.value($1)) }
                    try reserve(charge); seenBytes = saturatingAdd(seenBytes, charge); seen.insert(key)
                }
                let charge = MemorySizes.row(row)
                try reserve(charge); resultBytes = saturatingAdd(resultBytes, charge)
                result.append(row); next.append(row)
            }
        }
        try retain(augment(collect(anchorPlan), anchor: true))
        // Earlier materialized bindings remain visible to the term. Their
        // independent retained buffer copy is admitted once, after the seed.
        try reserve(priorBufferBytes)
        while !next.isEmpty {
            try context.meter.pollStep()
            release(measure(active)); active = next; next = []
            working(active)
            var produced = try collect(recursive.term)
            let before = measure(produced)
            for row in produced.indices {
                for column in anchorPlan.columns.indices {
                    if let target = anchorPlan.columns[column].type {
                        produced[row][column] = try context.convert(produced[row][column],
                            from: recursive.term.columns[column].type?.type, to: target)
                    }
                }
            }
            try adjust(from: before, to: measure(produced))
            try retain(augment(produced, anchor: false))
            try context.meter.guardCost()
        }
        working([])
        release(measure(active)); release(priorBufferBytes); release(seenBytes)
        // Transfer the full result reservation to the WITH runtime.
        precondition(reserved == resultBytes)
        reserved = 0
        return (result, resultBytes)
    }
    deinit { context.meter.resources.query.release(reserved) }
}
