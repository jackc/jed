/// Detached access metadata. Binding substitutes the executing pin's physical
/// roots and statement contexts without resolving SQL or running the estimator.
internal struct PreparedIndexDefinition {
    let index: CatalogIndex
    let keys: [ResolvedExpression]
    let types: [TypeDescriptor]
    let predicate: ResolvedExpression?
    let touched: [Int]
    init(_ plan: OrderedIndexPlan) {
        index = plan.index; keys = plan.keys; types = plan.types
        predicate = plan.predicate; touched = plan.touched
    }
    func bind(_ context: ExpressionContext) -> OrderedIndexPlan {
        let meter = CostMeter(resources: context.meter.resources, cancellation: context.meter.cancellation)
        var current = ExpressionContext(meter: meter, timezone: context.scalar.timezone)
        current.scalar = context.scalar; current.scalar.meter = meter
        current.collatedKey = context.collatedKey; current.functions = context.functions
        return OrderedIndexPlan(index: index, keys: keys, types: types, predicate: predicate, touched: touched, context: current)
    }
}

internal struct PreparedAccessPlan {
    struct Ordered {
        let definition: PreparedIndexDefinition
        let terms: [[OrderedIndexBound.Term]]
        let predicate: ResolvedExpression?
        let prefix: Int
        let equal: Int
        init(_ value: OrderedIndexBound) {
            definition = PreparedIndexDefinition(value.definition); terms = value.terms
            predicate = value.accessPredicate; prefix = value.prefixCount; equal = value.equalCount
        }
        func bind(_ context: ExpressionContext) -> OrderedIndexBound {
            OrderedIndexBound(definition: definition.bind(context), terms: terms,
                accessPredicate: predicate, prefixCount: prefix, equalCount: equal)
        }
    }
    struct Intervals {
        let members: [IntervalPlan.Member]
        let keyCount: Int
        let index: PreparedIndexDefinition?
        let fixedWidth: Bool
        let replacesContiguous: Bool
        init(_ value: IntervalPlan) {
            members = value.members; keyCount = value.keyCount; index = value.index.map(PreparedIndexDefinition.init)
            fixedWidth = value.fixedWidth; replacesContiguous = value.replacesContiguous
        }
        func bind(_ context: ExpressionContext) -> IntervalPlan {
            IntervalPlan(members: members, keyCount: keyCount, index: index?.bind(context),
                fixedWidth: fixedWidth, replacesContiguous: replacesContiguous)
        }
    }
    enum Path {
        case primary(BaseScanPlan), full(BaseScanPlan)
        case ordered(Ordered)
        case intervals(Intervals)
        case gist(CatalogIndex, Int, GistOpclass, ResolvedExpression, GistStrategy)
        case gin(CatalogIndex, Int, Int, [GinPredicate])
    }
    let domain: SQLText
    let table: SQLText
    let predicate: ResolvedExpression?
    let path: Path
    init(_ candidate: ScanCandidate, domain: SQLText) {
        self.domain = domain; table = candidate.table.name; predicate = candidate.predicate
        switch candidate.path {
        case .primary(let value): path = .primary(value)
        case .full(let value): path = .full(value)
        case .ordered(let value, _): path = .ordered(Ordered(value))
        case .primaryIntervals(let value), .orderedIntervals(let value, _): path = .intervals(Intervals(value))
        case .gist(let value): path = .gist(value.index, value.column, value.opclass, value.operand, value.strategy)
        case .gin(let value): path = .gin(value.index, value.column, value.keyOrdinal, value.arms)
        }
    }
    func bind(domains: QueryDomains, context: ExpressionContext) throws -> ScanCandidate {
        let snapshot = try domains.snapshot(domain)
        guard let table = snapshot.catalog.tables.first(where: { $0.name == table }),
              let store = snapshot.tables[table.name] else { throw JedError("XX000", "prepared access signature was not checked") }
        func index(_ name: SQLText) throws -> TableStore {
            guard let value = snapshot.indexes[name] else { throw JedError("XX000", "prepared index signature was not checked") }
            return value
        }
        let current: ScanCandidate.Path
        switch path {
        case .primary(let value): current = .primary(value)
        case .full(let value): current = .full(value)
        case .ordered(let value): current = .ordered(value.bind(context), try index(value.definition.index.name))
        case .intervals(let value):
            if let definition = value.index { current = .orderedIntervals(value.bind(context), try index(definition.index.name)) }
            else { current = .primaryIntervals(value.bind(context)) }
        case .gist(let definition, let column, let opclass, let operand, let strategy):
            guard let tree = snapshot.gistTrees[definition.name] else { throw JedError("XX000", "prepared GiST signature was not checked") }
            current = .gist(GistScanPlan(index: definition, tree: tree, column: column,
                opclass: opclass, operand: operand, strategy: strategy))
        case .gin(let definition, let column, let ordinal, let arms):
            current = .gin(GinScanPlan(index: definition, store: try index(definition.name), column: column, keyOrdinal: ordinal, arms: arms))
        }
        return ScanCandidate(path: current, table: table, store: store, snapshot: snapshot, predicate: predicate)
    }
}
