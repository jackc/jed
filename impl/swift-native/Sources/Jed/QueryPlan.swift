/// Plans retain static metadata and packed storage roots. Scan execution owns a
/// bounded tree cursor and decodes only the statement's touched columns.
internal struct SelectPlan {
    /// Stored fields live in one copy-on-write box, so passing this value
    /// around costs one retain instead of a field-by-field copy.
    private final class Storage {
        var columns: [ResultColumn]
        var projections: [ResolvedExpression]
        var predicate: ResolvedExpression?
        var store: TableStore?
        var touched: [Int]
        var parameters: [TypeDescriptor]
        var limit: Int64?
        var offset: Int64
        var offsetPresent: Bool
        var order: [ResolvedOrderTerm]
        var orderedByScan: Bool
        var descending: Bool
        var scan: BaseScanPlan
        var aggregation: AggregateQueryPlan?
        var compound: CompoundQueryPlan?
        var relation: RelationPlan?
        var productionCharge: Bool
        var distinct: Bool
        var window: WindowQueryPlan?
        var withQuery: WithQueryPlan?
        var materializeSource: Bool
        var distinctOn: DistinctOnPlan?
        var sourceDomain: SQLText?
        var sourceSnapshot: DomainSnapshot?
        var sourceTable: CatalogTable?
        var profileIdentity: QueryProfileIdentity
        var profilesRoot: Bool
        var accessCandidates: [ScanCandidate]?
        var chosenAccess: ScanCandidate?
        var chosenJoin: JoinAccessPlan?
        var chosenNWay: NWayAccessPlan?
        var completePredicate: ResolvedExpression?
        init(columns: [ResultColumn], projections: [ResolvedExpression], predicate: ResolvedExpression?, store: TableStore?, touched: [Int], parameters: [TypeDescriptor], limit: Int64?, offset: Int64, offsetPresent: Bool, order: [ResolvedOrderTerm], orderedByScan: Bool, descending: Bool, scan: BaseScanPlan, aggregation: AggregateQueryPlan?, compound: CompoundQueryPlan?, relation: RelationPlan?, productionCharge: Bool, distinct: Bool, window: WindowQueryPlan?, withQuery: WithQueryPlan?, materializeSource: Bool, distinctOn: DistinctOnPlan?, sourceDomain: SQLText?, sourceSnapshot: DomainSnapshot?, sourceTable: CatalogTable?, profileIdentity: QueryProfileIdentity, profilesRoot: Bool, accessCandidates: [ScanCandidate]?, chosenAccess: ScanCandidate?, chosenJoin: JoinAccessPlan?, chosenNWay: NWayAccessPlan?, completePredicate: ResolvedExpression?) {
            self.columns = columns
            self.projections = projections
            self.predicate = predicate
            self.store = store
            self.touched = touched
            self.parameters = parameters
            self.limit = limit
            self.offset = offset
            self.offsetPresent = offsetPresent
            self.order = order
            self.orderedByScan = orderedByScan
            self.descending = descending
            self.scan = scan
            self.aggregation = aggregation
            self.compound = compound
            self.relation = relation
            self.productionCharge = productionCharge
            self.distinct = distinct
            self.window = window
            self.withQuery = withQuery
            self.materializeSource = materializeSource
            self.distinctOn = distinctOn
            self.sourceDomain = sourceDomain
            self.sourceSnapshot = sourceSnapshot
            self.sourceTable = sourceTable
            self.profileIdentity = profileIdentity
            self.profilesRoot = profilesRoot
            self.accessCandidates = accessCandidates
            self.chosenAccess = chosenAccess
            self.chosenJoin = chosenJoin
            self.chosenNWay = chosenNWay
            self.completePredicate = completePredicate
        }
        func copy() -> Storage {
            Storage(columns: columns, projections: projections, predicate: predicate, store: store, touched: touched, parameters: parameters, limit: limit, offset: offset, offsetPresent: offsetPresent, order: order, orderedByScan: orderedByScan, descending: descending, scan: scan, aggregation: aggregation, compound: compound, relation: relation, productionCharge: productionCharge, distinct: distinct, window: window, withQuery: withQuery, materializeSource: materializeSource, distinctOn: distinctOn, sourceDomain: sourceDomain, sourceSnapshot: sourceSnapshot, sourceTable: sourceTable, profileIdentity: profileIdentity, profilesRoot: profilesRoot, accessCandidates: accessCandidates, chosenAccess: chosenAccess, chosenJoin: chosenJoin, chosenNWay: chosenNWay, completePredicate: completePredicate)
        }
    }
    private var storage: Storage
    @inline(__always) private mutating func unique() {
        if !isKnownUniquelyReferenced(&storage) { storage = storage.copy() }
    }
    init(columns: [ResultColumn], projections: [ResolvedExpression], predicate: ResolvedExpression? = nil, store: TableStore? = nil, touched: [Int], parameters: [TypeDescriptor], limit: Int64?, offset: Int64, offsetPresent: Bool = false, order: [ResolvedOrderTerm], orderedByScan: Bool, descending: Bool, scan: BaseScanPlan, aggregation: AggregateQueryPlan? = nil, compound: CompoundQueryPlan? = nil, relation: RelationPlan? = nil, productionCharge: Bool = true, distinct: Bool = false, window: WindowQueryPlan? = nil, withQuery: WithQueryPlan? = nil, materializeSource: Bool = false, distinctOn: DistinctOnPlan? = nil, sourceDomain: SQLText? = nil, sourceSnapshot: DomainSnapshot? = nil, sourceTable: CatalogTable? = nil, profileIdentity: QueryProfileIdentity = QueryProfileIdentity(), profilesRoot: Bool = true, accessCandidates: [ScanCandidate]? = nil, chosenAccess: ScanCandidate? = nil, chosenJoin: JoinAccessPlan? = nil, chosenNWay: NWayAccessPlan? = nil, completePredicate: ResolvedExpression? = nil) {
        storage = Storage(columns: columns, projections: projections, predicate: predicate, store: store, touched: touched, parameters: parameters, limit: limit, offset: offset, offsetPresent: offsetPresent, order: order, orderedByScan: orderedByScan, descending: descending, scan: scan, aggregation: aggregation, compound: compound, relation: relation, productionCharge: productionCharge, distinct: distinct, window: window, withQuery: withQuery, materializeSource: materializeSource, distinctOn: distinctOn, sourceDomain: sourceDomain, sourceSnapshot: sourceSnapshot, sourceTable: sourceTable, profileIdentity: profileIdentity, profilesRoot: profilesRoot, accessCandidates: accessCandidates, chosenAccess: chosenAccess, chosenJoin: chosenJoin, chosenNWay: chosenNWay, completePredicate: completePredicate)
    }
    var columns: [ResultColumn] {
        get { storage.columns }
        _modify { unique(); yield &storage.columns }
    }
    var projections: [ResolvedExpression] {
        get { storage.projections }
        _modify { unique(); yield &storage.projections }
    }
    var predicate: ResolvedExpression? {
        get { storage.predicate }
        _modify { unique(); yield &storage.predicate }
    }
    var store: TableStore? {
        get { storage.store }
        _modify { unique(); yield &storage.store }
    }
    var touched: [Int] {
        get { storage.touched }
        _modify { unique(); yield &storage.touched }
    }
    var parameters: [TypeDescriptor] {
        get { storage.parameters }
        _modify { unique(); yield &storage.parameters }
    }
    var limit: Int64? { storage.limit }
    var offset: Int64 { storage.offset }
    var offsetPresent: Bool {
        get { storage.offsetPresent }
        _modify { unique(); yield &storage.offsetPresent }
    }
    var order: [ResolvedOrderTerm] { storage.order }
    var orderedByScan: Bool {
        get { storage.orderedByScan }
        _modify { unique(); yield &storage.orderedByScan }
    }
    var descending: Bool {
        get { storage.descending }
        _modify { unique(); yield &storage.descending }
    }
    var scan: BaseScanPlan {
        get { storage.scan }
        _modify { unique(); yield &storage.scan }
    }
    var aggregation: AggregateQueryPlan? {
        get { storage.aggregation }
        _modify { unique(); yield &storage.aggregation }
    }
    var compound: CompoundQueryPlan? {
        get { storage.compound }
        _modify { unique(); yield &storage.compound }
    }
    var relation: RelationPlan? {
        get { storage.relation }
        _modify { unique(); yield &storage.relation }
    }
    var productionCharge: Bool {
        get { storage.productionCharge }
        _modify { unique(); yield &storage.productionCharge }
    }
    var distinct: Bool {
        get { storage.distinct }
        _modify { unique(); yield &storage.distinct }
    }
    var window: WindowQueryPlan? {
        get { storage.window }
        _modify { unique(); yield &storage.window }
    }
    var withQuery: WithQueryPlan? {
        get { storage.withQuery }
        _modify { unique(); yield &storage.withQuery }
    }
    var materializeSource: Bool {
        get { storage.materializeSource }
        _modify { unique(); yield &storage.materializeSource }
    }
    var distinctOn: DistinctOnPlan? {
        get { storage.distinctOn }
        _modify { unique(); yield &storage.distinctOn }
    }
    var sourceDomain: SQLText? {
        get { storage.sourceDomain }
        _modify { unique(); yield &storage.sourceDomain }
    }
    var sourceSnapshot: DomainSnapshot? {
        get { storage.sourceSnapshot }
        _modify { unique(); yield &storage.sourceSnapshot }
    }
    var sourceTable: CatalogTable? {
        get { storage.sourceTable }
        _modify { unique(); yield &storage.sourceTable }
    }
    var profileIdentity: QueryProfileIdentity {
        get { storage.profileIdentity }
        _modify { unique(); yield &storage.profileIdentity }
    }
    var profilesRoot: Bool {
        get { storage.profilesRoot }
        _modify { unique(); yield &storage.profilesRoot }
    }
    var accessCandidates: [ScanCandidate]? {
        get { storage.accessCandidates }
        _modify { unique(); yield &storage.accessCandidates }
    }
    var chosenAccess: ScanCandidate? {
        get { storage.chosenAccess }
        _modify { unique(); yield &storage.chosenAccess }
    }
    var chosenJoin: JoinAccessPlan? {
        get { storage.chosenJoin }
        _modify { unique(); yield &storage.chosenJoin }
    }
    var chosenNWay: NWayAccessPlan? {
        get { storage.chosenNWay }
        _modify { unique(); yield &storage.chosenNWay }
    }
    var completePredicate: ResolvedExpression? {
        get { storage.completePredicate }
        _modify { unique(); yield &storage.completePredicate }
    }
    var keyEvaluationOrder: [Int] { distinctOn?.evaluationOrder ?? Array(order.indices) }

