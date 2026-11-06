/// Handwritten recursive descent over validated UTF-8 tokens. Logical nesting
/// bounds recursion and left-deep trees before resolution can walk them.
internal final class SQLParser {
    let tokens: [SQLToken]
    let sourceUTF8: [UInt8]
    let recordPositions: Bool
    var cursor = 0
    var furthest = 0
    var depth = 0
    private(set) var maximumDepth = 0
    var parenthesizedQuery: [Bool] = []
    var intoAllowed = false
    var intoTop = false
    var intoView = false
    var intoNesting = 0
    var bareRowEnd = -1
    static let maximumNesting = 256
    static let maximumInElements = 1000
    static let intervalQualifiers: Set<String> = {
        guard case .array(let rows) = GeneratedSpec.datetimeUnits["interval_qualifier"] else { return [] }
        return Set(rows.compactMap {
            guard case .table(let row) = $0, case .text(let name) = row["name"] else { return nil }
            return name
        })
    }()

    convenience init(_ sql: String, recordPositions: Bool = true) throws {
        try self.init(utf8: Array(sql.utf8), recordPositions: recordPositions)
    }
    init(utf8: [UInt8], recordPositions: Bool = true, tokenPositions: [Int]? = nil) throws {
        sourceUTF8 = utf8
        let scanned = try SQLLexer(utf8: utf8).tokenize()
        if let tokenPositions {
            guard tokenPositions.count == scanned.count else { throw JedError("XX000", "token position map has the wrong size") }
            tokens = zip(scanned, tokenPositions).map { token, position in
                SQLToken(kind: token.kind, byteStart: token.byteStart, byteEnd: token.byteEnd, position: position)
            }
        } else { tokens = scanned }
        self.recordPositions = recordPositions
        computeQueryBoundaries()
    }

