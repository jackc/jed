/// Persisted views contain canonical SQL and names, never a cached typed plan.
/// Replanning deliberately observes the current catalog and host registrations.
internal enum ViewDefinition {
    struct Prepared {
        let definition: SQLText
        let columns: [ResultColumn]
        let dependencies: [CatalogView.Dependency]
        let collations: [CollationDerivation]
        let viewUses: [SQLText: Set<Int>]
    }
    static func query(_ view: CatalogView) throws -> SQLQuery {
        try parsed(view).query
    }
    static func parsed(_ view: CatalogView) throws -> (query: SQLQuery, depth: Int) {
        do {
            let parser = try SQLParser(utf8: view.definition.utf8)
            let query = try parser.nestedQuery()
            guard parser.peek().kind == .end else { throw JedError("XX003", "stored view definition is not a query") }
            return (query, parser.maximumDepth)
        } catch let error as JedError where error.code == "54001" { throw error }
        catch { throw JedError("XX003", "stored view definition cannot be parsed: \(view.name)") }
    }
    static func quoted(_ name: SQLText) -> [UInt8] {
        [34] + name.utf8.flatMap { byte -> [UInt8] in byte == 34 ? [34, 34] : [byte] } + [34]
    }
    static func prepare(_ query: SQLQuery, source: SQLText, snapshot: DomainSnapshot,
                        settings: SessionSettings, context: ExpressionContext,
                        validateOutput: ([ResultColumn]) throws -> Void = { _ in }) throws -> Prepared {
        var scalar = context.scalar; scalar.meter = nil
        var planningSettings = settings
        planningSettings.privileges = settings.privileges.withTableAccess()
        let domains = QueryDomains.statement(snapshot: snapshot, settings: settings)
        planningSettings.domains = domains
        // Structural exclusions precede resolution; a known type for $1 must
        // never make a persisted view capture a statement binding.
        try ViewSyntax.walk(query, expression: { expression in
            if case .parameter(let number) = expression.kind { throw JedError("42P02", "there is no parameter $\(number)") }
        })
        let plan = try SelectPlan.resolve(query, snapshot: snapshot, settings: planningSettings, scalar: scalar)
        var dependencies: [CatalogView.Dependency] = []
        var viewUses: [SQLText: Set<Int>] = [:]
        func add(_ kind: UInt8, _ name: SQLText, _ columns: [SQLText] = []) {
            if let index = dependencies.firstIndex(where: { $0.kind == kind && $0.name == name }) {
                dependencies[index].columns = Array(Set(dependencies[index].columns + columns)).sorted()
            } else { dependencies.append(.init(kind: kind, name: name, columns: columns.sorted())) }
        }
        func addType(_ type: SQLType) {
            switch type {
            case .composite(let type): add(3, type.name)
            case .array(let element): addType(element)
            default: break
            }
        }
        for column in plan.columns {
            let type = column.type ?? TypeDescriptor(.scalar(.text))
            addType(type.type)
        }
        var edits: [(SQLSourceSpan, [UInt8])] = []
        struct Binding {
            var label: SQLText?
            var name: SQLText
            var type: TypeDescriptor
            var collation: CollationDerivation?
            var unresolved = false
            var table: SQLText?
            var column: SQLText?
            var view: SQLText?
            var ordinal: Int?
            var starVisible = true
            var qualifiedOnly = false
            var mergedName = false
        }
        struct QueryIdentity: Hashable {
            let start: Int
            let end: Int
            init(_ query: SQLQuery) { start = query.span.byteStart; end = query.span.byteEnd }
        }
        // FROM subqueries inherit exactly their source's lexical scopes. A
        // LATERAL item sees preceding bindings even inside an aliased fence;
        // it cannot use labels introduced later by the surrounding group.
        var inheritedScopes: [[Binding]] = []
        var scopeStack: [[[Binding]]] = []
        var sourceScopes: [QueryIdentity: [[Binding]]] = [:]
        var lexical: [SQLText: CTEBinding] = [:]
        var lexicalStack: [[SQLText: CTEBinding]] = []
        struct WithFrame {
            let inherited: [SQLText: CTEBinding]
            let bindings: [CTEBinding]
        }
        var withFrames: [WithFrame] = []
        func expressionScope(_ bindings: [Binding]) -> [ExpressionColumn] {
            bindings.enumerated().map { index, column in
                ExpressionColumn(name: column.name, qualifier: column.label, type: column.type, index: index,
                    collation: column.collation, qualifiedOnly: column.qualifiedOnly, unresolved: column.unresolved)
            }
        }
        try ViewSyntax.walk(query, query: { block in
            lexicalStack.append(lexical)
            scopeStack.append(inheritedScopes)
            let outers = sourceScopes[QueryIdentity(block)] ?? inheritedScopes
            inheritedScopes = outers
            let blockResolver = ExpressionResolver(composites: snapshot.types, scalar: scalar)
            blockResolver.outerScopes = outers.map(expressionScope)
            blockResolver.cteBindings = lexical
            if case .with = block.body {
                let scoped = try SelectPlan.resolve(block, snapshot: snapshot, settings: planningSettings, scalar: scalar, using: blockResolver)
                guard let withQuery = scoped.withQuery else { throw JedError("XX000", "view WITH plan has no bindings") }
                withFrames.append(WithFrame(inherited: lexical, bindings: withQuery.bindings))
                return
            }
            guard case .select(let select) = block.body else { return }
            let blockPlan = try SelectPlan.resolve(block, snapshot: snapshot, settings: planningSettings, scalar: scalar, using: blockResolver)
            var joins: [(SQLExpression, [Binding])] = []
            var implicitJoinUses: [Binding] = []
            var sourceUses: [Binding] = []
            // FROM producers resolve against preceding siblings, just as the
            // ordinary relation planner does. Embedded queries must retain
            // that frame when the later syntax walk traces their dependencies.
            func sourceResolver(_ expressions: [SQLExpression], preceding: [Binding]) -> ExpressionResolver {
                let nested = ExpressionResolver(columns: expressionScope(preceding), composites: snapshot.types, scalar: scalar)
                nested.queryScope = SubqueryScope(snapshot: snapshot, settings: planningSettings)
                nested.outerScopes = outers.map(expressionScope); nested.cteBindings = lexical
                nested.hostFunctions = planningSettings.environment.signatures
                nested.unicode = planningSettings.environment.unicode
                nested.checkFunction = { try planningSettings.privileges.require(.execute, on: $0, function: true) }
                nested.resolveAggregate = AggregateQueryPlan.forbidden
                var pending = expressions
                while let expression = pending.popLast() {
                    switch expression.kind {
                    case .scalarQuery(let child), .exists(let child):
                        sourceScopes[QueryIdentity(child)] = [preceding] + outers
                    case .memberQuery(let left, let child, _), .quantifiedQuery(_, let left, _, let child):
                        sourceScopes[QueryIdentity(child)] = [preceding] + outers
                        pending.append(left)
                    default: pending += CatalogExpressionPolicy.children(expression)
                    }
                }
                return nested
            }
            func sourceColumns(_ columns: [ExpressionColumn], resolver: ExpressionResolver,
                               preceding: [Binding]) -> [Binding] {
                for index in resolver.referencedColumns.sorted() where preceding.indices.contains(index) {
                    sourceUses.append(preceding[index])
                }
                for reference in resolver.outerReferences where reference.depth > 0 && reference.depth <= outers.count {
                    let bindings = outers[reference.depth - 1]
                    if bindings.indices.contains(reference.index) { sourceUses.append(bindings[reference.index]) }
                }
                return columns.map { column in
                    addType(column.type.type)
                    return Binding(label: column.qualifier, name: column.name, type: column.type,
                        collation: column.collation, unresolved: column.unresolved,
                        starVisible: !column.hiddenFromStar, qualifiedOnly: column.qualifiedOnly)
                }
            }
            func bind(_ from: SQLFrom, preceding: [Binding]) throws -> [Binding] {
                var columns: [Binding]
                switch from.source {
                case .table(let name):
                    guard name.database == nil || name.database == DomainNames.main || name.database == DomainNames.temporary else {
                        throw JedError("0A000", "views cannot reference attached databases")
                    }
                    if name.database == nil, let binding = lexical[name.name] {
                        columns = binding.columns.enumerated().map { index, column in
                            Binding(label: name.name, name: column.name, type: column.type ?? TypeDescriptor(.scalar(.text)),
                                collation: binding.collations[index]?.asColumn, unresolved: column.type == nil,
                                starVisible: binding.hiddenColumns.map { index < $0 } ?? true)
                        }
                    } else if let system = try SystemRelationPlan.resolve(name, alias: nil, snapshot: snapshot, settings: planningSettings) {
                        // System relations have fixed engine-owned columns and
                        // no persisted catalog object to pin as a dependency.
                        columns = system.columns.map { Binding(label: name.name, name: $0.name, type: $0.type) }
                    } else {
                        let owner = try domains.relation(name)
                        if let table = owner.snapshot.catalog.tables.first(where: { $0.name == name.name }) {
                            add(0, name.name)
                            let types = try owner.snapshot.tableTypes(table)
                            columns = table.columns.enumerated().map { index, column in
                                Binding(label: name.name, name: column.name, type: types[index], table: name.name, column: column.name)
                            }
                        } else if let view = owner.snapshot.catalog.views.first(where: { $0.name == name.name }) {
                            add(1, name.name)
                            var inner = planningSettings
                            inner.domains = try domains.viewScope(owner: owner.name); inner.targetDomain = owner.name
                            let body = try SelectPlan.resolve(ViewDefinition.query(view), snapshot: owner.snapshot, settings: inner, scalar: scalar)
                            columns = view.columns.enumerated().map { index, column in
                                Binding(label: name.name, name: column.name, type: body.columns[index].type ?? TypeDescriptor(.scalar(.text)),
                                    collation: body.projections[index].collation?.asColumn, unresolved: body.columns[index].type == nil,
                                    view: name.name, ordinal: index)
                            }
                        } else { throw JedError("42P01", "relation does not exist: \(name.name)") }
                    }
                case .query(let child):
                    let nested = ExpressionResolver(composites: snapshot.types, scalar: scalar)
                    let childScopes = from.lateral ? [preceding] + outers : outers
                    sourceScopes[QueryIdentity(child)] = childScopes
                    nested.cteBindings = lexical; nested.outerScopes = childScopes.map(expressionScope)
                    let childPlan = try SelectPlan.resolve(child, snapshot: snapshot, settings: planningSettings, scalar: scalar, using: nested)
                    columns = childPlan.columns.enumerated().map { index, column in
                        Binding(label: nil, name: column.name, type: column.type ?? TypeDescriptor(.scalar(.text)),
                            collation: childPlan.projections[index].collation?.asColumn, unresolved: column.type == nil)
                    }
                case .joined(let nested): columns = try bind(nested, preceding: preceding)
                case .function(let expression):
                    let nested = sourceResolver([expression], preceding: preceding)
                    let function = try TableFunctionPlan.resolve(from, syntax: expression, resolver: nested)
                    columns = sourceColumns(function.columns, resolver: nested, preceding: preceding)
                case .jsonTable(let syntax):
                    let nested = sourceResolver(ViewSyntax.tableExpressions(syntax), preceding: preceding)
                    let table = try JSONTablePlan.resolve(from, syntax: syntax, resolver: nested)
                    columns = sourceColumns(table.columns, resolver: nested, preceding: preceding)
                }
                for index in columns.indices {
                    if let alias = from.alias { columns[index].label = alias; columns[index].mergedName = false }
                    if index < from.columnAliases.count { columns[index].name = from.columnAliases[index] }
                }
                for join in from.joins {
                    var right = try bind(join.right, preceding: preceding + columns)
                    let names = join.natural ? columns.filter { !$0.qualifiedOnly && $0.starVisible }.map(\.name).filter { name in
                        right.contains { !$0.qualifiedOnly && $0.starVisible && $0.name == name }
                    } : join.using
                    for name in names {
                        implicitJoinUses += columns.filter { !$0.qualifiedOnly && $0.name == name }
                        implicitJoinUses += right.filter { !$0.qualifiedOnly && $0.name == name }
                        if join.kind == "right" {
                            if let position = right.firstIndex(where: { !$0.qualifiedOnly && $0.name == name }) { right[position].mergedName = true }
                        } else if let position = columns.firstIndex(where: { !$0.qualifiedOnly && $0.name == name }) { columns[position].mergedName = true }
                    }
                    columns += right
                    if let predicate = join.condition { joins.append((predicate, columns)) }
                }
                return columns
            }
            var scope: [Binding] = []
            for from in select.from { scope += try bind(from, preceding: scope) }
            // Row slots retain source order, while USING/NATURAL output metadata
            // carries merged-first star order and qualifier-only hidden copies.
            // Use the ordinary planner's mapping instead of a second join rule.
            if let relation = blockPlan.relation {
                for column in relation.columns where scope.indices.contains(column.index) {
                    scope[column.index].label = column.qualifier; scope[column.index].name = column.name
                    scope[column.index].type = column.type; scope[column.index].collation = column.collation
                    scope[column.index].qualifiedOnly = column.qualifiedOnly
                    scope[column.index].starVisible = !column.hiddenFromStar
                }
            }
            func use(_ column: Binding) {
                if let table = column.table, let name = column.column { add(0, table, [name]) }
                if let view = column.view, let ordinal = column.ordinal { viewUses[view, default: []].insert(ordinal) }
            }
            for ordinal in blockPlan.touched where scope.indices.contains(ordinal) { use(scope[ordinal]) }
            for column in implicitJoinUses + sourceUses { use(column) }
            for reference in blockResolver.outerReferences where reference.depth > 0 && reference.depth <= outers.count {
                let bindings = outers[reference.depth - 1]
                if bindings.indices.contains(reference.index) { use(bindings[reference.index]) }
            }
            // Join predicates resolve before the SELECT expression scope. They
            // must still pin their input columns, including nested join labels.
            for (predicate, columns) in joins {
                var pending = [predicate]
                while let value = pending.popLast() {
                    if case .column(let label, let name) = value.kind {
                        for column in columns where column.name == name && (label == nil || column.label == label) { use(column) }
                    }
                    pending += CatalogExpressionPolicy.children(value)
                }
            }
            for item in select.items where item.star {
                let selected: [Binding]
                if let relation = blockPlan.relation {
                    var columns = relation.columns.filter { !$0.hiddenFromStar && (item.qualifier == nil ? !$0.qualifiedOnly : $0.qualifier == item.qualifier) }
                    if item.qualifier != nil { columns.sort { $0.index < $1.index } }
                    selected = columns.compactMap { column in
                        guard scope.indices.contains(column.index) else { return nil }
                        var binding = scope[column.index]
                        // A USING merge and its qualified source copy share a
                        // physical slot. Render the selected output metadata,
                        // not whichever copy last populated that slot.
                        binding.label = item.qualifier == nil && binding.mergedName ? nil : column.qualifier
                        binding.name = column.name
                        return binding
                    }
                } else { selected = scope.filter { $0.starVisible && (item.qualifier == nil ? !$0.qualifiedOnly : $0.label == item.qualifier) } }
                var bytes: [UInt8] = []
                for column in selected {
                    if !bytes.isEmpty { bytes += [32, 44, 32] }
                    if let label = column.label { bytes += quoted(label) + [32, 46, 32] }
                    bytes += quoted(column.name)
                }
                edits.append((item.span, bytes))
            }
            inheritedScopes = [scope] + outers
        }, leaveQuery: { block in
            inheritedScopes = scopeStack.removeLast()
            if case .with = block.body { _ = withFrames.popLast() }
            lexical = lexicalStack.removeLast()
        }, withPart: { clause, ordinal in
            guard let frame = withFrames.last else { throw JedError("XX000", "missing view WITH scope") }
            lexical = frame.inherited
            let preceding = ordinal ?? frame.bindings.count
            for binding in frame.bindings.prefix(preceding) { lexical[binding.name] = binding }
            if let ordinal, clause.recursive, frame.bindings[ordinal].recursive != nil {
                let binding = frame.bindings[ordinal]
                guard let anchor = binding.plan else { throw JedError("XX000", "recursive view binding has no anchor plan") }
                // The traced recursive body's self-reference sees named SEARCH
                // and CYCLE outputs, but its wildcard keeps the anchor arity.
                let recursive = CTEBinding(name: binding.name, columns: binding.columns, plan: anchor,
                    settings: binding.settings, materialized: binding.materialized)
                recursive.collations = binding.collations
                recursive.hiddenColumns = anchor.columns.count; recursive.resolvingSelf = true
                lexical[binding.name] = recursive
            }
        }, expression: { expression in
            switch expression.kind {
            case .call(let call) where ["nextval", "currval", "setval"].contains(call.name.description):
                if let argument = call.arguments.first, case .string(let text) = argument.value.kind {
                    let name = try SequenceName.parse(text)
                    guard domains.snapshots[DomainNames.temporary]?.catalog.sequences.contains(where: { $0.name == name }) == true || domains.snapshots[DomainNames.main]?.catalog.sequences.contains(where: { $0.name == name }) == true else {
                        throw JedError("42P01", "relation does not exist: \(name)")
                    }
                    add(2, name)
                }
            case .cast(_, let type), .typedString(let type, _):
                if let composite = snapshot.types[type.name] { add(3, composite.name) }
            default: break
            }
        })
        let columns = plan.columns.map { ResultColumn(name: $0.name, type: $0.type ?? TypeDescriptor(.scalar(.text))) }
        try validateOutput(columns)
        let span = query.span
        guard span.byteStart >= 0, span.byteEnd <= source.utf8.count else { throw JedError("XX000", "view source span is outside the statement") }
        var bytes: [UInt8] = [], cursor = span.byteStart
        for (range, replacement) in edits.sorted(by: { $0.0.byteStart < $1.0.byteStart }) {
            guard range.byteStart >= cursor, range.byteEnd <= span.byteEnd else { throw JedError("XX000", "overlapping view star substitutions") }
            bytes += source.utf8[cursor..<range.byteStart]; bytes += replacement; cursor = range.byteEnd
        }
        bytes += source.utf8[cursor..<span.byteEnd]
        let text = try CatalogSQL.render(bytes)
        // Freezing is checked by the same planner; alias capture must not change
        // the query's output contract when wildcard syntax is removed.
        guard let frozen = try SQLParser(utf8: text.utf8).parse().queryBody else { throw JedError("XX000", "canonical view is not a query") }
        let verified: SelectPlan
        do { verified = try SelectPlan.resolve(frozen, snapshot: snapshot, settings: planningSettings, scalar: scalar) }
        catch { throw JedError("0A000", "wildcard expansion changes the view definition") }
        guard verified.columns.count == plan.columns.count else { throw JedError("0A000", "wildcard expansion changes the view definition") }
        for index in plan.columns.indices {
            var oldType = plan.columns[index].type, newType = verified.columns[index].type
            oldType?.collation = nil; newType?.collation = nil
            guard plan.columns[index].name == verified.columns[index].name, oldType == newType,
                  try outputCollation(plan.projections[index], image: snapshot.catalog) == outputCollation(verified.projections[index], image: snapshot.catalog) else {
                throw JedError("0A000", "wildcard expansion changes the view definition")
            }
        }
        // Creator capabilities are checked only after names/types and wildcard
        // freezing, preserving CREATE VIEW's specified validation order.
        _ = try SelectPlan.resolve(frozen, snapshot: snapshot, settings: settings, scalar: scalar)
        dependencies.sort { $0.name == $1.name ? $0.kind < $1.kind : $0.name < $1.name }
        return Prepared(definition: text, columns: columns, dependencies: dependencies,
                        collations: try plan.projections.map { try outputCollation($0, image: snapshot.catalog) }, viewUses: viewUses)
    }
    static func outputCollation(_ expression: ResolvedExpression, image: CatalogImage) throws -> CollationDerivation {
        guard ExpressionCollation.collatable(expression.type?.type ?? .scalar(.text)) else { return .none }
        switch try ExpressionCollation.derive(expression) {
        case .none: return .implicit(image.collations.first(where: \.isDefault)?.name ?? SQLText(validatedUTF8: [67]))
        case .implicit(let name), .explicit(let name): return .implicit(name)
        case .indeterminate, .conflicted: return .conflicted
        }
    }
}

