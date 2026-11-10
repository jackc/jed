/// Immutable INSERT resolution only. In particular, index evaluation contexts
/// are rebuilt on every execution; retaining OrderedIndexPlan directly would
/// retain an old meter, resource account, clock, and host-function environment.
internal struct PreparedInsertPlan {
    struct Index {
        let definition: CatalogIndex
        let keys: [ResolvedExpression]
        let types: [TypeDescriptor]
        let predicate: ResolvedExpression?
        let touched: [Int]
        let opclasses: [GistOpclass]?
        let ginOpclasses: [GinOpclass]?

        init(_ value: StoredIndexPlan) {
            definition = value.index; keys = value.ordered.keys; types = value.ordered.types
            predicate = value.ordered.predicate; touched = value.touched
            opclasses = value.opclasses; ginOpclasses = value.ginOpclasses
        }
        func bind(context: ExpressionContext) -> StoredIndexPlan {
            let meter = CostMeter(resources: context.meter.resources, cancellation: context.meter.cancellation)
            var unmetered = ExpressionContext(meter: meter, timezone: context.scalar.timezone)
            unmetered.scalar = context.scalar; unmetered.scalar.meter = meter
            unmetered.collatedKey = context.collatedKey; unmetered.functions = context.functions
            var ordered = definition; ordered.kind = 0
            let plan = OrderedIndexPlan(index: ordered, keys: keys, types: types, predicate: predicate,
                touched: touched, context: unmetered)
            return StoredIndexPlan(index: definition, ordered: plan, opclasses: opclasses, ginOpclasses: ginOpclasses)
        }
    }
    let signature: PreparedSchemaSignature
    let domain: SQLText
    let destinations: [Int]
    let discarded: Set<Int>
    let slots: [MutationExecutor.InsertSlot]
    let defaults: [Int: ResolvedExpression]
    let checks: [MutationExecutor.Check]
    let indexes: [Index]
    let returning: MutationExecutor.Returning
    let parameters: [TypeDescriptor]

    static func eligible(_ syntax: SQLInsert, domain: SQLText) -> Bool {
        guard domain != DomainNames.temporary, syntax.conflict == nil,
              case .values(let rows) = syntax.source, !rows.isEmpty else { return false }
        return true
    }
    init?(_ plan: MutationExecutor.InsertPreparation, signature: PreparedSchemaSignature) {
        guard plan.relation.view == nil, plan.sourcePlan == nil, plan.conflict == nil,
              !plan.indexes.contains(where: { $0.index.dynamicZones }) else { return nil }
        let expressions = plan.slots.flatMap(\.expressions) + plan.defaults.keys.sorted().compactMap { plan.defaults[$0] }
            + plan.checks.map(\.expression) + plan.returning.expressions
            + plan.indexes.flatMap { $0.ordered.keys + ($0.ordered.predicate.map { [$0] } ?? []) }
        guard PreparedExpressions.reusable(expressions) else { return nil }
        self.signature = signature; domain = plan.settings.targetDomain
        destinations = plan.destinations; discarded = plan.discarded; slots = plan.slots
        defaults = plan.defaults; checks = plan.checks; indexes = plan.indexes.map(Index.init)
        returning = plan.returning; parameters = plan.parameterTypes
    }
    func bind(_ syntax: SQLInsert, snapshot: DomainSnapshot, settings: SessionSettings,
              context: ExpressionContext, writable: Bool = false) throws -> MutationExecutor.InsertPreparation {
        let relation = try MutationExecutor.Relation(syntax.table, snapshot: snapshot, domain: domain)
        try settings.privileges.require(.insert, on: relation.table.name)
        if !writable {
            try CatalogEnvironment.checkWritable(relation.table, image: snapshot.catalog,
                environment: settings.environment, borrowedTypes: snapshot.borrowedTypes, rowTypes: relation.types)
        }
        if !returning.oldTouched.isEmpty || !returning.newTouched.isEmpty { try relation.requireSelect(settings) }
        return MutationExecutor.InsertPreparation(read: snapshot, settings: settings, inputSettings: settings,
            relation: relation, destinations: destinations, discarded: discarded, sourcePlan: nil, inputLiterals: [],
            slots: slots, defaults: defaults, checks: checks, indexes: indexes.map { $0.bind(context: context) },
            returning: returning, conflict: nil, parameterTypes: parameters)
    }
}
