internal struct WindowOrder {
    let slot: Int
    let type: TypeDescriptor
    let descending: Bool
    let nullsFirst: Bool
}
internal struct ResolvedWindow {
    let name: String
    let arguments: [ResolvedExpression]
    let aggregate: AggregatePlan?
    let filter: ResolvedExpression?
    let partition: [Int]
    let order: [WindowOrder]
    let frame: WindowFrame?
}
internal struct WindowQueryPlan {
    static let keyBase = 1 << 27
    static let resultBase = 1 << 28
    let calls: [ResolvedWindow]
    let keys: [ResolvedExpression]
    let inputWidth: Int
    let groupingBase: Int
    var expressions: [ResolvedExpression] {
        keys + calls.flatMap { $0.arguments + ($0.filter.map { [$0] } ?? []) }
    }
    func slot(_ index: Int) -> Int {
        if index >= Self.resultBase { return inputWidth + keys.count + index - Self.resultBase }
        if index >= Self.keyBase { return inputWidth + index - Self.keyBase }
        if index < 0 { return groupingBase - index - 1 }
        return index
    }
    func rowContext(_ row: [SQLValue], parent: ExpressionContext) -> ExpressionContext {
        var current = parent
        current.column = { index in
            let at = slot(index)
            guard row.indices.contains(at) else { throw JedError("XX000", "window column is outside the synthetic row") }
            return row[at]
        }
        return current
    }
}

/// Collects window calls while the ordinary aggregate collector stays active.
/// Placeholders avoid rebuilding expression trees when the final row grows.
internal final class WindowQueryBuilder {
    static let names: Set<String> = Set(GeneratedSpec.functions.filter { $0.kind == "window" }.map(\.name))
    unowned let resolver: ExpressionResolver
    private var definitions: [(SQLText, SQLWindow)] = []
    private var calls: [ResolvedWindow] = []
    private var keys: [ResolvedExpression] = []
    private var cached: [Int: ResolvedExpression] = [:]
    var validateGrouped: ((ResolvedExpression) throws -> Void)?
    /// ORDER BY can reference an existing target's window result. Separate
    /// authored SELECT occurrences still retain separate result/cost slots.
    var reuseResults = false

