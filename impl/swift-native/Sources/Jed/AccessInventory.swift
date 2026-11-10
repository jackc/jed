/// Every candidate retains its physical tree and owner facts. Inventory and
/// estimation inspect catalog/skeleton metadata only; binding is execution work.
internal struct ScanCandidate {
    enum Path {
        case primary(BaseScanPlan)
        case ordered(OrderedIndexBound, TableStore)
        case gist(GistScanPlan)
        case gin(GinScanPlan)
        case primaryIntervals(IntervalPlan)
        case orderedIntervals(IntervalPlan, TableStore)
        case full(BaseScanPlan)
    }
    /// Stored fields live in one copy-on-write box, so passing this value
    /// around costs one retain instead of a field-by-field copy.
    private final class Storage {
        let path: Path
        let table: CatalogTable
        let store: TableStore
        let snapshot: DomainSnapshot
        let predicate: ResolvedExpression?
        /// The key-less plan of `predicate`, which every candidate of one
        /// inventory uses for the same contradiction check.
        let check: BaseScanPlan?
        init(path: Path, table: CatalogTable, store: TableStore, snapshot: DomainSnapshot,
             predicate: ResolvedExpression?, check: BaseScanPlan?) {
            self.path = path
            self.table = table
            self.store = store
            self.snapshot = snapshot
            self.predicate = predicate
            self.check = check
        }
    }
    private let storage: Storage
    init(path: Path, table: CatalogTable, store: TableStore, snapshot: DomainSnapshot, predicate: ResolvedExpression?,
         check: BaseScanPlan? = nil) {
        storage = Storage(path: path, table: table, store: store, snapshot: snapshot, predicate: predicate, check: check)
    }
    var path: Path { storage.path }
    var table: CatalogTable { storage.table }
    var store: TableStore { storage.store }
    var snapshot: DomainSnapshot { storage.snapshot }
    var predicate: ResolvedExpression? { storage.predicate }

