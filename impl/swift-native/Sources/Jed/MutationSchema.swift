/// Early WITH planning uses the ordinary target and RETURNING implementations.
/// It deliberately leaves assignments, predicates and source rows for the
/// ordinary mutation executor, preserving their execution-time error order.
extension MutationExecutor {
    static func prepareReturning(_ statement: SQLStatement, snapshot: DomainSnapshot,
                                 settings: SessionSettings, context: ExpressionContext,
                                 using shared: ExpressionResolver) throws -> MutationSchema {
        let name: SQLName, returning: SQLReturning?, deleting: Bool, alias: SQLText?, operation: String
        switch statement {
        case .insert(let syntax):
            name = syntax.table; returning = syntax.returning; deleting = false
            alias = syntax.alias; operation = "insert into"
        case .update(let syntax):
            name = syntax.table; returning = syntax.returning; deleting = false
            alias = nil; operation = "update"
        case .delete(let syntax):
            name = syntax.table; returning = syntax.returning; deleting = true
            alias = nil; operation = "delete from"
        default: throw JedError("0A000", "this data-modifying WITH body is not connected")
        }
        try SystemRelationPlan.rejectTarget(name.name)
        let prepared = try MutationTarget.resolve(name, operation: operation, snapshot: snapshot,
                                                 settings: settings, context: context)
        let relation = prepared.relation
        if let view = relation.view {
            let definition = view.layers[0].view
            // Target rewriting validates view-column mappings even when the
            // statement has no RETURNING. Reuse the same mapping implementation.
            switch statement {
            case .insert(let syntax):
                let targets: [Int]
                if case .defaultValues = syntax.source { targets = [] }
                else { targets = try syntax.columns.isEmpty ? Array(definition.columns.indices) : syntax.columns.map(view.column) }
                guard Set(targets).count == targets.count else { throw JedError("42701", "column specified more than once") }
                if let conflict = syntax.conflict, !conflict.doNothing {
                    let updates = try conflict.assignments.map { try view.column($0.column) }
                    guard Set(updates).count == updates.count else { throw JedError("42701", "multiple assignments to same column") }
                    _ = try view.destinations(Array(Set(targets + updates)).sorted(), allowDuplicates: true)
                    _ = try view.destinations(updates)
                }
                _ = try view.destinations(targets)
            case .update(let syntax):
                let targets = try syntax.assignments.map { try view.column($0.column) }
                guard Set(targets).count == targets.count else { throw JedError("42701", "multiple assignments to same column") }
                _ = try view.destinations(targets)
            default: break
            }
        }
        let resolver = resolver(relation, snapshot: snapshot, settings: settings, context: context, columns: true)
        resolver.cteBindings = shared.cteBindings
        resolver.outerScopes = shared.outerScopes
        resolver.parameters = shared.parameters; resolver.parameterUses = shared.parameterUses
        resolver.viewExpansion = shared.viewExpansion
        let result = try Returning(returning, relation: relation, resolver: resolver, oldDefault: deleting, qualifier: alias)
        shared.parameters = resolver.parameters; shared.parameterUses.formUnion(resolver.parameterUses)
        shared.outerReferences.formUnion(resolver.outerReferences)
        return MutationSchema(columns: result.columns, expressions: result.expressions, noReturning: returning == nil)
    }
}
