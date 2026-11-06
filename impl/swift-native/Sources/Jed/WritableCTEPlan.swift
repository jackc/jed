/// A writable WITH has one early query/schema-planning phase. DML source and
/// assignment resolution stays in the same executor used by standalone DML.
internal struct WritableCTEPlan {
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
        let bindings = try WithQueryPlan.resolveBindings(part.clause, snapshot: snapshot, settings: settings,
            scalar: context.scalar, using: resolver, mutationContext: context)
        let query = try part.body.queryBody.map {
            try SelectPlan.resolve($0, snapshot: snapshot, settings: settings, scalar: context.scalar,
                                   using: resolver, finalizeParameters: false)
        }
        // Query parts have been counted by resolution. DML parts have only had
        // their RETURNING schemas planned; count their full syntax exactly once.
        for statement in bindings.compactMap(\.mutation) + (isMutation(part.body) ? [part.body] : []) {
            for (name, count) in CTEReferenceCounts.count(statement) {
                resolver.cteBindings[name]?.references += count
            }
        }
        // DML resolution runs later and visits its references again. Its static
        // tally already chose these modes; late visits must not turn an inline
        // CTE into a buffer which the lexical materialization pass skipped.
        for binding in bindings { binding.executionBuffers = binding.buffers }
        return Self(bindings: bindings, body: part.body, query: query,
                    parameters: try ParameterBinding.finalize(resolver), visibleBindings: resolver.cteBindings)
    }
}