    var ordered: OrderedIndexBound? {
        if case .ordered(let bound, _) = path { return bound }; return nil
    }
    var primary: BaseScanPlan? {
        switch path { case .primary(let bound), .full(let bound): return bound; case .ordered, .gist, .gin, .primaryIntervals, .orderedIntervals: return nil }
    }
    var gist: GistScanPlan? { if case .gist(let bound) = path { return bound }; return nil }
    var gin: GinScanPlan? { if case .gin(let bound) = path { return bound }; return nil }
    var intervals: IntervalPlan? {
        switch path { case .primaryIntervals(let plan), .orderedIntervals(let plan, _): plan; default: nil }
    }
    var orderedDefinition: OrderedIndexPlan? { ordered?.definition ?? intervals?.index }
    var tableOrder: Bool { orderedDefinition == nil }
    var accessRank: Int {
        estimateKind.rank
    }
    private var estimateKind: CandidateEstimate.Kind {
        switch path { case .primary: .primary; case .ordered: .ordered; case .gist: .gist; case .gin: .gin
        case .primaryIntervals: .primaryIntervals; case .orderedIntervals: .orderedIntervals; case .full: .full }
    }
    var indexName: SQLText? { orderedDefinition?.index.name ?? gist?.index.name ?? gin?.index.name }
    var usesOuter: Bool {
        if let intervals { return intervals.usesOuter }
        let expressions: [ResolvedExpression]
        switch path {
        case .primary(let bound): expressions = bound.accessPredicate.map { [$0] } ?? []
        case .ordered(let bound, _): expressions = bound.terms.flatMap { $0.map(\.source) }
        case .gist(let bound): expressions = [bound.operand]
        case .gin(let bound): expressions = bound.arms.map(\.operand)
        case .full, .primaryIntervals, .orderedIntervals: expressions = []
        }
        var found = false
        StoredExpressionDependencies.walk(expressions) { if case .outerColumn = $0.node { found = true } }
        return found
    }
    func bind(_ context: ExpressionContext) throws -> ScanAccess {
        switch path {
        case .primary(let bound), .full(let bound): return try bound.contiguousBound(context)
        case .ordered(let bound, _):
            let contradiction = BaseScanPlan(predicate: predicate, keys: [], types: store.tree.types)
            if try contradiction.hasKnownContradiction(parameters: context.parameters, collatedKey: context.collatedKey) { return .empty }
            return try bound.bind(context)
        case .primaryIntervals(let plan), .orderedIntervals(let plan, _):
            let contradiction = BaseScanPlan(predicate: predicate, keys: [], types: store.tree.types)
            if try contradiction.hasKnownContradiction(parameters: context.parameters, collatedKey: context.collatedKey) { return .empty }
            return try plan.bind(context: context)
        case .gin, .gist:
            let contradiction = BaseScanPlan(predicate: predicate, keys: [], types: store.tree.types)
            return try contradiction.hasKnownContradiction(parameters: context.parameters, collatedKey: context.collatedKey) ? .empty : .full
        }
    }
    func estimate(parameters: [SQLValue]?, context: ExpressionContext) throws -> PlanEstimate {
        let model = PlanCardinality(table: table, snapshot: snapshot, parameters: parameters, collatedKey: context.collatedKey, planningProbe: context.planningProbe)
        let check = storage.check ?? BaseScanPlan(predicate: predicate, keys: [], types: store.tree.types)
        if try check.hasKnownContradiction(parameters: parameters, collatedKey: context.collatedKey, planningProbe: context.planningProbe) {
            return PlanEstimate(rows: 0, cost: 0, logicalRows: 0)
        }
        if let intervals {
            let index: TableStore?
            if case .orderedIntervals(_, let tree) = path { index = tree } else { index = nil }
            return try IntervalEstimate(plan: intervals, table: table, snapshot: snapshot, parameters: parameters, context: context)
                .estimate(store: store, index: index, tableRows: table.rowCount)
        }
        if let gin { return try gin.estimate(parameters: parameters, table: table, snapshot: snapshot, planningProbe: context.planningProbe) }
        if let gist { return gist.estimate(table: table, store: store) }
        let access: ResolvedExpression?, tree: BPlusTree, unique: Bool, pointPages: Int64
        switch path {
        case .primary(let bound):
            access = bound.accessPredicate; tree = store.tree
            unique = !bound.keys.isEmpty && bound.keys.allSatisfy { bound.terms[$0]?.contains(where: { $0.operation == "=" }) == true }
            pointPages = 0
        case .ordered(let bound, let index):
            access = bound.accessPredicate; tree = index.tree
            unique = bound.definition.index.unique && bound.equalCount == bound.definition.keys.count
            pointPages = Int64(store.tree.height)
        case .full:
            access = nil; tree = store.tree; unique = false; pointPages = 0
        case .gist, .gin, .primaryIntervals, .orderedIntervals: throw JedError("XX000", "specialized access estimate dispatch failed")
        }
        // Unique expression keys need the same NULL/pair checks as columns; the
        // WHERE contradiction grammar deliberately recognizes bare columns only.
        let equalitySources: [(TypeDescriptor, [ResolvedExpression])]
        switch path {
        case .primary(let bound):
            equalitySources = bound.keys.map { key in
                (bound.types[key], (bound.terms[key] ?? []).filter { $0.operation == "=" }.map(\.source))
            }
        case .ordered(let bound, _):
            equalitySources = bound.terms.indices.map { key in
                (bound.definition.types[key], bound.terms[key].filter { $0.operation == "=" }.map(\.source))
            }
        default: equalitySources = []
        }
        for (type, sources) in equalitySources {
            for (index, source) in sources.enumerated() {
                if model.isNull(source) { return PlanEstimate(rows: 0, cost: 0, logicalRows: 0) }
                if case .constant(.integer(let value)) = source.node,
                   case .scalar(let scalar) = type.type, scalar.integerWidth != nil,
                   IntervalPlan.pointValue(.integer(value), type: type) == nil {
                    return PlanEstimate(rows: 0, cost: 0, logicalRows: 0)
                }
                for prior in sources.prefix(index) {
                    guard PlanningProbe.literal(prior), PlanningProbe.literal(source),
                          let a = model.known(prior), let b = model.known(source) else { continue }
                    if PredicateContradiction.bounds("=", a, "=", b) {
                        return PlanEstimate(rows: 0, cost: 0, logicalRows: 0)
                    }
                }
            }
        }
        var partial: [ResolvedExpression] = []
        if let bound = ordered, !unique, let predicate = bound.definition.predicate {
            func columns(_ expression: ResolvedExpression) -> Set<Int> {
                var pending = [expression], result: Set<Int> = []
                while let item = pending.popLast() {
                    if case .column(let ordinal) = item.node { result.insert(ordinal) }
                    pending += StoredExpressionDependencies.children(item)
                }
                return result
            }
            let constrained = bound.definition.keys.prefix(bound.prefixCount).reduce(into: Set<Int>()) { $0.formUnion(columns($1)) }
            partial = QueryEstimate.conjuncts(predicate).filter { columns($0).isDisjoint(with: constrained) }
        }
        let structural = PlanCardinality(table: nil, snapshot: nil, parameters: parameters, collatedKey: context.collatedKey, planningProbe: context.planningProbe)
        func selected(_ input: Int64) throws -> Int64 {
            // A complete unique tuple is value-blind after NULL and pairwise
            // contradiction checks. Statistics cannot refine its one-row cap.
            if unique { return min(input, 1) }
            var result = try model.rows(access, input: input)
            for predicate in partial { result = try structural.rows(predicate, input: result) }
            return result
        }
        let rows = try selected(table.rowCount)
        let nodes = Int64(tree.nodeCount), height = Int64(tree.height)
        let pages = rows == 0 ? 0 : access == nil && partial.isEmpty ? nodes
            : min(nodes, max(height, try selected(nodes)))
        let work = CandidateEstimate(kind: estimateKind, scanRows: rows, outputRows: rows,
            accessPages: pages, tableHeight: pointPages)
        return PlanEstimate(rows: rows, cost: work.cost,
            logicalRows: table.rowCount, prefix: .init(scanPages: pages, minimumScanPages: height, pointFetchPagesPerRow: pointPages,
                pagePolicy: ordered.map { $0.bounded || $0.definition.predicate != nil } == true ? .retained : .clamped))
    }
    func providesOrder(_ order: [ResolvedOrderTerm]) -> Bool {
        guard let definition = orderedDefinition, !order.isEmpty, order.count == definition.keys.count,
              table.primaryKey.allSatisfy({ ValueCodec.fixedWidth(store.tree.types[$0].type) != nil }) else { return false }
        for ordinal in order.indices {
            guard case .column(let column) = order[ordinal].expression.node,
                  case .column(let key) = definition.keys[ordinal].node, column == key else { return false }
            let option = definition.index.keyOptions.isEmpty ? 0 : definition.index.keyOptions[ordinal]
            if order[ordinal].descending != (option & 1 != 0) || order[ordinal].nullsFirst != (option & 2 != 0) ||
                (order[ordinal].type.collation?.description ?? "C") != (definition.types[ordinal].collation?.description ?? "C") { return false }
        }
        return true
    }
    func makeCursor(context: ExpressionContext, touched: [Int], descending: Bool = false, access: ScanAccess? = nil,
                    incrementalFetch: Bool = false) throws -> AccessCursor {
        try AccessCursor(candidate: self, context: context, touched: touched, descending: descending, access: access,
            incrementalFetch: incrementalFetch)
    }
}

