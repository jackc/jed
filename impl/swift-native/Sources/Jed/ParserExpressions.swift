extension SQLParser {
    func expression() throws -> SQLExpression {
        try deepen(); defer { depth -= 1 }
        return try binaryChain(level: 0)
    }
    /// The ladder is explicit: OR, AND, NOT/comparison, generic operators,
    /// additive, multiplicative, AT TIME ZONE, unary, postfix, primary.
    func binaryChain(level: Int) throws -> SQLExpression {
        if level == 2 { return try notExpression() }
        if level == 7 { return try unaryExpression() }
        let start = cursor
        let base = depth; defer { depth = base }
        return try finishBinaryChain(binaryChain(level: level + 1), level: level, start: start)
    }
    private func finishBinaryChain(_ initial: SQLExpression, level: Int, start: Int) throws -> SQLExpression {
        var left = initial
        while true {
            var op: String?
            switch level {
            case 0: if word() == "or" { op = "or" }
            case 1: if word() == "and" { op = "and" }
            case 3:
                if case .symbol(let s) = peek().kind,
                   ["||", "@>", "<@", "&&", "<<", ">>", "&<", "&>", "-|-", "->", "->>", "#>", "#>>", "?", "?|", "?&", "#-", "@?", "@@"].contains(s) { op = s }
            case 4:
                if case .symbol(let s) = peek().kind, ["+", "-"].contains(s) { op = s }
            case 5:
                if case .symbol(let s) = peek().kind, ["*", "/", "%"].contains(s) { op = s }
            case 6: if word() == "at" && word(1) == "time" && word(2) == "zone" { op = "at time zone" }
            default: preconditionFailure("precedence ladder")
            }
            guard let op else { return left }
            try deepen()
            let position = sourcePosition(cursor)
            advance()
            if op == "at time zone" { advance(); advance() }
            let right = try binaryChain(level: level + 1)
            left = expressionNode(.binary(op, left, right), from: start, at: position)
        }
    }
    func notExpression() throws -> SQLExpression {
        if word() == "not" && !(symbol(".", 1) && isIdentifier(2)) {
            let start = cursor; advance(); try deepen(); defer { depth -= 1 }
            return expressionNode(.unary("not", try notExpression()), from: start)
        }
        return try comparison()
    }
    func comparison() throws -> SQLExpression {
        let start = cursor
        return try finishComparison(binaryChain(level: 3), start: start)
    }
    private func finishComparison(_ left: SQLExpression, start: Int) throws -> SQLExpression {
        let position = sourcePosition(cursor)
        if eat("is") {
            let negated = eat("not")
            if eat("distinct") {
                try expect("from")
                return expressionNode(.isDistinct(left, try binaryChain(level: 3), negated: negated), from: start, at: position)
            }
            if eat("json") {
                var kind = "value"
                if ["value", "scalar", "array", "object"].contains(word()) { kind = word(); advance() }
                let unique = uniqueKeysClause()
                return expressionNode(.isJSON(left, kind: kind, unique: unique, negated: negated), from: start, at: position)
            }
            try expect("null")
            return expressionNode(.isNull(left, negated: negated), from: start, at: position)
        }
        let negated = word() == "not" && (["in", "between", "like", "ilike"].contains(word(1)) || (word(1) == "similar" && word(2) == "to"))
        if negated { advance() }
        if eat("in") {
            try expectSymbol("(")
            if queryStart() {
                let body = try nestedQuery(); try expectSymbol(")")
                return expressionNode(.memberQuery(left, body, negated: negated), from: start, at: position)
            }
            var elements = [try binaryChain(level: 3)]
            while eatSymbol(",") {
                elements.append(try binaryChain(level: 3))
                if elements.count > Self.maximumInElements { throw error("54001", "statement too complex: IN list exceeds the maximum of 1000 elements") }
            }
            try expectSymbol(")")
            return expressionNode(.member(left, elements, negated: negated), from: start, at: position)
        }
        if eat("between") {
            let lower = try binaryChain(level: 3); try expect("and")
            let upper = try binaryChain(level: 3)
            return expressionNode(.between(left, lower: lower, upper: upper, negated: negated), from: start, at: position)
        }
        if ["like", "ilike"].contains(word()) || (word() == "similar" && word(1) == "to") {
            let op = word(); advance(); if op == "similar" { advance() }
            let pattern = try binaryChain(level: 3)
            let escape = try eat("escape") ? binaryChain(level: 3) : nil
            return expressionNode(.pattern(op, left, pattern, escape: escape, negated: negated), from: start, at: position)
        }
        guard case .symbol(let op) = peek().kind,
              ["=", "<>", "<", ">", "<=", ">=", "~", "~*", "!~", "!~*"].contains(op) else { return left }
        advance()
        if ["any", "some", "all"].contains(word()) && ["=", "<>", "<", ">", "<=", ">="].contains(op) {
            let all = word() == "all"; advance(); try expectSymbol("(")
            if queryStart() {
                let body = try nestedQuery(); try expectSymbol(")")
                return expressionNode(.quantifiedQuery(op, left, all: all, body), from: start, at: position)
            }
            let operand = try expression(); try expectSymbol(")")
            return expressionNode(.quantified(op, left, all: all, operand), from: start, at: position)
        }
        let right = try binaryChain(level: 3)
        return expressionNode(.binary(op, left, right), from: start, at: position)
    }
    func unaryExpression() throws -> SQLExpression {
        if eatSymbol("-") {
            let start = cursor - 1
            if case .number(let raw) = peek().kind, !symbol("::", 1) {
                advance(); return expressionNode(.number("-" + raw), from: start)
            }
            try deepen(); defer { depth -= 1 }
            return expressionNode(.unary("-", try unaryExpression()), from: start)
        }
        return try postfixExpression()
    }
    func postfixExpression() throws -> SQLExpression {
        let start = cursor
        let base = depth; defer { depth = base }
        let fieldAccessible = symbol("(")
        return try finishPostfix(primary(), start: start, fieldAccessible: fieldAccessible)
    }
    private func finishPostfix(_ initial: SQLExpression, start: Int, fieldAccessible initialAccess: Bool) throws -> SQLExpression {
        var value = initial, fieldAccessible = initialAccess
        if bareRowEnd == cursor { fieldAccessible = false }
        while symbol("::") || symbol("[") || (fieldAccessible && symbol(".")) || word() == "collate" {
            try deepen()
            let position = sourcePosition(cursor)
            if eatSymbol("::") { value = expressionNode(.cast(value, try typeSyntax()), from: start, at: position); fieldAccessible = false }
            else if eat("collate") { value = expressionNode(.collate(value, try quotedCollation()), from: start, at: position); fieldAccessible = false }
            else if symbol("[") {
                var indices: [SQLSubscript] = []
                while eatSymbol("[") {
                    let lower = try (!symbol(":") && !symbol("]")) ? expression() : nil
                    let slice = eatSymbol(":")
                    let upper = try slice && !symbol("]") ? expression() : nil
                    if !slice && lower == nil { throw syntax("array subscript requires an index") }
                    try expectSymbol("]")
                    indices.append(SQLSubscript(lower: lower, upper: upper, isSlice: slice))
                }
                value = expressionNode(.subscripted(value, indices), from: start, at: position); fieldAccessible = false
            } else {
                advance()
                if eatSymbol("*") { value = expressionNode(.field(value, nil), from: start, at: position); fieldAccessible = false }
                else { value = expressionNode(.field(value, try identifier()), from: start, at: position) }
            }
        }
        return value
    }
    func primary() throws -> SQLExpression {
        let start = cursor
        if isIdentifier() && symbol(".", 1) && isIdentifier(2) {
            let qualifier = try identifier(); advance()
            return expressionNode(.column(qualifier: qualifier, name: try identifier()), from: start)
        }
        if eatSymbol("(") {
            if queryStart() {
                let body = try nestedQuery(); try expectSymbol(")")
                return expressionNode(.scalarQuery(body), from: start)
            }
            let first = try expression()
            if eatSymbol(",") {
                var fields = [first, try expression()]
                while eatSymbol(",") { fields.append(try expression()) }
                try expectSymbol(")"); bareRowEnd = cursor
                return expressionNode(.row(fields), from: start)
            }
            try expectSymbol(")")
            return expressionNode(.parenthesized(first), from: start, at: first.position)
        }
        let keyword = word()
        if keyword == "exists" && symbol("(", 1) && queryStart(at: cursor + 2) {
            advance(); advance(); let body = try nestedQuery(); try expectSymbol(")")
            return expressionNode(.exists(body), from: start)
        }
        if keyword == "row" && symbol("(", 1) {
            advance(); advance(); return expressionNode(.row(try expressionList(close: ")", allowEmpty: true)), from: start)
        }
        if keyword == "array" && symbol("[", 1) {
            advance(); advance(); return expressionNode(.array(try expressionList(close: "]", allowEmpty: true)), from: start)
        }
        if keyword == "cast" {
            advance(); try expectSymbol("("); let value = try expression(); try expect("as")
            let type = try typeSyntax(); try expectSymbol(")")
            return expressionNode(.cast(value, type), from: start)
        }
        if keyword == "extract" && symbol("(", 1) {
            advance(); advance()
            let field: SQLText
            if case .string(let name) = peek().kind { advance(); field = name }
            else { field = try identifier() }
            try expect("from"); let value = try expression(); try expectSymbol(")")
            return expressionNode(.extract(field.asciiLowercased, value), from: start)
        }
        let twoWordType = (keyword == "double" && word(1) == "precision") || (keyword == "character" && word(1) == "varying")
        if isIdentifier() && !["true", "false", "null"].contains(keyword),
           case .string(let body) = peek(twoWordType ? 2 : 1).kind {
            var name = try identifier()
            if twoWordType { name = SQLText(validatedUTF8: name.utf8 + [32] + (try identifier()).utf8) }
            let typeSpan = span(start)
            let bodyPosition = sourcePosition(cursor)
            advance()
            var type = SQLTypeSyntax(name: name, span: typeSpan)
            if name.utf8 == Array("interval".utf8) { try intervalQualifier(&type) }
            return expressionNode(.typedString(type, body), from: start, at: bodyPosition)
        }
        if ["json", "json_exists", "json_value", "json_query"].contains(keyword) && symbol("(", 1) {
            return try jsonExpression()
        }
        if ["coalesce", "greatest", "least", "nullif"].contains(keyword) && symbol("(", 1) {
            advance(); advance()
            let values = try expressionList(close: ")")
            if keyword == "nullif" && values.count != 2 { throw syntax("NULLIF requires two arguments") }
            return expressionNode(.conditionalFunction(keyword, values), from: start)
        }
        if eat("case") {
            let operand = try word() == "when" ? nil : expression()
            var arms: [SQLCaseArm] = []
            while word() == "when" {
                let whenPosition = sourcePosition(cursor); advance()
                let condition = try expression(); try expect("then")
                arms.append(SQLCaseArm(condition: condition, result: try expression(), whenPosition: whenPosition))
            }
            guard !arms.isEmpty else { throw syntax("CASE requires at least one WHEN clause") }
            let fallback = try eat("else") ? expression() : nil
            try expect("end")
            return expressionNode(.conditional(operand: operand, arms: arms, fallback: fallback), from: start)
        }
        switch peek().kind {
        case .number(let raw): advance(); return expressionNode(.number(raw), from: start)
        case .string(let body): advance(); return expressionNode(.string(body), from: start)
        case .parameter(let index): advance(); return expressionNode(.parameter(index), from: start)
        case .word:
            if eat("null") { return expressionNode(.null, from: start) }
            if eat("true") { return expressionNode(.boolean(true), from: start) }
            if eat("false") { return expressionNode(.boolean(false), from: start) }
            if symbol("(", 1) { return try functionCall() }
            if eat("current_timestamp") { return expressionNode(.currentTimestamp, from: start) }
            if eat("current_date") { return expressionNode(.currentDate, from: start) }
            return expressionNode(.column(qualifier: nil, name: try identifier()), from: start)
        default: throw syntax("expected an expression")
        }
    }
    func functionCall() throws -> SQLExpression {
        let start = cursor
        let special = word()
        let name = try identifier(); try expectSymbol("(")
        if special == "position" {
            try deepen(); defer { depth -= 1 }
            let needle = try binaryChain(level: 3); try expect("in")
            let haystack = try binaryChain(level: 3); try expectSymbol(")")
            return expressionNode(.call(SQLCall(name: name, arguments: [.init(value: haystack), .init(value: needle)])), from: start)
        }
        var call = SQLCall(name: name)
        call.distinct = eat("distinct")
        if call.distinct && symbol("*") { throw syntax("DISTINCT cannot be used with *") }
        if call.distinct && symbol(")") { throw syntax("DISTINCT requires an aggregate argument") }
        if eatSymbol("*") { call.star = true }
        else if !symbol(")") {
            var named = false
            repeat {
                let variadic = eat("variadic")
                var argumentName: SQLText?
                if !variadic && isIdentifier() && symbol("=>", 1) { argumentName = try identifier(); advance(); named = true }
                else if !variadic && named { throw syntax("positional argument cannot follow named argument") }
                let value = try expression()
                call.arguments.append(SQLArgument(name: argumentName, value: value, variadic: variadic))
                if call.arguments.count == 1 && argumentName == nil && !call.distinct && !variadic {
                    if special == "substring" && ["from", "for", "similar"].contains(word()) {
                        return try substringTail(name: name, source: value, start: start)
                    }
                    if special == "overlay" && eat("placing") {
                        let replacement = try expression(); try expect("from")
                        let offset = try expression()
                        var arguments = [value, replacement, offset]
                        if eat("for") { arguments.append(try expression()) }
                        try expectSymbol(")")
                        return expressionNode(.call(SQLCall(name: name, arguments: arguments.map { .init(value: $0) })), from: start)
                    }
                }
                if eat("order") { try expect("by"); call.orderBy = try orderTerms(); break }
                if variadic && symbol(",") { throw syntax("VARIADIC argument must be the last argument") }
                if !eatSymbol(",") { break }
            } while true
        }
        try expectSymbol(")")
        if eat("within") {
            if !call.orderBy.isEmpty { throw syntax("cannot use multiple ORDER BY clauses with WITHIN GROUP") }
            try expect("group"); try expectSymbol("("); try expect("order"); try expect("by")
            call.withinGroup = try orderTerms(); try expectSymbol(")")
        }
        if eat("filter") {
            try expectSymbol("("); try expect("where"); call.filter = try expression(); try expectSymbol(")")
        }
        if eat("over") {
            if eatSymbol("(") { call.over = try windowDefinition(); try expectSymbol(")") }
            else { call.overName = try identifier() }
        }
        return expressionNode(.call(call), from: start)
    }
    func substringTail(name: SQLText, source: SQLExpression, start: Int) throws -> SQLExpression {
        var offset: SQLExpression?; var count: SQLExpression?
        if eat("similar") { offset = try expression(); try expect("escape"); count = try expression() }
        else if eat("from") { offset = try expression(); if eat("for") { count = try expression() } }
        else { try expect("for"); count = try expression(); if eat("from") { offset = try expression() } }
        try expectSymbol(")")
        let one = SQLExpression(kind: .number("1"), span: source.span, position: 0)
        var arguments = [source, offset ?? one]
        if let count { arguments.append(count) }
        return expressionNode(.call(SQLCall(name: name, arguments: arguments.map { .init(value: $0) })), from: start)
    }
    func uniqueKeysClause() -> Bool {
        guard ["with", "without"].contains(word()) && word(1) == "unique" else { return false }
        let unique = word() == "with"; advance(); advance(); eat("keys"); return unique
    }
    func formatJSON() throws -> (Bool, SQLText?) {
        guard word() == "format" && word(1) == "json" else { return (false, nil) }
        advance(); advance()
        if eat("encoding") {
            let encoding = try identifier()
            guard ["utf8", "utf16", "utf32"].contains(encoding.asciiLowercased.description) else {
                throw error("22023", "unrecognized JSON encoding: \(encoding)")
            }
            return (true, SQLText(validatedUTF8: Array(encoding.asciiLowercased.description.uppercased().utf8)))
        }
        return (true, nil)
    }
    func jsonPassing() throws -> [SQLJSONPassing] {
        guard eat("passing") else { return [] }
        var values: [SQLJSONPassing] = []
        repeat {
            let value = try expression(); let (format, encoding) = try formatJSON()
            try expect("as")
            values.append(SQLJSONPassing(value: value, formatJSON: format, encoding: encoding, name: try identifier()))
        } while eatSymbol(",")
        return values
    }
    func jsonWrapperQuotes() throws -> (String?, Bool) {
        var wrapper: String?
        if eat("with") {
            wrapper = eat("conditional") ? "conditional" : "unconditional"
            if wrapper == "unconditional" { eat("unconditional") }
            eat("array"); try expect("wrapper")
        } else if eat("without") { eat("array"); try expect("wrapper"); wrapper = "without" }
        var keep = true
        if ["keep", "omit"].contains(word()) {
            keep = word() == "keep"; advance(); try expect("quotes")
            if word() == "on" && word(1) == "scalar" { advance(); advance(); eat("string") }
        }
        return (wrapper, keep)
    }
    func jsonBehaviors(errorOnly: Bool = false) throws -> (SQLJSONBehavior?, SQLJSONBehavior?) {
        var empty: SQLJSONBehavior?; var failure: SQLJSONBehavior?
        for _ in 0..<2 {
            guard ["error", "null", "true", "false", "unknown", "empty", "default"].contains(word()) else { break }
            var action = word(); advance()
            var value: SQLExpression?
            if action == "default" {
                if errorOnly { throw syntax("invalid ON ERROR behavior: only ERROR, TRUE, FALSE, or UNKNOWN is allowed in ON ERROR for JSON_EXISTS()") }
                value = try expression()
            } else if action == "empty" { action = eat("object") ? "empty object" : "empty array"; if action == "empty array" { eat("array") } }
            try expect("on")
            let behavior = SQLJSONBehavior(action: action, value: value)
            if eat("empty") {
                if errorOnly || empty != nil || failure != nil { throw syntax("ON EMPTY must come before ON ERROR") }
                empty = behavior
            } else {
                try expect("error"); if failure != nil { throw syntax("duplicate ON ERROR clause") }
                failure = behavior
            }
            if errorOnly { break }
        }
        return (empty, failure)
    }
    func jsonExpression() throws -> SQLExpression {
        let start = cursor; let name = word(); advance(); try expectSymbol("(")
        let context = try expression()
        var result = SQLJSONExpression(name: name, context: context)
        if name == "json" { result.uniqueKeys = uniqueKeysClause() }
        else {
            (result.formatJSON, result.encoding) = try formatJSON()
            try expectSymbol(","); result.path = try expression(); result.passing = try jsonPassing()
            if name != "json_exists" && eat("returning") { result.returning = try typeSyntax(); _ = try formatJSON() }
            if name == "json_query" { (result.wrapper, result.keepQuotes) = try jsonWrapperQuotes() }
            (result.onEmpty, result.onError) = try jsonBehaviors(errorOnly: name == "json_exists")
        }
        try expectSymbol(")")
        return expressionNode(.json(result), from: start)
    }
    func jsonTable() throws -> SQLJSONTable {
        try expect("json_table"); try expectSymbol("(")
        let context = try expression(); let (format, encoding) = try formatJSON()
        try expectSymbol(","); let path = try expression()
        let pathName = try eat("as") ? identifier() : nil
        var table = SQLJSONTable(context: context, formatJSON: format, encoding: encoding, path: path, pathName: pathName)
        table.passing = try jsonPassing(); try expect("columns"); table.columns = try jsonTableColumns()
        if word() == "plan" { throw error("0A000", "JSON_TABLE explicit PLAN clause is not supported yet") }
        try expectSymbol(")")
        return table
    }
    func jsonTableColumns() throws -> [SQLJSONTableColumn] {
        try expectSymbol("(")
        var columns: [SQLJSONTableColumn] = []
        repeat {
            if eat("nested") {
                eat("path")
                guard case .string(let path) = advance().kind else { throw syntax("expected a string path after NESTED PATH") }
                let name = try eat("as") ? identifier() : nil
                try expect("columns"); try deepen()
                let children = try jsonTableColumns(); depth -= 1
                columns.append(.nested(path, name: name, children))
                continue
            }
            let name = try identifier()
            if eat("for") { try expect("ordinality"); columns.append(.ordinality(name)); continue }
            let type = try typeSyntax()
            let exists = eat("exists")
            let (format, _) = try exists ? (false, nil as SQLText?) : formatJSON()
            var path: SQLText?
            if eat("path") {
                guard case .string(let text) = advance().kind else { throw syntax("expected a string after PATH") }
                path = text
            }
            let (wrapper, keep) = try exists ? (nil as String?, true) : jsonWrapperQuotes()
            let (empty, failure) = try jsonBehaviors(errorOnly: exists)
            columns.append(.regular(name, type, formatJSON: format, exists: exists, path: path,
                                    wrapper: wrapper, keepQuotes: keep, onEmpty: empty, onError: failure))
        } while eatSymbol(",")
        try expectSymbol(")")
        return columns
    }
}
