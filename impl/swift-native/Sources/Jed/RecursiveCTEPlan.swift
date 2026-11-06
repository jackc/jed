/// A recursive binding has one immutable anchor and one term. The term's
/// reference is non-owning; the enclosing WITH owns their common binding.
internal struct RecursiveCTEPlan {
    let term: SelectPlan
    let all: Bool
    let augmentation: RecursiveColumns?
    var expressions: [ResolvedExpression] {
        term.expressions + (augmentation.map { $0.anchor + $0.recursive + ($0.filter.map { [$0] } ?? []) } ?? [])
    }

    struct Shape {
        let anchor: SQLQuery
        let term: SQLQuery
        let all: Bool
        let label: SQLText
    }
    static func unwrap(_ query: SQLQuery) -> SQLQuery {
        if case .nested(let inner) = query.body, query.orderBy.isEmpty, query.limit == nil, query.offset == nil { return unwrap(inner) }
        return query
    }
    static func analyze(_ query: SQLQuery, name: SQLText, augmented: Bool) throws -> Shape? {
        guard count(query, name: name) > 0 else { return nil }
        let query = unwrap(query)
        guard case .set("union", let all, let lhs, let rhs) = query.body else {
            throw JedError("42P19", "recursive query must have a non-recursive term UNION recursive term")
        }
        guard query.orderBy.isEmpty, query.limit == nil, query.offset == nil else {
            throw JedError("0A000", "ORDER BY, LIMIT and OFFSET are not supported in a recursive query")
        }
        guard count(lhs, name: name) == 0 else { throw JedError("42P19", "recursive reference must not appear in the non-recursive term") }
        let anchor = unwrap(lhs), term = unwrap(rhs)
        if augmented {
            if case .set = anchor.body { throw JedError("0A000", "SEARCH or CYCLE requires a SELECT anchor") }
            if case .set = term.body { throw JedError("42601", "SEARCH or CYCLE requires a SELECT recursive term") }
        }
        guard case .select(let select) = term.body else { throw JedError("0A000", "a compound recursive term is not supported") }
        guard (expressions(select) + term.orderBy.map(\.expression)).reduce(0, { $0 + count($1, name: name) }) == 0 else {
            throw JedError("42P19", "recursive reference must not appear within a subquery")
        }
        var direct: [(SQLText, Bool)] = []
        func relation(_ input: SQLFrom) throws -> [(SQLText, Bool)] {
            var found: [(SQLText, Bool)] = []
            switch input.source {
            case .table(let table):
                if table.database == nil && table.name == name { found.append((input.alias ?? name, false)) }
            case .joined(let inner): found = try relation(inner)
            case .query(let query):
                if count(query, name: name) > 0 { throw JedError("0A000", "recursive reference inside a derived table is not supported") }
            case .function(let expression):
                if count(expression, name: name) > 0 { throw JedError("0A000", "recursive reference inside a table function is not supported") }
            case .jsonTable(let table):
                if ([table.context, table.path] + table.passing.map(\.value)).contains(where: { count($0, name: name) > 0 }) {
                    throw JedError("0A000", "recursive reference inside JSON_TABLE is not supported")
                }
            }
            for join in input.joins {
                var right = try relation(join.right)
                if join.kind == "right" || join.kind == "full" { found = found.map { ($0.0, true) } }
                if join.kind == "left" || join.kind == "full" { right = right.map { ($0.0, true) } }
                found += right
            }
            return found
        }
        for input in select.from { direct += try relation(input) }
        guard direct.count == 1 else { throw JedError("42P19", "recursive reference must appear exactly once") }
        if (select.items.compactMap(\.expression) + (select.having.map { [$0] } ?? [])).contains(where: AggregateQueryPlan.containsAggregate) {
            throw JedError("42P19", "aggregate functions are not allowed in a recursive term")
        }
        guard !direct[0].1 else { throw JedError("42P19", "recursive reference must not appear on the nullable side of an outer join") }
        return Shape(anchor: anchor, term: term, all: all, label: direct[0].0)
    }
    static func expressions(_ select: SQLSelect) -> [SQLExpression] {
        func group(_ value: SQLGroup) -> [SQLExpression] {
            switch value { case .set(let values): values; case .rollup(let values), .cube(let values), .sets(let values): values.flatMap(group) }
        }
        func joins(_ input: SQLFrom) -> [SQLExpression] {
            let nested: [SQLExpression]
            if case .joined(let inner) = input.source { nested = joins(inner) } else { nested = [] }
            return nested + input.joins.flatMap { ($0.condition.map { [$0] } ?? []) + joins($0.right) }
        }
        return select.items.compactMap(\.expression) + [select.predicate, select.having].compactMap { $0 }
            + select.grouping.flatMap(group) + select.distinctOn + select.from.flatMap(joins)
            + select.windows.flatMap { windowExpressions($0.definition) }
    }
    static func windowExpressions(_ window: SQLWindow) -> [SQLExpression] {
        var result = window.partitionBy + window.orderBy.map(\.expression)
        if let frame = window.frame {
            if case .offset(let expression, _) = frame.start { result.append(expression) }
            if case .offset(let expression, _)? = frame.end { result.append(expression) }
        }
        return result
    }
    static func count(_ expression: SQLExpression, name: SQLText) -> Int {
        let nested: Int
        switch expression.kind {
        case .scalarQuery(let query), .exists(let query): nested = count(query, name: name)
        case .memberQuery(let child, let query, _), .quantifiedQuery(_, let child, _, let query):
            return count(child, name: name) + count(query, name: name)
        default: nested = 0
        }
        var children = CatalogExpressionPolicy.children(expression)
        if case .call(let call) = expression.kind, let over = call.over { children += windowExpressions(over) }
        return nested + children.reduce(0) { $0 + count($1, name: name) }
    }
    static func count(_ query: SQLQuery, name: SQLText) -> Int {
        func from(_ input: SQLFrom) -> Int {
            let own: Int
            switch input.source {
            case .table(let table): own = table.database == nil && table.name == name ? 1 : 0
            case .query(let query): own = count(query, name: name)
            case .function(let expression): own = count(expression, name: name)
            case .joined(let inner): own = from(inner)
            case .jsonTable(let table): own = ([table.context, table.path] + table.passing.map(\.value)).reduce(0) { $0 + count($1, name: name) }
            }
            return own + input.joins.reduce(0) { $0 + from($1.right) }
        }
        let own: Int
        switch query.body {
        case .select(let select): own = select.from.reduce(0) { $0 + from($1) } + expressions(select).reduce(0) { $0 + count($1, name: name) }
        case .values(let rows): own = rows.flatMap { $0 }.reduce(0) { $0 + count($1, name: name) }
        case .set(_, _, let a, let b): own = count(a, name: name) + count(b, name: name)
        case .nested(let query): own = count(query, name: name)
        case .with(let clause, let body):
            var visible = true, total = 0
            for binding in clause.bindings {
                if binding.name == name && clause.recursive { visible = false }
                if visible, let query = binding.body.queryBody { total += count(query, name: name) }
                if binding.name == name { visible = false }
            }
            own = total + (visible ? count(body, name: name) : 0)
        }
        return own + query.orderBy.reduce(0) { $0 + count($1.expression, name: name) }
    }
    static func checkColumns(anchor: SelectPlan, term: SelectPlan, all: Bool, carried: Int) throws {
        guard term.columns.count == anchor.columns.count + carried else { throw JedError("42601", "each UNION query must have the same number of columns") }
        for index in anchor.columns.indices {
            let a = anchor.projections[index], b = term.projections[index]
            let common = try TypeRules.common(a.type?.type, b.type?.type)
            guard common == a.type?.type else { throw JedError("42804", "recursive term widens its anchor column type") }
            if let type = a.type, type.decimal != nil || type.textLimit != nil || type.temporal != nil {
                guard type.decimal == b.type?.decimal, type.textLimit == b.type?.textLimit, type.temporal == b.type?.temporal else {
                    throw JedError("42804", "recursive term changes its anchor column type modifier")
                }
            }
            if !all { try TypeRules.comparable(common, common) }
            if ExpressionCollation.collatable(common) {
                let first = try ExpressionCollation.derive(a), combined = try ExpressionCollation.combine([a, b])
                if !all { _ = try combined.orderingName() }
                func identity(_ value: CollationDerivation) -> CollationDerivation {
                    value == .none ? .implicit(SQLText(validatedUTF8: [67])) : value.asColumn
                }
                guard identity(first) == identity(combined) else { throw JedError("42P21", "recursive term changes its anchor column collation") }
            }
        }
    }
}

