/// Aggregate calls become ordinary synthetic-column reads. Input expressions
/// keep the source scope; grouping validation applies only to output trees.
internal struct ResolvedAggregateCall {
    /// Stored fields live in one copy-on-write box, so passing this value
    /// around costs one retain instead of a field-by-field copy.
    private final class Storage {
        let plan: AggregatePlan
        let arguments: [ResolvedExpression]
        let direct: [ResolvedExpression]
        let filter: ResolvedExpression?
        let distinct: Bool
        let order: [ResolvedOrderTerm]
        init(plan: AggregatePlan, arguments: [ResolvedExpression], direct: [ResolvedExpression], filter: ResolvedExpression?, distinct: Bool, order: [ResolvedOrderTerm]) {
            self.plan = plan
            self.arguments = arguments
            self.direct = direct
            self.filter = filter
            self.distinct = distinct
            self.order = order
        }
    }
    private let storage: Storage
    init(plan: AggregatePlan, arguments: [ResolvedExpression], direct: [ResolvedExpression], filter: ResolvedExpression?, distinct: Bool, order: [ResolvedOrderTerm]) {
        storage = Storage(plan: plan, arguments: arguments, direct: direct, filter: filter, distinct: distinct, order: order)
    }
    var plan: AggregatePlan { storage.plan }
    var arguments: [ResolvedExpression] { storage.arguments }
    var direct: [ResolvedExpression] { storage.direct }
    var filter: ResolvedExpression? { storage.filter }
    var distinct: Bool { storage.distinct }
    var order: [ResolvedOrderTerm] { storage.order }
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
        keys + calls.flatMap { $0.arguments + $0.direct + ($0.filter.map { [$0] } ?? []) + $0.order.map(\.expression) }
            + (having.map { [$0] } ?? []) + projections + order.map(\.expression)
    }
    var foldExpressions: [ResolvedExpression] {
        keys + calls.flatMap { $0.arguments + $0.direct + ($0.filter.map { [$0] } ?? []) + $0.order.map(\.expression) }
            + (having.map { [$0] } ?? []) + projections
    }
    /// Repeated aggregate spellings keep their fold occurrences but designate
    /// the same DISTINCT ON sort key. This does not perform aggregate CSE.
    func sameKey(_ a: ResolvedExpression, _ b: ResolvedExpression,
                 otherColumns: @escaping (Int, Int) -> Bool = { _, _ in false }) -> Bool {
        Self.sameKey(a, b, calls: calls, grouping: grouping, width: layoutWidth, otherColumns: otherColumns)
    }
    private static func sameKey(_ a: ResolvedExpression, _ b: ResolvedExpression,
                                calls: [ResolvedAggregateCall], grouping: [[Int]], width: Int,
                                otherColumns: @escaping (Int, Int) -> Bool) -> Bool {
        ExpressionIdentity.same(a, b, columns: { a, b in
            if a == b || otherColumns(a, b) { return true }
            if a < 0, b < 0 {
                let x = -1 - a, y = -1 - b
                return grouping.indices.contains(x) && grouping.indices.contains(y) && grouping[x] == grouping[y]
            }
            let x = a - width, y = b - width
            guard calls.indices.contains(x), calls.indices.contains(y) else { return false }
            let left = calls[x], right = calls[y]
            func list(_ a: [ResolvedExpression], _ b: [ResolvedExpression]) -> Bool {
                a.count == b.count && zip(a, b).allSatisfy(ExpressionIdentity.same)
            }
            func order(_ a: [ResolvedOrderTerm], _ b: [ResolvedOrderTerm]) -> Bool {
                a.count == b.count && zip(a, b).allSatisfy {
                    $0.type == $1.type && $0.descending == $1.descending && $0.nullsFirst == $1.nullsFirst && ExpressionIdentity.same($0.expression, $1.expression)
                }
            }
            guard left.plan.name == right.plan.name, left.plan.star == right.plan.star, left.distinct == right.distinct,
                  list(left.arguments, right.arguments), list(left.direct, right.direct), order(left.order, right.order),
                  left.plan.withinGroup.count == right.plan.withinGroup.count,
                  zip(left.plan.withinGroup, right.plan.withinGroup).allSatisfy({ $0.type == $1.type && $0.descending == $1.descending && $0.nullsFirst == $1.nullsFirst }) else { return false }
            if let a = left.filter, let b = right.filter { return ExpressionIdentity.same(a, b) }
            return left.filter == nil && right.filter == nil
        })
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
            if case .call(let call) = node.kind, let window = call.over {
                work.append(contentsOf: window.partitionBy + window.orderBy.map(\.expression))
            }
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
                        scope: [ExpressionColumn], primaryKey: [Int],
                        functionalDependencies: [(primaryKey: [Int], columns: [Int])] = [],
                        windows: WindowQueryBuilder? = nil, planningProbe: PlanningProbe? = nil) throws -> Self? {
        let syntax = select.items.compactMap(\.expression) + order.map(\.expression) + select.windows.flatMap { $0.definition.partitionBy + $0.definition.orderBy.map(\.expression) }
        guard !select.grouping.isEmpty || select.having != nil || syntax.contains(where: containsAggregate) else { return nil }
        let previous = resolver.resolveAggregate
        let previousRewrite = resolver.rewriteResolved
        let previousWindow = resolver.resolveWindow
        let previousGrouped = resolver.groupedColumns
        resolver.resolveAggregate = forbidden
        resolver.resolveWindow = WindowQueryBuilder.forbidden
        resolver.rewriteResolved = nil
        resolver.groupedColumns = nil
        defer { resolver.resolveAggregate = previous; resolver.rewriteResolved = previousRewrite; resolver.resolveWindow = previousWindow; resolver.groupedColumns = previousGrouped; windows?.validateGrouped = nil }
        var keys: [ResolvedExpression] = [], keyColumns: [Int] = []
        var sets: [[Int]] = [], grouping: [[Int]] = []
        var materializedKeys = 0
        // Expand the target list once for SQL92 ordinal and alias lookup.
        var targets: [(SQLText, SQLExpression)] = []
        for item in select.items {
            if item.star {
                var selected = scope.filter { !$0.hiddenFromStar && (item.qualifier == nil ? !$0.qualifiedOnly : item.qualifier == $0.qualifier) }
                if item.qualifier != nil { selected.sort { $0.index < $1.index } }
                for column in selected {
                    targets.append((column.name, SQLExpression(kind: .column(qualifier: column.qualifier, name: column.name), span: item.span, position: item.span.position)))
                }
            } else if let expression = item.expression {
                if case .field(let source, nil) = expression.kind {
                    let value = try resolver.resolve(source)
                    guard let type = value.type?.type, let fields = TypeRules.fields(type) else { throw JedError("42809", "record wildcard requires a composite type") }
                    for field in fields { targets.append((field.name, SQLExpression(kind: .field(source, field.name), span: expression.span, position: expression.position))) }
                } else { targets.append((item.alias ?? SelectPlan.outputName(expression), expression)) }
            }
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
                try equalityType(key, at: original.expressionLocation)
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
        let dependencies = primaryKey.isEmpty ? functionalDependencies : [(primaryKey: primaryKey, columns: scope.map(\.index))]
        for dependency in dependencies where plain && !dependency.primaryKey.isEmpty && dependency.primaryKey.allSatisfy(keyColumns.contains) {
            for column in scope where dependency.columns.contains(column.index) && !keyColumns.contains(column.index) {
                let expression = ResolvedExpression(type: column.type, node: .column(column.index), position: 0,
                    collation: column.collation ?? (ExpressionCollation.collatable(column.type.type) ? .implicit(column.type.collation ?? SQLText(validatedUTF8: [67])) : nil))
                sets[0].append(keys.count)
                keyColumns.append(column.index); keys.append(expression); resolver.referencedColumns.insert(column.index)
            }
        }
        let allowed = Set(keyColumns), width = scope.count + materializedKeys
        resolver.groupedColumns = allowed
        resolver.resolveWindow = previousWindow
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
                let collector = resolver.resolveAggregate, rewrite = resolver.rewriteResolved, windowCollector = resolver.resolveWindow
                let grouped = resolver.groupedColumns
                resolver.resolveAggregate = forbidden; resolver.rewriteResolved = nil
                resolver.resolveWindow = WindowQueryBuilder.aggregateForbidden
                resolver.groupedColumns = nil
                defer { resolver.resolveAggregate = collector; resolver.rewriteResolved = rewrite; resolver.resolveWindow = windowCollector; resolver.groupedColumns = grouped }
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
            if !call.withinGroup.isEmpty {
                let name = call.name.description, hypothetical = AggregateOrderedSet.hypothetical.contains(call.name.description)
                guard hypothetical || AggregateOrderedSet.ordered.contains(name) else { throw JedError("42883", "aggregate does not exist: \(name)") }
                guard !call.distinct else { throw JedError("42601", "DISTINCT is not allowed with ordered-set aggregates") }
                guard !call.star, call.orderBy.isEmpty,
                      hypothetical ? !call.arguments.isEmpty && call.arguments.count == call.withinGroup.count : call.withinGroup.count == 1 else {
                    throw JedError("42883", "aggregate does not exist: \(name)")
                }
                let collector = resolver.resolveAggregate, rewrite = resolver.rewriteResolved, windowCollector = resolver.resolveWindow
                let grouped = resolver.groupedColumns
                resolver.resolveAggregate = forbidden; resolver.rewriteResolved = nil
                resolver.resolveWindow = WindowQueryBuilder.aggregateForbidden
                resolver.groupedColumns = nil
                defer { resolver.resolveAggregate = collector; resolver.rewriteResolved = rewrite; resolver.resolveWindow = windowCollector; resolver.groupedColumns = grouped }
                var arguments: [ResolvedExpression] = [], direct: [ResolvedExpression] = [], ordering: [AggregateOrder] = []
                for (index, term) in call.withinGroup.enumerated() {
                    resolver.rewriteResolved = nil
                    resolver.groupedColumns = nil
                    let expression = try resolver.resolve(term.expression)
                    var type = expression.type ?? TypeDescriptor(.scalar(.text))
                    type.collation = try ExpressionCollation.derive(expression).orderingName()
                    try ResolutionDiagnostics.comparable(type.type, at: term.expression.expressionLocation, ordering: true)
                    arguments.append(expression)
                    ordering.append(AggregateOrder(type: type, descending: term.descending, nullsFirst: term.nullsFirst))
                    if hypothetical {
                        resolver.rewriteResolved = rewrite
                        resolver.groupedColumns = grouped
                        let value = try resolver.resolve(call.arguments[index].value, expected: type)
                        try validate(value)
                        guard value.type == nil || AggregatePlan.hypotheticalCompatible(value.type!.type, type.type) else {
                            throw JedError("42883", "aggregate does not exist: \(name)")
                        }
                        direct.append(value)
                    }
                }
                if !hypothetical {
                    guard call.arguments.count == (name == "mode" ? 0 : 1) else { throw JedError("42883", "aggregate does not exist: \(name)") }
                    resolver.rewriteResolved = rewrite
                    resolver.groupedColumns = grouped
                    for argument in call.arguments {
                        let value = try resolver.resolve(argument.value, expected: TypeDescriptor(.scalar(.f64)))
                        try validate(value); direct.append(value)
                    }
                }
                resolver.rewriteResolved = nil
                resolver.groupedColumns = nil
                let filter = try call.filter.map { try resolver.resolve($0, expected: TypeDescriptor(.scalar(.boolean))) }
                if let type = filter?.type, type.type != .scalar(.boolean) { throw JedError("42804", "FILTER requires a boolean expression").at(filter?.location ?? 0) }
                let types = arguments.enumerated().map { $0.element.type ?? ordering[$0.offset].type }
                let directTypes = direct.enumerated().map { $0.element.type ?? (hypothetical ? types[$0.offset] : TypeDescriptor(.scalar(.f64))) }
                let plan = try AggregatePlan.resolve(name: name, inputTypes: types, withinGroup: ordering, directTypes: directTypes)
                try AggregateScope.rejectOuterOnly(arguments + (filter.map { [$0] } ?? []), position: position)
                try resolver.checkFunction?(call.name)
                let slot = width + calls.count
                calls.append(ResolvedAggregateCall(plan: plan, arguments: arguments, direct: direct, filter: filter, distinct: false, order: []))
                let derivation = ExpressionCollation.collatable(plan.resultType.type) ? try ExpressionCollation.combine(arguments) : CollationDerivation.none
                let result = ResolvedExpression(type: plan.resultType, node: .column(slot), position: position, collation: derivation)
                resolvedCalls[position] = result; return result
            }
            if call.star && call.name.description != "count" { throw JedError("42601", "* is only valid in COUNT") }
            let collector = resolver.resolveAggregate
            let rewrite = resolver.rewriteResolved
            let windowCollector = resolver.resolveWindow
            let grouped = resolver.groupedColumns
            resolver.resolveAggregate = forbidden
            resolver.resolveWindow = WindowQueryBuilder.aggregateForbidden
            resolver.rewriteResolved = nil
            resolver.groupedColumns = nil
            defer { resolver.resolveAggregate = collector; resolver.rewriteResolved = rewrite; resolver.resolveWindow = windowCollector; resolver.groupedColumns = grouped }
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
                types[0].collation = try ExpressionCollation.derive(arguments[0]).comparisonName()
            }
            let plan = try AggregatePlan.resolve(name: call.name.description, inputTypes: types, star: call.star)
            let order = try call.orderBy.map { term -> ResolvedOrderTerm in
                let expression = try resolver.resolve(term.expression)
                if call.distinct, !arguments.contains(where: { ExpressionIdentity.same($0, expression) }) {
                    throw JedError("42P10", "in an aggregate with DISTINCT, ORDER BY expressions must appear in argument list")
                }
                var type = expression.type ?? TypeDescriptor(.scalar(.text))
                type.collation = try ExpressionCollation.derive(expression).orderingName()
                try ResolutionDiagnostics.comparable(type.type, at: term.expression.expressionLocation, ordering: true)
                return ResolvedOrderTerm(expression: expression, type: type, descending: term.descending, nullsFirst: term.nullsFirst ?? term.descending)
            }
            let filter = try call.filter.map { try resolver.resolve($0, expected: TypeDescriptor(.scalar(.boolean))) }
            if let type = filter?.type, type.type != .scalar(.boolean) { throw JedError("42804", "FILTER requires a boolean expression").at(filter?.location ?? 0) }
            try AggregateScope.rejectOuterOnly(arguments + order.map(\.expression) + (filter.map { [$0] } ?? []), position: position)
            try resolver.checkFunction?(call.name)
            let slot = width + calls.count
            calls.append(ResolvedAggregateCall(plan: plan, arguments: arguments, direct: [], filter: filter, distinct: call.distinct, order: order))
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
        windows?.validateGrouped = validate
        var projections: [ResolvedExpression] = [], columns: [ResultColumn] = []
        for (name, syntax) in targets {
            let expression = try resolver.resolve(syntax)
            try validate(expression)
            projections.append(expression); columns.append(ResultColumn(name: name, type: expression.type))
        }
        guard !projections.isEmpty else { throw JedError("42601", "SELECT requires a target list") }
        resolver.resolveWindow = WindowQueryBuilder.forbidden
        let having = try select.having.map { PredicateOrdering.order(try resolver.resolve($0, expected: TypeDescriptor(.scalar(.boolean))), planningProbe: planningProbe) }
        resolver.resolveWindow = previousWindow
        if let having {
            guard having.type == nil || having.type?.type == .scalar(.boolean) else { throw JedError("42804", "HAVING requires a boolean expression").at(having.location) }
            try validate(having)
        }
        var resolvedOrder: [ResolvedOrderTerm] = []
        for (ordinal, term) in order.enumerated() {
            let priorCalls = calls, priorGrouping = grouping, priorResolved = resolvedCalls
            let restoreWindow = windows?.restorePoint()
            windows?.beginOrderExpression()
            // A computed ordinal/alias is resolved as an ORDER expression. Its
            // aggregate occurrences fold separately from the SELECT target;
            // only repeated probes within this one expression share a slot.
            resolvedCalls = [:]
            var expression: ResolvedExpression
            let source: SQLExpression
            if case .number(let spelling) = term.expression.kind, let ordinal = Int(spelling) {
                guard ordinal > 0 && ordinal <= projections.count else { throw JedError("42P10", "ORDER BY position is not in select list") }
                source = targets[ordinal - 1].1
                expression = try resolver.resolve(source)
            } else if case .column(nil, let name) = term.expression.kind, columns.contains(where: { $0.name == name }) {
                let matches = columns.indices.filter { columns[$0].name == name }
                guard matches.dropFirst().allSatisfy({ ExpressionIdentity.same(projections[matches[0]], projections[$0]) }) else {
                    throw JedError("42702", "ORDER BY reference is ambiguous")
                }
                source = targets[matches[0]].1
                expression = try resolver.resolve(source)
            } else { source = term.expression; expression = try resolver.resolve(source) }
            try validate(expression)
            var type = expression.type ?? TypeDescriptor(.scalar(.text))
            type.collation = try ExpressionCollation.derive(expression).orderingName()
            try ResolutionDiagnostics.comparable(type.type, at: term.expression.expressionLocation, ordering: true)
            // Appended DISTINCT ON keys only discover the effective sort.
            // A matched key does not collect another aggregate/window fold.
            if ordinal >= order.count - select.distinctOn.count,
               let existing = resolvedOrder.first(where: {
                   Self.sameKey(expression, $0.expression, calls: calls, grouping: grouping, width: width,
                                otherColumns: { windows?.sameColumn($0, $1) ?? false })
               }) {
                calls = priorCalls; grouping = priorGrouping; resolvedCalls = priorResolved
                restoreWindow?(); expression = existing.expression
            }
            resolvedOrder.append(ResolvedOrderTerm(expression: expression, type: type,
                descending: term.descending, nullsFirst: term.nullsFirst ?? term.descending,
                materialized: OrderSyntax.materialized(source)))
        }
        // Unused named definitions are validated under the grouped scope, but
        // their scratch aggregate occurrences must not change execution.
        let savedCalls = calls, savedGrouping = grouping, savedResolved = resolvedCalls
        try windows?.validateDefinitions()
        calls = savedCalls; grouping = savedGrouping; resolvedCalls = savedResolved
        return Self(keys: keys, keyColumns: keyColumns, sets: sets, grouping: grouping, calls: calls, having: having, projections: projections,
                    columns: columns, order: resolvedOrder, sourceWidth: scope.count, layoutWidth: width)
    }
    static func equalityType(_ expression: ResolvedExpression, at position: Int? = nil) throws {
        guard let type = expression.type else { return }
        try ResolutionDiagnostics.comparable(type.type, at: position ?? expression.location, ordering: false)
        _ = try ExpressionCollation.derive(expression).orderingName()
    }
}
