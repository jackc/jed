/// Every candidate retains its physical tree and owner facts. Inventory and
/// estimation inspect catalog/skeleton metadata only; binding is execution work.
internal struct ScanCandidate {
    enum Path {
        case primary(BaseScanPlan)
        case ordered(OrderedIndexBound, TableStore)
        case full(BaseScanPlan)
    }
    let path: Path
    let table: CatalogTable
    let store: TableStore
    let snapshot: DomainSnapshot
    let predicate: ResolvedExpression?

    var ordered: OrderedIndexBound? {
        if case .ordered(let bound, _) = path { return bound }; return nil
    }
    var primary: BaseScanPlan? {
        switch path { case .primary(let bound), .full(let bound): return bound; case .ordered: return nil }
    }
    var tableOrder: Bool { ordered == nil }
    func bind(_ context: ExpressionContext) throws -> ScanAccess {
        switch path {
        case .primary(let bound), .full(let bound): return try bound.bind(context)
        case .ordered(let bound, _):
            let contradiction = BaseScanPlan(predicate: predicate, keys: [], types: store.tree.types)
            if try contradiction.hasKnownContradiction(parameters: context.parameters, collatedKey: context.collatedKey) { return .empty }
            return try bound.bind(context)
        }
    }
    func estimate(parameters: [SQLValue]?, context: ExpressionContext) throws -> PlanEstimate {
        let model = PlanCardinality(table: table, snapshot: snapshot, parameters: parameters, collatedKey: context.collatedKey)
        let check = BaseScanPlan(predicate: predicate, keys: [], types: store.tree.types)
        if try check.hasKnownContradiction(parameters: parameters, collatedKey: context.collatedKey) {
            return PlanEstimate(rows: 0, cost: 0, logicalRows: 0)
        }
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
        let structural = PlanCardinality(table: nil, snapshot: nil, parameters: parameters, collatedKey: context.collatedKey)
        func selected(_ input: Int64) throws -> Int64 {
            var result = try model.rows(access, input: input)
            if unique { return min(result, 1) }
            for predicate in partial { result = try structural.rows(predicate, input: result) }
            return result
        }
        let rows = try selected(table.rowCount)
        let nodes = Int64(tree.nodeCount), height = Int64(tree.height)
        let pages = rows == 0 ? 0 : access == nil && partial.isEmpty ? nodes
            : min(nodes, max(height, try selected(nodes)))
        return PlanEstimate(rows: rows, cost: saturatingAdd(pages, QueryEstimate.multiply(rows, saturatingAdd(pointPages, 1))),
            logicalRows: table.rowCount, prefix: .init(scanPages: pages, minimumScanPages: height, pointFetchPagesPerRow: pointPages))
    }
    func providesOrder(_ order: [ResolvedOrderTerm]) -> Bool {
        guard let bound = ordered, !order.isEmpty, order.count == bound.definition.keys.count,
              table.primaryKey.allSatisfy({ ValueCodec.fixedWidth(store.tree.types[$0].type) != nil }) else { return false }
        for ordinal in order.indices {
            guard case .column(let column) = order[ordinal].expression.node,
                  case .column(let key) = bound.definition.keys[ordinal].node, column == key else { return false }
            let option = bound.definition.index.keyOptions.isEmpty ? 0 : bound.definition.index.keyOptions[ordinal]
            if order[ordinal].descending != (option & 1 != 0) || order[ordinal].nullsFirst != (option & 2 != 0) ||
                (order[ordinal].type.collation?.description ?? "C") != (bound.definition.types[ordinal].collation?.description ?? "C") { return false }
        }
        return true
    }
    func makeCursor(context: ExpressionContext, touched: [Int], descending: Bool = false, access: ScanAccess? = nil) throws -> AccessCursor {
        try AccessCursor(candidate: self, context: context, touched: touched, descending: descending, access: access)
    }
}

internal enum AccessInventory {
    static func resolve(table: CatalogTable, store: TableStore, snapshot: DomainSnapshot, domain: SQLText,
                        predicate: ResolvedExpression?, base: BaseScanPlan, settings: SessionSettings,
                        context: ExpressionContext, orderOnly: [ResolvedOrderTerm] = []) throws -> [ScanCandidate] {
        func candidate(_ path: ScanCandidate.Path) -> ScanCandidate {
            ScanCandidate(path: path, table: table, store: store, snapshot: snapshot, predicate: predicate)
        }
        let full = candidate(.full(BaseScanPlan(predicate: predicate, keys: [], types: store.tree.types)))
        guard !DomainNames.isAttachment(domain) else { return [full] }
        var result: [ScanCandidate] = []
        if base.accessPredicate != nil || !base.pointSets.isEmpty { result.append(candidate(.primary(base))) }
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
            let bound = OrderedIndexBound(definition: definition, predicate: predicate)
            // Frozen bound admission permits container equality prefixes only;
            // a container range or unbounded suffix remains a full-table scan.
            let scalarSuffix = definition.types.dropFirst(bound.equalCount).allSatisfy {
                if case .scalar = $0.type { return true }; return false
            }
            let item = candidate(.ordered(bound, tree))
            if (predicate != nil && scalarSuffix && (bound.bounded || definition.predicate != nil)) || item.providesOrder(orderOnly) {
                result.append(item)
            }
        }
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

/// A secondary cursor walks actual index entries and fetches one table record
/// at a time. The caller owns storage_row_read and the residual predicate.
internal final class AccessCursor {
    private let candidate: ScanCandidate
    private let context: ExpressionContext
    private let touched: [Int]
    private var cursor: ScanCursor
    init(candidate: ScanCandidate, context: ExpressionContext, touched: [Int], descending: Bool, access: ScanAccess? = nil) throws {
        self.candidate = candidate; self.context = context; self.touched = touched
        let access = try access ?? candidate.bind(context)
        switch candidate.path {
        case .ordered(_, let index):
            context.meter.charge(access.pages(index.tree)); try context.meter.guardCost()
            cursor = access.cursor(index)
        case .primary, .full:
            try ScanCost.charge(candidate.store, touched: touched, meter: context.meter, access: access)
            cursor = access.cursor(candidate.store, descending: descending)
        }
    }
    func next() throws -> PackedRecord? {
        guard let entry = try cursor.next() else { return nil }
        guard let bound = candidate.ordered else { return entry }
        let key = try IndexKeyBoundary.suffix(entry.key, types: bound.definition.types, options: bound.definition.index.keyOptions)
        let access = ScanAccess(lower: TreeBound(key), upper: TreeBound(key), isEmpty: false)
        try ScanCost.charge(candidate.store, touched: touched, meter: context.meter, access: access)
        guard let record = try candidate.store.get(key) else { throw JedError.corruption("index entry refers to a missing table row") }
        return record
    }
    func close() { cursor.close() }
    deinit { close() }
}