/// SEARCH and CYCLE are ordinary resolved expression trees over a synthetic
/// row. Their existing operators and growth kernels own all evaluation charges.
internal struct RecursiveColumns {
    let names: [SQLText]
    let anchor: [ResolvedExpression]
    let recursive: [ResolvedExpression]
    let filter: ResolvedExpression?
    static let span = SQLSourceSpan(byteStart: 0, byteEnd: 0, position: 1)
    static func expression(_ kind: SQLExpression.Kind) -> SQLExpression { SQLExpression(kind: kind, span: span, position: 1) }
    static func text(_ value: String) -> SQLText { SQLText(validatedUTF8: Array(value.utf8)) }
    static func column(_ index: Int) -> SQLExpression { expression(.column(qualifier: nil, name: text("*recursive\(index)*"))) }
    static func integer(_ value: Int) -> SQLExpression { expression(.number(String(value))) }
    static func binary(_ op: String, _ a: SQLExpression, _ b: SQLExpression) -> SQLExpression { expression(.binary(op, a, b)) }
    struct Marks { let value: SQLExpression; let fallback: SQLExpression; let type: TypeDescriptor; let fallbackType: TypeDescriptor? }
    static func marks(_ syntax: SQLCycle?, resolver: ExpressionResolver) throws -> Marks? {
        guard let syntax else { return nil }
        var value = syntax.markValue ?? expression(.boolean(true)), fallback = syntax.defaultValue ?? expression(.boolean(false))
        let a = try resolver.resolve(value), b = try resolver.resolve(fallback)
        func adopt(_ syntax: SQLExpression, other: TypeDescriptor?) -> SQLExpression {
            if case .string(let text) = syntax.kind, let other, case .scalar(let scalar) = other.type, scalar != .text {
                return expression(.typedString(SQLTypeSyntax(name: Self.text(scalar.name), span: span), text))
            }
            return syntax
        }
        value = adopt(value, other: b.type); fallback = adopt(fallback, other: a.type)
        let left = try resolver.resolve(value), right = try resolver.resolve(fallback)
        let type = TypeDescriptor(try TypeRules.common(left.type?.type, right.type?.type) ?? .scalar(.text))
        try TypeRules.comparable(type.type, type.type)
        return Marks(value: value, fallback: fallback, type: type, fallbackType: right.type)
    }
    static func resolve(_ syntax: SQLCTE, columns: [ResultColumn], marks: Marks?, resolver: ExpressionResolver) throws -> Self? {
        guard syntax.search != nil || syntax.cycle != nil else { return nil }
        func indices(_ names: [SQLText]) throws -> [Int] {
            var seen: Set<SQLText> = []
            return try names.map { name in
                guard let index = columns.firstIndex(where: { $0.name == name }) else { throw JedError("42601", "SEARCH or CYCLE column is not in the WITH output") }
                guard seen.insert(name).inserted else { throw JedError("42701", "SEARCH or CYCLE column is repeated") }
                return index
            }
        }
        let searchIndices = try syntax.search.map { try indices($0.columns) }
        let cycleIndices = try syntax.cycle.map { try indices($0.columns) }
        let names = (syntax.search.map { [$0.output] } ?? []) + (syntax.cycle.map { [$0.mark, $0.path] } ?? [])
        guard Set(names).count == names.count, !names.contains(where: { name in columns.contains { $0.name == name } }) else {
            throw JedError("42601", "SEARCH and CYCLE output names must be new and distinct")
        }
        let old = resolver.columns
        defer { resolver.columns = old }
        resolver.columns = columns.enumerated().map { ExpressionColumn(name: text("*recursive\($0.offset)*"), qualifier: nil,
            type: $0.element.type ?? TypeDescriptor(.scalar(.text)), index: $0.offset) }
        func record(_ indices: [Int]) -> SQLExpression { expression(.row(indices.map(column))) }
        func path(_ indices: [Int]) -> SQLExpression { expression(.array([record(indices)])) }
        var initial: [SQLExpression] = [], next: [SQLExpression] = [], predicate: SQLExpression?
        if let search = syntax.search, let indices = searchIndices {
            let parent = column(columns.count + initial.count)
            if search.depthFirst {
                initial.append(path(indices)); next.append(binary("||", parent, path(indices)))
            } else {
                initial.append(expression(.row([integer(0)] + indices.map(column))))
                let depth = binary("+", expression(.field(parent, text("f1"))), integer(1))
                next.append(expression(.row([depth] + indices.map(column))))
            }
        }
        if let indices = cycleIndices, let marks {
            let mark = column(columns.count + initial.count), parent = column(columns.count + initial.count + 1)
            var fallback = marks.fallback
            if let source = marks.fallbackType, source.type != marks.type.type {
                fallback = expression(.cast(fallback, SQLTypeSyntax(name: text(marks.type.type.name), span: span)))
            }
            initial += [fallback, path(indices)]
            let found = expression(.quantified("=", record(indices), all: false, parent))
            next.append(expression(.conditional(operand: nil, arms: [SQLCaseArm(condition: found, result: marks.value)], fallback: marks.fallback)))
            next.append(binary("||", parent, path(indices)))
            predicate = binary("<>", mark, marks.value)
        }
        let markIndex = syntax.search == nil ? 0 : 1
        let first = try initial.enumerated().map { index, expression in
            try resolver.resolve(expression, expected: marks != nil && index == markIndex ? marks?.type : nil)
        }
        for (index, output) in first.enumerated() {
            resolver.columns.append(ExpressionColumn(name: text("*recursive\(columns.count + index)*"), qualifier: nil,
                type: output.type ?? marks?.type ?? TypeDescriptor(.scalar(.text)), index: columns.count + index))
        }
        return RecursiveColumns(names: names, anchor: first, recursive: try next.map { try resolver.resolve($0) },
                                filter: try predicate.map { try resolver.resolve($0) })
    }
}
