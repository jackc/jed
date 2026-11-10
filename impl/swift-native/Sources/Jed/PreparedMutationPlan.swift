/// Immutable UPDATE/DELETE resolution only, as PreparedInsertPlan is for
/// INSERT. The physical access path is chosen again on every execution: its
/// estimator inputs change with each write, so only name and type resolution
/// is reused. Roots, settings, privileges and index contexts are rebound.
internal struct PreparedMutationPlan {
    let signature: PreparedSchemaSignature
    let domain: SQLText
    let deleting: Bool
    let predicate: ResolvedExpression?
    let changes: [(Int, ResolvedExpression)]
    let assigned: Set<Int>
    let touched: Set<Int>
    let checks: [MutationExecutor.Check]
    let indexes: [PreparedInsertPlan.Index]
    let returning: MutationExecutor.Returning
    let readsColumns: Bool
    let parameters: [TypeDescriptor]

    struct Syntax {
        let table: SQLName
        let assignments: [SQLAssignment]?
        let predicate: SQLExpression?
        let returning: SQLReturning?
    }
    static func syntax(_ statement: SQLStatement) -> Syntax? {
        switch statement {
        case .update(let value):
            return Syntax(table: value.table, assignments: value.assignments, predicate: value.predicate, returning: value.returning)
        case .delete(let value):
            return Syntax(table: value.table, assignments: nil, predicate: value.predicate, returning: value.returning)
        default: return nil
        }
    }
    init?(_ plan: MutationExecutor.MutationPreparation, signature: PreparedSchemaSignature) {
        guard plan.relation.view == nil, plan.target == nil, plan.filters.isEmpty,
              plan.settings.targetDomain != DomainNames.temporary,
              !plan.indexes.contains(where: { $0.index.dynamicZones }) else { return nil }
        let expressions = (plan.predicate.map { [$0] } ?? []) + plan.changes.map { $0.1 }
            + plan.checks.map(\.expression) + plan.returning.expressions
            + plan.indexes.flatMap { $0.ordered.keys + ($0.ordered.predicate.map { [$0] } ?? []) }
        guard PreparedExpressions.reusable(expressions) else { return nil }
        self.signature = signature; domain = plan.settings.targetDomain; deleting = plan.deleting
        predicate = plan.predicate; changes = plan.changes; assigned = plan.assigned; touched = plan.touched
        checks = plan.checks; indexes = plan.indexes.map(PreparedInsertPlan.Index.init)
        returning = plan.returning; readsColumns = plan.readsColumns; parameters = plan.parameterTypes
    }
    /// Repeats every admission step of `prepareMutation` that depends on the
    /// executing session or snapshot, in the same order.
    func bind(_ name: SQLName, snapshot: DomainSnapshot, settings: SessionSettings,
              context: ExpressionContext, writable: Bool = false) throws -> MutationExecutor.MutationPreparation {
        do {
            try SystemRelationPlan.rejectTarget(name.name)
            let relation = try MutationExecutor.Relation(name, snapshot: snapshot, domain: domain)
            try settings.privileges.require(deleting ? .delete : .update, on: relation.table.name)
            if !writable {
                try CatalogEnvironment.checkWritable(relation.table, image: snapshot.catalog,
                    environment: settings.environment, borrowedTypes: snapshot.borrowedTypes, rowTypes: relation.types)
            }
            if readsColumns || !returning.oldTouched.isEmpty || !returning.newTouched.isEmpty {
                try relation.requireSelect(settings)
            }
            return MutationExecutor.MutationPreparation(read: snapshot, settings: settings, inputSettings: settings,
                relation: relation, deleting: deleting, predicate: predicate, changes: changes, assigned: assigned,
                touched: touched, checks: checks, indexes: indexes.map { $0.bind(context: context) },
                returning: returning, filters: [], target: nil, parameterTypes: parameters, readsColumns: readsColumns)
        } catch let error as JedError { throw error.inDatabase(settings.targetDomain) }
    }
}
