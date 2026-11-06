/// RETURNING has its own flat scope: source, current target, old target, new
/// target, then one synthetic action slot. Only this scope admits merge_action.
internal struct MergeReturning {
    let present: Bool
    let columns: [ResultColumn]
    let expressions: [ResolvedExpression]
    let touched: Set<Int>
    let sourceWidth: Int
    let targetWidth: Int
    init(_ syntax: SQLReturning?, relation: MutationExecutor.Relation, source: [ExpressionColumn], sourceOrder: [Int: Int],
         sourceWidth: Int, targetLabel: SQLText, resolver: ExpressionResolver) throws {
        self.sourceWidth = sourceWidth; targetWidth = relation.visibleCount
        present = syntax != nil
        guard let syntax else { columns = []; expressions = []; touched = []; return }
        let width = relation.visibleCount
        let previous = resolver.columns, references = resolver.referencedColumns, action = resolver.mergeActionSlot
        defer { resolver.columns = previous; resolver.referencedColumns = references; resolver.mergeActionSlot = action }
        var scope = source + relation.columns(qualifier: targetLabel, offset: sourceWidth)
        let explicit = [(syntax.oldAlias, sourceWidth + width), (syntax.newAlias, sourceWidth + 2 * width)]
        for (name, offset) in explicit {
            guard let name else { continue }
            guard !scope.contains(where: { $0.qualifier == name }) else { throw JedError("42712", "table name specified more than once: \(name)") }
            scope += relation.columns(qualifier: name, offset: offset, qualifiedOnly: true)
        }
        for (name, offset, hidden) in [("old", sourceWidth + width, syntax.oldAlias != nil), ("new", sourceWidth + 2 * width, syntax.newAlias != nil)] {
            let label = SQLText(validatedUTF8: Array(name.utf8))
            if !hidden, !scope.contains(where: { $0.qualifier == label }) {
                scope += relation.columns(qualifier: label, offset: offset, qualifiedOnly: true)
            }
        }
        resolver.columns = MergeExecutor.ambiguous(scope, target: targetLabel, source: source)
        resolver.referencedColumns = []; resolver.mergeActionSlot = sourceWidth + 3 * width
        var resultColumns: [ResultColumn] = [], resultExpressions: [ResolvedExpression] = []
        for item in syntax.items {
            if item.star {
                if let label = item.qualifier, resolver.columns.contains(where: { $0.qualifier == label && $0.ambiguousRelation }) {
                    throw JedError("42P09", "table reference is ambiguous: \(label)")
                }
                var selected = resolver.columns.filter {
                    guard !$0.hiddenFromStar else { return false }
                    if let label = item.qualifier { return $0.qualifier == label }
                    return !$0.qualifiedOnly
                }
                // USING/NATURAL reorders unqualified output but a relation's
                // qualified star retains its original column declaration order.
                if item.qualifier != nil {
                    selected.sort { (sourceOrder[$0.index] ?? $0.index) < (sourceOrder[$1.index] ?? $1.index) }
                }
                guard !selected.isEmpty else { throw JedError("42P01", "RETURNING wildcard has no matching relation") }
                for column in selected {
                    resultColumns.append(ResultColumn(name: column.name, type: column.unresolved ? nil : column.type))
                    resultExpressions.append(ResolvedExpression(type: column.unresolved ? nil : column.type,
                        node: .column(column.index), position: item.span.position, collation: column.collation))
                    resolver.referencedColumns.insert(column.index)
                }
            } else if let syntax = item.expression {
                let value = try resolver.resolve(syntax)
                resultColumns.append(ResultColumn(name: item.alias ?? SelectPlan.outputName(syntax), type: value.type))
                resultExpressions.append(value)
            }
        }
        columns = resultColumns; expressions = resultExpressions
        touched = Set(resolver.referencedColumns.compactMap { index in
            guard index >= sourceWidth, index < sourceWidth + 3 * width else { return nil }
            return (index - sourceWidth) % width
        })
    }
    func prepare(relation: MutationExecutor.Relation, context: ExpressionContext) throws {
        if let view = relation.view {
            try view.prepare(expressions, context: context, column: { index in
                guard index >= sourceWidth, index < sourceWidth + 3 * targetWidth else { return nil }
                return (index - sourceWidth) % targetWidth
            })
        } else { try context.subqueries.prepare(expressions, context: context) }
    }
    func emit(source: [SQLValue]?, old: ((Int) throws -> SQLValue)?, new: ((Int) throws -> SQLValue)?,
              action: String, relation: MutationExecutor.Relation, context: ExpressionContext,
              result: MutationResult) throws {
        guard !expressions.isEmpty else { return }
        context.meter.charge(1); try context.meter.guardCost()
        var row = context
        row.column = { index in
            if index < sourceWidth { return source?[index] ?? .null }
            if index == sourceWidth + 3 * targetWidth { return .text(SQLText(validatedUTF8: Array(action.utf8))) }
            let version = (index - sourceWidth) / targetWidth, column = (index - sourceWidth) % targetWidth
            let base = version == 0 ? (action == "DELETE" ? old : new) : version == 1 ? old : new
            guard let base else { return .null }
            if let view = relation.view { return try view.value(column, base: base, context: context) }
            return try base(column)
        }
        try result.append(expressions.map { try row.evaluate($0) })
    }
}
