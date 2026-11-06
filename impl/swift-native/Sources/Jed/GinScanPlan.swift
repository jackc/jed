/// One frozen GIN predicate shape. Query extraction is separate from gathering,
/// so a full-scan fallback is known before any posting list is read.
internal struct GinPredicate {
    let column: Int
    let opclass: GinOpclass
    let operand: ResolvedExpression
    let strategy: GinStrategy?
    let pathMatch: Bool?
    let isArrayEquality: Bool

    static func match(_ expression: ResolvedExpression, columns: [Int], types: [TypeDescriptor],
                      allowOuter: Bool = false) -> GinPredicate? {
        func column(_ expression: ResolvedExpression) -> Int? {
            if case .column(let ordinal) = expression.node, columns.contains(ordinal), types.indices.contains(ordinal) { return ordinal }
            return nil
        }
        func source(_ expression: ResolvedExpression) -> Bool {
            // A join probe may read one bare earlier column. Constructed
            // sibling expressions remain residual, even when row independent
            // after binding that particular outer row.
            if allowOuter { if case .outerColumn = expression.node { return true }; return false }
            return FunctionResolver.isConstant(expression)
        }
        func result(_ key: ResolvedExpression, _ value: ResolvedExpression, operation: String, member: Bool = false) -> GinPredicate? {
            guard let ordinal = column(key), source(value), let opclass = try? GinOpclass(type: types[ordinal]) else { return nil }
            let strategy: GinStrategy?, path: Bool?
            if opclass.isJSON {
                switch operation {
                case "@>": strategy = .jsonContains; path = nil
                case "?": strategy = .jsonHasKey; path = nil
                case "?|": strategy = .jsonHasAnyKey; path = nil
                case "?&": strategy = .jsonHasAllKeys; path = nil
                case "@?": strategy = nil; path = false
                case "@@": strategy = nil; path = true
                default: return nil
                }
            } else {
                path = nil
                if member { strategy = .member }
                else {
                    switch operation {
                    case "@>": strategy = .contains
                    case "&&": strategy = .overlaps
                    case "=": strategy = .equal
                    default: return nil
                    }
                }
            }
            return GinPredicate(column: ordinal, opclass: opclass, operand: value, strategy: strategy,
                pathMatch: path, isArrayEquality: !opclass.isJSON && !member && operation == "=")
        }
        switch expression.node {
        case .quantified("=", false, let value, let key, _): return result(key, value, operation: "=", member: true)
        case .binary("=", let a, let b, _): return result(a, b, operation: "=") ?? result(b, a, operation: "=")
        case .call(let call) where call.hostID == nil && call.arguments.count == 2:
            let a = call.arguments[0], b = call.arguments[1]
            if call.name == "<@", a.type?.type == .scalar(.jsonb) { return result(b, a, operation: "@>") }
            if call.name == "&&" { return result(a, b, operation: call.name) ?? result(b, a, operation: call.name) }
            return result(a, b, operation: call.name)
        default: return nil
        }
    }
    private func extracted(_ value: SQLValue) throws -> GinQuery {
        if value.isNull { return .empty }
        if let pathMatch {
            guard case .jsonpath(let path) = value else { throw JedError("42804", "GIN path operand is not jsonpath") }
            guard let tree = try GinJSONPath.extract(JSONPathProgram.compile(path), match: pathMatch) else { return .fullScan }
            return .gather(tree)
        }
        guard let strategy else { throw JedError("XX000", "GIN predicate has no strategy") }
        return try opclass.query(value, strategy: strategy)
    }
    func knownQuery(parameters: [SQLValue]?, planningProbe: PlanningProbe? = nil) throws -> GinQuery? {
        func literal(_ expression: ResolvedExpression) -> SQLValue? {
            switch expression.node {
            case .constant(let value): return value
            case .parameter(let ordinal):
                return PlanningProbe.parameter(ordinal, parameters: parameters, probe: planningProbe)
            default: return nil
            }
        }
        if case .array(let children, false) = operand.node {
            var elements: [SQLValue] = []
            for child in children {
                guard let value = literal(child) else { return nil }
                elements.append(value)
            }
            return try extracted(.array(SQLArray(elements: elements)))
        }
        guard let value = literal(operand) else { return nil }
        return try extracted(value)
    }
    func query(_ context: ExpressionContext) throws -> GinQuery {
        let meter = CostMeter(resources: context.meter.resources, cancellation: context.meter.cancellation)
        var unmetered = ExpressionContext(meter: meter, timezone: context.scalar.timezone,
            parameters: context.parameters, column: context.column)
        unmetered.outerRows = context.outerRows
        unmetered.functions = context.functions
        unmetered.collatedKey = context.collatedKey
        return try extracted(unmetered.evaluate(operand))
    }
}