    var expressions: [ResolvedExpression] { QueryExpressions.collect(.query(self), folding: false) }

    /// WITH bodies fold only after their runtime binding layer is installed.
    var foldExpressions: [ResolvedExpression] { QueryExpressions.collect(.query(self), folding: true) }

    static func resolve(_ query: SQLQuery, snapshot: DomainSnapshot, settings: SessionSettings,
                        scalar: ScalarCasts.Context, using shared: ExpressionResolver? = nil,
                        finalizeParameters: Bool = true) throws -> SelectPlan {
        // Each admitted derived/view level keeps this dispatcher on the native
        // stack. Keep syntax construction and branch-specific plan temporaries
        // in separate frames, including in unoptimized builds.
        if case .select(let select) = query.body {
            return try resolveSelect(query, select: select, snapshot: snapshot, settings: settings,
                scalar: scalar, using: shared, finalizeParameters: finalizeParameters)
        }
        if case .with(let clause, let child) = query.body {
            return try resolveWith(query, clause: clause, child: child, snapshot: snapshot, settings: settings,
                scalar: scalar, using: shared, finalizeParameters: finalizeParameters)
        }
        if case .nested(let nested) = query.body, query.orderBy.isEmpty, query.limit == nil, query.offset == nil {
            return try resolve(nested, snapshot: snapshot, settings: settings, scalar: scalar, using: shared, finalizeParameters: finalizeParameters)
        }
        if case .values = query.body {
            return try resolveValues(query, snapshot: snapshot, settings: settings, scalar: scalar,
                using: shared, finalizeParameters: finalizeParameters)
        }
        return try CompoundQueryPlan.resolve(query, snapshot: snapshot, settings: settings, scalar: scalar,
            using: shared, finalizeParameters: finalizeParameters)
    }

