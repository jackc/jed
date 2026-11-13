/// Materialized FROM inputs retain packed base stores or independent query
/// plans. All observable traversal follows authored relation and row order.
internal final class RelationPlan {
    enum Body {
        case base(TableStore), derived(SelectPlan, SessionSettings), lateralDerived(SelectPlan, SessionSettings)
        case cte(CTEReference), function(TableFunctionPlan), jsonTable(JSONTablePlan), system(SystemRelationPlan), join(RelationPlan, RelationPlan, String, ResolvedExpression?)
    }
    enum SourceKind { case base, derived, cte, function, jsonTable, system, join }
    var sourceKind: SourceKind {
        switch body {
        case .base: .base
        case .derived, .lateralDerived: .derived
        case .cte: .cte
        case .function: .function
        case .jsonTable: .jsonTable
        case .system: .system
        case .join: .join
        }
    }
    var body: Body
    let columns: [ExpressionColumn]
    let functionalDependencies: [(primaryKey: [Int], columns: [Int])]
    let baseTable: CatalogTable?
    let sourceDomain: SQLText?
    let sourceSnapshot: DomainSnapshot?
    let sourceSettings: SessionSettings?
    let subqueryAlias: SQLText?
    var pushedOn: [ResolvedExpression]?
    var effectiveOn: ResolvedExpression? {
        if let pushedOn { return QueryEstimate.and(pushedOn) }
        if case .join(_, _, _, let predicate) = body { return predicate }
        return nil
    }
    let lateralColumns: Set<Int>
    let precedingWidth: Int
    var repeatsRight: Bool {
        guard case .join(_, let right, let kind, _) = body else { return false }
        // A fenced FROM shares its preceding namespace with local siblings.
        // INNER/CROSS/LEFT therefore repeat a correlated item even when its
        // references reach only that preceding namespace. RIGHT/FULL detach
        // such outside-only references and materialize them once.
        return ["inner", "cross", "left"].contains(kind) && !right.lateralColumns.isEmpty
    }
    var scanFilter: ResolvedExpression?
    var scanDisplayFilter: ResolvedExpression?
    var allowsIndexAccess: Bool { sourceDomain.map { !DomainNames.isAttachment($0) } ?? true }
    init(body: Body, columns: [ExpressionColumn], functionalDependencies: [(primaryKey: [Int], columns: [Int])] = [], baseTable: CatalogTable? = nil,
         lateralColumns: Set<Int> = [], precedingWidth: Int = 0,
         sourceDomain: SQLText? = nil, sourceSnapshot: DomainSnapshot? = nil, sourceSettings: SessionSettings? = nil,
         subqueryAlias: SQLText? = nil) {
        self.body = body; self.columns = columns; self.functionalDependencies = functionalDependencies; self.baseTable = baseTable
        self.lateralColumns = lateralColumns
        self.precedingWidth = precedingWidth
        self.sourceDomain = sourceDomain; self.sourceSnapshot = sourceSnapshot; self.sourceSettings = sourceSettings
        self.subqueryAlias = subqueryAlias
    }
    var initialFunctionOuterTouched: Set<Int> {
        var first = self
        while case .join(let left, _, _, _) = first.body { first = left }
        guard case .function(let function) = first.body else { return [] }
        return function.initialOuterTouched
    }
    var expressions: [ResolvedExpression] { QueryExpressions.collect(.relation(self), folding: false) }
    var foldExpressions: [ResolvedExpression] { QueryExpressions.collect(.relation(self), folding: true) }
    static func resolve(_ from: [SQLFrom], snapshot: DomainSnapshot, settings: SessionSettings,
                        resolver: ExpressionResolver) throws -> RelationPlan {
        var hintFrameWidth = 0
        func shifted(_ column: ExpressionColumn, by offset: Int) -> ExpressionColumn {
            ExpressionColumn(name: column.name, qualifier: column.qualifier, type: column.type,
                index: column.index + offset, collation: column.collation, qualifiedOnly: column.qualifiedOnly,
                unresolved: column.unresolved, scalarFunction: column.scalarFunction, hiddenFromStar: column.hiddenFromStar,
                ambiguousRelation: column.ambiguousRelation, lateralDenied: column.lateralDenied,
                ungroupedAncestor: column.ungroupedAncestor, catalogName: column.catalogName,
                hintSubrelation: column.hintSubrelation, hintQualifiedOnly: column.hintQualifiedOnly, hintSkipHidden: column.hintSkipHidden, rowName: column.rowName, rowNotNull: column.rowNotNull, rowNullable: column.rowNullable)
        }
        func source(_ input: SQLFrom, preceding: [ExpressionColumn] = [], predicateOffset: Int = 0) throws -> RelationPlan {
            let result: RelationPlan
            switch input.source {
            case .table(let name): result = try tableSource(input, name: name)
            case .query(let query): result = try querySource(input, query: query, preceding: preceding)
            case .function(let syntax): result = try functionSource(input, syntax: syntax, preceding: preceding)
            case .jsonTable(let syntax): result = try jsonTableSource(input, syntax: syntax, preceding: preceding)
            case .joined(let nested):
                let frames = resolver.nameHintFrames, priorWidth = hintFrameWidth
                let hidden = Array(preceding.dropFirst(hintFrameWidth))
                if !hidden.isEmpty {
                    resolver.nameHintFrames.append(NameHintFrame(depth: resolver.outerScopes.count,
                        parenthesizedJoin: true, relations: NameDiagnostics.relations(hidden, hidden: true)))
                }
                hintFrameWidth = preceding.count
                defer { resolver.nameHintFrames = frames; hintFrameWidth = priorWidth }
                result = fence(try source(nested, preceding: preceding), aliased: input.alias != nil)
            default: throw JedError("0A000", "this relation source is not yet connected")
            }
            return try finishSource(result, input: input, preceding: preceding, predicateOffset: predicateOffset)
        }
        func finishSource(_ result: RelationPlan, input: SQLFrom, preceding: [ExpressionColumn],
                          predicateOffset: Int) throws -> RelationPlan {
            guard input.columnAliases.count <= result.columns.count else {
                throw JedError("42P10", "too many column aliases")
            }
            var columns = result.columns
            for index in columns.indices {
                let old = columns[index]
                columns[index] = ExpressionColumn(name: index < input.columnAliases.count ? input.columnAliases[index] : old.name,
                    qualifier: input.alias ?? old.qualifier, type: old.type, index: old.index, collation: old.collation,
                    qualifiedOnly: old.qualifiedOnly, unresolved: old.unresolved,
                    scalarFunction: old.scalarFunction, hiddenFromStar: old.hiddenFromStar,
                    catalogName: old.catalogName, hintSubrelation: old.hintSubrelation,
                    hintQualifiedOnly: old.hintQualifiedOnly, hintSkipHidden: old.hintSkipHidden, rowName: old.rowName, rowNotNull: old.rowNotNull, rowNullable: old.rowNullable)
            }
            var combined = RelationPlan(body: result.body, columns: columns, functionalDependencies: result.functionalDependencies, baseTable: result.baseTable,
                lateralColumns: result.lateralColumns, precedingWidth: result.precedingWidth,
                sourceDomain: result.sourceDomain, sourceSnapshot: result.sourceSnapshot, sourceSettings: result.sourceSettings,
                subqueryAlias: input.alias ?? result.subqueryAlias)
            for (joinOrdinal, join) in input.joins.enumerated() {
                let denied = join.kind == "right" || join.kind == "full"
                let scope = preceding + combined.columns.map { column in
                    ExpressionColumn(name: column.name, qualifier: column.qualifier, type: column.type,
                        index: preceding.count + column.index, collation: column.collation, qualifiedOnly: column.qualifiedOnly,
                        unresolved: column.unresolved, scalarFunction: column.scalarFunction, hiddenFromStar: column.hiddenFromStar,
                        lateralDenied: denied, catalogName: column.catalogName, hintSubrelation: column.hintSubrelation,
                        hintQualifiedOnly: denied && (joinOrdinal > 0 || column.hintSubrelation), hintSkipHidden: column.hintSkipHidden, rowName: column.rowName, rowNotNull: column.rowNotNull, rowNullable: column.rowNullable)
                }
                combined = try combine(combined, source(join.right, preceding: scope), kind: join.kind, syntax: join.condition,
                    natural: join.natural, using: join.using, precedingWidth: preceding.count, predicateOffset: predicateOffset, unseen: Array(preceding.dropFirst(hintFrameWidth)))
            }
            return combined
        }
        func tableSource(_ input: SQLFrom, name: SQLName) throws -> RelationPlan {
            let result: RelationPlan
                if name.database == nil, let binding = resolver.cteBindings[name.name] {
                    let columns = binding.columns.enumerated().map { index, column in ExpressionColumn(name: column.name,
                        qualifier: input.alias ?? name.name, type: column.type ?? TypeDescriptor(.scalar(.text)), index: index,
                        collation: binding.collations[index]?.asColumn, unresolved: column.type == nil,
                        hiddenFromStar: binding.hiddenColumns.map { index >= $0 } ?? false) }
                    result = RelationPlan(body: .cte(try binding.reference(using: resolver)), columns: columns)
                    return result
                }
                if let system = try SystemRelationPlan.resolve(name, alias: input.alias, snapshot: snapshot, settings: settings) {
                    result = RelationPlan(body: .system(system), columns: system.columns)
                    return result
                }
                let domains = QueryDomains.statement(snapshot: snapshot, settings: settings)
                let selected: (name: SQLText, snapshot: DomainSnapshot)
                do { selected = try domains.relation(name) }
                catch let error as JedError where error.code == "42P01" { throw error.at(name.span.position) }
                let owner = selected.snapshot
                if let view = owner.catalog.views.first(where: { $0.name == name.name }) {
                    guard !DomainNames.isAttachment(selected.name) || domains.implicitDomain == selected.name else {
                        throw JedError("0A000", "views in attached databases are not supported")
                    }
                    if selected.name != DomainNames.temporary { try settings.privileges.require(.select, on: view.name) }
                    var inner = settings
                    inner.domains = try domains.viewScope(owner: selected.name)
                    inner.targetDomain = selected.name
                    if !view.securityInvoker { inner.privileges = settings.privileges.withTableAccess() }
                    let parsed = try ViewDefinition.parsed(view)
                    let isolated = ExpressionResolver(composites: owner.types, scalar: resolver.scalar)
                    isolated.viewExpansion = resolver.viewExpansion
                    isolated.cteExpansionEnabled = resolver.cteExpansionEnabled
                    let plan = try resolver.viewExpansion.withBody(view.name, referenceDepth: input.parserDepth,
                        offset: input.span.byteStart, bodyDepth: parsed.depth) {
                        try SelectPlan.resolve(parsed.query, snapshot: owner, settings: inner, scalar: resolver.scalar,
                                               using: isolated, finalizeParameters: false)
                    }
                    guard plan.columns.count == view.columns.count else { throw JedError("XX003", "stored view output changed") }
                    let columns = plan.columns.enumerated().map { ExpressionColumn(name: view.columns[$0.offset].name,
                        qualifier: input.alias ?? view.name, type: $0.element.type ?? TypeDescriptor(.scalar(.text)), index: $0.offset,
                        collation: plan.projections[$0.offset].collation?.asColumn, catalogName: view.name, rowName: view.columns[$0.offset].name) }
                    result = RelationPlan(body: .derived(plan, inner), columns: columns, subqueryAlias: view.name)
                    return result
                }
                guard let table = owner.catalog.tables.first(where: { $0.name == name.name }), let store = owner.tables[name.name] else {
                    throw JedError("42P01", "relation does not exist: \(name.name)").at(name.span.position)
                }
                if selected.name != DomainNames.temporary { try settings.privileges.require(.select, on: table.name) }
                let types = try owner.tableTypes(table)
                let columns = table.columns.enumerated().map { ExpressionColumn(name: $0.element.name,
                    qualifier: input.alias ?? table.name, type: types[$0.offset], index: $0.offset, catalogName: table.name, rowName: $0.element.name, rowNotNull: $0.element.notNull) }
                result = RelationPlan(body: .base(store), columns: columns,
                    functionalDependencies: [(primaryKey: table.primaryKey, columns: Array(columns.indices))], baseTable: table,
                    sourceDomain: selected.name, sourceSnapshot: owner, sourceSettings: settings)
            return result
        }
        func querySource(_ input: SQLFrom, query: SQLQuery, preceding: [ExpressionColumn]) throws -> RelationPlan {
            // Only the child resolver and one returned plan cross recursive
            // planning. Diagnostic-frame construction and branch temporaries
            // finish before descending into the next admitted query level.
            let child = queryResolver(input, preceding: preceding)
            let plan = try queryBody(query, child: child)
            return try finishQuerySource(plan, input: input, child: child)
        }
        func queryResolver(_ input: SQLFrom, preceding: [ExpressionColumn]) -> ExpressionResolver {
            let child = ExpressionResolver(composites: resolver.composites, scalar: resolver.scalar)
            child.outerScopes = input.lateral ? [preceding] + resolver.outerScopes : resolver.outerScopes
            child.outerNameHints = input.lateral ? [NameHintScope()] + resolver.outerNameHints : resolver.outerNameHints
            child.nameHintFrames = resolver.nameHintFrames
            let hidden = Array(preceding.dropFirst(hintFrameWidth))
            if !input.lateral && !hidden.isEmpty {
                child.nameHintFrames.append(NameHintFrame(depth: child.outerScopes.count,
                    parenthesizedJoin: false, relations: NameDiagnostics.relations(hidden, hidden: true)))
            }
            child.cteBindings = resolver.cteBindings; child.viewExpansion = resolver.viewExpansion
            child.cteExpansionEnabled = resolver.cteExpansionEnabled
            child.parameters = resolver.parameters; child.parameterUses = resolver.parameterUses
            return child
        }
        func queryBody(_ query: SQLQuery, child: ExpressionResolver) throws -> SelectPlan {
            if case .values = query.body, query.orderBy.isEmpty, query.limit == nil, query.offset == nil {
                return try CompoundQueryPlan.resolve(query, snapshot: snapshot, settings: settings,
                    scalar: resolver.scalar, using: child, finalizeParameters: false)
            }
            return try SelectPlan.resolve(query, snapshot: snapshot, settings: settings,
                scalar: resolver.scalar, using: child, finalizeParameters: false)
        }
        func finishQuerySource(_ initial: SelectPlan, input: SQLFrom, child: ExpressionResolver) throws -> RelationPlan {
            var plan = initial
            let result: RelationPlan
                resolver.parameters = child.parameters; resolver.parameterUses.formUnion(child.parameterUses)
                var lateral: Set<Int> = []
                for reference in child.outerReferences {
                    if input.lateral, reference.depth == 1 { lateral.insert(reference.index) }
                    else { resolver.outerReferences.insert(OuterColumnReference(depth: reference.depth - (input.lateral ? 1 : 0), index: reference.index)) }
                }
                let columns = plan.columns.enumerated().map { ExpressionColumn(name: $0.element.name, qualifier: input.alias,
                    type: $0.element.type ?? TypeDescriptor(.scalar(.text)), index: $0.offset,
                    collation: plan.projections[$0.offset].collation?.asColumn, unresolved: $0.element.type == nil) }
                result = RelationPlan(body: input.lateral ? .lateralDerived(plan, settings) : .derived(plan, settings), columns: columns, lateralColumns: lateral)
            return result
        }
        func functionSource(_ input: SQLFrom, syntax: SQLExpression, preceding: [ExpressionColumn]) throws -> RelationPlan {
            let result: RelationPlan
                let previous = resolver.columns, hints = resolver.nameHints
                resolver.columns = preceding; resolver.nameHints = NameHintScope(lateralActive: true)
                defer { resolver.columns = previous; resolver.nameHints = hints }
                let function = try TableFunctionPlan.resolve(input, syntax: syntax, resolver: resolver)
                result = RelationPlan(body: .function(function), columns: function.columns, lateralColumns: function.lateralColumns)
            return result
        }
        func jsonTableSource(_ input: SQLFrom, syntax: SQLJSONTable, preceding: [ExpressionColumn]) throws -> RelationPlan {
            let result: RelationPlan
                let previous = resolver.columns, hints = resolver.nameHints
                resolver.columns = preceding; resolver.nameHints = NameHintScope(lateralActive: true)
                defer { resolver.columns = previous; resolver.nameHints = hints }
                let table = try JSONTablePlan.resolve(input, syntax: syntax, resolver: resolver)
                result = RelationPlan(body: .jsonTable(table), columns: table.columns, lateralColumns: table.lateralColumns)
            return result
        }
        func fence(_ input: RelationPlan, aliased: Bool) -> RelationPlan {
            let physical = aliased ? input.columns.filter { !$0.qualifiedOnly && !$0.hiddenFromStar } : input.columns.sorted { $0.index < $1.index }
            let projections = physical.map { ResolvedExpression(type: $0.unresolved ? nil : $0.type, node: .column($0.index), position: 0, collation: $0.collation) }
            let plan = SelectPlan(columns: physical.map { ResultColumn(name: $0.name, type: $0.unresolved ? nil : $0.type) },
                projections: projections, predicate: nil, store: nil, touched: physical.map(\.index), parameters: [], limit: nil,
                offset: 0, order: [], orderedByScan: false, descending: false,
                scan: BaseScanPlan(predicate: nil, keys: [], types: []), relation: input)
            // An alias makes a parenthesized join a SELECT * boundary: merged
            // USING columns occur once, and all inner labels become hidden.
            let columns = aliased ? physical.enumerated().map { index, column in
                ExpressionColumn(name: column.name, qualifier: nil, type: column.type, index: index,
                    collation: column.collation?.asColumn, unresolved: column.unresolved, hintSkipHidden: true)
            } : input.columns.map { column in
                var column = column; column.hintSubrelation = true; return column
            }
            return RelationPlan(body: .derived(plan, settings), columns: columns,
                lateralColumns: input.lateralColumns, precedingWidth: input.precedingWidth)
        }
        func combine(_ left: RelationPlan, _ right: RelationPlan, kind: String, syntax: SQLExpression?,
                     natural: Bool = false, using: [SQLText] = [], precedingWidth: Int = 0,
                     predicateOffset: Int = 0, unseen: [ExpressionColumn] = []) throws -> RelationPlan {
            let leftLabels = Set(left.columns.compactMap(\.qualifier)), rightLabels = Set(right.columns.compactMap(\.qualifier))
            guard leftLabels.isDisjoint(with: rightLabels) else { throw JedError("42712", "table name specified more than once") }
            if right.lateralColumns.contains(where: { $0 >= precedingWidth }) && (kind == "right" || kind == "full") {
                throw JedError("42P10", "invalid reference to FROM-clause entry in a lateral outer join").detailed(ErrorHints.lateralDetail)
            }
            var columns = left.columns
            for column in right.columns {
                columns.append(ExpressionColumn(name: column.name, qualifier: column.qualifier, type: column.type,
                    index: left.columns.count + column.index, collation: column.collation, qualifiedOnly: column.qualifiedOnly, unresolved: column.unresolved,
                    scalarFunction: column.scalarFunction, hiddenFromStar: column.hiddenFromStar,
                    catalogName: column.catalogName, hintSubrelation: column.hintSubrelation,
                    hintQualifiedOnly: column.hintQualifiedOnly, hintSkipHidden: column.hintSkipHidden, rowName: column.rowName, rowNotNull: column.rowNotNull, rowNullable: column.rowNullable))
            }
            for index in columns.indices {
                if index < left.columns.count ? (kind == "right" || kind == "full") : (kind == "left" || kind == "full") {
                    columns[index].rowNullable = true
                }
            }
            let previous = resolver.columns, aggregate = resolver.resolveAggregate, window = resolver.resolveWindow, hints = resolver.nameHints
            resolver.columns = columns.map { shifted($0, by: predicateOffset) }
            resolver.nameHints = NameHintScope(unseen: NameDiagnostics.relations(unseen, hidden: true), onClause: true)
            resolver.resolveAggregate = AggregateQueryPlan.forbidden
            resolver.resolveWindow = WindowQueryBuilder.forbidden
            defer { resolver.columns = previous; resolver.resolveAggregate = aggregate; resolver.resolveWindow = window; resolver.nameHints = hints }
            var predicate = try syntax.map { try resolver.resolve($0, expected: TypeDescriptor(.scalar(.boolean))) }
            var mergeNames = using
            if natural {
                var seen: Set<SQLText> = []
                mergeNames = left.columns.filter { !$0.qualifiedOnly && !$0.hiddenFromStar }.compactMap { column in
                    guard right.columns.contains(where: { !$0.qualifiedOnly && !$0.hiddenFromStar && $0.name == column.name }),
                          seen.insert(column.name).inserted else { return nil }
                    return column.name
                }
            }
            guard mergeNames.isEmpty || kind != "full" else { throw JedError("0A000", "FULL JOIN USING is not supported") }
            var merged: [Int] = [], names: Set<SQLText> = []
            for name in mergeNames {
                guard names.insert(name).inserted else { throw JedError("42701", "column name appears more than once in USING clause") }
                let a = left.columns.filter { !$0.qualifiedOnly && $0.name == name }
                let b = right.columns.filter { !$0.qualifiedOnly && $0.name == name }
                guard !a.isEmpty, !b.isEmpty else { throw JedError("42703", "column specified in USING clause does not exist: \(name)") }
                guard a.count == 1, b.count == 1 else { throw JedError("42702", "common column name appears more than once: \(name)") }
                let lhs = a[0], rhs = b[0]
                func expression(_ column: ExpressionColumn, offset: Int = 0) -> ResolvedExpression {
                    ResolvedExpression(type: column.unresolved ? nil : column.type, node: .column(column.index + offset + predicateOffset), position: 0,
                        collation: column.collation ?? (ExpressionCollation.collatable(column.type.type)
                            ? .implicit(column.type.collation ?? SQLText(validatedUTF8: [67])) : nil))
                }
                let x = expression(lhs), y = expression(rhs, offset: left.columns.count)
                try TypeRules.comparable(x.type?.type, y.type?.type)
                let common = try TypeRules.common(x.type?.type, y.type?.type)
                var type = TypeDescriptor(common ?? .scalar(.text))
                if ExpressionCollation.collatable(common) { type.collation = try ExpressionCollation.combine([x, y]).orderingName() }
                let equal = ResolvedExpression(type: TypeDescriptor(.scalar(.boolean)), node: .binary("=", x, y, type), position: 0)
                predicate = predicate.map { ResolvedExpression(type: TypeDescriptor(.scalar(.boolean)), node: .binary("and", $0, equal, TypeDescriptor(.scalar(.boolean))), position: 0) } ?? equal
                let kept = kind == "right" ? rhs.index + left.columns.count : lhs.index
                let hidden = kind == "right" ? lhs.index : rhs.index + left.columns.count
                merged.append(kept)
                if let index = columns.firstIndex(where: { $0.index == hidden }) { columns[index].qualifiedOnly = true }
            }
            if !merged.isEmpty {
                let first = merged.compactMap { key in columns.first { $0.index == key } }
                columns = first + columns.filter { !merged.contains($0.index) }
            }
            guard predicate?.type == nil || predicate?.type?.type == .scalar(.boolean) else { throw JedError("42804", "JOIN ON requires a boolean expression").at(syntax?.expressionLocation ?? 0) }
            predicate = predicate.map { PredicateOrdering.order($0, planningProbe: settings.planningProbe) }
            let dependencies = left.functionalDependencies + right.functionalDependencies.map { fact in
                (primaryKey: fact.primaryKey.map { $0 + left.columns.count }, columns: fact.columns.map { $0 + left.columns.count })
            }
            return RelationPlan(body: .join(left, right, kind, predicate), columns: columns, functionalDependencies: dependencies,
                lateralColumns: left.lateralColumns.union(right.lateralColumns.filter { $0 < precedingWidth }), precedingWidth: precedingWidth)
        }
        // An unaliased group beginning a FROM item is the prefix of that
        // item's left-deep chain. Groups on a JOIN's right remain fences.
        func leadingGroup(_ input: SQLFrom) -> SQLFrom {
            var item = input
            while item.alias == nil, case .joined(var nested) = item.source {
                nested.joins.append(contentsOf: item.joins); item = nested
            }
            return item
        }
        // Namespace resolution remains local to each comma item. Once resolved,
        // an unfenced item joins the common left-deep physical chain. Its ON
        // columns were assigned global slots without exposing earlier labels.
        func appendItem(_ prefix: RelationPlan, _ input: RelationPlan) throws -> RelationPlan {
            let labels = Set(prefix.columns.compactMap(\.qualifier))
            guard labels.isDisjoint(with: Set(input.columns.compactMap(\.qualifier))) else {
                throw JedError("42712", "table name specified more than once")
            }
            var spine: [RelationPlan] = [], leaf = input
            while case .join(let left, _, _, _) = leaf.body { spine.append(leaf); leaf = left }
            var result = try combine(prefix, leaf, kind: "cross", syntax: nil)
            let offset = prefix.columns.count
            for item in spine.reversed() {
                guard case .join(_, let right, let kind, let predicate) = item.body else { preconditionFailure("invalid comma join spine") }
                let columns = prefix.columns + item.columns.map { shifted($0, by: offset) }
                let dependencies = prefix.functionalDependencies + item.functionalDependencies.map { fact in
                    (primaryKey: fact.primaryKey.map { $0 + offset }, columns: fact.columns.map { $0 + offset })
                }
                result = RelationPlan(body: .join(result, right, kind, predicate), columns: columns,
                    functionalDependencies: dependencies, lateralColumns: prefix.lateralColumns)
            }
            return result
        }
        guard let first = from.first else { throw JedError("XX000", "empty materialized FROM") }
        var result = try source(leadingGroup(first))
        for authored in from.dropFirst() {
            let next = leadingGroup(authored)
            let fenced = next.joins.contains { $0.kind == "right" || $0.kind == "full" }
            var input = try source(next, preceding: result.columns, predicateOffset: fenced ? 0 : result.columns.count)
            // A comma before RIGHT/FULL fences that complete join subtree as
            // an independently projected relation, once per preceding row.
            if fenced {
                input = fence(input, aliased: false)
                result = try combine(result, input, kind: "cross", syntax: nil)
            } else { result = try appendItem(result, input) }
        }
        return result
    }
}

