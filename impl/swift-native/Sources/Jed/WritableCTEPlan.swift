/// A writable WITH has one early query/schema-planning phase. DML source and
/// assignment resolution stays in the same executor used by standalone DML.
internal struct WritableCTEPlan {
    let inference: ParameterInference
    let bindings: [CTEBinding]
    let body: SQLStatement
    let query: SelectPlan?
    let parameters: [TypeDescriptor]
    let visibleBindings: [SQLText: CTEBinding]

    static func extract(_ statement: SQLStatement) -> (clause: SQLWith, body: SQLStatement)? {
        switch statement {
        case .with(let clause, let body): return (clause, body)
        case .query(let query):
            if case .with(let clause, let child) = query.body {
                let body = query.orderBy.isEmpty && query.limit == nil && query.offset == nil ? child
                    : SQLQuery(body: .nested(child), orderBy: query.orderBy, limit: query.limit, offset: query.offset, span: query.span)
                return (clause, .query(body))
            }
            if case .nested(let child) = query.body, query.orderBy.isEmpty, query.limit == nil, query.offset == nil {
                return extract(.query(child))
            }
            return nil
        default: return nil
        }
    }
    static func isMutation(_ statement: SQLStatement) -> Bool {
        switch statement { case .insert, .update, .delete, .merge: true; default: false }
    }
    static func containsWrite(_ statement: SQLStatement) -> Bool {
        guard let part = extract(statement) else { return false }
        return isMutation(part.body) || part.clause.bindings.contains { isMutation($0.body) }
    }
    static func resolve(_ statement: SQLStatement, snapshot: DomainSnapshot, settings: SessionSettings,
                        context: ExpressionContext) throws -> Self? {
        guard let part = extract(statement), containsWrite(statement) else { return nil }
        let resolver = ExpressionResolver(composites: snapshot.types, scalar: context.scalar)
        resolver.hostFunctions = settings.environment.signatures; resolver.unicode = settings.environment.unicode
        resolver.queryScope = SubqueryScope(snapshot: snapshot, settings: settings)
        resolver.checkFunction = { try settings.privileges.require(.execute, on: $0, function: true) }
        var sharedContext = context
        sharedContext.parameterInference = resolver.inference
        let bindings = try WithQueryPlan.resolveBindings(part.clause, snapshot: snapshot, settings: settings,
            scalar: context.scalar, using: resolver, mutationContext: sharedContext, primary: part.body)
        let query = try part.body.queryBody.map {
            try SelectPlan.resolve($0, snapshot: snapshot, settings: settings, scalar: context.scalar,
                                   using: resolver, finalizeParameters: false)
        }
        let domains = QueryDomains.statement(snapshot: snapshot, settings: settings)
        for statement in bindings.compactMap(\.mutation) + (isMutation(part.body) ? [part.body] : []) {
            let owner = try StatementDomain.target(statement, in: domains)
            let read = try domains.snapshot(owner)
            var partSettings = settings; partSettings.targetDomain = owner
            sharedContext.writeScope = StatementWriteScope(readSnapshot: read, cteBindings: resolver.cteBindings, foreignKeys: ForeignKeyActions.State())
            _ = try MutationExecutor.prepare(statement, snapshot: read, settings: partSettings, context: sharedContext)
        }
        // Preparation counted each DML source once. Freeze the chosen mode
        // before execution prepares those sources again with bound values.
        for binding in bindings { binding.executionBuffers = binding.buffers }
        return Self(inference: resolver.inference, bindings: bindings, body: part.body, query: query,
                    parameters: try ParameterBinding.finalize(resolver), visibleBindings: resolver.cteBindings)
    }
}