    private static func resolveWith(_ query: SQLQuery, clause: SQLWith, child: SQLQuery,
                                    snapshot: DomainSnapshot, settings: SessionSettings, scalar: ScalarCasts.Context,
                                    using shared: ExpressionResolver?, finalizeParameters: Bool) throws -> SelectPlan {
        let body = query.orderBy.isEmpty && query.limit == nil && query.offset == nil ? child
            : SQLQuery(body: .nested(child), orderBy: query.orderBy, limit: query.limit, offset: query.offset, span: query.span)
        return try WithQueryPlan.resolve(clause, body: body, snapshot: snapshot, settings: settings,
            scalar: scalar, using: shared, finalizeParameters: finalizeParameters)
    }

    private static func resolveValues(_ query: SQLQuery, snapshot: DomainSnapshot, settings: SessionSettings,
                                      scalar: ScalarCasts.Context, using shared: ExpressionResolver?,
                                      finalizeParameters: Bool) throws -> SelectPlan {
        // A VALUES query is SELECT * over an intrinsic VALUES relation. Its
        // ORDER BY sees the ordinary scope and its rows carry the SELECT charge.
        let values = SQLQuery(body: query.body, span: query.span)
        let source = SQLFrom(source: .query(values), alias: SQLText(validatedUTF8: Array("*VALUES*".utf8)), span: query.span)
        let select = SQLSelect(items: [SQLSelectItem(star: true, span: query.span)], from: [source])
        let wrapped = SQLQuery(body: .select(select), orderBy: query.orderBy,
            limit: query.limit, offset: query.offset, span: query.span)
        return try resolve(wrapped, snapshot: snapshot, settings: settings, scalar: scalar,
            using: shared, finalizeParameters: finalizeParameters)
    }

    private static func resolveSelect(_ query: SQLQuery, select: SQLSelect, snapshot: DomainSnapshot,
                                      settings: SessionSettings, scalar: ScalarCasts.Context,
                                      using shared: ExpressionResolver?, finalizeParameters: Bool) throws -> SelectPlan {
        guard select.into == nil else {
            throw JedError("0A000", "blocking query plan is not yet connected")
        }
        let resolver = selectResolver(snapshot: snapshot, settings: settings, scalar: scalar, using: shared)
        let input = try resolveInput(select, snapshot: snapshot, settings: settings, resolver: resolver)
        return try finishSelect(query, select: select, resolver: resolver, settings: settings,
            finalizeParameters: finalizeParameters, input: input)
    }

    private static func selectResolver(snapshot: DomainSnapshot, settings: SessionSettings,
                                      scalar: ScalarCasts.Context, using shared: ExpressionResolver?) -> ExpressionResolver {
        let resolver = shared ?? ExpressionResolver(composites: snapshot.types, scalar: scalar)
        resolver.queryScope = SubqueryScope(snapshot: snapshot, settings: settings)
        resolver.hostFunctions = settings.environment.signatures; resolver.unicode = settings.environment.unicode
        resolver.checkFunction = { try settings.privileges.require(.execute, on: $0, function: true) }
        return resolver
    }

    private struct ResolvedInput {
        var source: TableStore?
        var scope: [ExpressionColumn]
        var primaryKey: [Int]
        var sourceDomain: SQLText?
        var sourceSnapshot: DomainSnapshot?
        var sourceTable: CatalogTable?
        var functionalKey: [Int]
        var relation: RelationPlan?
    }
    private static func resolveInput(_ select: SQLSelect, snapshot: DomainSnapshot, settings: SessionSettings,
                                     resolver: ExpressionResolver) throws -> ResolvedInput {
        let domains = QueryDomains.statement(snapshot: snapshot, settings: settings)
        if select.from.count == 1, let from = select.from.first, from.joins.isEmpty,
           case .table(let name) = from.source, !SystemRelationPlan.contains(name.name),
           (name.database != nil || resolver.cteBindings[name.name] == nil) {
            let selected: (name: SQLText, snapshot: DomainSnapshot)
            do { selected = try domains.relation(name) }
            catch let error as JedError where error.code == "42P01" { throw error.at(name.span.position) }
            if let table = selected.snapshot.catalog.tables.first(where: { $0.name == name.name }) {
                return try resolveBaseInput(from, name: name, table: table, selected: selected, settings: settings)
            }
        }
        let relation = try select.from.isEmpty ? nil : RelationPlan.resolve(select.from, snapshot: snapshot, settings: settings, resolver: resolver)
        return ResolvedInput(source: nil, scope: relation?.columns ?? [], primaryKey: [], sourceDomain: nil,
            sourceSnapshot: nil, sourceTable: nil, functionalKey: [], relation: relation)
    }

    private static func resolveBaseInput(_ from: SQLFrom, name: SQLName, table: CatalogTable,
                                         selected: (name: SQLText, snapshot: DomainSnapshot), settings: SessionSettings) throws -> ResolvedInput {
        var source: TableStore?, scope: [ExpressionColumn] = [], primaryKey: [Int] = []
        var sourceDomain: SQLText?, sourceSnapshot: DomainSnapshot?, sourceTable: CatalogTable?, functionalKey: [Int] = []
        let owner = selected.snapshot
        guard let store = owner.tables[name.name] else { throw JedError("42P01", "relation does not exist: \(name.name)").at(name.span.position) }
        if selected.name != DomainNames.temporary { try settings.privileges.require(.select, on: table.name) }
        guard from.columnAliases.count <= table.columns.count else { throw JedError("42P10", "too many column aliases") }
        source = store
        sourceDomain = selected.name; sourceSnapshot = owner
        sourceTable = table
        functionalKey = table.primaryKey
        primaryKey = table.primaryKey
        if primaryKey.contains(where: { !CatalogEnvironment.collationMatches(table.columns[$0].type.collation,
            image: owner.catalog, environment: settings.environment) }) { primaryKey = [] }
        let types = try owner.tableTypes(table)
        scope = table.columns.enumerated().map { index, column in
            ExpressionColumn(name: index < from.columnAliases.count ? from.columnAliases[index] : column.name,
                             qualifier: from.alias ?? table.name, type: types[index], index: index, catalogName: table.name)
        }
        return ResolvedInput(source: source, scope: scope, primaryKey: primaryKey, sourceDomain: sourceDomain,
            sourceSnapshot: sourceSnapshot, sourceTable: sourceTable, functionalKey: functionalKey, relation: nil)
    }