internal final class RelationExecution {
    private let plan: RelationPlan
    private let context: ExpressionContext
    private let touched: Set<Int>
    private var rows: [[SQLValue]?] = []
    private var reserved: Int64 = 0
    private var position = 0
    private var started = false
    init(plan: RelationPlan, context: ExpressionContext, touched: Set<Int>) { self.plan = plan; self.context = context; self.touched = touched }
    private func reserve(_ row: [SQLValue]) throws {
        let bytes = MemorySizes.row(row); try context.meter.reserveQuery(bytes); reserved = saturatingAdd(reserved, bytes)
    }
    private func release(_ bytes: Int64) { context.meter.resources.query.release(bytes); reserved -= bytes }
    func next(retaining: Bool = false) throws -> [SQLValue]? {
        if !started { rows = try materialize(plan, touched: touched).map(Optional.some); started = true }
        guard position < rows.count, let row = rows[position] else { return nil }
        rows[position] = nil; position += 1
        if !retaining { release(MemorySizes.row(row)) }
        return row
    }
    func discardRetained(_ row: [SQLValue]) { release(MemorySizes.row(row)) }
    var hostBatchWindow: (index: Int, count: Int)? { position > 0 ? (position - 1, rows.count) : nil }
    func hostBatchValue(row: Int, column: Int, current: [SQLValue]) -> SQLValue? {
        let values = row == position - 1 ? current : rows.indices.contains(row) ? rows[row] : nil
        return values.flatMap { $0.indices.contains(column) ? $0[column] : nil }
    }
    private func materialize(_ plan: RelationPlan, touched: Set<Int>, context: ExpressionContext? = nil) throws -> [[SQLValue]] {
        let context = context ?? self.context
        let before = context.meter.accrued
        defer { context.executionProfile?.add(plan, .total, context.meter.accrued - before) }
        let needed = touched.union(plan.scanFilter.map(PushdownSafety.columns) ?? [])
        let produced: [[SQLValue]]
        switch plan.sourceKind {
        case .cte: produced = try materializeCTE(plan, touched: needed, context: context)
        case .function: produced = try materializeFunction(plan, touched: needed, context: context)
        case .system: produced = try materializeSystem(plan, touched: needed, context: context)
        case .jsonTable: produced = try materializeJSONTable(plan, touched: needed, context: context)
        case .base: return try materializeBase(plan, touched: needed, context: context)
        case .derived: produced = try materializeDerived(plan, touched: needed, context: context)
        case .join: produced = try materializeJoin(plan, touched: needed, context: context)
        }
        // Non-base producers finish and reserve their complete source before
        // scan filtering releases rejected rows. Base scans filter on admission.
        guard let filter = plan.scanFilter else { return produced }
        var kept: [[SQLValue]] = []
        for row in produced {
            var current = context; current.column = { row[$0] }
            if try current.matches(filter) { kept.append(row) }
            else { release(MemorySizes.row(row)) }
        }
        return kept
    }
    private func materializeCTE(_ plan: RelationPlan, touched: Set<Int>, context: ExpressionContext) throws -> [[SQLValue]] {
        guard case .cte(let reference) = plan.body else { throw JedError("XX000", "invalid CTE source") }
        func masked(_ row: [SQLValue]) -> [SQLValue] {
            row.enumerated().map { touched.contains($0.offset) ? $0.element : .null }
        }
        guard let ctes = context.ctes else { throw JedError("XX000", "WITH reference has no runtime scope") }
        var rows: [[SQLValue]] = []
        if reference.binding.buffers {
            try ctes.scan(reference.binding, plan: reference.inlinePlan, meter: context.meter) { row in let value = masked(row); try reserve(value); rows.append(value) }
        } else {
            // An inline body is collected in its full projected shape while
            // its producer remains live, then becomes a masked relation.
            try ctes.scan(reference.binding, plan: reference.inlinePlan, meter: context.meter) { row in try reserve(row); rows.append(row) }
            for index in rows.indices {
                let value = masked(rows[index])
                release(MemorySizes.row(rows[index]) - MemorySizes.row(value)); rows[index] = value
            }
        }
        return rows
    }
    private func materializeFunction(_ plan: RelationPlan, touched: Set<Int>, context: ExpressionContext) throws -> [[SQLValue]] {
        guard case .function(let function) = plan.body else { throw JedError("XX000", "invalid function source") }
        func masked(_ row: [SQLValue]) -> [SQLValue] {
            row.enumerated().map { touched.contains($0.offset) ? $0.element : .null }
        }
        var rows: [[SQLValue]] = []
        let admitsAsGenerated = function.function.name == "generate_series" || function.function.name == "unnest"
        try function.produce(context: context) { row in
            let value = masked(row)
            if admitsAsGenerated { try reserve(value) }
            rows.append(value)
        }
        if !admitsAsGenerated { for row in rows { try reserve(row) } }
        return rows
    }
    private func materializeSystem(_ plan: RelationPlan, touched: Set<Int>, context: ExpressionContext) throws -> [[SQLValue]] {
        guard case .system(let system) = plan.body else { throw JedError("XX000", "invalid system source") }
        func masked(_ row: [SQLValue]) -> [SQLValue] {
            row.enumerated().map { touched.contains($0.offset) ? $0.element : .null }
        }
        var rows: [[SQLValue]] = []
        try system.produce(context: context) { rows.append(masked($0)) }
        for row in rows { try reserve(row) }
        return rows
    }
    private func materializeJSONTable(_ plan: RelationPlan, touched: Set<Int>, context: ExpressionContext) throws -> [[SQLValue]] {
        guard case .jsonTable(let table) = plan.body else { throw JedError("XX000", "invalid JSON_TABLE source") }
        func masked(_ row: [SQLValue]) -> [SQLValue] {
            row.enumerated().map { touched.contains($0.offset) ? $0.element : .null }
        }
        var rows: [[SQLValue]] = []
        try table.produce(context: context) { rows.append(masked($0)) }
        // Bounded JSON sources finish evaluating and generating their
        // rows before the relation admits the completed output (Q1).
        for row in rows { try reserve(row) }
        return rows
    }
    private func materializeBase(_ plan: RelationPlan, touched: Set<Int>, context: ExpressionContext) throws -> [[SQLValue]] {
        guard case .base(let store) = plan.body else { throw JedError("XX000", "invalid base source") }
        let meter = context.meter
        let scan = BaseScanPlan(predicate: plan.scanFilter, keys: plan.allowsIndexAccess ? plan.baseTable?.primaryKey ?? [] : [], types: store.tree.types)
        let access = try scan.bind(context)
        try ScanCost.charge(store, touched: touched.sorted(), meter: meter, access: access)
        var cursor = access.cursor(store); defer { cursor.close() }
        var rows: [[SQLValue]] = []
        while let record = try cursor.next() {
            try meter.step(); meter.charge(1)
            var row = Array(repeating: SQLValue.null, count: plan.columns.count)
            for column in touched.sorted() { row[column] = try record.materialize(column: column, type: store.tree.types[column], pager: store.tree.pool?.pager) }
            if let filter = plan.scanFilter {
                var current = context; current.column = { row[$0] }
                if try !current.matches(filter) { continue }
            }
            try reserve(row); rows.append(row)
        }
        return rows
    }
    private func materializeDerived(_ plan: RelationPlan, touched: Set<Int>, context: ExpressionContext) throws -> [[SQLValue]] {
        let childProfile = context.executionProfile.map { _ in ExecutionProfile() }
        let execution = derivedExecution(plan, context: context, profile: childProfile)
        defer {
            execution.close()
            if let childProfile { context.executionProfile?.captureFirst(plan, from: childProfile) }
        }
        var rows: [[SQLValue]] = []
        while let row = try execution.next() { try reserve(row); rows.append(row) }
        execution.close()
        for index in rows.indices {
            let value = rows[index].enumerated().map { touched.contains($0.offset) ? $0.element : .null }
            release(MemorySizes.row(rows[index]) - MemorySizes.row(value)); rows[index] = value
        }
        return rows
    }
    private func derivedExecution(_ plan: RelationPlan, context: ExpressionContext, profile: ExecutionProfile?) -> SelectExecution {
        let child: SelectPlan, settings: SessionSettings, lateral: Bool
        switch plan.body {
        case .derived(let query, let configuration): child = query; settings = configuration; lateral = false
        case .lateralDerived(let query, let configuration): child = query; settings = configuration; lateral = true
        default: preconditionFailure("derived execution requires a derived source")
        }
        var current = context; current.patterns = PatternExecution()
        current.executionProfile = profile
        current.eagerSubqueries = false
        if lateral {
            current.outerRows = [context.column] + context.outerRows
            current.encloseHostBatchRow(from: context)
        }
        StatementServices(settings: settings, meter: context.meter).install(into: &current)
        return SelectExecution(plan: child, context: current)
    }
    private func materializeJoin(_ plan: RelationPlan, touched: Set<Int>, context: ExpressionContext) throws -> [[SQLValue]] {
        guard case .join(let lhs, let rhs, let kind, let predicate) = plan.body else { throw JedError("XX000", "invalid join source") }
        let meter = context.meter
        let residual = plan.effectiveOn
        if let island = NWayJoinPlan.resolve(plan) { return try materialize(island, touched: touched, context: context) }
        let localLateral = Set(rhs.lateralColumns.filter { $0 >= plan.precedingWidth }.map { $0 - plan.precedingWidth })
        var needed = touched.union(localLateral)
        if let predicate { StoredExpressionDependencies.walk([predicate]) { if case .column(let index) = $0.node { needed.insert(index) } } }
        let width = lhs.columns.count
        let physical = JoinPlan.choose(left: lhs, right: rhs, kind: kind, predicate: predicate)
        let leftTouched = Set(needed.filter { $0 < width }), rightTouched = Set(needed.filter { $0 >= width }.map { $0 - width })
        if plan.repeatsRight {
            let left = try materialize(lhs, touched: leftTouched, context: context)
            var output: [[SQLValue]] = []
            for a in left {
                var current = context
                current.column = { index in
                    if index < plan.precedingWidth { return try context.column(index) }
                    return a[index - plan.precedingWidth]
                }
                current.subqueries = SubqueryExecution()
                let right = try materialize(rhs, touched: rightTouched, context: current)
                var matched = false
                for b in right {
                    try meter.pollStep()
                    let joined = a + b
                    var joinedContext = context; joinedContext.column = { joined[$0] }
                    if let residual, try !joinedContext.matches(residual) { continue }
                    try reserve(joined); output.append(joined); matched = true
                }
                if !matched && kind == "left" {
                    let joined = a + Array(repeating: SQLValue.null, count: rhs.columns.count)
                    try reserve(joined); output.append(joined)
                }
                for row in right { release(MemorySizes.row(row)) }
            }
            for row in left { release(MemorySizes.row(row)) }
            return output
        }
        if case .indexed(let bound) = physical.algorithm {
            let outer = try materialize(physical.swapped ? rhs : lhs, touched: physical.swapped ? rightTouched : leftTouched, context: context)
            let inner = physical.swapped ? lhs : rhs
            guard case .base(let store) = inner.body else { throw JedError("XX000", "index join requires a base input") }
            let columns = (physical.swapped ? leftTouched : rightTouched).sorted()
            var output: [[SQLValue]] = []
            for row in outer {
                let placeholder = Array(repeating: SQLValue.null, count: inner.columns.count)
                let logical = physical.swapped ? placeholder + row : row + placeholder
                var boundsContext = context; boundsContext.outerRows = [{ logical[$0] }] + context.outerRows
                let access = try bound.bind(boundsContext)
                try context.profile(inner, .total) {
                    try ScanCost.charge(store, touched: columns, meter: meter, access: access)
                }
                var cursor = access.cursor(store); defer { cursor.close() }
                var candidates: [[SQLValue]] = []
                while let record = try cursor.next() {
                    let innerRow: [SQLValue]? = try context.profile(inner, .total) {
                        try meter.step(); meter.charge(1)
                        var value = placeholder
                        for column in columns { value[column] = try record.materialize(column: column, type: store.tree.types[column], pager: store.tree.pool?.pager) }
                        if let filter = inner.scanFilter {
                            var filtered = context; filtered.column = { value[$0] }
                            if try !filtered.matches(filter) { return nil }
                        }
                        return value
                    }
                    guard let innerRow else { continue }
                    try reserve(innerRow); candidates.append(innerRow)
                }
                var matched = false
                for innerRow in candidates {
                    let joined = physical.swapped ? innerRow + row : row + innerRow
                    var current = context; current.column = { joined[$0] }
                    if let residual, try !current.matches(residual) { continue }
                    try reserve(joined); output.append(joined); matched = true
                }
                if !matched && kind == "left" { try reserve(logical); output.append(logical) }
                for innerRow in candidates { release(MemorySizes.row(innerRow)) }
            }
            for row in outer { release(MemorySizes.row(row)) }
            return output
        }
        let left: [[SQLValue]], right: [[SQLValue]]
        if physical.swapped {
            right = try materialize(rhs, touched: rightTouched, context: context); left = try materialize(lhs, touched: leftTouched, context: context)
        } else {
            left = try materialize(lhs, touched: leftTouched, context: context); right = try materialize(rhs, touched: rightTouched, context: context)
        }
        var output: [[SQLValue]] = [], matchedRight: Set<Int> = []
        let outer = physical.swapped ? right : left, inner = physical.swapped ? left : right
        let hash: HashJoinTable?, probeKeys: [(column: Int, type: TypeDescriptor)]
        if case .hash(let keys) = physical.algorithm {
            let buildKeys = keys.map { (column: physical.swapped ? $0.left : $0.right, type: $0.type) }
            probeKeys = keys.map { (column: physical.swapped ? $0.right : $0.left, type: $0.type) }
            hash = try HashJoinTable(rows: inner, keys: buildKeys, context: context)
        } else { hash = nil; probeKeys = [] }
        defer { hash?.close() }
        for a in outer {
            var matched = false
            func candidate(_ index: Int) throws {
                try meter.pollStep()
                let b = inner[index], row = physical.swapped ? b + a : a + b
                var current = context; current.column = { row[$0] }
                if let residual, try !current.matches(residual) { return }
                try reserve(row); output.append(row); matched = true; matchedRight.insert(index)
            }
            if let hash { try hash.probe(a, keys: probeKeys, visit: candidate) }
            else { for index in inner.indices { try candidate(index) } }
            if !matched && (kind == "left" || kind == "full") {
                let row = a + Array(repeating: SQLValue.null, count: rhs.columns.count)
                try reserve(row); output.append(row)
            }
        }
        if kind == "right" || kind == "full" {
            for (index, b) in right.enumerated() where !matchedRight.contains(index) {
                let row = Array(repeating: SQLValue.null, count: width) + b
                try reserve(row); output.append(row)
            }
        }
        for row in left + right { release(MemorySizes.row(row)) }
        return output
    }
    private func materialize(_ island: NWayJoinPlan, touched: Set<Int>, context: ExpressionContext) throws -> [[SQLValue]] {
        var needed = touched
        StoredExpressionDependencies.walk(island.predicates) { if case .column(let index) = $0.node { needed.insert(index) } }
        let masks = island.inputs.map { input in Set(needed.filter { $0 >= input.offset && $0 < input.offset + input.source.columns.count }.map { $0 - input.offset }) }
        var relations = Array(repeating: [[SQLValue]](), count: island.inputs.count)
        let indexed = Set(island.winner.steps.compactMap { step -> Int? in if case .indexed = step.algorithm { return step.input }; return nil })
        for index in island.winner.order where !indexed.contains(index) {
            relations[index] = try materialize(island.inputs[index].source, touched: masks[index], context: context)
        }
        func combined(_ prefix: [SQLValue]?, _ row: [SQLValue], input: NWayJoinPlan.Input) -> [SQLValue] {
            var result = prefix ?? Array(repeating: .null, count: island.width)
            for index in row.indices { result[input.offset + index] = row[index] }
            return result
        }
        let driver = island.winner.order[0]
        var running: [[SQLValue]] = []
        for row in relations[driver] {
            let value = combined(nil, row, input: island.inputs[driver]); try reserve(value); running.append(value)
        }
        for step in island.winner.steps {
            let input = island.inputs[step.input]
            var output: [[SQLValue]] = []
            func keep(_ prefix: [SQLValue], _ row: [SQLValue]) throws {
                try context.meter.pollStep()
                let value = combined(prefix, row, input: input)
                var current = context; current.column = { value[$0] }
                for predicate in step.predicates where try !current.matches(predicate) { return }
                try reserve(value); output.append(value)
            }
            switch step.algorithm {
            case .indexed(let bound):
                guard case .base(let store) = input.source.body else { throw JedError("XX000", "indexed island input is not a table") }
                for prefix in running {
                    var current = context; current.outerRows = [{ prefix[$0] }] + context.outerRows
                    let access = try bound.bind(current)
                    let columns = masks[step.input].sorted()
                    let candidates: [[SQLValue]] = try context.profile(input.source, .total) {
                        try ScanCost.charge(store, touched: columns, meter: context.meter, access: access)
                        var cursor = access.cursor(store); defer { cursor.close() }
                        var candidates: [[SQLValue]] = []
                        while let record = try cursor.next() {
                            try context.meter.step(); context.meter.charge(1)
                            var row = Array(repeating: SQLValue.null, count: input.source.columns.count)
                            for column in columns { row[column] = try record.materialize(column: column, type: store.tree.types[column], pager: store.tree.pool?.pager) }
                            if let filter = input.source.scanFilter {
                                var filtered = context; filtered.column = { row[$0] }
                                if try !filtered.matches(filter) { continue }
                            }
                            try reserve(row); candidates.append(row)
                        }
                        return candidates
                    }
                    // Each seek materializes its candidate relation before ON.
                    // Those rows overlap this prefix's output until its probe ends.
                    for row in candidates { try keep(prefix, row) }
                    for row in candidates { release(MemorySizes.row(row)) }
                }
            case .hash(let keys):
                let rows = relations[step.input]
                let hash = try HashJoinTable(rows: rows, keys: keys.map { (column: $0.inner, type: $0.type) }, context: context)
                defer { hash.close() }
                for prefix in running {
                    try hash.probe(prefix, keys: keys.map { (column: $0.outer, type: $0.type) }) { try keep(prefix, rows[$0]) }
                }
            case .nested:
                for prefix in running { for row in relations[step.input] { try keep(prefix, row) } }
            }
            for row in running { release(MemorySizes.row(row)) }
            running = output
        }
        for rows in relations { for row in rows { release(MemorySizes.row(row)) } }
        return running
    }
    func releaseCharge() { context.meter.resources.query.release(reserved); reserved = 0 }
    func close() { rows = []; context.meter.resources.query.release(reserved); reserved = 0 }
    deinit { close() }
}

