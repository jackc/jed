/// A write retains each view level. Projection expressions are evaluated on
/// demand for every reference; they are not precomputed into a replacement row.
internal struct UpdatableView {
    struct Layer {
        let view: CatalogView
        let owner: SQLText
        let baseDomain: SQLText
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
    let tableDomain: SQLText
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
        let domains = QueryDomains.statement(snapshot: snapshot, settings: settings)
        var currentOwner = settings.targetDomain, currentSnapshot = snapshot
        while true {
            guard visited.insert(current.name).inserted, layers.count < 256 else { throw JedError("54001", "view expansion exceeds maximum nesting") }
            let parsed = try ViewDefinition.parsed(current), query = parsed.query
            try checkShape(query, name: current.name, operation: operation)
            guard case .select(let select) = query.body, let from = select.from.first, case .table(let base) = from.source else {
                throw JedError("XX000", "updatable view shape changed during planning")
            }
            let scope = try domains.viewScope(owner: currentOwner)
            let selected = try scope.relation(base)
            bodySettings.domains = scope; bodySettings.targetDomain = currentOwner
            if current.securityInvoker && selected.name != DomainNames.temporary {
                let privilege: PrivilegeSet = operation == "insert into" ? .insert : operation == "update" ? .update : .delete
                try bodySettings.privileges.require(privilege, on: base.name)
                if operation != "insert into", select.predicate != nil { try bodySettings.privileges.require(.select, on: base.name) }
            } else if !current.securityInvoker { bodySettings.privileges = bodySettings.privileges.withTableAccess() }
            // INSERT does not read the body's rows or WHERE. Planning its
            // output types must not add an unrelated SELECT capability gate.
            var planSettings = bodySettings
            if operation == "insert into" || select.predicate == nil { planSettings.privileges = bodySettings.privileges.withTableAccess() }
            let isolated = ExpressionResolver(composites: currentSnapshot.types, scalar: context.scalar)
            let plan = try isolated.viewExpansion.withBody(current.name, referenceDepth: 0, offset: 0, bodyDepth: parsed.depth) {
                try SelectPlan.resolve(query, snapshot: currentSnapshot, settings: planSettings, scalar: context.scalar, using: isolated)
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
            layers.append(Layer(view: current, owner: currentOwner, baseDomain: selected.name, base: base, columns: columns, projections: plan.projections,
                                predicate: plan.predicate, mapping: mapping, settings: bodySettings))
            if let table = selected.snapshot.catalog.tables.first(where: { $0.name == base.name }) {
                return UpdatableView(layers: layers, table: table, tableDomain: selected.name, operation: operation)
            }
            guard let nested = selected.snapshot.catalog.views.first(where: { $0.name == base.name }) else { throw JedError("42P01", "relation does not exist: \(base.name)") }
            current = nested; currentOwner = selected.name; currentSnapshot = selected.snapshot
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
    func destinations(_ columns: [Int], allowDuplicates: Bool = false) throws -> [Int] {
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
        guard allowDuplicates || Set(mapped).count == mapped.count else { throw JedError("42701", "column assigned more than once") }
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

    /// A caller supplies the physical old/new row. A computed output remains an
    /// expression at every level and is evaluated once per reference, including
    /// volatile outputs used several times by a predicate or RETURNING list.
    func value(_ column: Int, level: Int = 0, base: @escaping (Int) throws -> SQLValue,
               context: ExpressionContext) throws -> SQLValue {
        if level == layers.count { return try base(column) }
        guard layers[level].projections.indices.contains(column) else { throw JedError("XX000", "view column is outside its projection") }
        var row = context
        row.column = { try self.value($0, level: level + 1, base: base, context: context) }
        return try row.evaluate(layers[level].projections[column])
    }
    func physicalColumns(_ columns: Set<Int>, level: Int = 0) -> Set<Int> {
        var result: Set<Int> = []
        var pending = columns.map { (ResolvedExpression(type: nil, node: .column($0), position: 0), level) }
        while let (expression, depth) = pending.popLast() {
            if case .column(let index) = expression.node {
                if depth == layers.count { result.insert(index) }
                else if layers[depth].projections.indices.contains(index) { pending.append((layers[depth].projections[index], depth + 1)) }
            } else {
                if case .subquery(let query) = expression.node {
                    pending += query.outerReferences.filter { $0.depth == 1 }.map {
                        (ResolvedExpression(type: nil, node: .column($0.index), position: 0), depth)
                    }
                }
                pending += StoredExpressionDependencies.children(expression).map { ($0, depth) }
            }
        }
        return result
    }
    func physicalColumns(_ expression: ResolvedExpression, level: Int) -> Set<Int> {
        var columns: Set<Int> = []
        StoredExpressionDependencies.walk([expression]) {
            if case .column(let index) = $0.node { columns.insert(index) }
            if case .subquery(let query) = $0.node { columns.formUnion(query.outerReferences.filter { $0.depth == 1 }.map(\.index)) }
        }
        return physicalColumns(columns, level: level)
    }
    /// Prepare the expressions that a textual view expansion would contain.
    /// Traversal substitutes only references to this enclosing scope; inner
    /// query-local columns remain local. RETURNING normalizes its old/new slot
    /// ordinals at the entry scope and drops the absent side's constant NULLs.
    func prepare(_ expressions: [ResolvedExpression], level: Int = 0, context: ExpressionContext,
                 column: (Int) -> Int? = { $0 }) throws {
        func visit(_ expression: ResolvedExpression, level: Int, queryDepth: Int,
                   column: (Int) -> Int?) throws {
            let index: Int?
            switch expression.node {
            case .column(let value) where queryDepth == 0: index = column(value)
            case .outerColumn(let reference) where queryDepth > 0 && reference.depth == queryDepth: index = column(reference.index)
            default: index = nil
            }
            if let index, level < layers.count, layers[level].projections.indices.contains(index) {
                try visit(layers[level].projections[index], level: level + 1, queryDepth: 0, column: { $0 })
            }
            for child in StoredExpressionDependencies.children(expression) {
                try visit(child, level: level, queryDepth: queryDepth, column: column)
            }
            if case .subquery(let query) = expression.node {
                for child in query.plan.expressions { try visit(child, level: level, queryDepth: queryDepth + 1, column: column) }
                try context.subqueries.prepare([expression], context: context)
            }
        }
        for expression in expressions { try visit(expression, level: level, queryDepth: 0, column: column) }
    }
    func prepareChecks(context: ExpressionContext) throws {
        for check in checked { try prepare([check.expression], level: check.level, context: context) }
    }
    /// Only scan-recognized nodes need physical remapping. Other nodes stay
    /// opaque to BaseScanPlan; their execution still uses the original scope.
    func scanExpression(_ input: ResolvedExpression, level: Int = 0) -> ResolvedExpression {
        var output = input
        switch input.node {
        case .column(let index) where level < layers.count:
            guard layers[level].projections.indices.contains(index) else { return input }
            return scanExpression(layers[level].projections[index], level: level + 1)
        case .binary(let operation, let a, let b, let type):
            output.node = .binary(operation, scanExpression(a, level: level), scanExpression(b, level: level), type)
        case .convert(let value, let type): output.node = .convert(scanExpression(value, level: level), type)
        default: break
        }
        return output
    }
    var filters: [(expression: ResolvedExpression, level: Int)] {
        layers.indices.reversed().compactMap { index in layers[index].predicate.map { ($0, index + 1) } }
    }
    var checked: [(name: SQLText, expression: ResolvedExpression, level: Int)] {
        var cascade = false, result: [(SQLText, ResolvedExpression, Int)] = []
        for (index, layer) in layers.enumerated() {
            cascade = cascade || layer.view.checkOption == 2
            if cascade || layer.view.checkOption != 0, let predicate = layer.predicate { result.append((layer.view.name, predicate, index + 1)) }
        }
        return Array(result.reversed())
    }
    func matches(_ expression: ResolvedExpression, level: Int, base: @escaping (Int) throws -> SQLValue,
                 context: ExpressionContext) throws -> Bool {
        var row = context
        row.column = { try self.value($0, level: level, base: base, context: context) }
        return try row.matches(expression)
    }
    func validateNew(_ base: @escaping (Int) throws -> SQLValue, context: ExpressionContext) throws {
        for check in checked {
            guard try matches(check.expression, level: check.level, base: base, context: context) else {
                throw JedError("44000", "new row violates check option for view \(check.name)")
            }
        }
    }
    func requireSelect(_ settings: SessionSettings) throws {
        guard let first = layers.first else { return }
        if first.owner != DomainNames.temporary { try settings.privileges.require(.select, on: first.view.name) }
        var privileges = settings.privileges
        for layer in layers {
            if layer.view.securityInvoker {
                if layer.baseDomain != DomainNames.temporary { try privileges.require(.select, on: layer.base.name) }
            }
            else { privileges = privileges.withTableAccess() }
        }
    }
}
