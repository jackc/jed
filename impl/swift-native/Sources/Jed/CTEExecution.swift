/// Each invocation captures the WITH node's enclosing rows. An inline binding
/// reached through a deeper subquery still evaluates in this captured frame.
internal final class CTEExecution {
    private let parent: CTEExecution?
    private let context: ExpressionContext
    private let bindings: [ObjectIdentifier: CTEBinding]
    private var rows: [ObjectIdentifier: [[SQLValue]]] = [:]
    private var bytes: Int64 = 0
    private var working: [ObjectIdentifier: [[SQLValue]]] = [:]
    init(plan: WithQueryPlan, context: ExpressionContext) {
        parent = context.ctes; self.context = context
        bindings = Dictionary(uniqueKeysWithValues: plan.bindings.map { (ObjectIdentifier($0), $0) })
    }
    func bodyContext() -> ExpressionContext {
        var current = context; current.ctes = self; current.subqueries = SubqueryExecution()
        return current
    }
    private func bindingContext(_ binding: CTEBinding) -> ExpressionContext {
        var current = bodyContext(); current.patterns = PatternExecution()
        StatementServices(settings: binding.settings, meter: context.meter).install(into: &current)
        return current
    }
    func prepare(_ plan: WithQueryPlan) throws {
        for binding in plan.bindings where binding.references > 0 && binding.buffers {
            if let recursive = binding.recursive {
                let key = ObjectIdentifier(binding)
                defer { working.removeValue(forKey: key) }
                let prior = rows.values.reduce(Int64(0)) { total, rows in
                    rows.reduce(total) { saturatingAdd($0, MemorySizes.row($1)) }
                }
                let execution = RecursiveCTEExecution(binding: binding, recursive: recursive, context: bindingContext(binding))
                let result = try execution.run(priorBufferBytes: prior) { working[key] = $0 }
                rows[key] = result.rows; bytes = saturatingAdd(bytes, result.bytes)
                continue
            }
            let execution = SelectExecution(plan: binding.plan, context: bindingContext(binding))
            defer { execution.close() }
            var output: [[SQLValue]] = []
            while let row = try execution.next() {
                let charge = MemorySizes.row(row)
                try context.meter.reserveQuery(charge); bytes = saturatingAdd(bytes, charge)
                output.append(row)
            }
            rows[ObjectIdentifier(binding)] = output
        }
    }
    func scan(_ binding: CTEBinding, visit: ([SQLValue]) throws -> Void) throws {
        let key = ObjectIdentifier(binding)
        guard bindings[key] != nil else {
            guard let parent else { throw JedError("XX000", "missing WITH binding execution") }
            try parent.scan(binding, visit: visit); return
        }
        if binding.buffers {
            guard let values = working[key] ?? rows[key] else { throw JedError("XX000", "WITH binding read before materialization") }
            for row in values { context.meter.charge(1); try context.meter.step(); try visit(row) }
        } else {
            let execution = SelectExecution(plan: binding.plan, context: bindingContext(binding))
            defer { execution.close() }
            while let row = try execution.next() { try visit(row) }
        }
    }
    func close() {
        rows = [:]; working = [:]
        context.meter.resources.query.release(bytes); bytes = 0
    }
    deinit { close() }
}

internal final class WithQueryExecution {
    private let plan: WithQueryPlan
    private let context: ExpressionContext
    private var runtime: CTEExecution?
    private var body: SelectExecution?
    private var rows: [[SQLValue]?] = []
    private var reserved: Int64 = 0
    private var position = 0
    private var started = false
    init(plan: WithQueryPlan, context: ExpressionContext) { self.plan = plan; self.context = context }
    func next() throws -> [SQLValue]? {
        if !started { try prepare(); started = true }
        guard position < rows.count, let row = rows[position] else { return nil }
        rows[position] = nil; position += 1
        let bytes = MemorySizes.row(row)
        context.meter.resources.query.release(bytes); reserved -= bytes
        return row
    }
    private func prepare() throws {
        do {
            let current = CTEExecution(plan: plan, context: context)
            runtime = current
            try current.prepare(plan)
            body = SelectExecution(plan: plan.body, context: current.bodyContext())
            // WITH owns a deferred projected result. During collection its
            // binding buffers and the body's retained source remain live.
            while let row = try body?.next() {
                let bytes = MemorySizes.row(row)
                try context.meter.reserveQuery(bytes); reserved = saturatingAdd(reserved, bytes)
                rows.append(row)
            }
            body?.close(); body = nil
            current.close(); runtime = nil
        } catch { close(); throw error }
    }
    func close() {
        body?.close(); body = nil; runtime?.close(); runtime = nil
        rows = []; context.meter.resources.query.release(reserved); reserved = 0
    }
    deinit { close() }
}
