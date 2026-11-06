/// All parts share one read image per domain, cost/resource account and
/// sequence state. Only RETURNING buffers expose earlier writes to later parts.
internal enum WritableCTEExecution {
    static func execute(_ statement: SQLStatement, snapshot: inout DomainSnapshot,
                        settings: SessionSettings, context: ExpressionContext,
                        parameters: [SQLValue]) throws -> MutationResult? {
        guard let plan = try WritableCTEPlan.resolve(statement, snapshot: snapshot,
            settings: settings, context: context) else { return nil }
        return try execute(plan, snapshot: &snapshot, settings: settings, context: context, parameters: parameters)
    }

    /// Single-domain callers, including the CTAS fill, retain ownership of
    /// their snapshot. A caller spanning domains must supply the complete map.
    static func execute(_ plan: WritableCTEPlan, snapshot: inout DomainSnapshot,
                        settings: SessionSettings, context: ExpressionContext,
                        parameters: [SQLValue]) throws -> MutationResult {
        var working = QueryDomains.statement(snapshot: snapshot, settings: settings)
        working.snapshots[settings.targetDomain] = snapshot
        let result = try execute(plan, working: &working, settings: settings, context: context,
            parameters: parameters, requireWritable: { owner in
                guard owner == settings.targetDomain else {
                    throw JedError("0A000", "this writable WITH caller needs multiple database domains")
                }
            })
        snapshot = try working.snapshot(settings.targetDomain)
        return result
    }

    static func execute(_ statement: SQLStatement, working: inout QueryDomains,
                        settings: SessionSettings, context: ExpressionContext,
                        parameters: [SQLValue], requireWritable: (SQLText) throws -> Void) throws -> MutationResult? {
        var planning = settings; planning.domains = working
        let snapshot = try working.snapshot(settings.targetDomain)
        guard let plan = try WritableCTEPlan.resolve(statement, snapshot: snapshot,
            settings: planning, context: context) else { return nil }
        return try execute(plan, working: &working, settings: planning, context: context,
                           parameters: parameters, requireWritable: requireWritable)
    }

    static func execute(_ plan: WritableCTEPlan, working: inout QueryDomains,
                        settings: SessionSettings, context: ExpressionContext,
                        parameters: [SQLValue], requireWritable: (SQLText) throws -> Void) throws -> MutationResult {
        let readDomains = working
        var pinned = settings; pinned.domains = readDomains
        let journal = ForeignKeyActions.Journal()
        var foreignKeys: [SQLText: ForeignKeyActions.State] = [:]
        func state(_ owner: SQLText) -> ForeignKeyActions.State {
            if let existing = foreignKeys[owner] { return existing }
            let value = ForeignKeyActions.State(deferUntilStatementEnd: true, journal: journal, domain: owner)
            foreignKeys[owner] = value
            return value
        }
        var current = context
        current.parameters = try ParameterBinding.bind(parameters, types: plan.parameters, timezone: settings.timeZone)
        // Every actual DML part installs its own owner-specific pin below.
        current.writeScope = nil
        let runtime = CTEExecution(bindings: plan.bindings, context: current)
        let oldPendingBase = context.meter.pendingDomainBase
        let oldPendingExcluded = context.meter.pendingDomainExcluded
        defer {
            runtime.close()
            context.meter.pendingDomainBase = oldPendingBase
            context.meter.pendingDomainExcluded = oldPendingExcluded
        }

        func mutate(_ statement: SQLStatement, context part: ExpressionContext,
                    visible: [SQLText: CTEBinding], working: inout QueryDomains) throws -> MutationResult {
            let owner = try StatementDomain.target(statement, in: readDomains)
            try requireWritable(owner)
            var executionSettings = pinned; executionSettings.targetDomain = owner
            var execution = part
            execution.writeScope = StatementWriteScope(readSnapshot: try readDomains.snapshot(owner),
                cteBindings: visible, foreignKeys: state(owner))
            var snapshot = try working.snapshot(owner)
            let excluded = owner == DomainNames.temporary
            context.meter.pendingDomainBase = working.pendingBytes - (excluded ? 0 : snapshot.pendingBytes)
            context.meter.pendingDomainExcluded = excluded
            guard let result = try MutationExecutor.execute(statement, snapshot: &snapshot,
                settings: executionSettings, context: execution, parameters: parameters) else {
                throw JedError("0A000", "this data-modifying WITH body is not connected")
            }
            working.snapshots[owner] = snapshot
            return result
        }

        var visible: [SQLText: CTEBinding] = [:]
        for binding in plan.bindings {
            try context.meter.checkpoint()
            if let statement = binding.mutation {
                let result = try mutate(statement, context: runtime.bindingContext(binding),
                                        visible: visible, working: &working)
                defer { result.close() }
                try runtime.adopt(result, for: binding)
            } else if binding.buffers {
                try runtime.prepare(binding)
            }
            visible[binding.name] = binding
        }
        var body = runtime.bodyContext()
        body.patterns = PatternExecution()
        StatementServices(settings: pinned, meter: context.meter).install(into: &body)
        let result: MutationResult
        if let query = plan.query {
            let execution = SelectExecution(plan: query, context: body)
            defer { execution.close() }
            result = MutationResult(command: "SELECT", columns: query.columns, meter: context.meter)
            // Keep CTE buffers and source rows live through result admission.
            do { while let row = try execution.next() { try result.append(row) } }
            catch { result.close(); throw error }
        } else {
            result = try mutate(plan.body, context: body, visible: plan.visibleBindings, working: &working)
        }
        do { try journal.settle(domains: working, settings: pinned, context: body) }
        catch { result.close(); throw error }
        return result
    }
}
