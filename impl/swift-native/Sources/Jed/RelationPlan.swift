/// Materialized FROM inputs retain packed base stores or independent query
/// plans. All observable traversal follows authored relation and row order.
internal final class RelationPlan {
    enum Body { case base(TableStore), derived(SelectPlan, SessionSettings), join(RelationPlan, RelationPlan, String, ResolvedExpression?) }
    let body: Body
    let columns: [ExpressionColumn]
    init(body: Body, columns: [ExpressionColumn]) { self.body = body; self.columns = columns }
    var expressions: [ResolvedExpression] {
        switch body {
        case .base: []
        case .derived(let plan, _): plan.expressions
        case .join(let left, let right, _, let predicate): left.expressions + right.expressions + (predicate.map { [$0] } ?? [])
        }
    }
    static func resolve(_ from: [SQLFrom], snapshot: DomainSnapshot, settings: SessionSettings,
                        resolver: ExpressionResolver) throws -> RelationPlan {
        func source(_ input: SQLFrom) throws -> RelationPlan {
            guard !input.lateral else { throw JedError("0A000", "lateral relation execution is not yet connected") }
            let result: RelationPlan
            switch input.source {
            case .table(let name):
                if (name.database == nil || name.database?.description == "main"),
                   let view = snapshot.catalog.views.first(where: { $0.name == name.name }) {
                    try settings.privileges.require(.select, on: view.name)
                    var inner = settings
                    if !view.securityInvoker { inner.privileges = settings.privileges.withTableAccess() }
                    let parsed = try ViewDefinition.parsed(view)
                    let isolated = ExpressionResolver(composites: snapshot.types, scalar: resolver.scalar)
                    isolated.viewExpansion = resolver.viewExpansion
                    let plan = try resolver.viewExpansion.withBody(view.name, referenceDepth: input.parserDepth,
                        offset: input.span.byteStart, bodyDepth: parsed.depth) {
                        try SelectPlan.resolve(parsed.query, snapshot: snapshot, settings: inner, scalar: resolver.scalar,
                                               using: isolated, finalizeParameters: false)
                    }
                    guard plan.columns.count == view.columns.count else { throw JedError("XX003", "stored view output changed") }
                    let columns = plan.columns.enumerated().map { ExpressionColumn(name: view.columns[$0.offset].name,
                        qualifier: input.alias ?? view.name, type: $0.element.type ?? TypeDescriptor(.scalar(.text)), index: $0.offset,
                        collation: plan.projections[$0.offset].collation?.asColumn) }
                    result = RelationPlan(body: .derived(plan, inner), columns: columns)
                    break
                }
                guard name.database == nil || name.database?.description == "main",
                      let table = snapshot.catalog.tables.first(where: { $0.name == name.name }), let store = snapshot.tables[name.name] else {
                    throw JedError("42P01", "relation does not exist: \(name.name)")
                }
                try settings.privileges.require(.select, on: table.name)
                let columns = try table.columns.enumerated().map { ExpressionColumn(name: $0.element.name,
                    qualifier: input.alias ?? table.name, type: try $0.element.type.resolve(snapshot.types), index: $0.offset) }
                result = RelationPlan(body: .base(store), columns: columns)
            case .query(let query):
                var plan = try SelectPlan.resolve(query, snapshot: snapshot, settings: settings, scalar: resolver.scalar,
                                                  using: resolver, finalizeParameters: false)
                if let compound = plan.compound, case .values(let rows, _) = compound.body {
                    plan.compound = CompoundQueryPlan(body: .values(rows, false), settings: settings)
                }
                let columns = plan.columns.enumerated().map { ExpressionColumn(name: $0.element.name, qualifier: input.alias,
                    type: $0.element.type ?? TypeDescriptor(.scalar(.text)), index: $0.offset,
                    collation: plan.projections[$0.offset].collation?.asColumn, unresolved: $0.element.type == nil) }
                result = RelationPlan(body: .derived(plan, settings), columns: columns)
            case .joined(let nested): result = try source(nested)
            default: throw JedError("0A000", "this relation source is not yet connected")
            }
            guard input.columnAliases.count <= result.columns.count else {
                let view = if case .table(let name) = input.source { snapshot.catalog.views.contains { $0.name == name.name } } else { false }
                throw JedError(view ? "42P10" : "42601", "too many column aliases")
            }
            var columns = result.columns
            for index in columns.indices {
                let old = columns[index]
                columns[index] = ExpressionColumn(name: index < input.columnAliases.count ? input.columnAliases[index] : old.name,
                    qualifier: input.alias ?? old.qualifier, type: old.type, index: index, collation: old.collation, unresolved: old.unresolved)
            }
            var combined = RelationPlan(body: result.body, columns: columns)
            for join in input.joins { combined = try combine(combined, source(join.right), kind: join.kind, syntax: join.condition,
                                                           natural: join.natural, using: join.using) }
            return combined
        }
        func combine(_ left: RelationPlan, _ right: RelationPlan, kind: String, syntax: SQLExpression?,
                     natural: Bool = false, using: [SQLText] = []) throws -> RelationPlan {
            guard !natural, using.isEmpty else { throw JedError("0A000", "JOIN USING and NATURAL column merging is not yet connected") }
            var columns = left.columns
            for column in right.columns {
                columns.append(ExpressionColumn(name: column.name, qualifier: column.qualifier, type: column.type,
                    index: left.columns.count + column.index, collation: column.collation, unresolved: column.unresolved))
            }
            let previous = resolver.columns, aggregate = resolver.resolveAggregate
            resolver.columns = columns; resolver.resolveAggregate = AggregateQueryPlan.forbidden
            defer { resolver.columns = previous; resolver.resolveAggregate = aggregate }
            let predicate = try syntax.map { try resolver.resolve($0, expected: TypeDescriptor(.scalar(.boolean))) }
            guard predicate?.type == nil || predicate?.type?.type == .scalar(.boolean) else { throw JedError("42804", "JOIN ON requires a boolean expression") }
            return RelationPlan(body: .join(left, right, kind, predicate), columns: columns)
        }
        guard let first = from.first else { throw JedError("XX000", "empty materialized FROM") }
        var result = try source(first)
        for next in from.dropFirst() { result = try combine(result, source(next), kind: "cross", syntax: nil) }
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
    func next() throws -> [SQLValue]? {
        if !started { rows = try materialize(plan, touched: touched).map(Optional.some); started = true }
        guard position < rows.count, let row = rows[position] else { return nil }
        rows[position] = nil; position += 1; release(MemorySizes.row(row)); return row
    }
    private func materialize(_ plan: RelationPlan, touched: Set<Int>) throws -> [[SQLValue]] {
        let meter = context.meter
        switch plan.body {
        case .base(let store):
            try ScanCost.charge(store, touched: touched.sorted(), meter: meter)
            var cursor = store.cursor(); defer { cursor.close() }
            var rows: [[SQLValue]] = []
            while let record = try cursor.next() {
                meter.charge(1); try meter.step()
                var row = Array(repeating: SQLValue.null, count: plan.columns.count)
                for column in touched.sorted() { row[column] = try record.materialize(column: column, pager: store.tree.pool?.pager) }
                try reserve(row); rows.append(row)
            }
            return rows
        case .derived(let plan, let settings):
            var current = context; current.patterns = PatternExecution()
            StatementServices(settings: settings, meter: meter).install(into: &current)
            let execution = SelectExecution(plan: plan, context: current); defer { execution.close() }
            var rows: [[SQLValue]] = []
            while let row = try execution.next() { try reserve(row); rows.append(row) }
            return rows
        case .join(let lhs, let rhs, let kind, let predicate):
            var needed = touched
            if let predicate { StoredExpressionDependencies.walk([predicate]) { if case .column(let index) = $0.node { needed.insert(index) } } }
            let width = lhs.columns.count
            let left = try materialize(lhs, touched: Set(needed.filter { $0 < width }))
            let right = try materialize(rhs, touched: Set(needed.filter { $0 >= width }.map { $0 - width }))
            var output: [[SQLValue]] = [], matchedRight: Set<Int> = []
            for a in left {
                var matched = false
                for (index, b) in right.enumerated() {
                    try meter.pollStep()
                    let row = a + b
                    var current = context; current.column = { row[$0] }
                    if let predicate, try !current.matches(predicate) { continue }
                    try reserve(row); output.append(row); matched = true; matchedRight.insert(index)
                }
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
    func close() { rows = []; context.meter.resources.query.release(reserved); reserved = 0 }
    deinit { close() }
}

/// Shared source driver for streaming, ordering, DISTINCT and aggregation.
/// It returns raw source rows, with WHERE and output charges left to its caller.
internal final class SelectInput {
    private let plan: SelectPlan
    private let context: ExpressionContext
    private var cursor: TreeCursor?
    private var relation: RelationExecution?
    private var started = false
    private var singleton = true
    private var empty = false
    init(plan: SelectPlan, context: ExpressionContext) { self.plan = plan; self.context = context }
    func prime() throws {
        if !started {
            started = true
            if let source = plan.relation { relation = RelationExecution(plan: source, context: context, touched: Set(plan.touched)) }
            else {
                let access = try plan.scan.bind(context); empty = access.isEmpty
                if let store = plan.store, !empty {
                    try ScanCost.charge(store, touched: plan.touched, meter: context.meter, access: access)
                    cursor = access.cursor(store, descending: plan.descending)
                }
            }
        }
    }
    func next() throws -> [SQLValue]? {
        try prime()
        if let relation { return try relation.next() }
        if empty { return nil }
        if let store = plan.store {
            guard let record = try cursor?.next() else { return nil }
            context.meter.charge(1); try context.meter.step()
            var row = Array(repeating: SQLValue.null, count: store.tree.types.count)
            for column in plan.touched { row[column] = try record.materialize(column: column, pager: store.tree.pool?.pager) }
            return row
        }
        guard singleton else { return nil }; singleton = false; return []
    }
    func close() { cursor?.close(); cursor = nil; relation?.close(); relation = nil }
    deinit { close() }
}
