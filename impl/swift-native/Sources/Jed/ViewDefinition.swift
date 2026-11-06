/// Persisted views contain canonical SQL and names, never a cached typed plan.
/// Replanning deliberately observes the current catalog and host registrations.
internal enum ViewDefinition {
    struct Prepared {
        let definition: SQLText
        let columns: [ResultColumn]
        let dependencies: [CatalogView.Dependency]
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
                        settings: SessionSettings, context: ExpressionContext) throws -> Prepared {
        var scalar = context.scalar; scalar.meter = nil
        // Structural exclusions precede resolution; a known type for $1 must
        // never make a persisted view capture a statement binding.
        try ViewSyntax.walk(query, expression: { expression in
            if case .parameter(let number) = expression.kind { throw JedError("42P02", "there is no parameter $\(number)") }
        })
        let plan = try SelectPlan.resolve(query, snapshot: snapshot, settings: settings, scalar: scalar)
        var dependencies: [CatalogView.Dependency] = []
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
            _ = try CatalogSQL.typeUse(type); addType(type.type)
        }
        var edits: [(SQLSourceSpan, [UInt8])] = []
        try ViewSyntax.walk(query, query: { block in
            guard case .select(let select) = block.body else { return }
            let blockPlan = try SelectPlan.resolve(block, snapshot: snapshot, settings: settings, scalar: scalar)
            var scope: [(SQLText?, SQLText)] = []
            for from in select.from {
                switch from.source {
                case .table(let name):
                    guard name.database == nil || name.database?.description == "main" else {
                        throw JedError("0A000", "views cannot reference attached databases")
                    }
                    let names: [SQLText]
                    if let table = snapshot.catalog.tables.first(where: { $0.name == name.name }) {
                        names = table.columns.map(\.name)
                        // The current relation planner supplies physical ordinals
                        // for this single base source, including filter/order uses.
                        let used = blockPlan.touched.filter { names.indices.contains($0) }.map { names[$0] }
                        add(0, name.name, used)
                    } else if let view = snapshot.catalog.views.first(where: { $0.name == name.name }) {
                        names = view.columns.map(\.name); add(1, name.name)
                    } else { throw JedError("42P01", "relation does not exist: \(name.name)") }
                    for index in names.indices {
                        scope.append((from.alias ?? name.name, index < from.columnAliases.count ? from.columnAliases[index] : names[index]))
                    }
                case .query(let child):
                    let childPlan = try SelectPlan.resolve(child, snapshot: snapshot, settings: settings, scalar: scalar)
                    for index in childPlan.columns.indices {
                        scope.append((from.alias, index < from.columnAliases.count ? from.columnAliases[index] : childPlan.columns[index].name))
                    }
                default: throw JedError("0A000", "view source dependency tracing is not yet connected")
                }
            }
            for item in select.items where item.star {
                let selected = scope.filter { item.qualifier == nil || $0.0 == item.qualifier }
                var bytes: [UInt8] = []
                for (label, name) in selected {
                    if !bytes.isEmpty { bytes += [32, 44, 32] }
                    if let label { bytes += quoted(label) + [32, 46, 32] }
                    bytes += quoted(name)
                }
                edits.append((item.span, bytes))
            }
        }, expression: { expression in
            switch expression.kind {
            case .call(let call) where ["nextval", "currval", "setval"].contains(call.name.description):
                if let argument = call.arguments.first, case .string(let text) = argument.value.kind {
                    let name = try SequenceName.parse(text)
                    guard snapshot.catalog.sequences.contains(where: { $0.name == name }) else {
                        throw JedError("42P01", "relation does not exist: \(name)")
                    }
                    add(2, name)
                }
            case .cast(_, let type), .typedString(let type, _):
                if let composite = snapshot.types[type.name] { add(3, composite.name) }
            default: break
            }
        })
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
        guard case .query(let frozen) = try SQLParser(utf8: text.utf8).parse() else { throw JedError("XX000", "canonical view is not a query") }
        let verified = try SelectPlan.resolve(frozen, snapshot: snapshot, settings: settings, scalar: scalar)
        guard verified.columns.count == plan.columns.count,
              zip(verified.columns, plan.columns).allSatisfy({ $0.name == $1.name && $0.type == $1.type }) else {
            throw JedError("0A000", "wildcard expansion changes the view definition")
        }
        dependencies.sort { $0.name == $1.name ? $0.kind < $1.kind : $0.name < $1.name }
        let columns = plan.columns.map { ResultColumn(name: $0.name, type: $0.type ?? TypeDescriptor(.scalar(.text))) }
        return Prepared(definition: text, columns: columns, dependencies: dependencies)
    }
}

/// A syntax walk visits isolated query blocks without treating their columns as
/// references in the surrounding block. Parser admission bounds this recursion.
internal enum ViewSyntax {
    static func walk(_ input: SQLQuery, query visitQuery: (SQLQuery) throws -> Void = { _ in },
                     expression visitExpression: (SQLExpression) throws -> Void = { _ in }) throws {
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
            switch value.source {
            case .query(let value): try query(value)
            case .function(let value): try expression(value)
            case .joined(let value): try from(value)
            case .jsonTable(let value):
                try expression(value.context); try expression(value.path)
                for item in value.passing { try expression(item.value) }
            case .table: break
            }
            for join in value.joins { try from(join.right); if let condition = join.condition { try expression(condition) } }
        }
        func query(_ value: SQLQuery) throws {
            try visitQuery(value)
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
                for binding in clause.bindings {
                    guard case .query(let value) = binding.body else { throw JedError("0A000", "views cannot contain a data-modifying WITH statement") }
                    try query(value)
                }
                try query(body)
            }
            for term in value.orderBy { try expression(term.expression) }
        }
        try query(input)
    }
}
