/// Ordinary DML and EXPLAIN share one preparation pass. These plans capture
/// read roots and resolved expressions without evaluating rows or defaults.
extension MutationExecutor {
    struct InsertPreparation {
        let read: DomainSnapshot
        let settings: SessionSettings
        let inputSettings: SessionSettings
        let relation: Relation
        let destinations: [Int]
        let discarded: Set<Int>
        var sourcePlan: SelectPlan?
        let inputLiterals: Set<Int>
        let slots: [InsertSlot]
        let defaults: [Int: ResolvedExpression]
        let checks: [Check]
        let indexes: [StoredIndexPlan]
        let returning: Returning
        let conflict: ConflictPlan?
        let parameterTypes: [TypeDescriptor]
        let profileIdentity = QueryProfileIdentity()
    }
    struct MutationPreparation {
        let read: DomainSnapshot
        let settings: SessionSettings
        let inputSettings: SessionSettings
        let relation: Relation
        let deleting: Bool
        let predicate: ResolvedExpression?
        let changes: [(Int, ResolvedExpression)]
        let assigned: Set<Int>
        let touched: Set<Int>
        let checks: [Check]
        let indexes: [StoredIndexPlan]
        let returning: Returning
        let filters: [(expression: ResolvedExpression, level: Int)]
        let target: ForeignKeyActions.Target?
        let parameterTypes: [TypeDescriptor]
        let profileIdentity = QueryProfileIdentity()
        var accessPredicate: ResolvedExpression? {
            var result: ResolvedExpression?
            func add(_ value: ResolvedExpression) {
                if let previous = result {
                    result = ResolvedExpression(type: TypeDescriptor(.scalar(.boolean)),
                        node: .binary("and", previous, value, TypeDescriptor(.scalar(.boolean))), position: 0)
                } else { result = value }
            }
            if let view = relation.view {
                for filter in filters { add(view.scanExpression(filter.expression, level: filter.level)) }
                if let predicate { add(view.scanExpression(predicate)) }
            } else { result = predicate }
            return result
        }
        var scan: BaseScanPlan {
            BaseScanPlan(predicate: accessPredicate, keys: relation.table.primaryKey, types: relation.types)
        }
    }
    enum Preparation {
        case insert(InsertPreparation)
        case mutation(MutationPreparation)
        var parameterTypes: [TypeDescriptor] {
            switch self { case .insert(let value): value.parameterTypes; case .mutation(let value): value.parameterTypes }
        }
        var returning: Returning {
            switch self { case .insert(let value): value.returning; case .mutation(let value): value.returning }
        }
        var profileIdentity: QueryProfileIdentity {
            switch self { case .insert(let value): value.profileIdentity; case .mutation(let value): value.profileIdentity }
        }
    }
    static func prepare(_ statement: SQLStatement, snapshot: DomainSnapshot,
                        settings: SessionSettings, context: ExpressionContext) throws -> Preparation? {
        switch statement {
        case .insert(let value): return .insert(try prepareInsert(value, snapshot: snapshot, settings: settings, context: context))
        case .update(let value): return .mutation(try prepareMutation(table: value.table, assignments: value.assignments,
            predicate: value.predicate, returning: value.returning, snapshot: snapshot, settings: settings, context: context))
        case .delete(let value): return .mutation(try prepareMutation(table: value.table, assignments: nil,
            predicate: value.predicate, returning: value.returning, snapshot: snapshot, settings: settings, context: context))
        default: return nil
        }
    }
    static func execute(_ plan: Preparation, snapshot: inout DomainSnapshot, context: ExpressionContext,
                        parameters: [SQLValue]) throws -> MutationResult {
        switch plan {
        case .insert(let value): return try executeInsert(value, snapshot: &snapshot, context: context, parameters: parameters)
        case .mutation(let value): return try executeMutation(value, snapshot: &snapshot, context: context, parameters: parameters)
        }
    }
}
