/// An exact source-ordered signature uses identity objects, never hashes or
/// txids. A private temporary name is checked again before every lookup.
internal struct PreparedQuerySignature: Equatable {
    struct Relation: Equatable {
        let schema: PreparedSchemaSignature
        let estimator: StorageRevision
        static func == (a: Self, b: Self) -> Bool { a.schema == b.schema && a.estimator === b.estimator }
    }
    let core: StorageRevision
    let relations: [Relation]
    init?(query: SQLQuery, core: StorageRevision, domains: QueryDomains) {
        var names: [SQLName] = [], admitted = true
        func source(_ from: SQLFrom) {
            switch from.source {
            case .table(let name): names.append(name)
            case .joined(let child): source(child)
            default: admitted = false
            }
            for join in from.joins { source(join.right) }
        }
        func body(_ query: SQLQuery) {
            switch query.body {
            case .select(let select): for from in select.from { source(from) }
            case .set(_, _, let a, let b): body(a); body(b)
            case .nested(let child): body(child)
            case .values: break
            case .with: admitted = false
            }
        }
        body(query)
        guard admitted else { return nil }
        var relations: [Relation] = []
        for name in names {
            guard let owner = try? domains.relation(name), owner.name != DomainNames.temporary,
                  owner.snapshot.tables[name.name] != nil,
                  let table = owner.snapshot.catalog.tables.first(where: { $0.name == name.name }),
                  !table.indexes.contains(where: \.dynamicZones),
                  let estimator = owner.snapshot.estimatorRevision(name.name) else { return nil }
            relations.append(Relation(schema: PreparedSchemaSignature(core: core, snapshot: owner.snapshot, name: name.name), estimator: estimator))
        }
        self.core = core; self.relations = relations
    }
    static func == (a: Self, b: Self) -> Bool { a.core === b.core && a.relations == b.relations }
}

internal final class PreparedSelectCache {
    struct Memo { let key: [UInt8]; let plan: PreparedSelectPlan }
    let signature: PreparedQuerySignature
    let parameters: [TypeDescriptor]
    let sensitive: Bool
    let nullRead: Bool
    let generic: PreparedSelectPlan?
    private(set) var memo: [Memo] = []
    init(signature: PreparedQuerySignature, parameters: [TypeDescriptor], sensitive: Bool,
         nullRead: Bool, generic: PreparedSelectPlan?) {
        self.signature = signature; self.parameters = parameters; self.sensitive = sensitive
        self.nullRead = nullRead; self.generic = generic
    }
    func key(_ values: [SQLValue]) -> [UInt8]? {
        guard values.count == parameters.count else { return nil }
        // Bound values may exceed the on-disk inline value limit. The private
        // exact row representation preserves scale, float bits and array shape
        // without applying storage admission to a host parameter.
        return try? SpillRowCodec.encode(values)
    }
    func find(_ key: [UInt8]) -> PreparedSelectPlan? { memo.first { $0.key == key }?.plan }
    func retain(_ plan: PreparedSelectPlan, key: [UInt8]) {
        if memo.contains(where: { $0.key == key }) { return }
        if memo.count == 8 { memo.removeFirst() }
        memo.append(Memo(key: key, plan: plan))
    }
}

/// Admitted parameters are carried only when the cache and returned generic
/// plan share the exact resolution that created their parameter descriptors.
/// Cold, custom and memo paths keep the caller's ordinary final admission.
internal typealias PreparedSelectBinding = (plan: SelectPlan, admittedParameters: [SQLValue]?,
                                           requirements: PreparedSelectRequirements?)

extension PreparedStatement {
    internal func select(_ query: SQLQuery, snapshot: DomainSnapshot, settings: SessionSettings,
                         context: ExpressionContext, supplied: [SQLValue], core: StorageRevision,
                         committed: QueryDomains) throws -> PreparedSelectBinding {
        guard let domains = settings.domains else { throw JedError("XX000", "prepared query has no domain map") }
        let signature = PreparedQuerySignature(query: query, core: core, domains: domains)
        func resolve(_ probe: PlanningProbe) throws -> SelectPlan {
            var planning = settings; planning.planningParameters = nil; planning.planningProbe = probe
            var context = context; context.planningProbe = probe
            let plan = try SelectPlan.resolve(query, snapshot: snapshot, settings: planning, scalar: context.scalar)
            return try plan.prepareAccess(parameters: nil, context: context)
        }
        if let signature, let cached = selectCache, cached.signature == signature {
            let bound = try ParameterBinding.bind(supplied, types: cached.parameters, timezone: settings.timeZone)
            if !cached.sensitive, !(cached.nullRead && bound.contains(where: \.isNull)), let generic = cached.generic {
                selectCacheHits += 1
                return (try generic.bind(settings: settings, context: context), bound, generic.requirements)
            }
            let key = cached.sensitive ? cached.key(bound) : nil
            if let key, let found = cached.find(key) {
                selectCacheHits += 1
                return (try found.bind(settings: settings, context: context), nil, found.requirements)
            }
            customPlans += 1
            let frame = PlanningProbe.custom(bound); defer { frame.end() }
            let result = try resolve(frame)
            if let key, let detached = PreparedSelectPlan(result) { cached.retain(detached, key: key) }
            return (result, nil, nil)
        }
        selectCacheMisses += 1; probePlans += 1
        let probe = PlanningProbe.probe()
        let generic = try resolve(probe)
        let bound = try ParameterBinding.bind(supplied, types: generic.parameters, timezone: settings.timeZone)
        let custom = probe.needsCustom(bound)
        let sensitive = probe.sensitive, nullRead = probe.nullRead
        probe.end()
        let result: SelectPlan
        if custom {
            customPlans += 1
            let frame = PlanningProbe.custom(bound); defer { frame.end() }
            result = try resolve(frame)
        } else { result = generic }
        if let signature, signature == PreparedQuerySignature(query: query, core: core, domains: committed),
           let detached = PreparedSelectPlan(result) {
            let entry = PreparedSelectCache(signature: signature, parameters: generic.parameters,
                sensitive: sensitive, nullRead: nullRead, generic: sensitive ? nil : PreparedSelectPlan(generic))
            if sensitive, let key = entry.key(bound) { entry.retain(detached, key: key) }
            selectCache = entry
        }
        return (result, nil, nil)
    }
}

extension SelectPlan {
    /// Compound arms select in the same planning frame, so their estimator
    /// observations belong to the owning prepared statement's probe.
    func prepareAccess(parameters: [SQLValue]?, context: ExpressionContext) throws -> SelectPlan {
        var plan = self
        if let compound {
            let body: CompoundQueryPlan.Body
            switch compound.body {
            case .set(let operation, let all, let a, let b):
                body = .set(operation, all, try a.prepareAccess(parameters: parameters, context: context),
                    try b.prepareAccess(parameters: parameters, context: context))
            case .nested(let child): body = .nested(try child.prepareAccess(parameters: parameters, context: context))
            case .values: body = compound.body
            }
            plan.compound = CompoundQueryPlan(body: body, settings: compound.settings)
        }
        return try plan.selectAccess(parameters: parameters, context: context)
    }
}
