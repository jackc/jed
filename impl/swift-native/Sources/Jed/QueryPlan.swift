/// Plans retain static metadata and packed storage roots. Scan execution owns a
/// bounded tree cursor and decodes only the statement's touched columns.
internal struct SelectPlan {
    let columns: [ResultColumn]
    let projections: [ResolvedExpression]
    let predicate: ResolvedExpression?
    let store: TableStore?
    let touched: [Int]
    let parameters: [TypeDescriptor]
    let limit: Int64?
    let offset: Int64
    let order: [ResolvedOrderTerm]
    let orderedByScan: Bool
    let descending: Bool
    let scan: BaseScanPlan
    var aggregation: AggregateQueryPlan? = nil
    var compound: CompoundQueryPlan? = nil
    var relation: RelationPlan? = nil
    var productionCharge = true
    var distinct = false

    var expressions: [ResolvedExpression] {
        projections + (predicate.map { [$0] } ?? []) + order.map(\.expression)
            + (aggregation?.expressions ?? []) + (compound?.expressions ?? []) + (relation?.expressions ?? [])
    }

    static func resolve(_ query: SQLQuery, snapshot: DomainSnapshot, settings: SessionSettings,
                        scalar: ScalarCasts.Context, using shared: ExpressionResolver? = nil,
                        finalizeParameters: Bool = true) throws -> SelectPlan {
        if case .nested(let nested) = query.body, query.orderBy.isEmpty, query.limit == nil, query.offset == nil {
            return try resolve(nested, snapshot: snapshot, settings: settings, scalar: scalar, using: shared, finalizeParameters: finalizeParameters)
        }
        guard case .select(let select) = query.body else {
            return try CompoundQueryPlan.resolve(query, snapshot: snapshot, settings: settings, scalar: scalar,
                using: shared, finalizeParameters: finalizeParameters)
        }
        guard select.distinctOn.isEmpty, select.windows.isEmpty, select.into == nil else {
            throw JedError("0A000", "blocking query plan is not yet connected")
        }
        let resolver = shared ?? ExpressionResolver(composites: snapshot.types, scalar: scalar)
        var source: TableStore?, scope: [ExpressionColumn] = [], primaryKey: [Int] = []
        var relation: RelationPlan?
        if select.from.count == 1, let from = select.from.first, from.joins.isEmpty, case .table(let name) = from.source,
           snapshot.catalog.tables.contains(where: { $0.name == name.name }) {
            guard name.database == nil || name.database?.description == "main",
                  let table = snapshot.catalog.tables.first(where: { $0.name == name.name }),
                  let store = snapshot.tables[name.name] else { throw JedError("42P01", "relation does not exist: \(name.name)") }
            try settings.privileges.require(.select, on: table.name)
            guard from.columnAliases.count <= table.columns.count else { throw JedError("42601", "too many column aliases") }
            source = store
            primaryKey = table.primaryKey
            if primaryKey.contains(where: { !CatalogEnvironment.collationMatches(table.columns[$0].type.collation,
                image: snapshot.catalog, environment: settings.environment) }) { primaryKey = [] }
            scope = try table.columns.enumerated().map { index, column in
                ExpressionColumn(name: index < from.columnAliases.count ? from.columnAliases[index] : column.name,
                                 qualifier: from.alias ?? table.name, type: try column.type.resolve(snapshot.types), index: index)
            }
        } else if !select.from.isEmpty {
            relation = try RelationPlan.resolve(select.from, snapshot: snapshot, settings: settings, resolver: resolver)
            scope = relation!.columns
        }
        let previousColumns = resolver.columns, previousTouched = resolver.referencedColumns
        let previousAggregate = resolver.resolveAggregate
        resolver.columns = scope; resolver.referencedColumns = []
        resolver.resolveAggregate = AggregateQueryPlan.forbidden
        defer { resolver.columns = previousColumns; resolver.referencedColumns = previousTouched; resolver.resolveAggregate = previousAggregate }
        resolver.hostFunctions = settings.environment.signatures
        resolver.unicode = settings.environment.unicode
        resolver.checkFunction = { try settings.privileges.require(.execute, on: $0, function: true) }
        let predicate = try select.predicate.map { try resolver.resolve($0, expected: TypeDescriptor(.scalar(.boolean))) }
        guard predicate?.type == nil || predicate?.type?.type == .scalar(.boolean) else { throw JedError("42804", "WHERE requires a boolean expression") }
        let aggregation = try AggregateQueryPlan.resolve(select, order: query.orderBy, resolver: resolver, scope: scope, primaryKey: primaryKey)
        var projections: [ResolvedExpression] = [], columns: [ResultColumn] = []
        if let aggregation { projections = aggregation.projections; columns = aggregation.columns }
        for item in aggregation == nil ? select.items : [] {
            if item.star {
                let selected = scope.filter { item.qualifier == nil || item.qualifier == $0.qualifier }
                guard !selected.isEmpty else { throw JedError(item.qualifier == nil ? "42601" : "42P01", "wildcard has no matching relation") }
                for column in selected {
                    resolver.referencedColumns.insert(column.index)
                    projections.append(ResolvedExpression(type: column.unresolved ? nil : column.type, node: .column(column.index), position: item.span.position,
                        collation: ExpressionCollation.collatable(column.type.type) ? .implicit(column.type.collation ?? SQLText(validatedUTF8: [67])) : nil))
                    columns.append(ResultColumn(name: column.name, type: column.unresolved ? nil : column.type))
                }
            } else if let syntax = item.expression {
                let expression = try resolver.resolve(syntax)
                columns.append(ResultColumn(name: item.alias ?? outputName(syntax), type: expression.type))
                projections.append(expression)
            }
        }
        guard !projections.isEmpty else { throw JedError("42601", "SELECT requires a target list") }
        let order = try aggregation?.order ?? query.orderBy.map { term -> ResolvedOrderTerm in
            let expression: ResolvedExpression
            if case .number(let spelling) = term.expression.kind, let ordinal = Int(spelling) {
                guard ordinal > 0 && ordinal <= projections.count else { throw JedError("42P10", "ORDER BY position is not in select list") }
                expression = projections[ordinal - 1]
            } else if case .column(nil, let name) = term.expression.kind,
                      columns.contains(where: { $0.name == name }) {
                let matches = columns.indices.filter { columns[$0].name == name }
                guard matches.dropFirst().allSatisfy({ ExpressionIdentity.same(projections[matches[0]], projections[$0]) }) else {
                    throw JedError("42702", "ORDER BY reference is ambiguous: \(name)")
                }
                expression = projections[matches[0]]
            } else { expression = try resolver.resolve(term.expression) }
            var type = expression.type ?? TypeDescriptor(.scalar(.text))
            type.collation = try ExpressionCollation.derive(expression).orderingName()
            try TypeRules.comparable(type.type, type.type)
            return ResolvedOrderTerm(expression: expression, type: type, descending: term.descending,
                                     nullsFirst: term.nullsFirst ?? term.descending)
        }
        let reverse = order.first?.descending ?? false
        if select.distinct {
            for expression in projections { try AggregateQueryPlan.equalityType(expression) }
            for term in order where !projections.contains(where: { ExpressionIdentity.same($0, term.expression) }) {
                throw JedError("42P10", "for SELECT DISTINCT, ORDER BY expressions must appear in select list")
            }
        }
        var orderedByScan = aggregation == nil && !order.isEmpty && !primaryKey.isEmpty && (!reverse || order.count >= primaryKey.count)
        if order.contains(where: { if case .column = $0.expression.node { return false }; return true }) { orderedByScan = false }
        for ordinal in 0..<min(order.count, primaryKey.count) {
            let key = primaryKey[ordinal]
            if case .column(let column) = order[ordinal].expression.node,
               column == key, order[ordinal].descending == reverse,
               (order[ordinal].type.collation?.description ?? "C") == (scope[key].type.collation?.description ?? "C") {} else { orderedByScan = false }
        }
        let parameters = try finalizeParameters ? ParameterBinding.finalize(resolver) : []
        func bound(_ spelling: String?, code: String) throws -> Int64? {
            guard let spelling else { return nil }
            guard let value = Int64(spelling), value >= 0 else { throw JedError(code, "invalid row-count bound") }
            return value
        }
        return SelectPlan(columns: columns, projections: projections, predicate: predicate, store: source,
                          touched: resolver.referencedColumns.sorted(), parameters: parameters,
                          limit: try bound(query.limit, code: "2201W"), offset: try bound(query.offset, code: "2201X") ?? 0,
                          order: order, orderedByScan: orderedByScan, descending: orderedByScan && reverse,
                          scan: BaseScanPlan(predicate: predicate, keys: primaryKey, types: source?.tree.types ?? []), aggregation: aggregation,
                          relation: relation, distinct: select.distinct)
    }