    func parse() throws -> SQLStatement {
        intoAllowed = true
        intoTop = true
        let result = try statement()
        guard peek().kind == .end else { throw syntax("unexpected trailing input") }
        return result
    }
    func parseExpressionOnly() throws -> SQLExpression {
        let result = try expression()
        guard peek().kind == .end else { throw syntax("unexpected trailing input") }
        return result
    }
    func deepen() throws {
        depth += 1
        maximumDepth = max(maximumDepth, depth)
        if depth > Self.maximumNesting {
            throw error("54001", "statement too complex: nesting depth exceeds the maximum of 256")
        }
    }
    func peek(_ offset: Int = 0) -> SQLToken {
        let index = min(cursor + offset, tokens.count - 1)
        if offset == 0 { furthest = max(furthest, index) }
        return tokens[index]
    }
    @discardableResult func advance() -> SQLToken {
        let token = peek()
        if cursor < tokens.count - 1 { cursor += 1 }
        return token
    }
    func word(_ offset: Int = 0) -> String {
        if case .word(let name, quoted: false) = peek(offset).kind { return name.description }
        return ""
    }
    func symbol(_ name: String, _ offset: Int = 0) -> Bool { peek(offset).kind == .symbol(name) }
    func isIdentifier(_ offset: Int = 0) -> Bool {
        if case .word = peek(offset).kind { return true }
        return false
    }
    @discardableResult func eat(_ keyword: String) -> Bool {
        guard word() == keyword else { return false }
        advance(); return true
    }
    @discardableResult func eatSymbol(_ name: String) -> Bool {
        guard symbol(name) else { return false }
        advance(); return true
    }
    func expect(_ keyword: String) throws {
        guard eat(keyword) else { advance(); throw syntax("expected keyword '\(keyword)'") }
    }
    func expectSymbol(_ name: String) throws {
        guard eatSymbol(name) else { advance(); throw syntax("unexpected token") }
    }
    func identifier() throws -> SQLText {
        guard case .word(let name, _) = advance().kind else { throw syntax("expected an identifier") }
        return name
    }
    func quotedCollation() throws -> SQLText {
        guard case .word(let name, quoted: true) = advance().kind else {
            throw syntax("expected a quoted collation name after COLLATE")
        }
        return name
    }
    func sourcePosition(_ index: Int) -> Int { recordPositions ? tokens[min(index, tokens.count - 1)].position : 0 }
    func span(_ start: Int, through end: Int? = nil) -> SQLSourceSpan {
        let last = max(start, (end ?? cursor) - 1)
        return SQLSourceSpan(byteStart: tokens[start].byteStart, byteEnd: tokens[last].byteEnd,
                             position: sourcePosition(start))
    }
    func error(_ code: String, _ message: String, at index: Int? = nil) -> JedError {
        JedError(code, message).at(code.hasPrefix("54") ? 0 : sourcePosition(index ?? furthest))
    }
    func syntax(_ message: String = "unexpected token", at index: Int? = nil) -> JedError {
        error("42601", message, at: index)
    }
    func expressionNode(_ kind: SQLExpression.Kind, from start: Int, at position: Int? = nil) -> SQLExpression {
        SQLExpression(kind: kind, span: span(start), position: position ?? sourcePosition(start))
    }
    func qualifiedName() throws -> SQLName {
        let start = cursor
        let first = try identifier()
        if eatSymbol(".") { return SQLName(database: first, name: try identifier(), span: span(start)) }
        return SQLName(name: first, span: span(start))
    }
    func identifierList(close: String = ")") throws -> [SQLText] {
        var result = [try identifier()]
        while eatSymbol(",") { result.append(try identifier()) }
        try expectSymbol(close)
        return result
    }
    func expressionList(close: String, allowEmpty: Bool = false) throws -> [SQLExpression] {
        if allowEmpty && eatSymbol(close) { return [] }
        var result = [try expression()]
        while eatSymbol(",") { result.append(try expression()) }
        try expectSymbol(close)
        return result
    }
    func integerSpelling(signed: Bool = true) throws -> String {
        let negative = signed && eatSymbol("-")
        guard case .number(let raw) = advance().kind, Self.isInteger(raw) else {
            throw syntax("expected an integer literal")
        }
        return negative ? "-" + raw : raw
    }
    static func isInteger(_ raw: String) -> Bool {
        let s = raw.lowercased()
        return s.hasPrefix("0x") || s.hasPrefix("0o") || s.hasPrefix("0b") || (!s.contains(".") && !s.contains("e"))
    }
    func typeSyntax() throws -> SQLTypeSyntax {
        let start = cursor
        var name: SQLText
        if (word() == "double" && word(1) == "precision") || (word() == "character" && word(1) == "varying") {
            let first = word(); advance()
            let second = word(); advance()
            name = SQLText(validatedUTF8: Array((first + " " + second).utf8))
        } else { name = try identifier() }
        var result = SQLTypeSyntax(name: name, span: span(start))
        if eatSymbol("(") {
            result.modifiers.append(try integerSpelling(signed: false))
            if eatSymbol(",") { result.modifiers.append(try integerSpelling(signed: false)) }
            try expectSymbol(")")
        } else if name.utf8 == Array("interval".utf8) {
            try intervalQualifier(&result)
        }
        while eatSymbol("[") { try expectSymbol("]"); result.arrayDimensions += 1 }
        result.span = span(start)
        return result
    }
    func intervalQualifier(_ type: inout SQLTypeSyntax) throws {
        guard ["year", "month", "day", "hour", "minute", "second"].contains(word()) else { return }
        type.intervalFields.append(word()); advance()
        if type.intervalFields[0] != "second" && eat("to") {
            let last = word()
            guard ["year", "month", "day", "hour", "minute", "second"].contains(last) else { throw syntax("expected an interval field after TO") }
            type.intervalFields.append(last); advance()
        }
        let name = type.intervalFields.joined(separator: " to ")
        guard Self.intervalQualifiers.contains(name) else { throw syntax("invalid interval qualifier: " + name) }
        if type.intervalFields.last == "second" && eatSymbol("(") {
            type.modifiers.append(try integerSpelling(signed: false)); try expectSymbol(")")
        }
    }