internal enum AccessInventory {
    static func resolve(table: CatalogTable, store: TableStore, snapshot: DomainSnapshot, domain: SQLText,
                        predicate: ResolvedExpression?, base: BaseScanPlan, settings: SessionSettings,
                        context: ExpressionContext, orderOnly: [ResolvedOrderTerm] = [],
                        forJoin: Bool = false, allowOuter: Bool = false) throws -> [ScanCandidate] {
        let unkeyed = BaseScanPlan(predicate: predicate, keys: [], types: store.tree.types)
        func candidate(_ path: ScanCandidate.Path) -> ScanCandidate {
            ScanCandidate(path: path, table: table, store: store, snapshot: snapshot, predicate: predicate, check: unkeyed)
        }
        let full = candidate(.full(unkeyed))
        guard !DomainNames.isAttachment(domain) else { return [full] }
        var result: [ScanCandidate] = []
        if base.accessPredicate != nil { result.append(candidate(.primary(base))) }
        let primarySet = IntervalPlan.primary(predicate: predicate, keys: table.primaryKey, types: store.tree.types)
        var orderedSets: [ScanCandidate] = []
        for index in table.indexes.filter({ $0.kind == 0 }).sorted(by: { $0.name < $1.name }) {
            guard let tree = snapshot.indexes[index.name] else { continue }
            let definition: OrderedIndexPlan
            do {
                try CatalogEnvironment.checkIndex(index, table: table, image: snapshot.catalog, environment: settings.environment,
                    borrowedTypes: snapshot.types, rowTypes: store.tree.types)
                definition = try OrderedIndexPlan.resolve(table: table, index: index, snapshot: snapshot, settings: settings, context: context)
            } catch { continue } // Stale definitions remain readable through another path.
            if let required = definition.predicate,
               !PredicateImplication.implies(predicate.map { [$0] } ?? [], predicate: required) { continue }
            if let set = IntervalPlan.ordered(predicate: predicate, definition: definition) {
                orderedSets.append(candidate(.orderedIntervals(set, tree)))
            }
            let bound = OrderedIndexBound(definition: definition, predicate: predicate,
                                          indexedJoin: forJoin && allowOuter)
            // Frozen bound admission permits container equality prefixes only;
            // a container range or unbounded suffix remains a full-table scan.
            let scalarSuffix = definition.types.dropFirst(bound.equalCount).allSatisfy {
                if case .scalar = $0.type { return true }; return false
            }
            let item = candidate(.ordered(bound, tree))
            if (predicate != nil && scalarSuffix && (bound.bounded || !forJoin && definition.predicate != nil))
                || !forJoin && item.providesOrder(orderOnly) {
                result.append(item)
            }
        }
        for index in table.indexes.filter({ $0.kind == 2 }).sorted(by: { $0.name < $1.name }) {
            guard let tree = snapshot.gistTrees[index.name],
                  let plan = GistScanPlan.resolve(index: index, tree: tree, predicate: predicate,
                    types: store.tree.types, allowOuter: allowOuter) else { continue }
            do {
                try CatalogEnvironment.checkIndex(index, table: table, image: snapshot.catalog, environment: settings.environment,
                    borrowedTypes: snapshot.types, rowTypes: store.tree.types)
            } catch { continue }
            result.append(candidate(.gist(plan)))
        }
        for index in table.indexes.filter({ $0.kind == 1 }).sorted(by: { $0.name < $1.name }) {
            guard let tree = snapshot.indexes[index.name],
                  let plan = GinScanPlan.resolve(index: index, store: tree, predicate: predicate, types: store.tree.types,
                    allowOuter: allowOuter) else { continue }
            do {
                try CatalogEnvironment.checkIndex(index, table: table, image: snapshot.catalog, environment: settings.environment,
                    borrowedTypes: snapshot.types, rowTypes: store.tree.types)
            } catch { continue }
            result.append(candidate(.gin(plan)))
        }
        if let primarySet { result.append(candidate(.primaryIntervals(primarySet))) }
        result += orderedSets
        result.append(full)
        return result
    }
}