/// A syntax walk visits isolated query blocks without treating their columns as
/// references in the surrounding block. Parser admission bounds this recursion.
internal enum ViewSyntax {
    /// Include column defaults as well as context/path/PASSING expressions:
    /// each can contain a stored dependency or a forbidden view parameter.
    static func tableExpressions(_ table: SQLJSONTable) -> [SQLExpression] {
        var result = [table.context, table.path] + table.passing.map(\.value)
        var pending = Array(table.columns.reversed())
        while let column = pending.popLast() {
            switch column {
            case .ordinality: break
            case .regular(_, _, _, _, _, _, _, let empty, let error):
                result += [empty?.value, error?.value].compactMap { $0 }
            case .nested(_, _, let children): pending += children.reversed()
            }
        }
        if let value = table.onError?.value { result.append(value) }
        return result
    }
    static func walk(_ input: SQLQuery, query visitQuery: (SQLQuery) throws -> Void = { _ in },
                     leaveQuery: (SQLQuery) -> Void = { _ in }, withPart: (SQLWith, Int?) throws -> Void = { _, _ in },
                     expression visitExpression: (SQLExpression) throws -> Void = { _ in },
                     from visitFrom: (SQLFrom, Set<SQLText>) throws -> Void = { _, _ in }) throws {
        var ctes: Set<SQLText> = []
        func expression(_ value: SQLExpression) throws {
            try visitExpression(value)
            switch value.kind {
            case .scalarQuery(let value), .exists(let value): try query(value)
            case .memberQuery(let left, let value, _), .quantifiedQuery(_, let left, _, let value):
                try expression(left); try query(value)
            case .call(let call):
                if let window = call.over { try visitWindow(window) }
            default: break
            }
            for child in CatalogExpressionPolicy.children(value) { try expression(child) }
        }
        func visitWindow(_ window: SQLWindow) throws {
            for value in window.partitionBy { try expression(value) }
            for term in window.orderBy { try expression(term.expression) }
            if let frame = window.frame {
                for bound in [frame.start, frame.end].compactMap({ $0 }) {
                    if case .offset(let value, _) = bound { try expression(value) }
                }
            }
        }
        func group(_ value: SQLGroup) throws {
            switch value {
            case .set(let values): for value in values { try expression(value) }
            case .rollup(let values), .cube(let values), .sets(let values): for value in values { try group(value) }
            }
        }
        func from(_ value: SQLFrom) throws {
            try visitFrom(value, ctes)
            switch value.source {
            case .query(let value): try query(value)
            case .function(let value): try expression(value)
            case .joined(let value): try from(value)
            case .jsonTable(let value):
                for value in tableExpressions(value) { try expression(value) }
            case .table: break
            }
            for join in value.joins { try from(join.right); if let condition = join.condition { try expression(condition) } }
        }
        func query(_ value: SQLQuery) throws {
            let enclosingCTEs = ctes
            defer { ctes = enclosingCTEs }
            try visitQuery(value)
            defer { leaveQuery(value) }
            switch value.body {
            case .select(let select):
                for item in select.items { if let value = item.expression { try expression(value) } }
                for value in select.distinctOn { try expression(value) }
                for value in select.from { try from(value) }
                if let value = select.predicate { try expression(value) }
                for value in select.grouping { try group(value) }
                if let value = select.having { try expression(value) }
                for value in select.windows { try visitWindow(value.definition) }
            case .values(let rows): for row in rows { for value in row { try expression(value) } }
            case .set(_, _, let left, let right): try query(left); try query(right)
            case .nested(let value): try query(value)
            case .with(let clause, let body):
                if clause.recursive { ctes.formUnion(clause.bindings.map(\.name)) }
                for (ordinal, binding) in clause.bindings.enumerated() {
                    guard case .query(let value) = binding.body else { throw JedError("0A000", "views cannot contain a data-modifying WITH statement") }
                    try withPart(clause, ordinal)
                    try query(value)
                    ctes.insert(binding.name)
                }
                try withPart(clause, nil)
                try query(body)
            }
            for term in value.orderBy { try expression(term.expression) }
        }
        try query(input)
    }
}
