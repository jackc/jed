/// EXPLAIN owns display rows; ANALYZE's inner statement keeps a separate meter
/// sharing the same lifetime/resource boundary. The session owns publication.
internal struct ExplainPlan {
    enum Body {
        case query(SelectPlan)
        case mutation(MutationExecutor.Preparation)
        case createTable(SQLCreateTable, CreateTableAs.Preparation)
    }
    let options: SQLExplain
    let statement: SQLStatement
    let body: Body
    let root: ExplainNode?
    let profile: ExecutionProfile
    let lane: String
    let parameters: [SQLValue]
    let targetDomain: SQLText
    var query: SelectPlan? { if case .query(let query) = body { query } else { nil } }
    var writes: Bool {
        switch body { case .query(let plan): SequenceExecution.mutates(plan.expressions); case .mutation, .createTable: true }
    }

    static func resolve(_ options: SQLExplain, statement: SQLStatement, snapshot: DomainSnapshot,
                        settings: SessionSettings, context: ExpressionContext,
                        parameters: [SQLValue]) throws -> Self {
        let statement = CreateTableAs.normalize(statement)
        let domains = QueryDomains.statement(snapshot: snapshot, settings: settings)
        var planning = settings; planning.domains = domains
        let profile = ExecutionProfile()
        let body: Body, types: [TypeDescriptor], skipped: Bool
        if case .query(let syntax) = statement, !WritableCTEPlan.containsWrite(statement) {
            let plan = try SelectPlan.resolve(syntax, snapshot: snapshot, settings: planning, scalar: context.scalar)
            body = .query(plan); types = plan.parameters; skipped = false
        } else if case .createTable(let definition) = statement, definition.query != nil {
            planning.targetDomain = try StatementDomain.target(statement, in: domains)
            let preparation = try CreateTableAs.prepare(definition, working: domains, settings: planning, context: context,
                parameters: parameters.isEmpty && !options.analyze ? nil : parameters)
            guard preparation.writable == nil else { throw JedError("0A000", "EXPLAIN of a writable WITH is not yet connected") }
            body = .createTable(definition, preparation); types = preparation.plan.parameters; skipped = preparation.skipped
        } else {
            planning.targetDomain = try StatementDomain.target(statement, in: domains)
            guard let preparation = try MutationExecutor.prepare(statement, snapshot: domains.snapshot(planning.targetDomain),
                settings: planning, context: context) else { throw JedError("0A000", "EXPLAIN of this statement is not yet connected") }
            body = .mutation(preparation); types = preparation.parameterTypes; skipped = false
        }
        let bound = parameters.isEmpty && !options.analyze ? [] : try ParameterBinding.bind(parameters, types: types, timezone: settings.timeZone)
        var profiled = context; profiled.executionProfile = profile; profiled.parameters = bound
        let tree = ExplainTree(options: options, parameters: parameters.isEmpty ? nil : bound, context: profiled)
        let root: ExplainNode?
        switch body {
        case .query(let plan): root = try tree.query(plan)
        case .mutation(let plan): root = try tree.mutation(plan)
        case .createTable(_, let preparation): root = skipped ? nil : try tree.query(preparation.plan)
        }
        let lane: String
        if case .query(let plan) = body, !SequenceExecution.mutates(plan.expressions) {
            if plan.withQuery != nil || plan.compound != nil { lane = "deferred" }
            else if plan.relation == nil, plan.aggregation == nil, plan.window == nil, !plan.distinct,
                    plan.distinctOn == nil, plan.order.isEmpty || plan.orderedByScan,
                    plan.limit != nil || plan.orderedByScan { lane = "streaming" }
            else { lane = "buffered" }
        } else { lane = "buffered" }
        return Self(options: options, statement: statement, body: body, root: root, profile: profile, lane: lane,
                    parameters: bound, targetDomain: planning.targetDomain)
    }
    var columns: [ResultColumn] {
        func column(_ name: String, _ scalar: ScalarType) -> ResultColumn {
            ResultColumn(name: SQLText(validatedUTF8: Array(name.utf8)), type: TypeDescriptor(.scalar(scalar)))
        }
        var result = [column("depth", .i32), column("node", .text), column("detail", .text)]
        if options.costs { result += [column("est_rows", .i64), column("est_cost", .i64)] }
        if options.analyze { result.append(column("actual_cost", .i64)) }
        if options.lane { result.append(column("lane", .text)) }
        return result
    }
    func context(from parent: ExpressionContext) -> ExpressionContext {
        var context = parent; context.executionProfile = profile; context.parameters = parameters
        return context
    }
    /// Execute the already resolved inner statement. Result rows remain admitted
    /// until their count is known, then release before display-row allocation.
    func analyze(working: inout QueryDomains, settings: SessionSettings, context parent: ExpressionContext,
                 requireWritable: (SQLText) throws -> Void,
                 checkRole: () throws -> Void = {}) throws -> Int64 {
        let context = self.context(from: parent)
        var scoped = settings; scoped.domains = working; scoped.targetDomain = targetDomain
        let result: MutationResult
        switch body {
        case .query(let query):
            let execution = SelectExecution(plan: query, context: context); defer { execution.close() }
            result = MutationResult(command: "SELECT", columns: query.columns, meter: context.meter)
            while true {
                try checkRole()
                guard let row = try execution.next() else { break }
                try result.append(row); result.affected += 1
            }
            execution.close()
        case .mutation(let prepared):
            try requireWritable(targetDomain)
            var snapshot = try working.snapshot(targetDomain)
            let base = context.meter.pendingDomainBase, excluded = context.meter.pendingDomainExcluded
            let temporary = targetDomain == DomainNames.temporary
            context.meter.pendingDomainBase = working.pendingBytes - (temporary ? 0 : snapshot.pendingBytes)
            context.meter.pendingDomainExcluded = temporary
            defer { context.meter.pendingDomainBase = base; context.meter.pendingDomainExcluded = excluded }
            result = try MutationExecutor.execute(prepared, snapshot: &snapshot, context: context, parameters: parameters)
            working.snapshots[targetDomain] = snapshot
        case .createTable(let definition, let prepared):
            try requireWritable(targetDomain)
            result = try CreateTableAs.execute(definition, prepared: prepared, working: &working,
                settings: scoped, context: context, parameters: parameters, requireWritable: requireWritable)
        }
        defer { result.close() }
        if result.columns.isEmpty { return result.affected }
        var count: Int64 = 0
        while result.next() != nil { count = saturatingAdd(count, 1) }
        return count
    }
    func render(meter: CostMeter, analyzedCost: Int64 = 0, analyzedRows: Int64 = 0) throws -> MutationResult {
        let result = MutationResult(command: "EXPLAIN", columns: columns, meter: meter)
        guard let root else { return result }
        var count: Int64 = options.analyze ? 1 : 0, counting = [root]
        while let node = counting.popLast() { count = saturatingAdd(count, 1); counting.append(contentsOf: node.children) }
        // This is intentionally one unguarded outcome charge. Q1/cancellation
        // admission still occurs for every displayed row (memory.md §5.1).
        meter.charge(count)
        func row(depth: Int, name: String, detail: String, rows: Int64, cost: Int64, actual: Int64) throws {
            var values: [SQLValue] = [.integer(Int64(depth)), .text(SQLText(validatedUTF8: Array(name.utf8))), .text(SQLText(validatedUTF8: Array(detail.utf8)))]
            if options.costs { values += [.integer(rows), .integer(cost)] }
            if options.analyze { values.append(.integer(actual)) }
            if options.lane { values.append(.text(SQLText(validatedUTF8: Array(lane.utf8)))) }
            try result.append(values)
        }
        do {
            if options.analyze {
                try row(depth: 0, name: "Analyze", detail: "cost=\(analyzedCost) rows=\(analyzedRows)", rows: root.rows, cost: root.cost, actual: analyzedCost)
            }
            var pending = [(root, options.analyze ? 1 : 0)]
            while let (node, depth) = pending.popLast() {
                try row(depth: depth, name: node.name, detail: node.detail, rows: node.rows, cost: node.cost, actual: node.actual?() ?? 0)
                pending.append(contentsOf: node.children.reversed().map { ($0, depth + 1) })
            }
            return result
        } catch { result.close(); throw error }
    }
}
