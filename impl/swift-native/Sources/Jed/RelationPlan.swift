/// Materialized FROM inputs retain packed base stores or independent query
/// plans. All observable traversal follows authored relation and row order.
internal final class RelationPlan {
    enum Body {
        case base(TableStore), derived(SelectPlan, SessionSettings), lateralDerived(SelectPlan, SessionSettings)
        case cte(CTEReference), function(TableFunctionPlan), jsonTable(JSONTablePlan), system(SystemRelationPlan), join(RelationPlan, RelationPlan, String, ResolvedExpression?)
    }
    let body: Body
    let columns: [ExpressionColumn]
    let functionalDependencies: [(primaryKey: [Int], columns: [Int])]
    let baseTable: CatalogTable?
    let sourceDomain: SQLText?
    let sourceSnapshot: DomainSnapshot?
    let lateralColumns: Set<Int>
    let precedingWidth: Int
    var scanFilter: ResolvedExpression?
    var allowsIndexAccess: Bool { sourceDomain.map { !DomainNames.isAttachment($0) } ?? true }
    init(body: Body, columns: [ExpressionColumn], functionalDependencies: [(primaryKey: [Int], columns: [Int])] = [], baseTable: CatalogTable? = nil,
         lateralColumns: Set<Int> = [], precedingWidth: Int = 0,
         sourceDomain: SQLText? = nil, sourceSnapshot: DomainSnapshot? = nil) {
        self.body = body; self.columns = columns; self.functionalDependencies = functionalDependencies; self.baseTable = baseTable
        self.lateralColumns = lateralColumns
        self.precedingWidth = precedingWidth
        self.sourceDomain = sourceDomain; self.sourceSnapshot = sourceSnapshot
    }
    var expressions: [ResolvedExpression] {
        switch body {
        case .base, .cte, .system: []
        case .derived(let plan, _), .lateralDerived(let plan, _): plan.expressions
        case .function(let function): function.expressions
        case .jsonTable(let table): table.expressions
        case .join(let left, let right, _, let predicate): left.expressions + right.expressions + (predicate.map { [$0] } ?? [])
        }
    }
    var foldExpressions: [ResolvedExpression] {
        switch body {
        case .base, .cte, .system: []
        case .derived(let plan, _), .lateralDerived(let plan, _): plan.foldExpressions
        case .function(let function): function.expressions
        case .jsonTable(let table): table.expressions
        case .join(let left, let right, _, let predicate): left.foldExpressions + right.foldExpressions + (predicate.map { [$0] } ?? [])
        }
    }
    static func resolve(_ from: [SQLFrom], snapshot: DomainSnapshot, settings: SessionSettings,
                        resolver: ExpressionResolver) throws -> RelationPlan {
        func source(_ input: SQLFrom, preceding: [ExpressionColumn] = []) throws -> RelationPlan {
            let result: RelationPlan
            switch input.source {
            case .table(let name):
                if name.database == nil, let binding = resolver.cteBindings[name.name] {
                    let columns = binding.columns.enumerated().map { index, column in ExpressionColumn(name: column.name,
                        qualifier: input.alias ?? name.name, type: column.type ?? TypeDescriptor(.scalar(.text)), index: index,
                        collation: binding.collations[index]?.asColumn, unresolved: column.type == nil,
                        hiddenFromStar: binding.hiddenColumns.map { index >= $0 } ?? false) }
                    result = RelationPlan(body: .cte(try binding.reference()), columns: columns)
                    break
                }
                if let system = try SystemRelationPlan.resolve(name, alias: input.alias, snapshot: snapshot, settings: settings) {
                    result = RelationPlan(body: .system(system), columns: system.columns)
                    break
                }
                let domains = QueryDomains.statement(snapshot: snapshot, settings: settings)
                let selected = try domains.relation(name), owner = selected.snapshot
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
                    let plan = try resolver.viewExpansion.withBody(view.name, referenceDepth: input.parserDepth,
                        offset: input.span.byteStart, bodyDepth: parsed.depth) {
                        try SelectPlan.resolve(parsed.query, snapshot: owner, settings: inner, scalar: resolver.scalar,
                                               using: isolated, finalizeParameters: false)
                    }
                    guard plan.columns.count == view.columns.count else { throw JedError("XX003", "stored view output changed") }
                    let columns = plan.columns.enumerated().map { ExpressionColumn(name: view.columns[$0.offset].name,
                        qualifier: input.alias ?? view.name, type: $0.element.type ?? TypeDescriptor(.scalar(.text)), index: $0.offset,
                        collation: plan.projections[$0.offset].collation?.asColumn) }
                    result = RelationPlan(body: .derived(plan, inner), columns: columns)
                    break
                }
                guard let table = owner.catalog.tables.first(where: { $0.name == name.name }), let store = owner.tables[name.name] else {
                    throw JedError("42P01", "relation does not exist: \(name.name)")
                }
                if selected.name != DomainNames.temporary { try settings.privileges.require(.select, on: table.name) }
                let types = try owner.tableTypes(table)
                let columns = table.columns.enumerated().map { ExpressionColumn(name: $0.element.name,
                    qualifier: input.alias ?? table.name, type: types[$0.offset], index: $0.offset) }
                result = RelationPlan(body: .base(store), columns: columns,
                    functionalDependencies: [(primaryKey: table.primaryKey, columns: Array(columns.indices))], baseTable: table,
                    sourceDomain: selected.name, sourceSnapshot: owner)
            case .query(let query):
                let child = ExpressionResolver(composites: resolver.composites, scalar: resolver.scalar)
                child.outerScopes = input.lateral ? [preceding] + resolver.outerScopes : resolver.outerScopes
                child.cteBindings = resolver.cteBindings; child.viewExpansion = resolver.viewExpansion
                child.parameters = resolver.parameters; child.parameterUses = resolver.parameterUses
                var plan = try SelectPlan.resolve(query, snapshot: snapshot, settings: settings, scalar: resolver.scalar,
                                                  using: child, finalizeParameters: false)
                resolver.parameters = child.parameters; resolver.parameterUses.formUnion(child.parameterUses)
                var lateral: Set<Int> = []
                for reference in child.outerReferences {
                    if input.lateral, reference.depth == 1 { lateral.insert(reference.index) }
                    else { resolver.outerReferences.insert(OuterColumnReference(depth: reference.depth - (input.lateral ? 1 : 0), index: reference.index)) }
                }
                if let compound = plan.compound, case .values(let rows, _) = compound.body {
                    plan.compound = CompoundQueryPlan(body: .values(rows, false), settings: settings)
                }
                let columns = plan.columns.enumerated().map { ExpressionColumn(name: $0.element.name, qualifier: input.alias,
                    type: $0.element.type ?? TypeDescriptor(.scalar(.text)), index: $0.offset,
                    collation: plan.projections[$0.offset].collation?.asColumn, unresolved: $0.element.type == nil) }
                result = RelationPlan(body: input.lateral ? .lateralDerived(plan, settings) : .derived(plan, settings), columns: columns, lateralColumns: lateral)
            case .function(let syntax):
                let previous = resolver.columns
                resolver.columns = preceding
                defer { resolver.columns = previous }
                let function = try TableFunctionPlan.resolve(input, syntax: syntax, resolver: resolver)
                result = RelationPlan(body: .function(function), columns: function.columns, lateralColumns: function.lateralColumns)
            case .jsonTable(let syntax):
                let previous = resolver.columns; resolver.columns = preceding
                defer { resolver.columns = previous }
                let table = try JSONTablePlan.resolve(input, syntax: syntax, resolver: resolver)
                result = RelationPlan(body: .jsonTable(table), columns: table.columns, lateralColumns: table.lateralColumns)
            case .joined(let nested): result = fence(try source(nested, preceding: preceding))
            default: throw JedError("0A000", "this relation source is not yet connected")
            }
            guard input.columnAliases.count <= result.columns.count else {
                let derived: Bool
                switch input.source {
                case .table:
                    switch result.body { case .cte, .derived: derived = true; default: derived = false }
                case .query, .function: derived = true
                default: derived = false
                }
                throw JedError(derived ? "42P10" : "42601", "too many column aliases")
            }
            var columns = result.columns
            for index in columns.indices {
                let old = columns[index]
                columns[index] = ExpressionColumn(name: index < input.columnAliases.count ? input.columnAliases[index] : old.name,
                    qualifier: input.alias ?? old.qualifier, type: old.type, index: old.index, collation: old.collation,
                    qualifiedOnly: old.qualifiedOnly, unresolved: old.unresolved,
                    scalarFunction: old.scalarFunction, hiddenFromStar: old.hiddenFromStar)
            }
            var combined = RelationPlan(body: result.body, columns: columns, functionalDependencies: result.functionalDependencies, baseTable: result.baseTable,
                lateralColumns: result.lateralColumns, precedingWidth: result.precedingWidth,
                sourceDomain: result.sourceDomain, sourceSnapshot: result.sourceSnapshot)
            for join in input.joins {
                let scope = preceding + combined.columns.map { column in
                    ExpressionColumn(name: column.name, qualifier: column.qualifier, type: column.type,
                        index: preceding.count + column.index, collation: column.collation, qualifiedOnly: column.qualifiedOnly,
                        unresolved: column.unresolved, scalarFunction: column.scalarFunction, hiddenFromStar: column.hiddenFromStar)
                }
                combined = try combine(combined, source(join.right, preceding: scope), kind: join.kind, syntax: join.condition,
                    natural: join.natural, using: join.using, precedingWidth: preceding.count)
            }
            return combined
        }
        func fence(_ input: RelationPlan) -> RelationPlan {
            let physical = input.columns.sorted { $0.index < $1.index }
            let projections = physical.map { ResolvedExpression(type: $0.unresolved ? nil : $0.type, node: .column($0.index), position: 0, collation: $0.collation) }
            let plan = SelectPlan(columns: physical.map { ResultColumn(name: $0.name, type: $0.unresolved ? nil : $0.type) },
                projections: projections, predicate: nil, store: nil, touched: Array(physical.indices), parameters: [], limit: nil,
                offset: 0, order: [], orderedByScan: false, descending: false,
                scan: BaseScanPlan(predicate: nil, keys: [], types: []), relation: input)
            return RelationPlan(body: .derived(plan, settings), columns: input.columns,
                lateralColumns: input.lateralColumns, precedingWidth: input.precedingWidth)
        }
        func combine(_ left: RelationPlan, _ right: RelationPlan, kind: String, syntax: SQLExpression?,
                     natural: Bool = false, using: [SQLText] = [], precedingWidth: Int = 0) throws -> RelationPlan {
            let leftLabels = Set(left.columns.compactMap(\.qualifier)), rightLabels = Set(right.columns.compactMap(\.qualifier))
            guard leftLabels.isDisjoint(with: rightLabels) else { throw JedError("42712", "table name specified more than once") }
            if right.lateralColumns.contains(where: { $0 >= precedingWidth }) && (kind == "right" || kind == "full") {
                throw JedError("42P10", "invalid reference to FROM-clause entry in a lateral outer join")
            }
            var columns = left.columns
            for column in right.columns {
                columns.append(ExpressionColumn(name: column.name, qualifier: column.qualifier, type: column.type,
                    index: left.columns.count + column.index, collation: column.collation, qualifiedOnly: column.qualifiedOnly, unresolved: column.unresolved,
                    scalarFunction: column.scalarFunction, hiddenFromStar: column.hiddenFromStar))
            }
            let previous = resolver.columns, aggregate = resolver.resolveAggregate, window = resolver.resolveWindow
            resolver.columns = columns; resolver.resolveAggregate = AggregateQueryPlan.forbidden
            resolver.resolveWindow = WindowQueryBuilder.forbidden
            defer { resolver.columns = previous; resolver.resolveAggregate = aggregate; resolver.resolveWindow = window }
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
                    ResolvedExpression(type: column.unresolved ? nil : column.type, node: .column(column.index + offset), position: 0,
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
            guard predicate?.type == nil || predicate?.type?.type == .scalar(.boolean) else { throw JedError("42804", "JOIN ON requires a boolean expression") }
            let dependencies = left.functionalDependencies + right.functionalDependencies.map { fact in
                (primaryKey: fact.primaryKey.map { $0 + left.columns.count }, columns: fact.columns.map { $0 + left.columns.count })
            }
            return RelationPlan(body: .join(left, right, kind, predicate), columns: columns, functionalDependencies: dependencies,
                lateralColumns: left.lateralColumns.union(right.lateralColumns.filter { $0 < precedingWidth }), precedingWidth: precedingWidth)
        }
        guard let first = from.first else { throw JedError("XX000", "empty materialized FROM") }
        var result = try source(first)
        for next in from.dropFirst() {
            var input = try source(next, preceding: result.columns)
            // A comma before RIGHT/FULL fences that complete join subtree as
            // an independently projected relation, once per preceding row.
            if next.joins.contains(where: { $0.kind == "right" || $0.kind == "full" }) { input = fence(input) }
            result = try combine(result, input, kind: "cross", syntax: nil)
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
    private func materialize(_ plan: RelationPlan, touched: Set<Int>, context: ExpressionContext? = nil) throws -> [[SQLValue]] {
        let context = context ?? self.context
        let meter = context.meter
        let before = meter.accrued
        defer { context.executionProfile?.add(plan, .total, meter.accrued - before) }
        func masked(_ row: [SQLValue]) -> [SQLValue] {
            row.enumerated().map { touched.contains($0.offset) ? $0.element : .null }
        }
        switch plan.body {
        case .cte(let reference):
            guard let ctes = context.ctes else { throw JedError("XX000", "WITH reference has no runtime scope") }
            var rows: [[SQLValue]] = []
            try ctes.scan(reference.binding) { row in let value = masked(row); try reserve(value); rows.append(value) }
            return rows
        case .function(let function):
            var rows: [[SQLValue]] = []
            let admitsAsGenerated = function.function.name == "generate_series" || function.function.name == "unnest"
            try function.produce(context: context) { row in
                let value = masked(row)
                if admitsAsGenerated { try reserve(value) }
                rows.append(value)
            }
            if !admitsAsGenerated { for row in rows { try reserve(row) } }
            return rows
        case .system(let system):
            var rows: [[SQLValue]] = []
            try system.produce(context: context) { rows.append(masked($0)) }
            for row in rows { try reserve(row) }
            return rows
        case .jsonTable(let table):
            var rows: [[SQLValue]] = []
            try table.produce(context: context) { rows.append(masked($0)) }
            // Bounded JSON sources finish evaluating and generating their
            // rows before the relation admits the completed output (Q1).
            for row in rows { try reserve(row) }
            return rows
        case .base(let store):
            let scan = BaseScanPlan(predicate: plan.scanFilter, keys: plan.allowsIndexAccess ? plan.baseTable?.primaryKey ?? [] : [], types: store.tree.types)
            let access = try scan.bind(context)
            try ScanCost.charge(store, touched: touched.sorted(), meter: meter, access: access)
            var cursor = access.cursor(store); defer { cursor.close() }
            var rows: [[SQLValue]] = []
            while let record = try cursor.next() {
                meter.charge(1); try meter.step()
                var row = Array(repeating: SQLValue.null, count: plan.columns.count)
                for column in touched.sorted() { row[column] = try record.materialize(column: column, pager: store.tree.pool?.pager) }
                if let filter = plan.scanFilter {
                    var current = context; current.column = { row[$0] }
                    if try !current.matches(filter) { continue }
                }
                try reserve(row); rows.append(row)
            }
            return rows
        case .derived(let child, let settings), .lateralDerived(let child, let settings):
            var current = context; current.patterns = PatternExecution()
            if case .lateralDerived = plan.body { current.outerRows = [context.column] + context.outerRows }
            StatementServices(settings: settings, meter: meter).install(into: &current)
            let execution = SelectExecution(plan: child, context: current); defer { execution.close() }
            var rows: [[SQLValue]] = []
            while let row = try execution.next() { let value = masked(row); try reserve(value); rows.append(value) }
            return rows
        case .join(let lhs, let rhs, let kind, let predicate):
            if let island = NWayJoinPlan.resolve(plan) { return try materialize(island, touched: touched, context: context) }
            let localLateral = Set(rhs.lateralColumns.filter { $0 >= plan.precedingWidth }.map { $0 - plan.precedingWidth })
            var needed = touched.union(localLateral)
            if let predicate { StoredExpressionDependencies.walk([predicate]) { if case .column(let index) = $0.node { needed.insert(index) } } }
            let width = lhs.columns.count
            let physical = JoinPlan.choose(left: lhs, right: rhs, kind: kind, predicate: predicate)
            let leftTouched = Set(needed.filter { $0 < width }), rightTouched = Set(needed.filter { $0 >= width }.map { $0 - width })
            if !localLateral.isEmpty {
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
                        if let predicate, try !joinedContext.matches(predicate) { continue }
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
                    try ScanCost.charge(store, touched: columns, meter: meter, access: access)
                    var cursor = access.cursor(store); defer { cursor.close() }
                    var matched = false
                    while let record = try cursor.next() {
                        meter.charge(1); try meter.step()
                        var innerRow = placeholder
                        for column in columns { innerRow[column] = try record.materialize(column: column, pager: store.tree.pool?.pager) }
                        let joined = physical.swapped ? innerRow + row : row + innerRow
                        var current = context; current.column = { joined[$0] }
                        if let predicate, try !current.matches(predicate) { continue }
                        try reserve(joined); output.append(joined); matched = true
                    }
                    if !matched && kind == "left" { try reserve(logical); output.append(logical) }
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
                    if let predicate, try !current.matches(predicate) { return }
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
                    try ScanCost.charge(store, touched: columns, meter: context.meter, access: access)
                    var cursor = access.cursor(store); defer { cursor.close() }
                    var candidates: [[SQLValue]] = []
                    while let record = try cursor.next() {
                        context.meter.charge(1); try context.meter.step()
                        var row = Array(repeating: SQLValue.null, count: input.source.columns.count)
                        for column in columns { row[column] = try record.materialize(column: column, pager: store.tree.pool?.pager) }
                        if let filter = input.source.scanFilter {
                            var filtered = context; filtered.column = { row[$0] }
                            if try !filtered.matches(filter) { continue }
                        }
                        try reserve(row); candidates.append(row)
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
    func close() { rows = []; context.meter.resources.query.release(reserved); reserved = 0 }
    deinit { close() }
}

/// Shared source driver for streaming, ordering, DISTINCT and aggregation.
/// It returns raw source rows, with WHERE and output charges left to its caller.
internal final class SelectInput {
    private let plan: SelectPlan
    private let context: ExpressionContext
    private var cursor: ScanCursor?
    private var relation: RelationExecution?
    private var started = false
    private var singleton = true
    private var empty = false
    private var buffered: [[SQLValue]?]?
    private var bufferedPosition = 0
    private var bufferedBytes: Int64 = 0
    private var virtualBytes: Int64 = 0
    private(set) var predicateApplied = false
    init(plan: SelectPlan, context: ExpressionContext) { self.plan = plan; self.context = context }
    func prime() throws {
        if !started {
            let before = context.meter.accrued
            let filters = context.executionProfile?.value(plan.profileIdentity, .filter) ?? 0
            defer {
                let after = context.executionProfile?.value(plan.profileIdentity, .filter) ?? 0
                context.executionProfile?.add(plan.profileIdentity, .source, max(0, context.meter.accrued - before - (after - filters)))
            }
            started = true
            if let source = plan.relation { relation = RelationExecution(plan: source, context: context, touched: Set(plan.touched)) }
            else {
                let access = try plan.scan.bind(context); empty = access.isEmpty
                if plan.store == nil && !empty {
                    try context.meter.reserveQuery(GeneratedSpec.memoryRow); virtualBytes = GeneratedSpec.memoryRow
                }
                if let store = plan.store, !empty {
                    try ScanCost.charge(store, touched: plan.touched, meter: context.meter, access: access)
                    cursor = access.cursor(store, descending: plan.descending)
                    if access.intervals != nil || plan.materializeSource {
                        buffered = []
                        while let record = try cursor?.next() {
                            let row = try materialize(record, store: store)
                            let bytes = MemorySizes.row(row)
                            try context.meter.reserveQuery(bytes); bufferedBytes = saturatingAdd(bufferedBytes, bytes)
                            buffered!.append(row)
                        }
                        cursor?.close(); cursor = nil
                        if plan.materializeSource, let predicate = plan.predicate {
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
            guard let record = try cursor?.next() else { return nil }
            return try materialize(record, store: store)
        }
        guard singleton else { return nil }; singleton = false; return []
    }
    private func materialize(_ record: PackedRecord, store: TableStore) throws -> [SQLValue] {
        context.meter.charge(1); try context.meter.step()
        var row = Array(repeating: SQLValue.null, count: store.tree.types.count)
        for column in plan.touched { row[column] = try record.materialize(column: column, pager: store.tree.pool?.pager) }
        return row
    }
    func discardRetained(_ row: [SQLValue]) {
        if let relation { relation.discardRetained(row) }
        else if buffered != nil {
            let bytes = MemorySizes.row(row)
            context.meter.resources.query.release(bytes); bufferedBytes -= bytes
        } else if plan.store == nil {
            context.meter.resources.query.release(virtualBytes); virtualBytes = 0
        }
    }
    func close() {
        cursor?.close(); cursor = nil; relation?.close(); relation = nil; buffered = nil
        context.meter.resources.query.release(bufferedBytes); bufferedBytes = 0
        context.meter.resources.query.release(virtualBytes); virtualBytes = 0
    }
    deinit { close() }
}