    init(select: SQLSelect, resolver: ExpressionResolver) throws {
        self.resolver = resolver
        for entry in select.windows {
            guard !definitions.contains(where: { $0.0 == entry.name }) else { throw JedError("42P20", "window is already defined: \(entry.name)") }
            definitions.append((entry.name, try expanded(entry.definition)))
        }
        resolver.resolveWindow = { [weak self] call, position in
            guard let self else { throw JedError("XX000", "window collector has expired") }
            return try self.resolve(call, position: position)
        }
    }
    static func forbidden(_ call: SQLCall, _ position: Int) throws -> ResolvedExpression? {
        if call.over != nil || call.overName != nil { throw JedError("42P20", "window functions are not allowed here").at(position) }
        if names.contains(call.name.description), call.withinGroup.isEmpty { throw JedError("42809", "window function requires an OVER clause").at(position) }
        return nil
    }
    static func aggregateForbidden(_ call: SQLCall, _ position: Int) throws -> ResolvedExpression? {
        if call.over != nil || call.overName != nil { throw JedError("42803", "aggregate arguments cannot contain window functions").at(position) }
        return nil
    }
    func finish(inputWidth: Int, aggregation: AggregateQueryPlan?) -> WindowQueryPlan? {
        guard !calls.isEmpty else { return nil }
        return WindowQueryPlan(calls: calls, keys: keys, inputWidth: inputWidth,
            groupingBase: aggregation.map { $0.layoutWidth + $0.calls.count } ?? inputWidth)
    }
    func validateDefinitions() throws {
        let previous = resolver.resolveWindow
        let touched = resolver.referencedColumns
        resolver.resolveWindow = Self.forbidden
        defer { resolver.resolveWindow = previous; resolver.referencedColumns = touched }
        for (_, definition) in definitions { _ = try resolveDefinition(definition, materialize: false) }
    }
    private func expanded(_ definition: SQLWindow) throws -> SQLWindow {
        guard let name = definition.baseName else { return definition }
        guard let base = definitions.first(where: { $0.0 == name })?.1 else { throw JedError("42704", "window does not exist: \(name)") }
        guard definition.partitionBy.isEmpty else { throw JedError("42P20", "cannot override PARTITION BY of a named window") }
        guard base.orderBy.isEmpty || definition.orderBy.isEmpty else { throw JedError("42P20", "cannot override ORDER BY of a named window") }
        guard base.frame == nil else { throw JedError("42P20", "cannot copy a window with a frame clause") }
        return SQLWindow(partitionBy: base.partitionBy, orderBy: base.orderBy.isEmpty ? definition.orderBy : base.orderBy, frame: definition.frame)
    }
    private func equivalent(_ a: ResolvedWindow, _ b: ResolvedWindow) throws -> Bool {
        func expressions(_ left: [ResolvedExpression], _ right: [ResolvedExpression]) -> Bool {
            left.count == right.count && zip(left, right).allSatisfy { ExpressionIdentity.same($0, $1) }
        }
        func slot(_ left: Int, _ right: Int) -> Bool {
            if left == right { return true }
            guard left >= WindowQueryPlan.keyBase, left < WindowQueryPlan.resultBase,
                  right >= WindowQueryPlan.keyBase, right < WindowQueryPlan.resultBase else { return false }
            return ExpressionIdentity.same(keys[left - WindowQueryPlan.keyBase], keys[right - WindowQueryPlan.keyBase])
        }
        func bound(_ left: WindowFrame.Bound, _ right: WindowFrame.Bound) throws -> Bool {
            switch (left, right) {
            case (.current, .current): return true
            case (.unbounded(let a), .unbounded(let b)): return a == b
            case (.offset(let a, let ap), .offset(let b, let bp)):
                return try ap == bp && ValueEqualityKey.encode([a]) == ValueEqualityKey.encode([b])
            default: return false
            }
        }
        guard a.name == b.name, expressions(a.arguments, b.arguments),
              expressions(a.filter.map { [$0] } ?? [], b.filter.map { [$0] } ?? []),
              a.partition.count == b.partition.count, zip(a.partition, b.partition).allSatisfy({ slot($0, $1) }),
              a.order.count == b.order.count else { return false }
        for (left, right) in zip(a.order, b.order) {
            guard slot(left.slot, right.slot), left.type == right.type,
                  left.descending == right.descending, left.nullsFirst == right.nullsFirst else { return false }
        }
        switch (a.frame, b.frame) {
        case (nil, nil): return true
        case (let a?, let b?): return try a.mode == b.mode && a.exclusion == b.exclusion && bound(a.start, b.start) && bound(a.end, b.end)
        default: return false
        }
    }
    private func resolveDefinition(_ definition: SQLWindow, materialize: Bool) throws -> ([Int], [WindowOrder], WindowFrame?) {
        func key(_ syntax: SQLExpression) throws -> (Int, ResolvedExpression) {
            let expression = try resolver.resolve(syntax)
            try validateGrouped?(expression)
            try StoredExpressionDependencies.walk([expression]) { node in
                if case .outerColumn = node.node { throw JedError("0A000", "window key may not reference an enclosing query") }
            }
            if case .column(let slot) = expression.node { return (slot, expression) }
            let slot = WindowQueryPlan.keyBase + keys.count
            if materialize { keys.append(expression) }
            return (slot, expression)
        }
        var partition: [Int] = [], order: [WindowOrder] = []
        for syntax in definition.partitionBy {
            let (slot, expression) = try key(syntax)
            try AggregateQueryPlan.equalityType(expression)
            partition.append(slot)
        }
        for syntax in definition.orderBy {
            let (slot, expression) = try key(syntax.expression)
            var type = expression.type ?? TypeDescriptor(.scalar(.text))
            type.collation = try ExpressionCollation.derive(expression).orderingName()
            try TypeRules.comparable(type.type, type.type)
            order.append(WindowOrder(slot: slot, type: type, descending: syntax.descending, nullsFirst: syntax.nullsFirst ?? syntax.descending))
        }
        let frame: WindowFrame?
        do { frame = try WindowFrame.resolve(definition.frame, order: order) }
        catch let error as JedError where !materialize && error.code == "22013" { frame = nil }
        return (partition, order, frame)
    }
    private func resolve(_ call: SQLCall, position: Int) throws -> ResolvedExpression? {
        guard call.over != nil || call.overName != nil else { return try Self.forbidden(call, position) }
        if let existing = cached[position] { return existing }
        let name = call.name.description
        guard name != "grouping" else { throw JedError("42601", "OVER is not supported for GROUPING") }
        guard !call.distinct, call.orderBy.isEmpty, call.withinGroup.isEmpty else {
            throw JedError("0A000", "DISTINCT, aggregate ORDER BY, and ordered-set OVER are not supported")
        }
        let aggregate = AggregateQueryPlan.names.contains(name)
        if call.filter != nil && !aggregate { throw JedError("0A000", "FILTER is not implemented for non-aggregate window functions") }
        guard call.arguments.allSatisfy({ $0.name == nil && !$0.variadic }) else { throw JedError("42883", "named window arguments are not supported") }
        let previous = resolver.resolveWindow
        resolver.resolveWindow = Self.forbidden
        defer { resolver.resolveWindow = previous }
        func argument(_ index: Int, expected: TypeDescriptor? = nil) throws -> ResolvedExpression {
            let value = try resolver.resolve(call.arguments[index].value, expected: expected)
            try validateGrouped?(value); return value
        }
        func requireInteger(_ value: ResolvedExpression) throws {
            if let type = value.type, ![SQLType.scalar(.i16), .scalar(.i32), .scalar(.i64)].contains(type.type) {
                throw JedError("42804", "window count or offset must be integer")
            }
        }
        var arguments: [ResolvedExpression] = [], plan: AggregatePlan?
        let type: TypeDescriptor?
        switch name {
        case "row_number", "rank", "dense_rank", "percent_rank", "cume_dist":
            guard !call.star, call.arguments.isEmpty else { throw JedError("42883", "window ranking function takes no arguments") }
            type = TypeDescriptor(.scalar(name == "percent_rank" || name == "cume_dist" ? .f64 : .i64))
        case "ntile":
            guard !call.star, call.arguments.count == 1 else { throw JedError("42883", "NTILE takes one argument") }
            let value = try argument(0); try requireInteger(value); arguments = [value]; type = TypeDescriptor(.scalar(.i64))
        case "lag", "lead":
            guard !call.star, (1...3).contains(call.arguments.count) else { throw JedError("42883", "offset window function takes one to three arguments") }
            let value = try argument(0); arguments = [value]; type = value.type
            if call.arguments.count >= 2 { let offset = try argument(1); try requireInteger(offset); arguments.append(offset) }
            if call.arguments.count == 3 {
                let scalar = value.type?.type
                let hintTypes: [SQLType] = [.scalar(.i16), .scalar(.i32), .scalar(.i64), .scalar(.f32), .scalar(.f64)]
                let hint = scalar.map(hintTypes.contains) == true ? value.type : nil
                var fallback = try argument(2, expected: hint)
                if let actual = fallback.type {
                    guard let target = value.type, actual.type == target.type ||
                        ([SQLType.scalar(.i16), .scalar(.i32), .scalar(.i64)].contains(actual.type) && [SQLType.scalar(.i16), .scalar(.i32), .scalar(.i64)].contains(target.type)) else {
                        throw JedError("42804", "window default must match the value type")
                    }
                    if actual != target { fallback = ResolvedExpression(type: target, node: .convert(fallback, target), position: fallback.position, collation: fallback.collation) }
                }
                arguments.append(fallback)
            }
        case "first_value", "last_value", "nth_value":
            guard !call.star else { throw JedError("42601", "* is only valid in COUNT") }
            guard call.arguments.count == (name == "nth_value" ? 2 : 1) else { throw JedError("42883", "wrong number of window value arguments") }
            let value = try argument(0); arguments = [value]; type = value.type
            if name == "nth_value" { let count = try argument(1); try requireInteger(count); arguments.append(count) }
        default:
            guard aggregate else { throw JedError("42883", "function is not a window function: \(name)") }
            if call.star && name != "count" { throw JedError("42601", "* is only valid in COUNT") }
            for index in call.arguments.indices { arguments.append(try argument(index)) }
            if name == "string_agg", arguments.count == 2 {
                func unknown(_ syntax: SQLExpression) -> Bool {
                    switch syntax.kind { case .null, .string, .parameter: true; case .parenthesized(let child): unknown(child); default: false }
                }
                let concrete = arguments.indices.filter { !unknown(call.arguments[$0].value) }.compactMap { arguments[$0].type?.type }
                let family: SQLType = concrete.contains(.scalar(.bytea)) ? .scalar(.bytea) : .scalar(.text)
                for index in arguments.indices where unknown(call.arguments[index].value) { arguments[index] = try argument(index, expected: TypeDescriptor(family)) }
            }
            if name == "array_agg", arguments.first?.type == nil { throw JedError("42725", "aggregate input type is ambiguous") }
            var types = arguments.map { $0.type ?? TypeDescriptor(.scalar(.text)) }
            if ["min", "max"].contains(name), !types.isEmpty { types[0].collation = try ExpressionCollation.derive(arguments[0]).orderingName() }
            plan = try AggregatePlan.resolve(name: name, inputTypes: types, star: call.star)
            type = plan!.resultType
        }
        let definition: SQLWindow
        if let name = call.overName {
            guard let found = definitions.first(where: { $0.0 == name })?.1 else { throw JedError("42704", "window does not exist: \(name)") }
            definition = found
        } else { definition = try expanded(call.over!) }
        let keyStart = keys.count
        let (partition, order, frame) = try resolveDefinition(definition, materialize: true)
        let aggregateCollector = resolver.resolveAggregate
        resolver.resolveAggregate = AggregateQueryPlan.forbidden
        let filter: ResolvedExpression?
        do { filter = try call.filter.map { try resolver.resolve($0, expected: TypeDescriptor(.scalar(.boolean))) } }
        catch { resolver.resolveAggregate = aggregateCollector; throw error }
        resolver.resolveAggregate = aggregateCollector
        if let filter {
            if let type = filter.type, type.type != .scalar(.boolean) { throw JedError("42804", "FILTER requires a boolean expression") }
            try validateGrouped?(filter)
        }
        try resolver.checkFunction?(call.name)
        let incoming = ResolvedWindow(name: name, arguments: arguments, aggregate: plan, filter: filter, partition: partition, order: order, frame: frame)
        let existing = try reuseResults ? calls.firstIndex(where: { try equivalent($0, incoming) }) : nil
        let result = ResolvedExpression(type: type, node: .column(WindowQueryPlan.resultBase + (existing ?? calls.count)), position: position,
            collation: type.map { ExpressionCollation.collatable($0.type) } == true ? try ExpressionCollation.combine(arguments) : CollationDerivation.none)
        if existing != nil { keys.removeSubrange(keyStart...) } else { calls.append(incoming) }
        cached[position] = result; return result
    }
}
