/// MERGE retains one source query and all clause scopes. Planning never runs a
/// row, a default, a host kernel, or a clock consumer.
internal struct MergePreparation {
    struct Clause {
        enum Action { case nothing, delete, update([(Int, ResolvedExpression)]), insert(MutationExecutor.Relation, [ResolvedExpression], [Int]) }
        let match: String
        let condition: ResolvedExpression?
        let action: Action
        var expressions: [ResolvedExpression] {
            let values: [ResolvedExpression]
            switch action { case .update(let changes): values = changes.map { $0.1 }; case .insert(_, let row, let supplied): values = supplied.map { row[$0] } + row.indices.filter { !supplied.contains($0) }.map { row[$0] }; default: values = [] }
            return (condition.map { [$0] } ?? []) + values
        }
        var assigned: Set<Int>? {
            switch action { case .update(let changes): Set(changes.map { $0.0 }); case .insert: nil; default: [] }
        }
    }
    let read: DomainSnapshot
    let settings: SessionSettings
    let inputSettings: SessionSettings
    let relation: MutationExecutor.Relation
    let source: SelectPlan
    let sourceColumns: [ExpressionColumn]
    let sourceWidth: Int
    let condition: ResolvedExpression
    let probe: ResolvedExpression
    let clauses: [Clause]
    let returning: MergeReturning
    let checks: [MutationExecutor.Check]
    let indexes: [StoredIndexPlan]
    let touched: [Int]
    let parameterTypes: [TypeDescriptor]
    let profileIdentity = QueryProfileIdentity()
    let probeProfileIdentity = QueryProfileIdentity()
    let loopProfileIdentity = QueryProfileIdentity()
    let unmatchedProfileIdentity = QueryProfileIdentity()
    var hasBySource: Bool { clauses.contains { $0.match == "not matched by source" } }
}