/// Shared source driver for streaming, ordering, DISTINCT and aggregation.
/// It returns raw source rows, with WHERE and output charges left to its caller.
internal final class SelectInput {
    private enum PointFeed {
        case deferred([UInt8]), prefetched(PackedRecord?), done
    }
    private let plan: SelectPlan
    private let context: ExpressionContext
    private var cursor: ScanCursor?
    private var physicalCursor: AccessCursor?
    private var pointFeed: PointFeed?
    private var directCursor = false
    private var relation: RelationExecution?
    private var joined: JoinAccessExecution?
    private var nway: NWayAccessExecution?
    private var started = false
    private var singleton = true
    private var empty = false
    private var buffered: [[SQLValue]?]?
    private var bufferedPosition = 0
    private var bufferedBytes: Int64 = 0
    private var virtualBytes: Int64 = 0
    private(set) var predicateApplied = false
    init(plan: SelectPlan, context: ExpressionContext) { self.plan = plan; self.context = context }
    func prime(directCursor: Bool = false) throws {
        if !started {
            let before = context.meter.accrued
            let filters = context.executionProfile?.value(plan.profileIdentity, .filter) ?? 0
            defer {
                let after = context.executionProfile?.value(plan.profileIdentity, .filter) ?? 0
                context.executionProfile?.add(plan.profileIdentity, .source, max(0, context.meter.accrued - before - (after - filters)))
            }
            started = true
            self.directCursor = directCursor
            if try plan.scan.hasKnownContradiction(parameters: context.parameters, collatedKey: context.collatedKey) {
                empty = true; return
            }
            if plan.chosenNWay != nil { nway = NWayAccessExecution(plan: plan, context: context, projected: false) }
            else if plan.chosenJoin != nil { joined = JoinAccessExecution(plan: plan, context: context, projected: false) }
            else if let source = plan.relation { relation = RelationExecution(plan: source, context: context, touched: Set(plan.touched)) }
            else {
                let access = try plan.chosenAccess?.bind(context) ?? plan.scan.bind(context); empty = access.isEmpty
                if plan.store == nil && !empty {
                    try context.meter.reserveQuery(GeneratedSpec.memoryRow); virtualBytes = GeneratedSpec.memoryRow
                }
                let incrementalFetch = !plan.materializeSource && plan.aggregation == nil && plan.window == nil
                    && plan.distinctOn == nil && (plan.order.isEmpty ? !plan.distinct && (plan.limit != nil || plan.streamsComputedProjection) : plan.orderedByScan)
                if let store = plan.store, !empty {
                    if directCursor, access.intervals == nil, let lower = access.lower,
                       let upper = access.upper, lower.inclusive, upper.inclusive, lower.key == upper.key {
                        let variable = plan.touched.contains { ValueCodec.fixedWidth(store.tree.types[$0].type) == nil }
                        if variable {
                            let point = try ScanCost.primaryPoint(lower.key, store: store, touched: plan.touched)
                            pointFeed = .prefetched(point.record)
                            context.meter.charge(point.units)
                        } else {
                            pointFeed = .deferred(lower.key)
                            context.meter.charge(Int64(store.tree.height))
                        }
                    } else if directCursor {
                        context.meter.charge(try ScanCost.units(store, touched: plan.touched, meter: context.meter, access: access))
                        cursor = access.cursor(store, descending: plan.descending)
                    } else if let candidate = plan.chosenAccess {
                        physicalCursor = try candidate.makeCursor(context: context, touched: plan.touched,
                            descending: plan.descending, access: access, incrementalFetch: incrementalFetch)
                    } else {
                        try ScanCost.charge(store, touched: plan.touched, meter: context.meter, access: access)
                        cursor = access.cursor(store, descending: plan.descending)
                    }

                    if plan.materializeSource || access.intervals != nil && !incrementalFetch {
                        buffered = []
                        while let record = try nextRecord() {
                            let row = try materialize(record, store: store)
                            let bytes = MemorySizes.row(row)
                            try context.meter.reserveQuery(bytes); bufferedBytes = saturatingAdd(bufferedBytes, bytes)
                            buffered!.append(row)
                        }
                        cursor?.close(); cursor = nil; physicalCursor?.close(); physicalCursor = nil
                        if plan.materializeSource, let predicate = plan.predicate {
                            // This is the whole WHERE, not a proven scan-pushed
                            // filter. Its source rows remain live through the
                            // eager admission boundary (memory.md §5).
                            var kept: [[SQLValue]?] = []
                            for item in buffered! {
                                guard let row = item else { continue }
                                var current = context; current.column = { row[$0] }
                                if try context.profile(plan.profileIdentity, .filter, { try current.matches(predicate) }) { kept.append(row) }
                                else {
                                    let bytes = MemorySizes.row(row); context.meter.resources.query.release(bytes); bufferedBytes -= bytes
                                }
                            }
                            buffered = kept; predicateApplied = true
                        }
                    }
                }
            }
        }
    }
    func next(retaining: Bool = false) throws -> [SQLValue]? {
        try prime()
        let before = context.meter.accrued
        defer { context.executionProfile?.add(plan.profileIdentity, .source, context.meter.accrued - before) }
        if let nway { return try nway.next(retaining: retaining) }
        if let joined { return try joined.next(retaining: retaining) }
        if let relation { return try relation.next(retaining: retaining) }
        if empty { return nil }
        if buffered != nil {
            guard bufferedPosition < buffered!.count, let row = buffered![bufferedPosition] else { return nil }
            buffered![bufferedPosition] = nil; bufferedPosition += 1
            if !retaining {
                let bytes = MemorySizes.row(row); context.meter.resources.query.release(bytes); bufferedBytes -= bytes
            }
            return row
        }
        if let store = plan.store {
            guard let record = try nextRecord() else { return nil }
            return try materialize(record, store: store)
        }
        guard singleton else { return nil }; singleton = false
        if !retaining { context.meter.resources.query.release(virtualBytes); virtualBytes = 0 }
        return []
    }
    func nextRecord() throws -> PackedRecord? {
        if let pointFeed {
            self.pointFeed = .done
            switch pointFeed {
            case .deferred(let key): return try plan.store!.get(key)
            case .prefetched(let record): return record
            case .done: return nil
            }
        }
        if let physicalCursor { return try physicalCursor.next() }
        return try cursor?.next()
    }
    var canReadAhead: Bool {
        guard plan.store != nil, relation == nil, joined == nil, nway == nil, buffered == nil, !empty else { return false }
        guard plan.orderedByScan || plan.order.isEmpty && (plan.limit != nil || plan.streamsComputedProjection) else { return false }
        if let candidate = plan.chosenAccess {
            switch candidate.path { case .primary, .full, .primaryIntervals: return true; default: return false }
        }
        return true
    }
    var hasHostBatchIntervals: Bool { physicalCursor?.hasHostBatchIntervals == true }
    func beginHostBatchInterval() throws -> Bool {
        let before = context.meter.accrued
        defer { context.executionProfile?.add(plan.profileIdentity, .source, context.meter.accrued - before) }
        return try physicalCursor?.beginHostBatchInterval() ?? true
    }
    func nextHostBatchRecord() throws -> PackedRecord? {
        if let physicalCursor { return try physicalCursor.nextHostBatchRecord() }
        return try nextRecord()
    }
    func materializeHostBatchRecord(_ record: PackedRecord) throws -> [SQLValue] {
        let before = context.meter.accrued
        defer { context.executionProfile?.add(plan.profileIdentity, .source, context.meter.accrued - before) }
        return try materialize(record, store: plan.store!)
    }
    var hostBatchWindow: (index: Int, count: Int)? {
        if let relation { return relation.hostBatchWindow }
        if let joined { return joined.hostBatchWindow }
        if let nway { return nway.hostBatchWindow }
        if let buffered, bufferedPosition > 0 { return (bufferedPosition - 1, buffered.count) }
        return nil
    }
    func hostBatchValue(row: Int, column: Int, current: [SQLValue]) -> SQLValue? {
        if let relation { return relation.hostBatchValue(row: row, column: column, current: current) }
        if let joined { return joined.hostBatchValue(row: row, column: column, current: current) }
        if let nway { return nway.hostBatchValue(row: row, column: column, current: current) }
        guard let buffered else { return nil }
        let values = row == bufferedPosition - 1 ? current : buffered.indices.contains(row) ? buffered[row] : nil
        return values.flatMap { $0.indices.contains(column) ? $0[column] : nil }
    }
    func materialize(_ record: PackedRecord, store: TableStore) throws -> [SQLValue] {
        // Scan rows check the prior work before charging and admitting the
        // next row. The terminal guard observes the last completed row.
        try context.meter.step(); context.meter.charge(1)
        var row = Array(repeating: SQLValue.null, count: store.tree.types.count)
        for column in plan.touched { row[column] = try record.materialize(column: column, type: store.tree.types[column], pager: store.tree.pool?.pager) }
        return row
    }
    func discardRetained(_ row: [SQLValue]) {
        if let nway { nway.discardRetained(row) }
        else if let joined { joined.discardRetained(row) }
        else if let relation { relation.discardRetained(row) }
        else if buffered != nil {
            let bytes = MemorySizes.row(row)
            context.meter.resources.query.release(bytes); bufferedBytes -= bytes
        } else if plan.store == nil {
            context.meter.resources.query.release(virtualBytes); virtualBytes = 0
        }
    }
    /// Materialized producers complete their original Q1 admission first.
    /// Transfer all of that ownership before the first bounded-spool append.
    func drainToSpool(config: SpillConfig) throws -> RowSpool {
        let output = RowSpool(config: config)
        do {
            let first = try next(retaining: true)
            relation?.releaseCharge(); joined?.releaseCharge(); nway?.releaseCharge()
            context.meter.resources.query.release(bufferedBytes); bufferedBytes = 0
            context.meter.resources.query.release(virtualBytes); virtualBytes = 0
            if let first { try output.append(first) }
            while let row = try next(retaining: true) { try output.append(row) }
            return output
        } catch { output.close(); throw error }
    }
    func close() {
        pointFeed = nil
        cursor?.close(); cursor = nil; physicalCursor?.close(); physicalCursor = nil; relation?.close(); relation = nil; joined?.close(); joined = nil; nway?.close(); nway = nil; buffered = nil
        context.meter.resources.query.release(bufferedBytes); bufferedBytes = 0
        context.meter.resources.query.release(virtualBytes); virtualBytes = 0
    }
    deinit { close() }
}
