/// Requirements belong to one admitted detached plan. Cache admission has
/// rejected every subquery, so reusing that exact plan needs no eager fold.
/// Mutable plans and runtime rewrites do not inherit this proof.
internal final class PreparedSelectRequirements {
    let mutatesSequences: Bool
    let needsEagerSubqueryPreparation = false

    fileprivate init(_ plan: SelectPlan) {
        mutatesSequences = SequenceExecution.mutates(plan.expressions)
    }
}

/// A cached plan has no physical roots or session settings. Only immutable
/// resolution/selection metadata crosses executions; binding installs current
/// pinned stores and fresh statement services without planning again.
internal final class PreparedSelectPlan {
    struct Relation {
        indirect enum Body { case base(SQLText, SQLText), join(Relation, Relation, String, ResolvedExpression?) }
        let body: Body
        let columns: [ExpressionColumn]
        let dependencies: [(primaryKey: [Int], columns: [Int])]
        let lateral: Set<Int>
        let preceding: Int
        let alias: SQLText?
        let pushedOn: [ResolvedExpression]?
        let filter: ResolvedExpression?
        let display: ResolvedExpression?
        init?(_ value: RelationPlan) {
            guard PreparedExpressions.reusable([value.scanFilter, value.scanDisplayFilter, value.effectiveOn].compactMap { $0 }
                + (value.pushedOn ?? [])) else { return nil }
            switch value.body {
            case .base:
                guard let domain = value.sourceDomain, domain != DomainNames.temporary, let table = value.baseTable else { return nil }
                body = .base(domain, table.name)
            case .join(let a, let b, let kind, let on):
                guard let a = Relation(a), let b = Relation(b) else { return nil }
                body = .join(a, b, kind, on)
            default: return nil
            }
            columns = value.columns; dependencies = value.functionalDependencies; lateral = value.lateralColumns
            preceding = value.precedingWidth; alias = value.subqueryAlias; pushedOn = value.pushedOn
            filter = value.scanFilter; display = value.scanDisplayFilter
        }
        func bind(settings: SessionSettings) throws -> RelationPlan {
            guard let domains = settings.domains else { throw JedError("XX000", "prepared relation has no executing domains") }
            let result: RelationPlan
            switch body {
            case .base(let domain, let name):
                let snapshot = try domains.snapshot(domain)
                guard let table = snapshot.catalog.tables.first(where: { $0.name == name }), let store = snapshot.tables[name] else {
                    throw JedError("XX000", "prepared relation signature was not checked")
                }
                result = RelationPlan(body: .base(store), columns: columns, functionalDependencies: dependencies,
                    baseTable: table, lateralColumns: lateral, precedingWidth: preceding,
                    sourceDomain: domain, sourceSnapshot: snapshot, sourceSettings: settings, subqueryAlias: alias)
            case .join(let a, let b, let kind, let on):
                result = RelationPlan(body: .join(try a.bind(settings: settings), try b.bind(settings: settings), kind, on),
                    columns: columns, functionalDependencies: dependencies, lateralColumns: lateral,
                    precedingWidth: preceding, subqueryAlias: alias)
            }
            result.pushedOn = pushedOn; result.scanFilter = filter; result.scanDisplayFilter = display
            return result
        }
    }
    struct Join {
        let left: Relation
        let right: Relation
        let kind: String
        let predicate: ResolvedExpression?
        let swapped: Bool
        let outer: PreparedAccessPlan
        let inner: PreparedAccessPlan
        let algorithm: JoinAccessPlan.Algorithm
        init?(_ value: JoinAccessPlan) {
            guard let left = Relation(value.left), let right = Relation(value.right),
                  let outerDomain = value.outer.sourceDomain, let innerDomain = value.inner.sourceDomain else { return nil }
            self.left = left; self.right = right; kind = value.kind; predicate = value.predicate
            swapped = value.swapped; algorithm = value.algorithm
            outer = PreparedAccessPlan(value.outerAccess, domain: outerDomain)
            inner = PreparedAccessPlan(value.innerAccess, domain: innerDomain)
        }
        func bind(settings: SessionSettings, context: ExpressionContext) throws -> JoinAccessPlan {
            guard let domains = settings.domains else { throw JedError("XX000", "prepared join has no executing domains") }
            return JoinAccessPlan(left: try left.bind(settings: settings), right: try right.bind(settings: settings),
                kind: kind, predicate: predicate, swapped: swapped, outerAccess: try outer.bind(domains: domains, context: context),
                innerAccess: try inner.bind(domains: domains, context: context), algorithm: algorithm)
        }
    }
    struct NWay {
        let source: Relation
        let inputs: [(Relation, Int)]
        let edges: [NWayAccessPlan.Edge]
        let order: [Int]
        let access: [(PreparedAccessPlan?, Bool, Int)]
        let steps: [NWayAccessPlan.Step]
        let estimate: PlanEstimate
        let ordered: Bool
        init?(_ value: NWayAccessPlan) {
            guard let source = Relation(value.source) else { return nil }
            self.source = source
            var inputs: [(Relation, Int)] = [], access: [(PreparedAccessPlan?, Bool, Int)] = []
            for input in value.inputs {
                guard let relation = Relation(input.source) else { return nil }
                inputs.append((relation, input.offset))
            }
            for (ordinal, item) in value.winner.access.enumerated() {
                let candidate: PreparedAccessPlan?
                if let selected = item.candidate {
                    guard value.winner.order.indices.contains(ordinal),
                          let domain = value.inputs[value.winner.order[ordinal]].source.sourceDomain else { return nil }
                    candidate = PreparedAccessPlan(selected, domain: domain)
                } else { candidate = nil }
                access.append((candidate, item.indexed, item.onIndex))
            }
            self.inputs = inputs; self.access = access; edges = value.edges; order = value.winner.order
            steps = value.winner.steps; estimate = value.winner.estimate; ordered = value.winner.ordered
        }
        func bind(settings: SessionSettings, context: ExpressionContext) throws -> NWayAccessPlan {
            guard let domains = settings.domains else { throw JedError("XX000", "prepared join has no executing domains") }
            return NWayAccessPlan(source: try source.bind(settings: settings),
                inputs: try inputs.map { .init(source: try $0.0.bind(settings: settings), offset: $0.1) }, edges: edges,
                winner: .init(order: order, access: try access.map {
                    .init(candidate: try $0.0?.bind(domains: domains, context: context), indexed: $0.1, onIndex: $0.2)
                }, steps: steps, estimate: estimate, ordered: ordered))
        }
    }
    enum Compound {
        case set(String, Bool, PreparedSelectPlan, PreparedSelectPlan)
        case nested(PreparedSelectPlan)
        case values([[ResolvedExpression]], Bool)
        init?(_ value: CompoundQueryPlan) {
            switch value.body {
            case .set(let operation, let all, let a, let b):
                guard let a = PreparedSelectPlan(a), let b = PreparedSelectPlan(b) else { return nil }
                self = .set(operation, all, a, b)
            case .nested(let value): guard let child = PreparedSelectPlan(value) else { return nil }; self = .nested(child)
            case .values(let rows, let flag): self = .values(rows, flag)
            }
        }
        func bind(settings: SessionSettings, context: ExpressionContext) throws -> CompoundQueryPlan {
            let body: CompoundQueryPlan.Body
            switch self {
            case .set(let operation, let all, let a, let b): body = .set(operation, all, try a.bind(settings: settings, context: context), try b.bind(settings: settings, context: context))
            case .nested(let value): body = .nested(try value.bind(settings: settings, context: context))
            case .values(let rows, let flag): body = .values(rows, flag)
            }
            return CompoundQueryPlan(body: body, settings: settings)
        }
    }
    let template: SelectPlan
    let relation: Relation?
    let compound: Compound?
    let access: PreparedAccessPlan?
    let join: Join?
    let nway: NWay?
    let requirements: PreparedSelectRequirements
    init?(_ value: SelectPlan) {
        guard value.withQuery == nil, value.sourceDomain != DomainNames.temporary,
              PreparedExpressions.reusable(value.expressions + [value.completePredicate].compactMap { $0 }) else { return nil }
        var candidates = value.accessCandidates ?? []
        if let selected = value.chosenAccess { candidates.append(selected) }
        if let selected = value.chosenJoin { candidates += [selected.outerAccess, selected.innerAccess] }
        if let selected = value.chosenNWay { candidates += selected.winner.access.compactMap(\.candidate) }
        for candidate in candidates {
            let definition = candidate.orderedDefinition
            guard PreparedExpressions.reusable([candidate.predicate, definition?.predicate].compactMap { $0 }
                + (definition?.keys ?? []) + (candidate.gin?.arms.map(\.operand) ?? [])
                + (candidate.gist.map { [$0.operand] } ?? [])) else { return nil }
        }
        if let source = value.relation { guard let detached = Relation(source) else { return nil }; relation = detached }
        else { relation = nil }
        if let source = value.compound { guard let detached = Compound(source) else { return nil }; compound = detached }
        else { compound = nil }
        if let source = value.chosenJoin { guard let detached = Join(source) else { return nil }; join = detached }
        else { join = nil }
        if let source = value.chosenNWay { guard let detached = NWay(source) else { return nil }; nway = detached }
        else { nway = nil }
        if let source = value.chosenAccess, let domain = value.sourceDomain { access = PreparedAccessPlan(source, domain: domain) }
        else { access = nil }
        var template = value
        template.store = nil; template.sourceSnapshot = nil; template.relation = nil; template.compound = nil
        template.accessCandidates = nil; template.chosenAccess = nil; template.chosenJoin = nil; template.chosenNWay = nil
        self.template = template
        requirements = PreparedSelectRequirements(value)
    }
    func bind(settings: SessionSettings, context: ExpressionContext) throws -> SelectPlan {
        guard let domains = settings.domains else { throw JedError("XX000", "prepared query has no executing domains") }
        var result = template
        if let domain = template.sourceDomain, let table = template.sourceTable {
            let snapshot = try domains.snapshot(domain)
            result.sourceSnapshot = snapshot; result.store = snapshot.tables[table.name]
            result.sourceTable = snapshot.catalog.tables.first { $0.name == table.name }
        }
        result.relation = try relation?.bind(settings: settings)
        result.compound = try compound?.bind(settings: settings, context: context)
        result.chosenAccess = try access?.bind(domains: domains, context: context)
        result.chosenJoin = try join?.bind(settings: settings, context: context)
        result.chosenNWay = try nway?.bind(settings: settings, context: context)
        return result
    }
}
