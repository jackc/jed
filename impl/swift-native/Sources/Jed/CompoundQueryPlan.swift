/// The indirection keeps query plans finite value types while retaining every
/// operand's typed plan. Set operations never substitute a decoded table store.
internal final class CompoundQueryPlan {
    enum Body { case set(String, Bool, SelectPlan, SelectPlan), values([[ResolvedExpression]], Bool), nested(SelectPlan) }
    let body: Body
    let settings: SessionSettings
    init(body: Body, settings: SessionSettings) { self.body = body; self.settings = settings }
    var expressions: [ResolvedExpression] {
        switch body {
        case .set(_, _, let left, let right): left.expressions + right.expressions
        case .values(let rows, _): rows.flatMap { $0 }
        case .nested(let child): child.expressions
        }
    }
    func context(from parent: ExpressionContext) -> ExpressionContext {
        var context = parent; context.patterns = PatternExecution()
        StatementServices(settings: settings, meter: parent.meter).install(into: &context)
        return context
    }
    static func resolve(_ query: SQLQuery, snapshot: DomainSnapshot, settings: SessionSettings,
                        scalar: ScalarCasts.Context, using shared: ExpressionResolver?, finalizeParameters: Bool) throws -> SelectPlan {
        let resolver = shared ?? ExpressionResolver(composites: snapshot.types, scalar: scalar)
        resolver.hostFunctions = settings.environment.signatures; resolver.unicode = settings.environment.unicode
        resolver.checkFunction = { try settings.privileges.require(.execute, on: $0, function: true) }
        let body: Body, columns: [ResultColumn], projections: [ResolvedExpression]
        switch query.body {
        case .set(let operation, let all, let lhs, let rhs):
            let left = try SelectPlan.resolve(lhs, snapshot: snapshot, settings: settings, scalar: scalar, using: resolver, finalizeParameters: false)
            let right = try SelectPlan.resolve(rhs, snapshot: snapshot, settings: settings, scalar: scalar, using: resolver, finalizeParameters: false)
            guard left.columns.count == right.columns.count else { throw JedError("42601", "set-operation operands have different column counts") }
            let matching = operation != "union" || !all
            projections = try left.columns.indices.map { index in
                let a = left.projections[index], b = right.projections[index]
                let type = try TypeRules.common(a.type?.type, b.type?.type)
                if matching { try TypeRules.comparable(type, type) }
                var descriptor = type.map(TypeDescriptor.init)
                if a.type == b.type { descriptor = a.type }
                let derivation = try ExpressionCollation.combine([a, b])
                if matching { _ = try derivation.orderingName() }
                descriptor?.collation = derivation.name
                return ResolvedExpression(type: descriptor, node: .column(index), position: 0, collation: derivation.asColumn)
            }
            columns = left.columns.indices.map { ResultColumn(name: left.columns[$0].name, type: projections[$0].type) }
            body = .set(operation, all, left, right)
        case .values(let syntax):
            guard let first = syntax.first, !first.isEmpty else { throw JedError("42601", "VALUES requires at least one column") }
            let oldColumns = resolver.columns, oldAggregate = resolver.resolveAggregate
            resolver.columns = []; resolver.resolveAggregate = AggregateQueryPlan.forbidden
            defer { resolver.columns = oldColumns; resolver.resolveAggregate = oldAggregate }
            var rows: [[ResolvedExpression]] = [], types = Array<SQLType?>(repeating: nil, count: first.count)
            for row in syntax {
                guard row.count == first.count else { throw JedError("42601", "VALUES lists must all be the same length") }
                let resolved = try row.map { try resolver.resolve($0) }; rows.append(resolved)
                for index in types.indices { types[index] = try TypeRules.common(types[index], resolved[index].type?.type) }
            }
            for row in rows.indices {
                for index in types.indices {
                    if case .parameter = rows[row][index].node, let type = types[index] {
                        rows[row][index] = try resolver.resolve(syntax[row][index], expected: TypeDescriptor(type))
                    }
                }
            }
            projections = try types.indices.map { index in
                let operands = rows.map { $0[index] }
                var type = types[index].map(TypeDescriptor.init)
                if operands.allSatisfy({ $0.type == operands.first?.type }) { type = operands.first?.type }
                let derivation = try ExpressionCollation.combine(operands)
                type?.collation = derivation.name
                return ResolvedExpression(type: type, node: .column(index), position: 0, collation: derivation.asColumn)
            }
            columns = types.indices.map { ResultColumn(name: SQLText(validatedUTF8: Array("column\($0 + 1)".utf8)),
                type: projections[$0].type) }
            body = .values(rows, true)
        case .nested(let query):
            let child = try SelectPlan.resolve(query, snapshot: snapshot, settings: settings, scalar: scalar, using: resolver, finalizeParameters: false)
            body = .nested(child); columns = child.columns
            projections = child.projections.enumerated().map { ResolvedExpression(type: $0.element.type, node: .column($0.offset), position: 0, collation: $0.element.collation) }
        default: throw JedError("0A000", "compound query body is not yet connected")
        }
        let order = try query.orderBy.map { term -> ResolvedOrderTerm in
            let index: Int
            switch term.expression.kind {
            case .number(let spelling):
                guard let ordinal = Int(spelling), ordinal > 0, ordinal <= columns.count else { throw JedError("42P10", "ORDER BY position is not in select list") }
                index = ordinal - 1
            case .column(nil, let name):
                let matches = columns.indices.filter { columns[$0].name == name }
                guard !matches.isEmpty else { throw JedError("42703", "column does not exist: \(name)") }
                guard matches.count == 1 else { throw JedError("42702", "ORDER BY reference is ambiguous: \(name)") }
                index = matches[0]
            case .column: throw JedError("42P01", "ORDER BY has no relation scope after a set operation")
            default: throw JedError("0A000", "invalid set-operation ORDER BY expression")
            }
            var type = columns[index].type ?? TypeDescriptor(.scalar(.text))
            type.collation = try ExpressionCollation.derive(projections[index]).orderingName()
            try TypeRules.comparable(type.type, type.type)
            return ResolvedOrderTerm(expression: projections[index], type: type, descending: term.descending, nullsFirst: term.nullsFirst ?? term.descending)
        }
        func bound(_ spelling: String?, code: String) throws -> Int64? {
            guard let spelling else { return nil }
            guard let value = Int64(spelling), value >= 0 else { throw JedError(code, "invalid row-count bound") }
            return value
        }
        return SelectPlan(columns: columns, projections: projections, predicate: nil, store: nil, touched: [],
            parameters: try finalizeParameters ? ParameterBinding.finalize(resolver) : [],
            limit: try bound(query.limit, code: "2201W"), offset: try bound(query.offset, code: "2201X") ?? 0,
            order: order, orderedByScan: false, descending: false, scan: BaseScanPlan(predicate: nil, keys: [], types: []),
            compound: CompoundQueryPlan(body: body, settings: settings))
    }
}