internal enum MergeExecutor {
    static func prepare(_ syntax: SQLMerge, snapshot: DomainSnapshot, settings inputSettings: SessionSettings,
                        context: ExpressionContext, using shared: ExpressionResolver? = nil,
                        schemaOnly: Bool = false) throws -> MergePreparation {
        do {
        try SystemRelationPlan.rejectTarget(syntax.table.name)
        let read = context.writeScope?.readSnapshot ?? snapshot
        // A DO NOTHING-only statement still requires an updatable target. Its
        // weakest view shape is DELETE, but its capability is SELECT.
        let top = try MutationTarget.resolve(syntax.table, operation: "merge", snapshot: read,
                                             settings: inputSettings, context: context)
        let relation = top.relation, settings = top.settings
        try relation.requireSelect(inputSettings)
        // View assignment mappings are checked in written action order before
        // the source plan, matching ordinary view writes.
        var actionTargets: [MutationTarget.Prepared?] = []
        for clause in syntax.clauses {
            let operation: String?
            switch clause.action { case .insert: operation = "insert into"; case .update: operation = "update"; case .delete: operation = "delete from"; case .nothing: operation = nil }
            let target = try operation.map { try MutationTarget.resolve(syntax.table, operation: $0, snapshot: read,
                                                                        settings: inputSettings, context: context) }
            if let view = target?.relation.view {
                let names: [SQLText]
                switch clause.action {
                case .update(let assignments): names = assignments.map(\.column)
                case .insert(let columns, _, let values):
                    names = columns.isEmpty ? Array(view.layers[0].view.columns.prefix(values?.count ?? 0)).map(\.name) : columns
                default: names = []
                }
                let targets = try names.map { try view.column($0) }
                guard Set(targets).count == targets.count else { throw JedError("42701", "column specified more than once") }
                _ = try view.destinations(targets)
            }
            actionTargets.append(target)
        }
        if !schemaOnly { try CatalogEnvironment.checkWritable(relation.table, image: read.catalog, environment: settings.environment,
                                             borrowedTypes: read.borrowedTypes, rowTypes: relation.types) }
        let resolver = MutationExecutor.resolver(relation, snapshot: read, settings: inputSettings, context: context, columns: false)
        if let shared {
            resolver.parameters = shared.parameters; resolver.parameterUses = shared.parameterUses
            resolver.cteBindings = shared.cteBindings; resolver.outerScopes = shared.outerScopes
            resolver.viewExpansion = shared.viewExpansion
        }
        defer {
            if let shared {
                shared.parameters = resolver.parameters; shared.parameterUses.formUnion(resolver.parameterUses)
                shared.outerReferences.formUnion(resolver.outerReferences)
            }
        }
        resolver.resolveAggregate = AggregateQueryPlan.forbidden
        let checks = try schemaOnly ? [] : MutationExecutor.checks(relation, resolver: resolver, environment: settings.environment)
        let indexes = try (schemaOnly ? [] : relation.table.indexes.sorted { $0.name < $1.name }).map {
            try StoredIndexPlan.resolve(table: relation.table, index: $0, snapshot: read, settings: settings, context: context)
        }
        let star = SQLSelectItem(star: true, span: syntax.source.span)
        let query = SQLQuery(body: .select(SQLSelect(items: [star], from: [syntax.source])), span: syntax.source.span)
        var source = try SelectPlan.resolve(query, snapshot: read, settings: inputSettings, scalar: context.scalar,
                                            using: resolver, finalizeParameters: false)
        // Joined sources keep their qualifier-only USING/NATURAL copies. A
        // plain source keeps exactly its SELECT * columns and output order.
        var sourceColumns: [ExpressionColumn] = [], sourceOrder: [Int: Int] = [:]
        let aliasesJoinedSource: Bool
        if case .joined = syntax.source.source { aliasesJoinedSource = syntax.source.alias != nil }
        else { aliasesJoinedSource = false }
        if let input = source.relation {
            var positions: [Int: Int] = [:]
            for (ordinal, projection) in source.projections.enumerated() {
                if case .column(let index) = projection.node { positions[index] = ordinal }
            }
            for column in input.columns {
                // An outer join alias exposes the merged SELECT * output; its
                // inner qualifiers and hidden USING copies no longer exist.
                if aliasesJoinedSource && column.qualifiedOnly { continue }
                var ordinal = positions[column.index]
                if ordinal == nil {
                    ordinal = source.projections.count; positions[column.index] = ordinal
                    source.projections.append(ResolvedExpression(type: column.unresolved ? nil : column.type,
                        node: .column(column.index), position: syntax.source.span.position, collation: column.collation))
                    source.columns.append(ResultColumn(name: column.name, type: column.unresolved ? nil : column.type))
                    if !source.touched.contains(column.index) { source.touched.append(column.index); source.touched.sort() }
                }
                sourceColumns.append(ExpressionColumn(name: column.name, qualifier: column.qualifier, type: column.type,
                    index: ordinal!, collation: column.collation, qualifiedOnly: column.qualifiedOnly,
                    unresolved: column.unresolved, scalarFunction: column.scalarFunction, hiddenFromStar: column.hiddenFromStar))
                sourceOrder[ordinal!] = aliasesJoinedSource ? ordinal! : column.index
            }
        } else {
            let label = ownLabel(syntax.source)
            sourceColumns = source.columns.enumerated().map { index, column in
                sourceOrder[index] = index
                return ExpressionColumn(name: column.name, qualifier: label, type: column.type ?? TypeDescriptor(.scalar(.text)),
                    index: index, collation: source.projections[index].collation?.asColumn, unresolved: column.type == nil)
            }
        }
        if !source.streamsComputedProjection, !ColumnarExecution.projection(source, context) {
            source.materializeSource = source.aggregation == nil && source.window == nil && source.relation == nil
                && !source.orderedByScan && source.order.isEmpty && source.limit == nil
        }
        let label = syntax.alias ?? syntax.table.name, width = relation.visibleCount
        if ownLabel(syntax.source) == label { throw JedError("42712", "name specified more than once: \(label)") }
        let targetColumns = relation.columns(qualifier: label)
        func scope(_ match: String) -> [ExpressionColumn] {
            if match == "not matched by target" { return sourceColumns }
            if match == "not matched by source" { return targetColumns }
            return ambiguous(targetColumns + shifted(sourceColumns, by: width), target: label, source: sourceColumns)
        }
        if schemaOnly {
            let returning = try MergeReturning(syntax.returning, relation: relation, source: sourceColumns, sourceOrder: sourceOrder,
                sourceWidth: source.columns.count, targetLabel: label, resolver: resolver)
            let truth = ResolvedExpression(type: TypeDescriptor(.scalar(.boolean)), node: .constant(.boolean(true)), position: 0)
            return MergePreparation(read: read, settings: settings, inputSettings: inputSettings, relation: relation,
                source: source, sourceColumns: sourceColumns, sourceWidth: source.columns.count, condition: truth,
                probe: truth, clauses: [], returning: returning, checks: [], indexes: [], touched: [], parameterTypes: [])
        }
        resolver.columns = scope("matched"); resolver.referencedColumns = []
        let condition = PredicateOrdering.order(try boolean(syntax.condition, resolver: resolver))
        var touched = Set(resolver.referencedColumns.filter { $0 < width })
        var clauses: [MergePreparation.Clause] = []
        for (ordinal, clause) in syntax.clauses.enumerated() {
            resolver.columns = scope(clause.match); resolver.referencedColumns = []
            let predicate = try clause.condition.map { try boolean($0, resolver: resolver) }
            let action: MergePreparation.Clause.Action
            switch clause.action {
            case .nothing: action = .nothing
            case .delete: action = .delete
            case .update(let assignments):
                let target = actionTargets[ordinal]!.relation
                var assigned: Set<Int> = [], changes: [(Int, ResolvedExpression)] = []
                for assignment in assignments {
                    let visible = try target.view.map { try $0.column(assignment.column) } ?? target.column(assignment.column)
                    let column = try target.view.map { try $0.destinations([visible])[0] } ?? visible
                    guard assigned.insert(column).inserted else { throw JedError("42701", "multiple assignments to same column") }
                    guard assignment.value == nil || target.table.columns[column].identity != true else {
                        throw JedError("428C9", "identity column can only be updated to DEFAULT").identity(target.table.columns[column].name, insert: false)
                    }
                    let expression: ResolvedExpression
                    if let value = assignment.value { expression = try AssignmentValues.resolve(value, to: target.types[column], resolver: resolver) }
                    else if let view = target.view {
                        if let text = view.layers[0].view.columns[visible].defaultText {
                            guard let syntax = CatalogExpression(text).parsed else { throw JedError("XX003", "stored view default cannot be parsed") }
                            let old = resolver.columns; resolver.columns = []; defer { resolver.columns = old }
                            expression = try AssignmentValues.resolve(syntax, to: target.types[column], resolver: resolver)
                        } else { expression = ResolvedExpression(type: target.types[column], node: .constant(.null), position: 0) }
                    } else { expression = try MutationExecutor.defaultExpression(column, relation: target, resolver: resolver) }
                    changes.append((column, expression))
                }
                action = .update(changes)
            case .insert(let names, let overriding, let values):
                let target = actionTargets[ordinal]!.relation
                let visible = try names.isEmpty ? Array(0..<(values?.count ?? 0))
                    : names.map { name in try target.view.map { try $0.column(name) } ?? target.column(name) }
                let destinations: [Int]
                if let view = target.view { destinations = try view.destinations(visible) }
                else {
                    guard visible.allSatisfy(target.types.indices.contains) else { throw JedError("42601", "INSERT has more expressions than target columns") }
                    destinations = visible
                }
                guard Set(destinations).count == destinations.count else { throw JedError("42701", "column specified more than once") }
                let supplied = values ?? []
                guard supplied.count <= destinations.count else { throw JedError("42601", "INSERT has more expressions than target columns") }
                guard names.isEmpty || values == nil || supplied.count == destinations.count else { throw JedError("42601", "INSERT has more target columns than expressions") }
                var table = target.table, viewDefaults: Set<Int> = []
                if let view = target.view {
                    for (column, expression) in try view.insertDefaults() where viewDefaults.insert(column).inserted {
                        if table.columns[column].identity != nil && overriding == "user" { continue }
                        table.columns[column].defaultValue = .expression(expression)
                    }
                }
                let rowRelation = try MutationExecutor.Relation(SQLName(name: table.name, span: syntax.table.span),
                    snapshot: read, tableOverride: table, view: target.view, domain: settings.targetDomain)
                var provided: [Int: ResolvedExpression] = [:]
                for (slot, value) in supplied.enumerated() {
                    let column = destinations[slot]
                    if overriding == "user" && table.columns[column].identity != nil { continue }
                    if let value {
                        guard table.columns[column].identity != true || overriding == "system" else { throw JedError("428C9", "cannot insert a non-DEFAULT value into an identity column").identity(table.columns[column].name, insert: true) }
                        provided[column] = try AssignmentValues.resolve(value, to: target.types[column], resolver: resolver)
                    }
                }
                var row: [ResolvedExpression] = []
                for column in target.types.indices {
                    if let expression = provided[column] { row.append(expression); continue }
                    if viewDefaults.contains(column), table.columns[column].identity == true, overriding != "system", overriding != "user" {
                        throw JedError("428C9", "cannot insert a view default into a GENERATED ALWAYS identity column").identity(table.columns[column].name, insert: true)
                    }
                    row.append(try MutationExecutor.defaultExpression(column, relation: rowRelation, resolver: resolver))
                }
                action = .insert(rowRelation, row, destinations.filter { provided[$0] != nil })
            }
            if clause.match != "not matched by target" { touched.formUnion(resolver.referencedColumns.filter { $0 < width }) }
            clauses.append(.init(match: clause.match, condition: predicate, action: action))
        }
        let returning = try MergeReturning(syntax.returning, relation: relation, source: sourceColumns, sourceOrder: sourceOrder,
            sourceWidth: source.columns.count, targetLabel: label, resolver: resolver)
        touched.formUnion(returning.touched)
        var physical = relation.physicalColumns(touched)
        // MERGE's touched mask is its authored target reads plus the view
        // filter. CHECK/default validation uses the ordinary demand backstop.
        if let view = relation.view {
            for filter in view.filters { physical.formUnion(view.physicalColumns(filter.expression, level: filter.level)) }
        }
        let probe = probeExpression(condition, relation: relation)
        let parameterTypes = try schemaOnly || context.parameterInference != nil ? [] : ParameterBinding.finalize(resolver)
        return MergePreparation(read: read, settings: settings, inputSettings: inputSettings, relation: relation,
            source: source, sourceColumns: sourceColumns, sourceWidth: source.columns.count, condition: condition,
            probe: probe, clauses: clauses, returning: returning, checks: checks, indexes: indexes,
            touched: physical.sorted(), parameterTypes: parameterTypes)
        } catch let error as JedError { throw error.inDatabase(inputSettings.targetDomain) }
    }
    static func shifted(_ columns: [ExpressionColumn], by offset: Int) -> [ExpressionColumn] {
        columns.map { ExpressionColumn(name: $0.name, qualifier: $0.qualifier, type: $0.type, index: $0.index + offset,
            collation: $0.collation, qualifiedOnly: $0.qualifiedOnly, unresolved: $0.unresolved,
            scalarFunction: $0.scalarFunction, hiddenFromStar: $0.hiddenFromStar, ambiguousRelation: $0.ambiguousRelation) }
    }
    static func ambiguous(_ columns: [ExpressionColumn], target: SQLText, source: [ExpressionColumn]) -> [ExpressionColumn] {
        guard source.contains(where: { $0.qualifier == target }) else { return columns }
        return columns.map { var column = $0; if column.qualifier == target { column.ambiguousRelation = true }; return column }
    }
    static func ownLabel(_ from: SQLFrom) -> SQLText? {
        guard from.joins.isEmpty else { return nil }
        if let alias = from.alias { return alias }
        switch from.source {
        case .table(let name): return name.name
        case .function(let expression): if case .call(let call) = expression.kind { return call.name }; return nil
        default: return nil
        }
    }
    static func boolean(_ syntax: SQLExpression, resolver: ExpressionResolver) throws -> ResolvedExpression {
        let value = try resolver.resolve(syntax, expected: TypeDescriptor(.scalar(.boolean)))
        guard value.type == nil || value.type?.type == .scalar(.boolean) else { throw JedError("42804", "MERGE condition requires a boolean expression") }
        return value
    }
    static func probeExpression(_ expression: ResolvedExpression, relation: MutationExecutor.Relation) -> ResolvedExpression {
        var result = expression
        switch expression.node {
        case .column(let column):
            if column >= relation.visibleCount { result.node = .outerColumn(OuterColumnReference(depth: 1, index: column - relation.visibleCount)) }
            else if let view = relation.view { return view.scanExpression(expression) }
        case .binary(let op, let a, let b, let type): result.node = .binary(op, probeExpression(a, relation: relation), probeExpression(b, relation: relation), type)
        case .convert(let value, let type): result.node = .convert(probeExpression(value, relation: relation), type)
        default: break
        }
        return result
    }
}