    private static func finishSelect(_ query: SQLQuery, select: SQLSelect, resolver: ExpressionResolver,
                                     settings: SessionSettings, finalizeParameters: Bool,
                                     input: ResolvedInput) throws -> SelectPlan {
        let source = input.source, scope = input.scope, primaryKey = input.primaryKey
        let sourceDomain = input.sourceDomain, sourceSnapshot = input.sourceSnapshot, sourceTable = input.sourceTable
        let functionalKey = input.functionalKey, relation = input.relation
        let previousColumns = resolver.columns, previousTouched = resolver.referencedColumns
        let previousAggregate = resolver.resolveAggregate
        let previousWindow = resolver.resolveWindow
        resolver.columns = scope; resolver.referencedColumns = []
        resolver.resolveAggregate = AggregateQueryPlan.forbidden
        resolver.resolveWindow = WindowQueryBuilder.forbidden
        defer {
            resolver.columns = previousColumns; resolver.referencedColumns = previousTouched
            resolver.resolveAggregate = previousAggregate; resolver.resolveWindow = previousWindow
        }
        resolver.hostFunctions = settings.environment.signatures
        resolver.unicode = settings.environment.unicode
        resolver.checkFunction = { try settings.privileges.require(.execute, on: $0, function: true) }
        var predicate = try select.predicate.map { try resolver.resolve($0, expected: TypeDescriptor(.scalar(.boolean))) }
        guard predicate?.type == nil || predicate?.type?.type == .scalar(.boolean) else { throw JedError("42804", "WHERE requires a boolean expression").at(select.predicate?.expressionLocation ?? 0) }
        predicate = predicate.map { PredicateOrdering.order($0, planningProbe: settings.planningProbe) }
        let completePredicate = predicate
        let proofValues = ParameterBinding.proofValues(settings.planningParameters,
            knownTypes: resolver.parameters, timezone: settings.timeZone)
        if let relation, !PredicateContradiction.proves(predicate, parameters: proofValues, planningProbe: settings.planningProbe) {
            predicate = try RelationPushdown.whereFilter(predicate, into: relation, parameters: proofValues, planningProbe: settings.planningProbe)
        }
        let windows = try WindowQueryBuilder(select: select, resolver: resolver)
        let orderSyntax = query.orderBy + select.distinctOn.map { SQLOrderTerm(expression: $0, span: $0.span) }
        let aggregation = try AggregateQueryPlan.resolve(select, order: orderSyntax, resolver: resolver, scope: scope, primaryKey: functionalKey,
            functionalDependencies: relation?.functionalDependencies ?? [], windows: windows, planningProbe: settings.planningProbe)
        var projections: [ResolvedExpression] = [], columns: [ResultColumn] = []
        var projectionSyntax: [SQLExpression?] = []
        if let aggregation { projections = aggregation.projections; columns = aggregation.columns }
        for item in aggregation == nil ? select.items : [] {
            if item.star {
                var selected = scope.filter { !$0.hiddenFromStar && (item.qualifier == nil ? !$0.qualifiedOnly : item.qualifier == $0.qualifier) }
                if item.qualifier != nil { selected.sort { $0.index < $1.index } }
                guard !selected.isEmpty else { throw JedError(item.qualifier == nil ? "42601" : "42P01", "wildcard has no matching relation") }
                for column in selected {
                    resolver.referencedColumns.insert(column.index)
                    projections.append(ResolvedExpression(type: column.unresolved ? nil : column.type, node: .column(column.index), position: item.span.position,
                        collation: ExpressionCollation.collatable(column.type.type) ? .implicit(column.type.collation ?? SQLText(validatedUTF8: [67])) : nil))
                    columns.append(ResultColumn(name: column.name, type: column.unresolved ? nil : column.type))
                    projectionSyntax.append(nil)
                }
            } else if let syntax = item.expression {
                if case .field(let source, nil) = syntax.kind {
                    let child = try resolver.resolve(source)
                    guard let type = child.type?.type, let fields = TypeRules.fields(type) else { throw JedError("42809", "record wildcard requires a composite type") }
                    for (index, field) in fields.enumerated() {
                        projections.append(ResolvedExpression(type: field.descriptor, node: .field(child, index), position: syntax.position))
                        columns.append(ResultColumn(name: field.name, type: field.descriptor))
                        projectionSyntax.append(SQLExpression(kind: .field(source, field.name), span: syntax.span, position: syntax.position))
                    }
                    continue
                }
                let expression = try resolver.resolve(syntax)
                columns.append(ResultColumn(name: item.alias ?? outputName(syntax), type: expression.type))
                projections.append(expression); projectionSyntax.append(syntax)
            }
        }
        guard !projections.isEmpty else { throw JedError("42601", "SELECT requires a target list") }
        let resolvedOrder = try aggregation?.order ?? resolveOutputOrder(orderSyntax, writtenOrderCount: query.orderBy.count,
            resolver: resolver, windows: windows, projectionSyntax: projectionSyntax, projections: projections, columns: columns)
        StoredExpressionDependencies.walk(Array(resolvedOrder.prefix(query.orderBy.count)).map(\.expression)) {
            if case .subquery(let query) = $0.node { query.runtimeOrderKey = true }
        }
        var order = Array(resolvedOrder.prefix(query.orderBy.count))
        var distinctOn: DistinctOnPlan?
        if !select.distinctOn.isEmpty {
            let resolved = try DistinctOnPlan.resolve(keys: Array(resolvedOrder.dropFirst(query.orderBy.count)), order: order,
                same: { a, b in aggregation?.sameKey(a, b, otherColumns: windows.sameColumn) ?? windows.sameKey(a, b) })
            distinctOn = resolved.plan; order = resolved.order
        }
        let reverse = order.first?.descending ?? false
        if aggregation == nil { try windows.validateDefinitions() }
        let window = windows.finish(inputWidth: aggregation.map { $0.layoutWidth + $0.calls.count + $0.grouping.count } ?? scope.count,
            aggregation: aggregation)
        if select.distinct && distinctOn == nil {
            for expression in projections { try AggregateQueryPlan.equalityType(expression) }
            for (index, term) in order.enumerated() {
                // SQL92 ordinals and output names already designate a target.
                // Re-resolving its aggregate for ORDER keeps a separate fold
                // slot, so raw resolved-column identity is insufficient here.
                let syntax = query.orderBy[index].expression
                if case .number(let spelling) = syntax.kind, Int(spelling) != nil { continue }
                if case .column(nil, let name) = syntax.kind, columns.contains(where: { $0.name == name }) { continue }
                guard projections.contains(where: { aggregation?.sameKey($0, term.expression, otherColumns: windows.sameColumn) ?? windows.sameKey($0, term.expression) }) else {
                    throw JedError("42P10", "for SELECT DISTINCT, ORDER BY expressions must appear in select list")
                }
            }
        }
        var orderedByScan = distinctOn == nil && aggregation == nil && !order.isEmpty && !primaryKey.isEmpty && (!reverse || order.count >= primaryKey.count)
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
            guard let value = try? TypeRules.unsignedLiteral(spelling), value <= UInt64(Int64.max) else {
                throw JedError(code, "invalid row-count bound")
            }
            return Int64(value)
        }
        var plan = SelectPlan(columns: columns, projections: projections, predicate: predicate, store: source,
                          touched: resolver.referencedColumns.union((relation?.initialFunctionOuterTouched ?? []).filter { index in scope.contains { $0.index == index } }).sorted(), parameters: parameters,
                          limit: try bound(query.limit, code: "2201W"), offset: try bound(query.offset, code: "2201X") ?? 0,
                          order: order, orderedByScan: orderedByScan, descending: orderedByScan && reverse,
                          scan: BaseScanPlan(predicate: completePredicate,
                              keys: sourceDomain.map(DomainNames.isAttachment) == true ? [] : primaryKey,
                              types: source?.tree.types ?? []), aggregation: aggregation,
                          relation: relation, distinct: select.distinct && distinctOn == nil, window: window, distinctOn: distinctOn,
                          sourceDomain: sourceDomain, sourceSnapshot: sourceSnapshot, sourceTable: sourceTable)
        plan.offsetPresent = query.offset != nil
        plan.completePredicate = completePredicate
        if let table = sourceTable, let owner = sourceSnapshot, let domain = sourceDomain, let store = source {
            let context = StatementServices(settings: settings, meter: CostMeter()).context()
            plan.accessCandidates = try AccessInventory.resolve(table: table, store: store, snapshot: owner, domain: domain,
                predicate: predicate, base: plan.scan, settings: settings, context: context,
                orderOnly: plan.limit != nil && aggregation == nil && distinctOn == nil ? order : [])
        }
        return plan
    }

    private static func resolveOutputOrder(_ orderSyntax: [SQLOrderTerm], writtenOrderCount: Int,
                                           resolver: ExpressionResolver, windows: WindowQueryBuilder,
                                           projectionSyntax: [SQLExpression?], projections: [ResolvedExpression],
                                           columns: [ResultColumn]) throws -> [ResolvedOrderTerm] {
        var resolvedOrder: [ResolvedOrderTerm] = []
        for (keyOrdinal, term) in orderSyntax.enumerated() {
            let restoreWindow = windows.restorePoint()
            windows.beginOrderExpression()
            var expression: ResolvedExpression
            let source: SQLExpression?
            if case .number(let spelling) = term.expression.kind, let ordinal = Int(spelling) {
                guard ordinal > 0 && ordinal <= projections.count else { throw JedError("42P10", "ORDER BY position is not in select list") }
                source = projectionSyntax[ordinal - 1]
                expression = try source.map { try resolver.resolve($0) } ?? projections[ordinal - 1]
            } else if case .column(nil, let name) = term.expression.kind,
                      columns.contains(where: { $0.name == name }) {
                let matches = columns.indices.filter { columns[$0].name == name }
                guard matches.dropFirst().allSatisfy({ ExpressionIdentity.same(projections[matches[0]], projections[$0]) }) else {
                    throw JedError("42702", "ORDER BY reference is ambiguous: \(name)")
                }
                source = projectionSyntax[matches[0]]
                expression = try source.map { try resolver.resolve($0) } ?? projections[matches[0]]
            } else { source = term.expression; expression = try resolver.resolve(term.expression) }
            var type = expression.type ?? TypeDescriptor(.scalar(.text))
            type.collation = try ExpressionCollation.derive(expression).orderingName()
            try ResolutionDiagnostics.comparable(type.type, at: term.expression.expressionLocation, ordering: true)
            if keyOrdinal >= writtenOrderCount,
               let existing = resolvedOrder.first(where: { windows.sameKey(expression, $0.expression) }) {
                restoreWindow(); expression = existing.expression
            }
            resolvedOrder.append(ResolvedOrderTerm(expression: expression, type: type, descending: term.descending,
                nullsFirst: term.nullsFirst ?? term.descending, materialized: source.map(OrderSyntax.materialized) ?? false))
        }
        return resolvedOrder
    }

    internal static func outputName(_ expression: SQLExpression) -> SQLText {
        switch expression.kind {
        case .currentTimestamp: return SQLText(validatedUTF8: Array("now".utf8))
        case .currentDate: return SQLText(validatedUTF8: Array("current_date".utf8))
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
    let types: [TypeDescriptor]
    let pager: Pager?
    private var values: [Int: SQLValue] = [:]
    init(_ record: PackedRecord, types: [TypeDescriptor], pager: Pager?) {
        self.record = record; self.types = types; self.pager = pager
    }
    func value(_ column: Int) throws -> SQLValue {
        guard record.cells.indices.contains(column) else { throw JedError("XX000", "resolved column is outside the row") }
        if let value = values[column] { return value }
        let value = try record.materialize(column: column, type: types[column], pager: pager)
        values[column] = value; return value
    }
}

internal final class SelectExecution {
    private(set) var plan: SelectPlan
    let context: ExpressionContext
    private let preparedRequirements: PreparedSelectRequirements?
    private var input: SelectInput?
    private var started = false
    private var skipped: Int64 = 0
    private var produced: Int64 = 0
    private var ordered: OrderedSelectExecution?
    private var grouped: GroupedSelectExecution?
    private var combined: CompoundQueryExecution?
    private var deduplicated: DistinctExecution?
    private var windowed: WindowSelectExecution?
    private var withQuery: WithQueryExecution?
    private var columnar: ColumnarProjectionExecution?
    private var orderedJoin: JoinAccessExecution?
    private var orderedNWay: NWayAccessExecution?
    private var blocking: BlockingSelectExecution?
    private var initializedSubqueries = false
    private var initializedBackend = false
    private var initializedBatches = false
    private var bufferedScan: [[SQLValue]?]?
    private var bufferedScanBytes: Int64 = 0
    private var bufferedScanPosition = 0
    private var projectionBatch: HostBatch?
    private var filterBatch: HostBatch?
    private var readAhead: HostReadAhead<PackedRecord>?
    private var rawIntervalStarted = false
    /// DISTINCT owns its survivor window while this producer must keep pulling
    /// duplicate rows. This bounds speculation without imposing a SQL LIMIT.
    var hostBatchRemaining: (() -> Int64?)?
    init(plan: SelectPlan, context: ExpressionContext, preparedRequirements: PreparedSelectRequirements? = nil) {
        self.plan = plan; self.context = context
        self.preparedRequirements = preparedRequirements
    }
    private func scalarInput() -> SelectInput {
        if let input { return input }
        let input = SelectInput(plan: plan, context: context)
        self.input = input
        return input
    }
    /// Only the public read cursor calls this. Nested execution continues using
    /// next(), so its caller owns the established evaluation boundary.
    func openCursor() throws {
        // WITH/set-operation bodies keep their complete execution deferred.
        guard plan.withQuery == nil, plan.compound == nil else { return }
        let before = context.meter.accrued
        defer { if plan.profilesRoot { context.executionProfile?.add(plan.profileIdentity, .total, context.meter.accrued - before) } }
        try initialize()
        try initializeSubqueries()
        // The direct pull lane covers one real table and a full/contiguous PK
        // bound. Generalized index/interval/opclass lanes remain buffered.
        guard plan.store != nil, plan.relation == nil, plan.aggregation == nil,
              plan.window == nil, plan.distinctOn == nil, !plan.materializeSource,
              plan.scan.pointSets.isEmpty,
              (plan.order.isEmpty ? !plan.distinct && plan.limit != nil : plan.orderedByScan) else { return }
        if let candidate = plan.chosenAccess {
            switch candidate.path {
            case .primary, .full: break
            default: return
            }
        }
        if let deduplicated { try deduplicated.openCursor() }
        else { try scalarInput().prime(directCursor: true); started = true }
    }
    private func initializeSubqueries() throws {
        if !initializedSubqueries {
            if context.eagerSubqueries {
                try context.profile(plan.profileIdentity, .folded) {
                    if preparedRequirements?.needsEagerSubqueryPreparation ?? true {
                        try context.subqueries.prepare(plan.foldExpressions, context: context)
                    }
                }
            }
            initializedSubqueries = true
        }
    }
    private func initialize() throws {
        guard !initializedBackend else { return }
        plan = try plan.selectAccess(parameters: context.parameters, context: context)
        // The scalar blocking lane owns its complete source before WHERE and
        // projection. Direct ordered/LIMIT and packed columnar lanes have their
        // own admission schedules (memory.md §5.1, streaming.md §4).
        if plan.store != nil, plan.relation == nil, plan.aggregation == nil,
           plan.window == nil, !plan.distinct, plan.distinctOn == nil,
           plan.order.isEmpty, plan.limit == nil, !ColumnarExecution.projection(plan, context) {
            plan.materializeSource = true
        }
        initializedBackend = true
        if let withPlan = plan.withQuery { withQuery = WithQueryExecution(plan: withPlan, context: context) }
        else if plan.distinct { deduplicated = DistinctExecution(plan: plan, context: context) }
        else if let compound = plan.compound { combined = CompoundQueryExecution(plan: plan, compound: compound, context: context) }
        else if let window = plan.window { windowed = WindowSelectExecution(plan: plan, window: window, context: context) }
        else if let aggregation = plan.aggregation { grouped = GroupedSelectExecution(plan: plan, aggregate: aggregation, context: context) }
        else if context.spill?.canSpill == true, plan.distinctOn == nil, BlockingJoinRows.eligible(plan) {
            blocking = BlockingSelectExecution(plan: plan, context: context)
        }
        else if !plan.order.isEmpty && !plan.orderedByScan { ordered = OrderedSelectExecution(plan: plan, context: context) }
        else if plan.chosenNWay != nil, plan.orderedByScan { orderedNWay = NWayAccessExecution(plan: plan, context: context, projected: true) }
        else if plan.chosenJoin != nil, plan.orderedByScan { orderedJoin = JoinAccessExecution(plan: plan, context: context, projected: true) }
        else if ColumnarExecution.projection(plan, context) { columnar = ColumnarProjectionExecution(plan: plan, context: context) }
    }
    func next() throws -> [SQLValue]? {
        try initialize()
        return try buffersScanOutput ? nextBufferedScan() : nextUnbuffered()
    }
    private func nextBufferedScan() throws -> [SQLValue]? {
        if bufferedScan == nil {
            bufferedScan = []
            do {
                while let row = try nextUnbuffered() {
                    let bytes = MemorySizes.row(row)
                    try context.meter.reserveQuery(bytes)
                    bufferedScanBytes = saturatingAdd(bufferedScanBytes, bytes)
                    bufferedScan!.append(row)
                }
                input?.close()
            } catch {
                bufferedScan = []
                context.meter.resources.query.release(bufferedScanBytes); bufferedScanBytes = 0
                throw error
            }
        }
        guard bufferedScanPosition < bufferedScan!.count,
              let row = bufferedScan![bufferedScanPosition] else { return nil }
        bufferedScan![bufferedScanPosition] = nil; bufferedScanPosition += 1
        let bytes = MemorySizes.row(row)
        context.meter.resources.query.release(bytes); bufferedScanBytes -= bytes
        return row
    }
    /// The public pull cursor is narrower than the streaming SQL execution
    /// lane: generalized bounds finish their projected result before yielding.
    /// Their candidate keys stay uncharged; the retained result owns Q1 rows.
    private var buffersScanOutput: Bool {
        guard plan.store != nil, plan.relation == nil, plan.aggregation == nil,
              plan.window == nil, !plan.distinct, plan.distinctOn == nil, !plan.materializeSource, plan.productionCharge,
              plan.order.isEmpty ? plan.limit != nil : plan.orderedByScan,
              let candidate = plan.chosenAccess else { return false }
        switch candidate.path {
        case .primary, .full: return false
        case .ordered, .gin, .gist, .primaryIntervals, .orderedIntervals: return true
        }
    }
    private func nextUnbuffered() throws -> [SQLValue]? {
        let before = context.meter.accrued
        defer { if plan.profilesRoot { context.executionProfile?.add(plan.profileIdentity, .total, context.meter.accrued - before) } }
        try initializeSubqueries()
        if let withQuery { return try withQuery.next() }
        if let deduplicated { return try deduplicated.next() }
        if let combined { return try combined.next() }
        if let windowed { return try windowed.next() }
        if let grouped { return try grouped.next() }
        if let blocking { return try blocking.next() }
        if let ordered { return try ordered.next() }
        if let columnar { return try columnar.next() }
        if let orderedJoin { return try orderedJoin.next() }
        if let orderedNWay { return try orderedNWay.next() }
        let input = scalarInput()
        let meter = context.meter
        if !started { try input.prime(); started = true }
        if !initializedBatches {
            initializedBatches = true
            projectionBatch = HostBatch(expressions: plan.projections, context: context)
            if input.canReadAhead {
                filterBatch = plan.predicate.flatMap { HostBatch(expressions: [$0], context: context) }
                if projectionBatch != nil || filterBatch != nil { readAhead = HostReadAhead() }
            }
        }
        if let limit = plan.limit, produced >= limit { return nil }
        while true {
            let row: [SQLValue]
            var rawIndex: Int?
            if let ahead = readAhead {
                if !rawIntervalStarted {
                    guard try input.beginHostBatchInterval() else { return nil }
                    rawIntervalStarted = true
                }
                let remaining = batchRemaining()
                let cap = HostReadAhead<PackedRecord>.cap(meter: meter, unit: 1, remaining: remaining)
                guard let index = try ahead.next(cap: cap, pull: input.nextHostBatchRecord) else {
                    if input.hasHostBatchIntervals {
                        ahead.reset(); filterBatch?.reset(); projectionBatch?.reset()
                        rawIntervalStarted = false; continue
                    }
                    return nil
                }
                if index == 0 { filterBatch?.reset(); projectionBatch?.reset() }
                rawIndex = index
                row = try input.materializeHostBatchRecord(ahead.rows[index])
            } else {
                guard let next = try input.next(retaining: true) else { return nil }; row = next
            }
            try meter.checkpoint()
            var rowContext = context; rowContext.column = { row[$0] }
            rowContext.hostBatchColumn = { row.indices.contains($0) ? row[$0] : nil }
            if let predicate = plan.predicate, !input.predicateApplied {
                if let index = rawIndex, let ahead = readAhead, let batch = filterBatch {
                    try batch.setRow(index, count: ahead.rows.count, context: rowContext) { r, c in
                        r == index ? row[c] : try Self.prefetch(ahead.rows[r], column: c, types: self.plan.store?.tree.types ?? [])
                    }
                    rowContext.hostBatch = batch
                }
                guard try context.profile(plan.profileIdentity, .filter, { try rowContext.matches(predicate) }) else { input.discardRetained(row); continue }
            }
            if skipped < plan.offset { skipped += 1; continue }
            if plan.productionCharge { meter.charge(1); try meter.guardCost() }
            rowContext.hostBatch = nil
            if let batch = projectionBatch {
                if let index = rawIndex, let ahead = readAhead {
                    try batch.setRow(index, count: ahead.rows.count, context: rowContext) { r, c in
                        r == index ? row[c] : try Self.prefetch(ahead.rows[r], column: c, types: self.plan.store?.tree.types ?? [])
                    }
                    rowContext.hostBatch = batch
                } else if let window = input.hostBatchWindow {
                    let remaining = batchRemaining()
                    let count = remaining.map { window.index + Int(min(Int64(window.count - window.index), $0)) } ?? window.count
                    try batch.setRow(window.index, count: count, context: rowContext) { r, c in
                        input.hostBatchValue(row: r, column: c, current: row)
                    }
                    rowContext.hostBatch = batch
                }
            }
            let values = try plan.projections.map { try rowContext.evaluate($0) }
            produced += 1
            return values
        }
    }
    private func batchRemaining() -> Int64? {
        let own = plan.limit.map { saturatingAdd(plan.offset - skipped, $0 - produced) }
        guard let outer = hostBatchRemaining?() else { return own }
        return own.map { min($0, outer) } ?? outer
    }
    private static func prefetch(_ record: PackedRecord, column: Int, types: [TypeDescriptor]) throws -> SQLValue? {
        guard record.cells.indices.contains(column), types.indices.contains(column),
              record.overflows[column] == nil else { return nil }
        let cell = record.cells[column]
        guard cell.isNull || cell.fixedWidth != nil || cell.bytes?.first == 0 else { return nil }
        return try cell.materialize(type: types[column])
    }
    func close() {
        bufferedScan = nil
        context.meter.resources.query.release(bufferedScanBytes); bufferedScanBytes = 0
        readAhead?.reset(); readAhead = nil; filterBatch?.reset(); filterBatch = nil
        projectionBatch?.reset(); projectionBatch = nil
        blocking?.close(); blocking = nil
        input?.close(); ordered?.close(); ordered = nil; grouped?.close(); grouped = nil
        combined?.close(); combined = nil; deduplicated?.close(); deduplicated = nil; windowed?.close(); windowed = nil
        withQuery?.close(); withQuery = nil; columnar?.close(); columnar = nil; orderedJoin?.close(); orderedJoin = nil; orderedNWay?.close(); orderedNWay = nil
    }
}

internal enum ScanCost {
    static func charge(_ store: TableStore, touched: [Int], meter: CostMeter, access: ScanAccess = .full) throws {
        meter.charge(try units(store, touched: touched, meter: meter, access: access)); try meter.guardCost()
    }
    static func units(_ store: TableStore, touched: [Int], meter: CostMeter, access: ScanAccess = .full) throws -> Int64 {
        guard !access.isEmpty else { return 0 }
        // Read the store's facts once: this loop visits every row in range.
        let tree = store.tree
        var units = access.pages(tree)
        let variable = touched.filter { ValueCodec.fixedWidth(tree.types[$0].type) == nil }
        if !variable.isEmpty {
            let slab = tree.codec.capacity, encrypted = tree.codec.reservedBytes != 0
            var scan = access.cursor(store); defer { scan.close() }
            // Leaf runs visit the rows `nextPosition` would, in its order.
            while let run = try scan.nextRun() {
                let node = run.node
                let packed: PackedLeaf? = node.pending.isEmpty ? node.leaf : nil
                for ordinal in 0..<run.count {
                    let row = run.row(ordinal)
                    try meter.pollStep()
                    // Inline and NULL cells have no large-value facts; only a
                    // row holding some other tag takes the general reading.
                    if let packed, variable.allSatisfy({ packed.hasNoLargeFacts(row: row, column: $0) }) { continue }
                    units = saturatingAdd(units, try payload(TreePosition(node: node, row: row), slab: slab,
                        encrypted: encrypted, touched: variable))
                }
            }
        }
        return units
    }
    /// `charge` for an access that names one key (`ScanAccess.pointKey`): the
    /// same units, charged at the same moment. One descent finds the row and
    /// counts the nodes the interval overlaps, so the scan need not descend
    /// again.
    static func chargePoint(_ store: TableStore, key: [UInt8], variable: [Int], meter: CostMeter) throws -> TreePosition? {
        let tree = store.tree
        let probe = try tree.probe(key)
        var units = Int64(probe.nodes)
        if !variable.isEmpty, let position = probe.position {
            try meter.pollStep()
            units = saturatingAdd(units, try payload(position, slab: tree.codec.capacity,
                encrypted: tree.codec.reservedBytes != 0, touched: variable))
        }
        meter.charge(units); try meter.guardCost()
        return probe.position
    }
    /// The touched columns whose cells can hold large-value facts.
    static func variableColumns(_ touched: [Int], types: [TypeDescriptor]) -> [Int] {
        touched.filter { ValueCodec.fixedWidth(types[$0].type) == nil }
    }
    /// Decode the point's packed record once; accounting consumes its retained
    /// large-value facts without walking the same table interval again.
    static func point(_ key: [UInt8], store: TableStore, touched: [Int]) throws -> (record: PackedRecord, units: Int64) {
        guard let record = try store.get(key) else { throw JedError.corruption("index entry refers to a missing table row") }
        return (record, saturatingAdd(Int64(store.tree.height), try payload(record, store: store, touched: touched)))
    }
    /// A primary point can legitimately miss. Keep its one reconstructed row
    /// with the scan-wide disposition cost for the first cursor pull.
    static func primaryPoint(_ key: [UInt8], store: TableStore, touched: [Int]) throws -> (record: PackedRecord?, units: Int64) {
        let record = try store.get(key)
        let units = try record.map { try payload($0, store: store, touched: touched) } ?? 0
        return (record, saturatingAdd(Int64(store.tree.height), units))
    }
    private static func payload(_ record: PackedRecord, store: TableStore, touched: [Int]) throws -> Int64 {
        let tree = store.tree, cells = record.cells
        let slab = tree.codec.capacity, encrypted = tree.codec.reservedBytes != 0
        let chainSlab = slab - (encrypted ? 16 : 0)
        var units: Int64 = 0
        for column in touched where ValueCodec.fixedWidth(tree.types[column].type) == nil {
            guard let facts = try cells[column].largeFacts(encrypted: encrypted) else { continue }
            if facts.external != nil { units = saturatingAdd(units, Int64((facts.stored + chainSlab - 1) / chainSlab)) }
            if facts.compressed { units = saturatingAdd(units, Int64((facts.raw + slab - 1) / slab)) }
        }
        return units
    }
    /// A preflight inspects the same packed tags, including dirty placeholder
    /// cells, without reconstructing an unused key and every column's cell.
    private static func payload(_ position: TreePosition, slab: Int, encrypted: Bool, touched: [Int]) throws -> Int64 {
        let chainSlab = slab - (encrypted ? 16 : 0)
        var units: Int64 = 0
        for column in touched {
            guard let facts = try position.cell(column).largeFacts(encrypted: encrypted) else { continue }
            if facts.external != nil { units = saturatingAdd(units, Int64((facts.stored + chainSlab - 1) / chainSlab)) }
            if facts.compressed { units = saturatingAdd(units, Int64((facts.raw + slab - 1) / slab)) }
        }
        return units
    }
}
