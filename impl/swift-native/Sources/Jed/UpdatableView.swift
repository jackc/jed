/// A write retains each view level. Projection expressions are evaluated on
/// demand for every reference; they are not precomputed into a replacement row.
internal struct UpdatableView {
    struct Layer {
        let view: CatalogView
        let base: SQLName
        let columns: [ResultColumn]
        let projections: [ResolvedExpression]
        let predicate: ResolvedExpression?
        /// Immediate-base ordinal, or nil for a read-only output expression.
        let mapping: [Int?]
        let settings: SessionSettings
    }
    let layers: [Layer] // target outward to the table
    let table: CatalogTable
    let operation: String

    static func checkShape(_ query: SQLQuery, name: SQLText, operation: String) throws {
        func reject() -> JedError {
            JedError(operation == "check" ? "0A000" : "55000",
                     operation == "check" ? "WITH CHECK OPTION is supported only on automatically updatable views" : "cannot \(operation) view \(name)")
        }
        guard case .select(let select) = query.body, query.limit == nil, query.offset == nil,
              !select.distinct, select.distinctOn.isEmpty, select.grouping.isEmpty, select.having == nil,
              select.from.count == 1, let from = select.from.first, from.joins.isEmpty,
              case .table(let base) = from.source, !base.name.utf8.starts(with: Array("jed_".utf8)) else { throw reject() }
        var pending = select.items.compactMap(\.expression) + select.distinctOn + query.orderBy.map(\.expression)
        if let predicate = select.predicate { pending.append(predicate) }
        while let expression = pending.popLast() {
            if case .call(let call) = expression.kind {
                if call.over != nil || call.overName != nil || GeneratedSpec.functions.contains(where: {
                    $0.name == call.name.description && ["aggregate", "window", "set_returning"].contains($0.kind)
                }) { throw reject() }
            }
            // Subqueries own their aggregate/window level.
            pending += CatalogExpressionPolicy.children(expression)
        }
        if operation != "delete from",
           !select.items.contains(where: { $0.star || $0.expression.map(isColumn) == true }) { throw reject() }
    }
    private static func isColumn(_ expression: SQLExpression) -> Bool {
        switch expression.kind {
        case .column: true
        case .parenthesized(let inner): isColumn(inner)
        default: false
        }
    }
    static func resolve(_ view: CatalogView, operation: String, snapshot: DomainSnapshot,
                        settings: SessionSettings, context: ExpressionContext) throws -> UpdatableView {
        var layers: [Layer] = [], current = view, bodySettings = settings, visited: Set<SQLText> = []
        while true {
            guard visited.insert(current.name).inserted, layers.count < 256 else { throw JedError("54001", "view expansion exceeds maximum nesting") }
            let parsed = try ViewDefinition.parsed(current), query = parsed.query
            try checkShape(query, name: current.name, operation: operation)
            guard case .select(let select) = query.body, let from = select.from.first, case .table(let base) = from.source else {
                throw JedError("XX000", "updatable view shape changed during planning")
            }
            if current.securityInvoker {
                let privilege: PrivilegeSet = operation == "insert into" ? .insert : operation == "update" ? .update : .delete
                try bodySettings.privileges.require(privilege, on: base.name)
                if operation != "insert into", select.predicate != nil { try bodySettings.privileges.require(.select, on: base.name) }
            } else { bodySettings.privileges = bodySettings.privileges.withTableAccess() }
            // INSERT does not read the body's rows or WHERE. Planning its
            // output types must not add an unrelated SELECT capability gate.
            var planSettings = bodySettings
            if operation == "insert into" || select.predicate == nil { planSettings.privileges = bodySettings.privileges.withTableAccess() }
            let isolated = ExpressionResolver(composites: snapshot.types, scalar: context.scalar)
            let plan = try isolated.viewExpansion.withBody(current.name, referenceDepth: 0, offset: 0, bodyDepth: parsed.depth) {
                try SelectPlan.resolve(query, snapshot: snapshot, settings: planSettings, scalar: context.scalar, using: isolated)
            }
            guard plan.columns.count == current.columns.count else { throw JedError("0A000", "stored view column count no longer matches its definition") }
            var mapping: [Int?] = [], ordinal = 0
            for item in select.items {
                if item.star {
                    // CREATE freezes every star. This branch remains for valid
                    // foreign catalogs written by an earlier compatible core.
                    throw JedError("XX003", "stored view contains an unfrozen wildcard")
                }
                if let expression = item.expression, isColumn(expression), case .column(let index) = plan.projections[ordinal].node { mapping.append(index) }
                else { mapping.append(nil) }
                ordinal += 1
            }
            let columns = plan.columns.enumerated().map { ResultColumn(name: current.columns[$0.offset].name, type: $0.element.type ?? TypeDescriptor(.scalar(.text))) }
            layers.append(Layer(view: current, base: base, columns: columns, projections: plan.projections,
                                predicate: plan.predicate, mapping: mapping, settings: bodySettings))
            if let table = snapshot.catalog.tables.first(where: { $0.name == base.name }) {
                return UpdatableView(layers: layers, table: table, operation: operation)
            }
            guard let nested = snapshot.catalog.views.first(where: { $0.name == base.name }) else { throw JedError("42P01", "relation does not exist: \(base.name)") }
            current = nested
        }
    }
    func column(_ name: SQLText) throws -> Int {
        guard let first = layers.first, let index = first.view.columns.firstIndex(where: { $0.name == name }) else {
            throw JedError("42703", "column \(name) of view does not exist")
        }
        return index
    }
    /// Check every level in its own column order before detecting a duplicate
    /// physical destination. This preserves the view-write error precedence.
    func destinations(_ columns: [Int]) throws -> [Int] {
        var mapped = columns
        for layer in layers {
            for index in Set(mapped).sorted() {
                guard layer.mapping.indices.contains(index), layer.mapping[index] != nil else {
                    let name = layer.view.columns.indices.contains(index) ? layer.view.columns[index].name.description : "?"
                    throw JedError("0A000", "cannot \(operation) column \(name) of view \(layer.view.name)")
                }
            }
            mapped = mapped.map { layer.mapping[$0]! }
        }
        guard Set(mapped).count == mapped.count else { throw JedError("42701", "column assigned more than once") }
        return mapped
    }
    func baseColumn(layer: Int, column: Int) throws -> Int {
        var index = column
        for item in layers[layer...] {
            guard item.mapping.indices.contains(index), let mapped = item.mapping[index] else {
                throw JedError("0A000", "cannot \(operation) column \(item.view.columns[index].name) of view \(item.view.name)")
            }
            index = mapped
        }
        return index
    }
    /// Defaults are returned in nearest-view order. The caller first removes
    /// explicitly supplied base destinations, then chooses the first default
    /// per remaining destination. A read-only default is still a write target.
    func insertDefaults() throws -> [(column: Int, expression: CatalogExpression)] {
        var result: [(Int, CatalogExpression)] = []
        for (level, layer) in layers.enumerated() {
            for (column, definition) in layer.view.columns.enumerated() {
                guard let text = definition.defaultText else { continue }
                result.append((try baseColumn(layer: level, column: column), CatalogExpression(text)))
            }
        }
        return result
    }
}