extension BaseScanPlan {
    var accessPredicate: ResolvedExpression? {
        var expressions: [ResolvedExpression] = []
        for key in keys {
            guard let found = terms[key], !found.isEmpty else { break }
            let equalities = found.filter { $0.operation == "=" }
            for term in equalities.isEmpty ? found : equalities {
                let column = ResolvedExpression(type: types[key], node: .column(key), position: 0)
                expressions.append(ResolvedExpression(type: TypeDescriptor(.scalar(.boolean)),
                    node: .binary(term.operation, column, term.source, types[key]), position: 0))
            }
            if equalities.isEmpty { break }
        }
        return QueryEstimate.and(expressions)
    }
}

/// A secondary cursor keeps a bounded tree walk or an opclass candidate owner.
/// Eager scans charge all fetch work before row evaluation. A streaming LIMIT
/// charges table fetches only as pulled. The caller owns row and residual units.
internal final class AccessCursor {
    private let candidate: ScanCandidate
    private let context: ExpressionContext
    private let touched: [Int]
    private let incrementalFetch: Bool
    private let descending: Bool
    private var cursor: ScanCursor?
    private var ginCandidates: GinCandidates?
    private var intervalCursor: IntervalCursor?
    private var secondary = false
    init(candidate: ScanCandidate, context: ExpressionContext, touched: [Int], descending: Bool, access: ScanAccess? = nil,
         incrementalFetch: Bool) throws {
        self.candidate = candidate; self.context = context; self.touched = touched
        self.incrementalFetch = incrementalFetch; self.descending = descending
        let access = try access ?? candidate.bind(context)
        guard !access.isEmpty else { return }
        switch candidate.path {
        case .primaryIntervals(let plan):
            intervalCursor = try IntervalCursor(plan: plan, table: candidate.store, index: nil,
                context: context, touched: touched, access: access, descending: descending, incremental: incrementalFetch)
        case .orderedIntervals(let plan, let index):
            intervalCursor = try IntervalCursor(plan: plan, table: candidate.store, index: index,
                context: context, touched: touched, access: access, descending: descending, incremental: incrementalFetch)
        case .ordered(let bound, let index):
            var units = access.pages(index.tree)
            if !incrementalFetch {
                var preflight = access.cursor(index); defer { preflight.close() }
                while let entry = try preflight.next() {
                    try context.meter.pollStep()
                    let key = try IndexKeyBoundary.suffix(entry.key, types: bound.definition.types, options: bound.definition.index.keyOptions)
                    units = saturatingAdd(units, try ScanCost.point(key, store: candidate.store, touched: touched).units)
                }
            }
            context.meter.charge(units); try context.meter.guardCost()
            secondary = true
            cursor = access.cursor(index)
        case .gist(let plan):
            guard let query = try plan.query(context) else { return }
            let result = try plan.tree.search(queries: [query], strategies: [plan.strategy], meter: context.meter)
            if result.requiresFullScan {
                try ScanCost.charge(candidate.store, touched: touched, meter: context.meter)
                cursor = ScanAccess.full.cursor(candidate.store, descending: descending)
            } else {
                let weight = GeneratedSpec.costs.first { $0.id == "gist_descent" }!.weight
                context.meter.charge(QueryEstimate.multiply(Int64(result.interiorsVisited), weight))
                var units = Int64(result.nodesVisited)
                if !incrementalFetch {
                    for key in result.storageKeys {
                        try context.meter.pollStep()
                        units = saturatingAdd(units, try ScanCost.point(key, store: candidate.store, touched: touched).units)
                    }
                }
                context.meter.charge(units); try context.meter.guardCost()
                secondary = true; ginCandidates = GinCandidates(result.storageKeys)
            }
        case .gin(let plan):
            let result = try GinGather.gather(plan.queries(context), index: GinIndex(tree: plan.store.tree), meter: context.meter)
            if result.requiresFullScan {
                try ScanCost.charge(candidate.store, touched: touched, meter: context.meter)
                cursor = ScanAccess.full.cursor(candidate.store, descending: descending)
            } else {
                var units = Int64(result.pageReads)
                if !incrementalFetch {
                    try result.candidates.forEach { key in
                        try context.meter.pollStep()
                        units = saturatingAdd(units, try ScanCost.point(key, store: candidate.store, touched: touched).units)
                    }
                }
                context.meter.charge(units); try context.meter.guardCost()
                secondary = true; ginCandidates = result.candidates
            }
        case .primary, .full:
            try ScanCost.charge(candidate.store, touched: touched, meter: context.meter, access: access)
            cursor = access.cursor(candidate.store, descending: descending)
        }
    }
    var hasHostBatchIntervals: Bool {
        if case .primaryIntervals = candidate.path { return true }; return false
    }
    /// Primary interval windows stop before the next interval's access charge.
    /// Ordinary full/PK cursors already admitted their whole access block.
    func beginHostBatchInterval() throws -> Bool {
        switch candidate.path {
        case .primaryIntervals: return try intervalCursor?.beginHostBatchInterval() ?? false
        case .primary, .full: return true
        default: throw JedError("XX000", "access path cannot supply unmetered host rows")
        }
    }
    func nextHostBatchRecord() throws -> PackedRecord? {
        switch candidate.path {
        case .primaryIntervals: return try intervalCursor?.nextHostBatchRecord()
        case .primary, .full: return try next()
        default: throw JedError("XX000", "access path cannot supply unmetered host rows")
        }
    }
    func next() throws -> PackedRecord? {
        if let intervalCursor { return try intervalCursor.next() }
        let key: [UInt8]
        if let ginCandidates {
            guard let next = ginCandidates.next(descending: descending) else { return nil }; key = next
        } else {
            guard let entry = try cursor?.next() else { return nil }
            guard secondary, let bound = candidate.ordered else { return entry }
            key = try IndexKeyBoundary.suffix(entry.key, types: bound.definition.types, options: bound.definition.index.keyOptions)
        }
        if incrementalFetch {
            let point = try ScanCost.point(key, store: candidate.store, touched: touched)
            context.meter.charge(point.units); try context.meter.guardCost()
            return point.record
        }
        guard let record = try candidate.store.get(key) else { throw JedError.corruption("index entry refers to a missing table row") }
        return record
    }
    func close() { cursor?.close(); cursor = nil; ginCandidates?.close(); ginCandidates = nil; intervalCursor?.close(); intervalCursor = nil }
    deinit { close() }
}
