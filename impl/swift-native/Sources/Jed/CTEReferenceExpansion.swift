/// Each eligible binding keeps its authored body and the preceding lexical
/// bindings. Definition planning suppresses expansion; its single real
/// reference gets one private body plan for incremental predicate pushdown.
internal struct CTEReferenceExpansion {
    let query: SQLQuery
    let snapshot: DomainSnapshot
    let settings: SessionSettings
    let scalar: ScalarCasts.Context
    let visible: [SQLText: CTEBinding]

    func resolve(using parent: ExpressionResolver) throws -> SelectPlan {
        let counts = visible.values.map { ($0, $0.references) }
        defer { for (binding, count) in counts { binding.references = count } }
        let resolver = ExpressionResolver(composites: snapshot.types, scalar: scalar)
        resolver.cteBindings = visible
        resolver.parameters = parent.parameters; resolver.parameterUses = parent.parameterUses
        resolver.viewExpansion = parent.viewExpansion
        // The definition proved it has no outer references. Expansion sees
        // exactly its preceding WITH bindings, never the referencing FROM.
        let plan = try SelectPlan.resolve(query, snapshot: snapshot, settings: settings,
            scalar: scalar, using: resolver, finalizeParameters: false)
        parent.parameters = resolver.parameters; parent.parameterUses.formUnion(resolver.parameterUses)
        return plan
    }
}