internal struct GinScanPlan {
    let index: CatalogIndex
    let store: TableStore
    let column: Int
    let keyOrdinal: Int
    let arms: [GinPredicate]
    var columns: [Int] { index.keys.compactMap { if case .column(let ordinal) = $0 { return ordinal }; return nil } }

    static func resolve(index: CatalogIndex, store: TableStore, predicate: ResolvedExpression?,
                        types: [TypeDescriptor], allowOuter: Bool = false) -> GinScanPlan? {
        guard index.kind == 1, let predicate else { return nil }
        let columns = index.keys.compactMap { if case .column(let ordinal) = $0 { return ordinal }; return nil }
        guard columns.count == index.keys.count else { return nil }
        let conjuncts = QueryEstimate.conjuncts(predicate)
        for ordinal in columns.indices {
            for conjunct in conjuncts {
                if let arm = GinPredicate.match(conjunct, columns: [columns[ordinal]], types: types, allowOuter: allowOuter) {
                    return GinScanPlan(index: index, store: store, column: columns[ordinal], keyOrdinal: ordinal, arms: [arm])
                }
            }
        }
        guard !allowOuter else { return nil } // sibling disjunctions are outside the frozen INL grammar
        for ordinal in columns.indices {
            for conjunct in conjuncts {
                guard case .binary("or", _, _, _) = conjunct.node else { continue }
                var pending = [conjunct], disjuncts: [ResolvedExpression] = []
                while let item = pending.popLast() {
                    if case .binary("or", let a, let b, _) = item.node { pending.append(b); pending.append(a) }
                    else { disjuncts.append(item) }
                }
                var arms: [GinPredicate] = []
                for disjunct in disjuncts {
                    let matched = QueryEstimate.conjuncts(disjunct).compactMap {
                        GinPredicate.match($0, columns: [columns[ordinal]], types: types)
                    }.first
                    guard let matched else { arms = []; break }
                    arms.append(matched)
                }
                if !arms.isEmpty { return GinScanPlan(index: index, store: store, column: columns[ordinal], keyOrdinal: ordinal, arms: arms) }
            }
        }
        return nil
    }
    func queries(_ context: ExpressionContext) throws -> [GinQuery] {
        try arms.map { arm in
            let query = try arm.query(context)
            if case .gather(let tree) = query { return .gather(try tree.tagged(column: keyOrdinal, columnCount: index.keys.count)) }
            return query
        }
    }

    func estimate(parameters: [SQLValue]?, table: CatalogTable, snapshot: DomainSnapshot, planningProbe: PlanningProbe? = nil) throws -> PlanEstimate {
        let nodes = Int64(store.tree.nodeCount), height = Int64(store.tree.height)
        let tableHeight = Int64(snapshot.tables[table.name]?.tree.height ?? 0)
        var known: [GinQuery] = []
        if snapshot.catalog.statistics.contains(where: { $0.table == table.name && $0.column == column && $0.termEligible && $0.analyzed > 0 }) {
            for arm in arms {
                guard let query = try arm.knownQuery(parameters: parameters, planningProbe: planningProbe) else { known = []; break }
                known.append(query)
            }
        }
        let statistics = known.isEmpty ? nil : GinEstimate.disjunction(known, column: column, table: table, snapshot: snapshot)
        func structural(_ population: Int64) -> Int64 {
            min(population, QueryEstimate.multiply(QueryEstimate.fraction("matching", rows: population), Int64(arms.count)))
        }
        let rows = statistics?.rows ?? structural(table.rowCount)
        let entries = statistics?.entries ?? rows
        let pages: Int64
        if let statistics {
            pages = statistics.pages(indexNodes: nodes, indexHeight: height,
                totalEntries: GinEstimate.totalEntries(columns: columns, table: table, snapshot: snapshot))
        } else { pages = rows == 0 ? 0 : min(nodes, max(height, structural(nodes))) }
        let gather = saturatingAdd(pages, entries)
        let work = CandidateEstimate(kind: .gin, scanRows: rows, outputRows: rows,
            accessPages: pages, tableHeight: tableHeight, accessWork: entries)
        return PlanEstimate(rows: rows, cost: work.cost,
            logicalRows: table.rowCount, prefix: .init(scanPages: gather, minimumScanPages: height,
                pointFetchPagesPerRow: tableHeight, pagePolicy: .retained))
    }
}