    internal static func outputName(_ expression: SQLExpression) -> SQLText {
        switch expression.kind {
        case .column(_, let name): return name
        case .call(let call): return call.name
        case .conditionalFunction(let name, _): return SQLText(validatedUTF8: Array(name.utf8))
        case .field(_, let name): if let name { return name }
        case .subscripted(let child, _), .parenthesized(let child): return outputName(child)
        default: break
        }
        return SQLText(validatedUTF8: Array("?column?".utf8))
    }
}

/// Keeps decoded values for exactly one packed row. This is also the unmetered
/// demand-fault backstop for a column missed by a static touched-set collector.
internal final class ScanRecord {
    let record: PackedRecord
    let pager: Pager?
    private var values: [Int: SQLValue] = [:]
    init(_ record: PackedRecord, pager: Pager?) { self.record = record; self.pager = pager }
    func value(_ column: Int) throws -> SQLValue {
        guard record.cells.indices.contains(column) else { throw JedError("XX000", "resolved column is outside the row") }
        if let value = values[column] { return value }
        let value = try record.materialize(column: column, pager: pager)
        values[column] = value; return value
    }
}

internal final class SelectExecution {
    let plan: SelectPlan
    let context: ExpressionContext
    private let input: SelectInput
    private var started = false
    private var skipped: Int64 = 0
    private var produced: Int64 = 0
    private var ordered: OrderedSelectExecution?
    private var grouped: GroupedSelectExecution?
    private var combined: CompoundQueryExecution?
    private var deduplicated: DistinctExecution?
    init(plan: SelectPlan, context: ExpressionContext) {
        self.plan = plan; self.context = context; input = SelectInput(plan: plan, context: context)
        if plan.distinct { deduplicated = DistinctExecution(plan: plan, context: context) }
        else if let compound = plan.compound { combined = CompoundQueryExecution(plan: plan, compound: compound, context: context) }
        else if let aggregation = plan.aggregation { grouped = GroupedSelectExecution(plan: plan, aggregate: aggregation, context: context) }
        else if !plan.order.isEmpty && !plan.orderedByScan { ordered = OrderedSelectExecution(plan: plan, context: context) }
    }
    func next() throws -> [SQLValue]? {
        if let deduplicated { return try deduplicated.next() }
        if let combined { return try combined.next() }
        if let grouped { return try grouped.next() }
        if let ordered { return try ordered.next() }
        let meter = context.meter
        if !started { try input.prime(); started = true }
        if let limit = plan.limit, produced >= limit { return nil }
        while true {
            try meter.checkpoint()
            guard let row = try input.next() else { return nil }
            var rowContext = context; rowContext.column = { row[$0] }
            if let predicate = plan.predicate {
                guard try rowContext.matches(predicate) else { continue }
            }
            if skipped < plan.offset { skipped += 1; continue }
            if plan.productionCharge { meter.charge(1); try meter.guardCost() }
            let values = try plan.projections.map { try rowContext.evaluate($0) }
            produced += 1
            return values
        }
    }
    func close() { input.close(); ordered?.close(); ordered = nil; grouped?.close(); grouped = nil; combined?.close(); combined = nil; deduplicated?.close(); deduplicated = nil }
}

internal enum ScanCost {
    static func charge(_ store: TableStore, touched: [Int], meter: CostMeter, access: ScanAccess = .full) throws {
        guard !access.isEmpty else { return }
        var units = access.pages(store.tree)
        let variable = touched.filter { ValueCodec.fixedWidth(store.tree.types[$0].type) == nil }
        if !variable.isEmpty {
            var scan = access.cursor(store); defer { scan.close() }
            let slab = store.tree.codec.capacity
            let chainSlab = slab - (store.tree.codec.reservedBytes == 0 ? 0 : 16)
            while let record = try scan.next() {
                try meter.pollStep()
                for column in variable {
                    guard let facts = try record.cells[column].largeFacts(encrypted: store.tree.codec.reservedBytes != 0) else { continue }
                    if facts.external != nil { units = saturatingAdd(units, Int64((facts.stored + chainSlab - 1) / chainSlab)) }
                    if facts.compressed { units = saturatingAdd(units, Int64((facts.raw + slab - 1) / slab)) }
                }
            }
        }
        meter.charge(units); try meter.guardCost()
    }
}
