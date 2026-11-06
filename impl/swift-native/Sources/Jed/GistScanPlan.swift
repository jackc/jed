/// GiST scan admission is a structural pass. Neither inventory nor estimates
/// evaluate the query operand or inspect table leaves.
internal struct GistScanPlan {
    let index: CatalogIndex
    let tree: GistTree
    let column: Int
    let opclass: GistOpclass
    let operand: ResolvedExpression
    let strategy: GistStrategy

    private static func representationOperand(_ expression: ResolvedExpression) -> ResolvedExpression {
        var result = expression
        while case .convert(let child, let target) = result.node,
              case .scalar(let source) = child.type?.type, case .scalar(let destination) = target.type,
              source.integerWidth != nil, destination.integerWidth != nil {
            result = child
        }
        return result
    }
    var usesOuter: Bool { if case .outerColumn = Self.representationOperand(operand).node { return true }; return false }

    static func resolve(index: CatalogIndex, tree: GistTree, predicate: ResolvedExpression?,
                        types: [TypeDescriptor], allowOuter: Bool = false) -> GistScanPlan? {
        guard index.kind == 2, index.predicate == nil, index.keys.count == 1,
              case .column(let column) = index.keys[0], types.indices.contains(column),
              let opclass = try? GistOpclass(type: types[column]) else { return nil }
        func isKey(_ expression: ResolvedExpression) -> Bool {
            if case .column(let ordinal) = representationOperand(expression).node { return ordinal == column }; return false
        }
        func isSource(_ expression: ResolvedExpression) -> Bool {
            if allowOuter { if case .outerColumn = representationOperand(expression).node { return true }; return false }
            return FunctionResolver.isConstant(expression)
        }
        func result(_ source: ResolvedExpression, _ strategy: GistStrategy) -> GistScanPlan? {
            guard isSource(source) else { return nil }
            // Range-element containment has no scan strategy in this slice.
            if opclass.isRange, source.type?.type != opclass.type.type { return nil }
            return GistScanPlan(index: index, tree: tree, column: column, opclass: opclass,
                operand: source, strategy: strategy)
        }
        for conjunct in QueryEstimate.conjuncts(predicate) {
            switch conjunct.node {
            case .rangeOperation(let operation, let a, let b, _) where opclass.isRange:
                if operation == "&&" {
                    if isKey(a), let plan = result(b, .overlaps) { return plan }
                    if isKey(b), let plan = result(a, .overlaps) { return plan }
                } else if operation == "@>", isKey(a), let plan = result(b, .contains) { return plan }
            case .binary("=", let a, let b, _) where !opclass.isRange:
                if isKey(a), let plan = result(b, .equal) { return plan }
                if isKey(b), let plan = result(a, .equal) { return plan }
            default: break
            }
        }
        return nil
    }

    func query(_ context: ExpressionContext) throws -> GistQuery? {
        let meter = CostMeter(resources: context.meter.resources, cancellation: context.meter.cancellation)
        var unmetered = ExpressionContext(meter: meter, timezone: context.scalar.timezone,
            parameters: context.parameters, column: context.column)
        unmetered.outerRows = context.outerRows
        unmetered.functions = context.functions
        unmetered.collatedKey = context.collatedKey
        let value = try unmetered.evaluate(operand)
        // A wider integer probe outside the key type cannot equal any row.
        // Do not narrow it with a value cast (or wrap its key bytes).
        if case .integer(let number) = value {
            if opclass.type.type == .scalar(.i16), Int16(exactly: number) == nil { return nil }
            if opclass.type.type == .scalar(.i32), Int32(exactly: number) == nil { return nil }
        }
        return try opclass.query(value)
    }

    func estimate(table: CatalogTable, store: TableStore) -> PlanEstimate {
        // The frozen estimator uses the table skeleton for GiST's structural
        // fallback; only ordered/GIN paths substitute their index skeleton.
        let nodes = Int64(store.tree.nodeCount), height = Int64(store.tree.height)
        let selectivity = opclass.isRange ? "matching" : "equality"
        let rows = QueryEstimate.fraction(selectivity, rows: table.rowCount)
        let pages = rows == 0 ? 0 : min(nodes, max(height, QueryEstimate.fraction(selectivity, rows: nodes)))
        let gather = QueryEstimate.multiply(pages, 2)
        let work = CandidateEstimate(kind: .gist, scanRows: rows, outputRows: rows,
            accessPages: pages, tableHeight: height, accessWork: pages)
        return PlanEstimate(rows: rows, cost: work.cost,
            logicalRows: table.rowCount, prefix: .init(scanPages: gather, minimumScanPages: height,
                pointFetchPagesPerRow: height, pagePolicy: .retained))
    }
}
