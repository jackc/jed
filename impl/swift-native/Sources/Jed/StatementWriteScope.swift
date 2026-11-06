/// Every ordinary sub-statement reads this immutable pre-statement image.
/// The executor receives the separate mutable working image explicitly. FK
/// actions replace the read image with the current working image, retaining
/// the same deferred queue until the outer WITH has finished.
internal struct StatementWriteScope {
    let readSnapshot: DomainSnapshot
    let cteBindings: [SQLText: CTEBinding]
    let foreignKeys: ForeignKeyActions.State
}

/// The CTE relation needs only the target's RETURNING metadata before execution.
/// Values, predicates and assignment preparation remain in the ordinary DML
/// executor, at their prescribed sub-statement execution point.
internal struct MutationSchema {
    let columns: [ResultColumn]
    let expressions: [ResolvedExpression]
    let noReturning: Bool
}