    private func computeQueryBoundaries() {
        var close = [Int](repeating: tokens.count, count: tokens.count)
        var opens: [Int] = []
        for (index, token) in tokens.enumerated() {
            if token.kind == .symbol("(") { opens.append(index) }
            else if token.kind == .symbol(")"), let open = opens.popLast() { close[open] = index }
        }
        parenthesizedQuery = [Bool](repeating: false, count: tokens.count)
        for index in tokens.indices.reversed() where tokens[index].kind == .symbol("(") {
            let next = index + 1
            let following = close[index] + 1
            guard next < tokens.count, following < tokens.count else { continue }
            let follow = tokens[following].kind
            var allowed = follow == .symbol(")")
            if case .word(let text, quoted: false) = follow {
                allowed = ["union", "intersect", "except", "order", "limit", "offset"].contains(text.description)
            }
            parenthesizedQuery[index] = allowed && queryStart(at: next)
        }
    }
    func withStart(at index: Int? = nil) -> Bool {
        let offset = (index ?? cursor) - cursor
        guard word(offset) == "with" else { return false }
        return word(offset + 1) == "recursive" || (isIdentifier(offset + 1) && (symbol("(", offset + 2) || word(offset + 2) == "as"))
    }
    func queryStart(at index: Int? = nil) -> Bool {
        let index = index ?? cursor
        guard index < tokens.count else { return false }
        let offset = index - cursor
        return word(offset) == "select" || (word(offset) == "values" && symbol("(", offset + 1))
            || withStart(at: index) || (!parenthesizedQuery.isEmpty && parenthesizedQuery[index])
    }
    func nestedQuery() throws -> SQLQuery {
        try deepen(); defer { depth -= 1 }
        let allowed = intoAllowed; intoAllowed = false; defer { intoAllowed = allowed }
        intoNesting += 1; defer { intoNesting -= 1 }
        return try query()
    }
    func query(allowWith: Bool = true) throws -> SQLQuery {
        let start = cursor
        if allowWith && withStart() {
            let bindings = try withClause()
            let body = try query(allowWith: false)
            if case .with = body.body { throw syntax("multiple WITH clauses not allowed") }
            return SQLQuery(body: .with(bindings, body), span: span(start))
        }
        return try finishQuery(setExpression(intersection: false), start: start)
    }
    private func finishQuery(_ initial: SQLQuery, start: Int) throws -> SQLQuery {
        var result = initial
        if eat("order") {
            if !result.orderBy.isEmpty { throw syntax("multiple ORDER BY clauses not allowed") }
            try expect("by"); result.orderBy = try orderTerms()
        }
        for _ in 0..<2 {
            if eat("limit") {
                guard result.limit == nil else { throw syntax("multiple LIMIT clauses not allowed") }
                result.limit = try integerSpelling()
            } else if eat("offset") {
                guard result.offset == nil else { throw syntax("multiple OFFSET clauses not allowed") }
                result.offset = try integerSpelling()
            } else { break }
        }
        result.span = span(start)
        return result
    }
    func setExpression(intersection: Bool) throws -> SQLQuery {
        let start = cursor
        let base = depth; defer { depth = base }
        var left = try intersection ? setOperand() : setExpression(intersection: true)
        while intersection ? word() == "intersect" : ["union", "except"].contains(word()) {
            try deepen()
            let op = word(); advance()
            let all = eat("all")
            if !all { eat("distinct") }
            let right = try intersection ? setOperand() : setExpression(intersection: true)
            left = SQLQuery(body: .set(op, all: all, left, right), span: span(start))
        }
        return left
    }
    func setOperand() throws -> SQLQuery {
        let start = cursor
        if eatSymbol("(") {
            // Parentheses here are grouping a query, so the leftmost INTO owner
            // remains available through them (unlike scalar subqueries).
            try deepen(); defer { depth -= 1 }
            var body = try query(); try expectSymbol(")")
            body.span = span(start); return body
        }
        if eat("values") { return try valuesOperand(start: start) }
        return SQLQuery(body: .select(try select()), span: span(start))
    }
    private func valuesOperand(start: Int) throws -> SQLQuery {
        intoAllowed = false
        var rows: [[SQLExpression]] = []
        repeat {
            try expectSymbol("(")
            var row: [SQLExpression] = []
            repeat {
                if word() == "default" && (symbol(",", 1) || symbol(")", 1)) { throw syntax("DEFAULT is not allowed in this context") }
                row.append(try expression())
            } while eatSymbol(",")
            try expectSymbol(")"); rows.append(row)
        } while eatSymbol(",")
        return SQLQuery(body: .values(rows), span: span(start))
    }
    func select() throws -> SQLSelect {
        try expect("select")
        var result = SQLSelect()
        if word() == "distinct" && peek(1).kind != .end && word(1) != "from" {
            advance(); result.distinct = true
            if word() == "on" && symbol("(", 1) {
                advance(); advance(); result.distinctOn = try expressionList(close: ")")
            }
        }
        let owner = intoAllowed; intoAllowed = false
        intoNesting += 1; defer { intoNesting -= 1 }
        result.items = try selectItems()
        if eat("into") {
            guard owner else {
                throw syntax(intoTop && intoNesting == 1 ? "INTO is only allowed on first SELECT of UNION/INTERSECT/EXCEPT" : "SELECT ... INTO is not allowed here")
            }
            if intoView { throw error("0A000", "views must not contain SELECT INTO") }
            let temporary = (word() == "temp" || word() == "temporary") && !symbol(".", 1)
            if temporary { advance() }
            if word() == "unlogged" && !symbol(".", 1) { throw error("0A000", "UNLOGGED tables are not supported") }
            if word() == "table" && !symbol(".", 1) { advance() }
            result.into = SQLInto(name: try qualifiedName(), temporary: temporary)
        }
        if eat("from") {
            repeat { result.from.append(try fromItem()) } while eatSymbol(",")
        }
        if eat("where") { result.predicate = try expression() }
        if eat("group") {
            try expect("by")
            repeat { result.grouping.append(try groupItem()) } while eatSymbol(",")
        }
        if eat("having") { result.having = try expression() }
        if eat("window") {
            repeat {
                let name = try identifier(); try expect("as"); try expectSymbol("(")
                let definition = try windowDefinition(); try expectSymbol(")")
                result.windows.append(SQLNamedWindow(name: name, definition: definition))
            } while eatSymbol(",")
        }
        return result
    }
    static let bareLabelStops: Set<String> = Set("array as char character create day except fetch filter for from grant group having hour intersect into isnull limit minute month notnull offset on order over overlaps precision returning second to union varying where window with within without year".split(separator: " ").map(String.init))
    static let relationAliasStops: Set<String> = Set("where group having window order limit offset union intersect except join inner left right full outer cross natural on as using returning when with".split(separator: " ").map(String.init))
    func mayAlias(stops: Set<String>) -> Bool {
        if case .word(_, quoted: true) = peek().kind { return true }
        return isIdentifier() && !stops.contains(word())
    }
    func selectItems() throws -> [SQLSelectItem] {
        if symbol("*") {
            let start = cursor; advance()
            return [SQLSelectItem(star: true, span: span(start))]
        }
        var items: [SQLSelectItem] = []
        repeat {
            let start = cursor
            var item = SQLSelectItem(span: span(start))
            if isIdentifier() && symbol(".", 1) && symbol("*", 2) {
                item.qualifier = try identifier(); advance(); advance(); item.star = true
            } else {
                item.expression = try expression()
                if eat("as") || mayAlias(stops: Self.bareLabelStops) { item.alias = try identifier() }
            }
            item.span = span(start); items.append(item)
        } while eatSymbol(",")
        return items
    }
    func orderTerms() throws -> [SQLOrderTerm] {
        var result: [SQLOrderTerm] = []
        repeat {
            let start = cursor
            let value = try expression()
            var term = SQLOrderTerm(expression: value, span: span(start))
            term.descending = eat("desc")
            if !term.descending { eat("asc") }
            if eat("nulls") {
                if eat("first") { term.nullsFirst = true }
                else { try expect("last"); term.nullsFirst = false }
            }
            term.span = span(start); result.append(term)
        } while eatSymbol(",")
        return result
    }
    func groupItem() throws -> SQLGroup {
        let start = word()
        if (["rollup", "cube"].contains(start) && symbol("(", 1)) || (start == "grouping" && word(1) == "sets") {
            advance(); if start == "grouping" { advance() }
            try expectSymbol("("); try deepen(); defer { depth -= 1 }
            var groups = [try groupItem()]
            while eatSymbol(",") { groups.append(try groupItem()) }
            try expectSymbol(")")
            return start == "cube" ? .cube(groups) : start == "rollup" ? .rollup(groups) : .sets(groups)
        }
        if eatSymbol("(") { return .set(try expressionList(close: ")", allowEmpty: true)) }
        return .set([try expression()])
    }
    func fromItem() throws -> SQLFrom {
        return try finishFromItem(tableReference())
    }
    private func finishFromItem(_ initial: SQLFrom) throws -> SQLFrom {
        var left = initial
        while ["join", "inner", "left", "right", "full", "cross", "natural"].contains(word()) {
            let natural = eat("natural")
            var kind = "inner"
            if ["inner", "left", "right", "full", "cross"].contains(word()) { kind = word(); advance(); eat("outer") }
            if natural && kind == "cross" { throw syntax("NATURAL CROSS JOIN is not supported") }
            try expect("join")
            let right = try tableReference()
            var join = SQLJoin(kind: kind, natural: natural, right: right)
            if !natural && kind != "cross" {
                if eat("on") { join.condition = try expression() }
                else { try expect("using"); try expectSymbol("("); join.using = try identifierList() }
            }
            left.joins.append(join)
        }
        return left
    }
    func tableReference() throws -> SQLFrom {
        let start = cursor
        let referenceDepth = depth
        let lateral = word() == "lateral" && (symbol("(", 1) || (isIdentifier(1) && symbol("(", 2)))
        if lateral { advance() }
        let source: SQLFrom.Source
        if eatSymbol("(") {
            if queryStart() { source = .query(try nestedQuery()) }
            else {
                if lateral { throw syntax("LATERAL requires a subquery or table function") }
                try deepen(); defer { depth -= 1 }
                let joined = try fromItem()
                if joined.joins.isEmpty {
                    if case .joined = joined.source {} else { throw syntax("a parenthesized FROM item must be a subquery, a VALUES list, or a join") }
                }
                source = .joined(joined)
            }
            try expectSymbol(")")
        } else if word() == "json_table" && symbol("(", 1) { source = .jsonTable(try jsonTable()) }
        else if isIdentifier() && symbol("(", 1) { source = .function(try functionCall()) }
        else { source = .table(try qualifiedName()) }
        return try finishTableReference(source, start: start, referenceDepth: referenceDepth, lateral: lateral)
    }
    private func finishTableReference(_ source: SQLFrom.Source, start: Int, referenceDepth: Int, lateral: Bool) throws -> SQLFrom {
        var result = SQLFrom(source: source, lateral: lateral, span: span(start), parserDepth: referenceDepth)
        let explicit = eat("as")
        if explicit && symbol("(") {
            advance(); result.columnDefinitions = try fieldDefinitions()
        } else if explicit || mayAlias(stops: Self.relationAliasStops) {
            result.alias = try identifier()
            if eatSymbol("(") {
                repeat {
                    let fieldStart = cursor
                    let name = try identifier()
                    if symbol(",") || symbol(")") { result.columnAliases.append(name) }
                    else {
                        let type = try typeSyntax()
                        result.columnDefinitions.append(SQLColumnDefinition(name: name, type: type, span: span(fieldStart)))
                    }
                } while eatSymbol(",")
                try expectSymbol(")")
                if !result.columnDefinitions.isEmpty && !result.columnAliases.isEmpty { throw syntax("mixed column aliases and definitions") }
            }
        }
        result.span = span(start)
        return result
    }
    func withClause() throws -> SQLWith {
        try expect("with")
        let recursive = eat("recursive")
        var bindings: [SQLCTE] = []
        repeat {
            let name = try identifier()
            var columns: [SQLText] = []
            if eatSymbol("(") { columns = try identifierList() }
            try expect("as")
            var materialized: Bool?
            if eat("not") { try expect("materialized"); materialized = false }
            else if eat("materialized") { materialized = true }
            try expectSymbol("(")
            try deepen()
            let oldInto = intoAllowed; intoAllowed = false; intoNesting += 1
            let body = try statement(queryOrDMLOnly: true)
            intoAllowed = oldInto; intoNesting -= 1; depth -= 1
            try expectSymbol(")")
            var binding = SQLCTE(name: name, columns: columns, materialized: materialized, body: body)
            if eat("search") {
                let first = eat("depth"); if !first { try expect("breadth") }
                try expect("first"); try expect("by")
                var keys = [try identifier()]
                while eatSymbol(",") { keys.append(try identifier()) }
                try expect("set")
                binding.search = SQLSearch(depthFirst: first, columns: keys, output: try identifier())
            }
            if eat("cycle") {
                var keys = [try identifier()]
                while eatSymbol(",") { keys.append(try identifier()) }
                try expect("set"); let mark = try identifier()
                var value: SQLExpression?; var defaultValue: SQLExpression?
                if eat("to") { value = try cycleConstant(); try expect("default"); defaultValue = try cycleConstant() }
                try expect("using")
                binding.cycle = SQLCycle(columns: keys, mark: mark, markValue: value, defaultValue: defaultValue, path: try identifier())
            }
            bindings.append(binding)
        } while eatSymbol(",")
        return SQLWith(recursive: recursive, bindings: bindings)
    }
    func cycleConstant() throws -> SQLExpression {
        if symbol("-") || symbol("(") { throw syntax("expected a constant in a CYCLE clause") }
        let value = try primary()
        switch value.kind {
        case .number, .string, .boolean, .null, .typedString: return value
        default: throw syntax("expected a constant in a CYCLE clause")
        }
    }
    func windowDefinition() throws -> SQLWindow {
        var window = SQLWindow()
        if mayAlias(stops: ["partition", "order", "rows", "range", "groups"]) { window.baseName = try identifier() }
        if eat("partition") {
            try expect("by")
            repeat { window.partitionBy.append(try expression()) } while eatSymbol(",")
        }
        if eat("order") { try expect("by"); window.orderBy = try orderTerms() }
        if ["rows", "range", "groups"].contains(word()) {
            let mode = word(); advance()
            let between = eat("between")
            let start = try frameBound()
            var end: SQLFrameBound?
            if between { try expect("and"); end = try frameBound() }
            var exclusion: String?
            if eat("exclude") {
                if eat("current") { try expect("row"); exclusion = "current row" }
                else if eat("no") { try expect("others"); exclusion = "no others" }
                else if ["group", "ties"].contains(word()) { exclusion = word(); advance() }
                else { throw syntax("invalid frame exclusion") }
            }
            window.frame = SQLWindowFrame(mode: mode, start: start, end: end, exclusion: exclusion)
        }
        return window
    }
    func frameBound() throws -> SQLFrameBound {
        if eat("current") { try expect("row"); return .currentRow }
        if eat("unbounded") {
            let preceding = eat("preceding"); if !preceding { try expect("following") }
            return .unbounded(preceding: preceding)
        }
        let value = try expression()
        let preceding = eat("preceding"); if !preceding { try expect("following") }
        return .offset(value, preceding: preceding)
    }
}
