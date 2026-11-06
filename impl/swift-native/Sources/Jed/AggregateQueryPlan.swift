/// Aggregate calls become ordinary synthetic-column reads. Input expressions
/// keep the source scope; grouping validation applies only to output trees.
internal struct ResolvedAggregateCall {
    let plan: AggregatePlan
    let arguments: [ResolvedExpression]
    let filter: ResolvedExpression?
    let distinct: Bool
    let order: [ResolvedOrderTerm]
}

internal struct AggregateQueryPlan {
    let keys: [ResolvedExpression]
    let keyColumns: [Int]
    let sets: [[Int]]
    let grouping: [[Int]]
    let calls: [ResolvedAggregateCall]
    let having: ResolvedExpression?
    let projections: [ResolvedExpression]
    let columns: [ResultColumn]
    let order: [ResolvedOrderTerm]
    let sourceWidth: Int
    let layoutWidth: Int

    var expressions: [ResolvedExpression] {
        keys + calls.flatMap { $0.arguments + ($0.filter.map { [$0] } ?? []) + $0.order.map(\.expression) }
            + (having.map { [$0] } ?? []) + projections + order.map(\.expression)
    }
    static let names: Set<String> = Set(GeneratedSpec.functions.filter { $0.kind == "aggregate" }.map(\.name))
        .union(["json_object_agg", "jsonb_object_agg", "json_object_agg_unique", "jsonb_object_agg_unique",
                "mode", "percentile_disc", "percentile_cont", "grouping"])
    static func isAggregate(_ call: SQLCall) -> Bool {
        names.contains(call.name.description) || (!call.withinGroup.isEmpty && ["rank", "dense_rank", "percent_rank", "cume_dist"].contains(call.name.description))
    }
    static func containsAggregate(_ expression: SQLExpression) -> Bool {
        var work = [expression]
        while let node = work.popLast() {
            if case .call(let call) = node.kind, isAggregate(call), call.over == nil, call.overName == nil { return true }
            work.append(contentsOf: CatalogExpressionPolicy.children(node))
        }
        return false
    }
    static func forbidden(_ call: SQLCall, _ position: Int) throws -> ResolvedExpression? {
        if isAggregate(call), call.over != nil || call.overName != nil {
            throw JedError("0A000", "window aggregate plan is not yet connected").at(position)
        }
        if isAggregate(call) { throw JedError("42803", "aggregate functions are not allowed in this expression").at(position) }
        return nil
    }
    static func resolve(_ select: SQLSelect, order: [SQLOrderTerm], resolver: ExpressionResolver,
                        scope: [ExpressionColumn], primaryKey: [Int]) throws -> Self? {
        let syntax = select.items.compactMap(\.expression) + order.map(\.expression)
        guard !select.grouping.isEmpty || select.having != nil || syntax.contains(where: containsAggregate) else { return nil }
        let previous = resolver.resolveAggregate
        let previousRewrite = resolver.rewriteResolved
        resolver.resolveAggregate = forbidden
        resolver.rewriteResolved = nil
        defer { resolver.resolveAggregate = previous; resolver.rewriteResolved = previousRewrite }
        var keys: [ResolvedExpression] = [], keyColumns: [Int] = []
        var sets: [[Int]] = [], grouping: [[Int]] = []
        var materializedKeys = 0
        // Expand the target list once for SQL92 ordinal and alias lookup.
        var targets: [(SQLText, SQLExpression)] = []
        for item in select.items {
            if item.star {
                for column in scope where item.qualifier == nil || item.qualifier == column.qualifier {
                    targets.append((column.name, SQLExpression(kind: .column(qualifier: column.qualifier, name: column.name), span: item.span, position: item.span.position)))
                }
            } else if let expression = item.expression { targets.append((item.alias ?? SelectPlan.outputName(expression), expression)) }
        }
        for expressions in try GroupingExpansion.expand(select.grouping) {
            var members: [Int] = []
            for original in expressions {
                var syntax = original
                if case .number(let spelling) = syntax.kind, let ordinal = Int(spelling) {
                    guard ordinal > 0 && ordinal <= targets.count else { throw JedError("42P10", "GROUP BY position is not in select list") }
                    syntax = targets[ordinal - 1].1
                } else if case .column(nil, let name) = syntax.kind, !scope.contains(where: { $0.name == name }) {
                    let matches = targets.filter { $0.0 == name }
                    if let first = matches.first {
                        let resolved = try resolver.resolve(first.1)
                        for match in matches.dropFirst() {
                            guard try ExpressionIdentity.same(resolved, resolver.resolve(match.1)) else { throw JedError("42702", "GROUP BY reference is ambiguous") }
                        }
                        syntax = first.1
                    }
                }
                let key = try resolver.resolve(syntax)
                try equalityType(key)
                if let index = keys.firstIndex(where: { ExpressionIdentity.same($0, key) }) {
                    if !members.contains(index) { members.append(index) }
                    continue
                }
                members.append(keys.count)
                if case .column(let index) = key.node { keyColumns.append(index) }
                else { keyColumns.append(scope.count + materializedKeys); materializedKeys += 1 }
                keys.append(key)
            }
            sets.append(members)
        }
        // A complete primary key makes the other source columns groupable.
        let plain = select.grouping.allSatisfy { if case .set = $0 { return true }; return false }
        if plain, !primaryKey.isEmpty, primaryKey.allSatisfy(keyColumns.contains) {
            for column in scope where !keyColumns.contains(column.index) {
                let expression = ResolvedExpression(type: column.type, node: .column(column.index), position: 0,
                    collation: column.collation ?? (ExpressionCollation.collatable(column.type.type) ? .implicit(column.type.collation ?? SQLText(validatedUTF8: [67])) : nil))
                sets[0].append(keys.count)
                keyColumns.append(column.index); keys.append(expression); resolver.referencedColumns.insert(column.index)
            }
        }
        let allowed = Set(keyColumns), width = scope.count + materializedKeys
        var calls: [ResolvedAggregateCall] = []
        var resolvedCalls: [Int: ResolvedExpression] = [:]
        resolver.resolveAggregate = { call, position in
            guard isAggregate(call) else { return nil }
            // Literal adoption may resolve the same syntax node twice. It is
            // still one aggregate occurrence; separately authored occurrences
            // retain distinct slots and their own accumulation charges.
            if let existing = resolvedCalls[position] { return existing }
            if call.name.description == "grouping" {
                guard !call.star, !call.arguments.isEmpty,
                      call.over == nil, call.overName == nil, !call.distinct,
                      call.orderBy.isEmpty, call.withinGroup.isEmpty, call.filter == nil else {
                    throw JedError("42601", "invalid GROUPING call")
                }
                let collector = resolver.resolveAggregate, rewrite = resolver.rewriteResolved
                resolver.resolveAggregate = forbidden; resolver.rewriteResolved = nil
                defer { resolver.resolveAggregate = collector; resolver.rewriteResolved = rewrite }
                let positions = try call.arguments.map { argument -> Int in
                    let expression = try resolver.resolve(argument.value)
                    guard case .column = expression.node,
                          let index = keys.firstIndex(where: { ExpressionIdentity.same($0, expression) }) else {
                        throw JedError("42803", "GROUPING argument must be a grouping column")
                    }
                    return index
                }
                let slot = -1 - grouping.count
                grouping.append(positions)
                let result = ResolvedExpression(type: TypeDescriptor(.scalar(.i32)), node: .column(slot), position: position)
                resolvedCalls[position] = result; return result
            }
            guard call.over == nil, call.overName == nil else { throw JedError("0A000", "window aggregate plan is not yet connected") }
            guard call.arguments.allSatisfy({ $0.name == nil && !$0.variadic }) else { throw JedError("42883", "aggregate named or variadic arguments are not supported") }
            guard call.withinGroup.isEmpty else { throw JedError("0A000", "ordered-set aggregate plan is not yet connected") }
            if call.star && call.name.description != "count" { throw JedError("42601", "* is only valid in COUNT") }
            let collector = resolver.resolveAggregate
            let rewrite = resolver.rewriteResolved
            resolver.resolveAggregate = forbidden
            resolver.rewriteResolved = nil
            defer { resolver.resolveAggregate = collector; resolver.rewriteResolved = rewrite }
            var arguments = try call.arguments.map { try resolver.resolve($0.value) }
            // The two string_agg slots adopt unknown string/NULL operands only
            // after an exact family match has failed (aggregates.md §23).
            if call.name.description == "string_agg", arguments.count == 2 {
                func unknown(_ syntax: SQLExpression) -> Bool {
                    switch syntax.kind { case .null, .string, .parameter: true; case .parenthesized(let child): unknown(child); default: false }
                }
                let concrete = arguments.indices.filter { !unknown(call.arguments[$0].value) }.compactMap { arguments[$0].type?.type }
                let family: SQLType = concrete.contains(.scalar(.bytea)) ? .scalar(.bytea) : .scalar(.text)
                if !arguments.allSatisfy({ $0.type?.type == family }) {
                    for index in arguments.indices where unknown(call.arguments[index].value) {
                        arguments[index] = try resolver.resolve(call.arguments[index].value, expected: TypeDescriptor(family))
                    }
                }
            }
            if call.name.description == "array_agg", arguments.first?.type == nil { throw JedError("42725", "aggregate input type is ambiguous") }
            var types = arguments.map { $0.type ?? TypeDescriptor(.scalar(.text)) }
            if call.distinct { for argument in arguments { try equalityType(argument) } }
            if ["min", "max"].contains(call.name.description), !arguments.isEmpty {
                types[0].collation = try ExpressionCollation.derive(arguments[0]).orderingName()
            }
            let plan = try AggregatePlan.resolve(name: call.name.description, inputTypes: types, star: call.star)
            let order = try call.orderBy.map { term -> ResolvedOrderTerm in
                let expression = try resolver.resolve(term.expression)
                if call.distinct, !arguments.contains(where: { ExpressionIdentity.same($0, expression) }) {
                    throw JedError("42P10", "in an aggregate with DISTINCT, ORDER BY expressions must appear in argument list")
                }
                var type = expression.type ?? TypeDescriptor(.scalar(.text))
                type.collation = try ExpressionCollation.derive(expression).orderingName()
                try TypeRules.comparable(type.type, type.type)
                return ResolvedOrderTerm(expression: expression, type: type, descending: term.descending, nullsFirst: term.nullsFirst ?? term.descending)
            }
            let filter = try call.filter.map { try resolver.resolve($0, expected: TypeDescriptor(.scalar(.boolean))) }
            if let type = filter?.type, type.type != .scalar(.boolean) { throw JedError("42804", "FILTER requires a boolean expression") }
            try resolver.checkFunction?(call.name)
            let slot = width + calls.count
            calls.append(ResolvedAggregateCall(plan: plan, arguments: arguments, filter: filter, distinct: call.distinct, order: order))
            let derivation = ExpressionCollation.collatable(plan.resultType.type) ? try ExpressionCollation.combine(arguments) : CollationDerivation.none
            let result = ResolvedExpression(type: plan.resultType, node: .column(slot), position: position, collation: derivation)
            resolvedCalls[position] = result
            return result
        }
        resolver.rewriteResolved = { expression in
            if let index = keys.firstIndex(where: { ExpressionIdentity.same($0, expression) }) {
                var result = expression; result.node = .column(keyColumns[index]); return result
            }
            return expression
        }
        func validate(_ expression: ResolvedExpression) throws {
            try StoredExpressionDependencies.walk([expression]) { node in
                if case .column(let index) = node.node, index >= 0, index < width, !allowed.contains(index) {
                    throw JedError("42803", "column must appear in GROUP BY or be used in an aggregate").at(node.position)
                }
            }
        }
        var projections: [ResolvedExpression] = [], columns: [ResultColumn] = []
        for (name, syntax) in targets {
            let expression = try resolver.resolve(syntax)
            try validate(expression)
            projections.append(expression); columns.append(ResultColumn(name: name, type: expression.type))
        }
        guard !projections.isEmpty else { throw JedError("42601", "SELECT requires a target list") }
        let having = try select.having.map { try resolver.resolve($0, expected: TypeDescriptor(.scalar(.boolean))) }
        if let having {
            guard having.type == nil || having.type?.type == .scalar(.boolean) else { throw JedError("42804", "HAVING requires a boolean expression") }
            try validate(having)
        }
        let resolvedOrder = try order.map { term -> ResolvedOrderTerm in
            let expression: ResolvedExpression
            if case .number(let spelling) = term.expression.kind, let ordinal = Int(spelling) {
                guard ordinal > 0 && ordinal <= projections.count else { throw JedError("42P10", "ORDER BY position is not in select list") }
                expression = projections[ordinal - 1]
            } else if case .column(nil, let name) = term.expression.kind, columns.contains(where: { $0.name == name }) {
                let matches = columns.indices.filter { columns[$0].name == name }
                guard matches.dropFirst().allSatisfy({ ExpressionIdentity.same(projections[matches[0]], projections[$0]) }) else {
                    throw JedError("42702", "ORDER BY reference is ambiguous")
                }
                expression = projections[matches[0]]
            } else { expression = try resolver.resolve(term.expression) }
            try validate(expression)
            var type = expression.type ?? TypeDescriptor(.scalar(.text))
            type.collation = try ExpressionCollation.derive(expression).orderingName()
            try TypeRules.comparable(type.type, type.type)
            return ResolvedOrderTerm(expression: expression, type: type, descending: term.descending, nullsFirst: term.nullsFirst ?? term.descending)
        }
        return Self(keys: keys, keyColumns: keyColumns, sets: sets, grouping: grouping, calls: calls, having: having, projections: projections,
                    columns: columns, order: resolvedOrder, sourceWidth: scope.count, layoutWidth: width)
    }
    static func equalityType(_ expression: ResolvedExpression) throws {
        guard let type = expression.type else { return }
        try TypeRules.comparable(type.type, type.type)
        _ = try ExpressionCollation.derive(expression).orderingName()
    }
}
