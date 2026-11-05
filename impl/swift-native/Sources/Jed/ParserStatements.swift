extension SQLParser {
    func statement(queryOrDMLOnly: Bool = false) throws -> SQLStatement {
        let keyword = word()
        if keyword == "with" {
            let bindings = try withClause()
            if word() == "with" { throw syntax("multiple WITH clauses not allowed") }
            let body = try statement(queryOrDMLOnly: true)
            if case .with = body { throw syntax("multiple WITH clauses not allowed") }
            if case .query(let query) = body, case .with = query.body { throw syntax("multiple WITH clauses not allowed") }
            return .with(bindings, body)
        }
        if ["select", "values"].contains(keyword) || symbol("(") { return .query(try query()) }
        switch keyword {
        case "insert": return .insert(try insert())
        case "update": return .update(try update())
        case "delete": return .delete(try deleteStatement())
        case "merge": return .merge(try merge())
        default: break
        }
        if queryOrDMLOnly { throw syntax("expected a query or data-modifying statement") }
        switch keyword {
        case "create": return try create()
        case "drop": return .drop(try drop())
        case "alter":
            advance()
            if eat("table") { return .alterTable(try alterTable()) }
            if eat("view") { return .alterView(try alterView()) }
            if eat("sequence") { return .alterSequence(try alterSequence()) }
            throw syntax("unexpected keyword 'alter'")
        case "analyze":
            advance(); let name = try qualifiedName()
            let columns = try eatSymbol("(") ? identifierList() : []
            return .analyze(name, columns: columns)
        case "reindex": return .reindex(try reindex())
        case "explain": return try explain()
        case "begin", "start":
            advance()
            if keyword == "start" { try expect("transaction") }
            else if !eat("transaction") { eat("work") }
            var writable: Bool?
            if eat("read") { if eat("only") { writable = false } else { try expect("write"); writable = true } }
            return .begin(writable: writable)
        case "commit", "end": advance(); if !eat("transaction") { eat("work") }; return .commit
        case "rollback":
            advance(); if !eat("transaction") { eat("work") }
            if eat("to") { if word() == "savepoint" && isIdentifier(1) { advance() }; return .rollbackTo(try identifier()) }
            return .rollback
        case "savepoint": advance(); return .savepoint(try identifier())
        case "release": advance(); if word() == "savepoint" && isIdentifier(1) { advance() }; return .release(try identifier())
        case "": throw syntax("expected a SQL statement")
        default: throw syntax("unexpected keyword '\(keyword)'")
        }
    }
    func ifNotExists() throws -> Bool {
        guard word() == "if" && word(1) == "not" else { return false }
        advance(); advance(); try expect("exists"); return true
    }
    func ifExists() throws -> Bool {
        guard word() == "if" && word(1) == "exists" else { return false }
        advance(); advance(); return true
    }
    func cascade() -> Bool { if eat("cascade") { return true }; eat("restrict"); return false }
    func create() throws -> SQLStatement {
        try expect("create")
        if eat("unique") { try expect("index"); return .createIndex(try createIndex(unique: true)) }
        if eat("index") { return .createIndex(try createIndex(unique: false)) }
        if eat("type") {
            let name = try qualifiedName(); try expect("as"); try expectSymbol("(")
            return .createType(SQLCreateType(name: name, fields: try fieldDefinitions()))
        }
        if eat("sequence") {
            let skip = try ifNotExists(); let name = try qualifiedName()
            return .createSequence(SQLCreateSequence(name: name, ifNotExists: skip, options: try sequenceOptions()))
        }
        let replace = eat("or"); if replace { try expect("replace") }
        let temporary = eat("temp") || eat("temporary")
        let recursive = eat("recursive")
        if eat("view") { return .createView(try createView(replace: replace, temporary: temporary, recursive: recursive)) }
        if word() == "materialized" { throw error("0A000", "this form of CREATE VIEW is not supported") }
        if eat("unlogged") { throw error("0A000", "UNLOGGED tables are not supported") }
        if replace || recursive { throw syntax("expected keyword 'view'") }
        try expect("table")
        let skip = try ifNotExists(); let name = try qualifiedName()
        var table = SQLCreateTable(name: name, temporary: temporary, ifNotExists: skip)
        var asColumnList = false
        if symbol("(") {
            var index = cursor + 1
            while index < tokens.count {
                if case .word = tokens[index].kind { index += 1 } else { break }
                if tokens[index].kind == .symbol(",") { index += 1; continue }
                if tokens[index].kind == .symbol(")") {
                    asColumnList = ["as", "on"].contains(word(index + 1 - cursor))
                }
                break
            }
        }
        if symbol("(") && !asColumnList {
            advance()
            repeat {
                if tableConstraintStart() { table.constraints.append(try tableConstraint()) }
                else { table.columns.append(try columnDefinition()) }
            } while eatSymbol(",")
            try expectSymbol(")")
            table.onCommit = try onCommit()
        } else {
            if eatSymbol("(") { table.queryColumnNames = try identifierList() }
            table.onCommit = try onCommit(); try expect("as")
            table.query = try nestedQuery()
            if eat("with") { table.withData = !eat("no"); try expect("data") }
        }
        return .createTable(table)
    }
    func onCommit() throws -> String? {
        guard eat("on") else { return nil }
        try expect("commit")
        if eat("drop") { return "drop" }
        let preserve = eat("preserve")
        if !preserve { try expect("delete") }
        try expect("rows"); return preserve ? "preserve rows" : "delete rows"
    }
    func tableConstraintStart() -> Bool {
        (word() == "primary" && word(1) == "key") || (word() == "foreign" && word(1) == "key")
            || (["check", "unique"].contains(word()) && symbol("(", 1))
            || (word() == "constraint" && ["check", "unique", "foreign", "primary", "exclude"].contains(word(2)))
            || (word() == "exclude" && (word(1) == "using" || symbol("(", 1)))
    }
    func tableConstraint() throws -> SQLConstraint {
        let start = cursor
        let name = try eat("constraint") ? identifier() : nil
        let kind: SQLConstraint.Kind
        if eat("primary") { try expect("key"); try expectSymbol("("); kind = .primaryKey(try identifierList()) }
        else if eat("unique") { try expectSymbol("("); kind = .unique(try identifierList()) }
        else if eat("check") { try expectSymbol("("); let value = try expression(); try expectSymbol(")"); kind = .check(value) }
        else if eat("foreign") {
            try expect("key"); try expectSymbol("("); let columns = try identifierList()
            kind = .foreignKey(columns: columns, try references())
        } else if eat("exclude") {
            let method = try eat("using") ? identifier() : nil
            try expectSymbol("(")
            var elements: [SQLExcludeElement] = []
            repeat {
                let columnStart = cursor
                let column = try identifier()
                let value = expressionNode(.column(qualifier: nil, name: column), from: columnStart)
                try expect("with")
                let op: String
                switch advance().kind {
                case .symbol(let spelling): op = spelling
                case .word(let spelling, _): op = spelling.description
                default: throw syntax("expected an exclusion operator")
                }
                elements.append(SQLExcludeElement(expression: value, op: op))
            } while eatSymbol(",")
            try expectSymbol(")")
            var predicate: SQLExpression?
            if eat("where") { try expectSymbol("("); predicate = try expression(); try expectSymbol(")") }
            kind = .exclude(method: method, elements: elements, predicate: predicate)
        } else { throw syntax("expected a table constraint") }
        return SQLConstraint(name: name, kind: kind, span: span(start))
    }
    func columnDefinition() throws -> SQLColumnDefinition {
        let start = cursor
        let name = try identifier(); let type = try typeSyntax()
        var column = SQLColumnDefinition(name: name, type: type, span: span(start))
        while true {
            let constraintStart = cursor
            var constraintName: SQLText?
            if eat("constraint") { constraintName = try identifier() }
            let kind: SQLConstraint.Kind
            if eat("primary") { try expect("key"); kind = .primaryKey([name]) }
            else if eat("not") { try expect("null"); kind = .notNull }
            else if eat("default") { kind = .default(try expression()) }
            else if eat("unique") { kind = .unique([name]) }
            else if eat("check") { try expectSymbol("("); let value = try expression(); try expectSymbol(")"); kind = .check(value) }
            else if word() == "references" { kind = .foreignKey(columns: [name], try references()) }
            else if eat("collate") { kind = .collate(try quotedCollation()) }
            else if eat("generated") {
                if column.constraints.contains(where: { if case .identity = $0.kind { return true }; return false }) {
                    throw syntax("multiple identity specifications for column \(name)")
                }
                kind = .identity(try identity())
            }
            else {
                if constraintName != nil { throw syntax("expected CHECK or UNIQUE after CONSTRAINT") }
                break
            }
            column.constraints.append(SQLConstraint(name: constraintName, kind: kind, span: span(constraintStart)))
        }
        column.span = span(start); return column
    }
    func fieldDefinitions() throws -> [SQLColumnDefinition] {
        var fields: [SQLColumnDefinition] = []
        repeat {
            let start = cursor
            let name = try identifier(); let type = try typeSyntax()
            var field = SQLColumnDefinition(name: name, type: type, span: span(start))
            if eat("not") { try expect("null"); field.constraints.append(.init(kind: .notNull, span: span(start))) }
            fields.append(field)
        } while eatSymbol(",")
        try expectSymbol(")"); return fields
    }
    func references() throws -> SQLReferences {
        try expect("references")
        var result = SQLReferences(table: try qualifiedName())
        if eatSymbol("(") { result.columns = try identifierList() }
        if eat("match") {
            if eat("partial") { throw error("0A000", "MATCH PARTIAL not yet implemented") }
            if eat("full") { result.match = "full" } else { try expect("simple") }
        }
        var actions = Set<String>()
        while eat("on") {
            let operation = word()
            guard ["delete", "update"].contains(operation) else { throw syntax("expected DELETE or UPDATE after ON") }
            advance()
            guard actions.insert(operation).inserted else { throw syntax("multiple ON \(operation.uppercased()) clauses") }
            let action: String
            if eat("no") { try expect("action"); action = "no action" }
            else if eat("set") { if eat("null") { action = "set null" } else { try expect("default"); action = "set default" } }
            else if eat("cascade") { action = "cascade" }
            else { try expect("restrict"); action = "restrict" }
            if operation == "delete" { result.onDelete = action } else { result.onUpdate = action }
        }
        return result
    }
    func identity() throws -> SQLIdentity {
        let always = eat("always")
        if !always { try expect("by"); try expect("default") }
        try expect("as"); try expect("identity")
        var result = SQLIdentity(always: always)
        if eatSymbol("(") { result.options = try sequenceOptions(); try expectSymbol(")") }
        return result
    }
    func createIndex(unique: Bool) throws -> SQLCreateIndex {
        let skip = try ifNotExists()
        var name: SQLText?
        let unnamed = !skip && word() == "on" && isIdentifier(1) && (symbol("(", 2) || word(2) == "using")
        if !unnamed { name = try identifier() }
        try expect("on"); let table = try qualifiedName()
        var result = SQLCreateIndex(name: name, table: table, unique: unique, ifNotExists: skip)
        if eat("using") { result.method = try identifier() }
        try expectSymbol("(")
        repeat {
            let start = cursor
            let key: SQLExpression
            if eatSymbol("(") { key = try expression(); try expectSymbol(")") }
            else if isIdentifier() && symbol("(", 1) { key = try primary() }
            else { key = expressionNode(.column(qualifier: nil, name: try identifier()), from: start) }
            var term = SQLOrderTerm(expression: key, span: span(start))
            term.descending = eat("desc"); if !term.descending { eat("asc") }
            if eat("nulls") { term.nullsFirst = eat("first"); if term.nullsFirst == false { try expect("last") } }
            term.span = span(start); result.keys.append(term)
        } while eatSymbol(",")
        try expectSymbol(")")
        if eat("where") { result.predicate = try expression() }
        return result
    }
    func sequenceOptions(allowRestart: Bool = false) throws -> [SQLSequenceOption] {
        var options: [SQLSequenceOption] = []
        var seen = Set<String>()
        while let option = try sequenceOption(allowRestart: allowRestart) {
            if !seen.insert(option.name).inserted { throw syntax("conflicting or redundant options: \(option.name.uppercased())") }
            options.append(option)
        }
        return options
    }
    func sequenceOption(allowRestart: Bool) throws -> SQLSequenceOption? {
        let keyword = word()
        guard ["as", "increment", "minvalue", "maxvalue", "start", "cache", "cycle", "no", "sequence"].contains(keyword)
                || (allowRestart && keyword == "restart") else { return nil }
        if keyword == "sequence" && word(1) != "name" { return nil }
        advance()
        var option = SQLSequenceOption(name: keyword)
        switch keyword {
        case "as": option.type = try typeSyntax()
        case "increment": eat("by"); option.number = try integerSpelling()
        case "start": eat("with"); option.number = try integerSpelling()
        case "cache", "minvalue", "maxvalue": option.number = try integerSpelling()
        case "cycle": option.enabled = true
        case "no":
            guard ["minvalue", "maxvalue", "cycle"].contains(word()) else { throw syntax("expected MINVALUE, MAXVALUE, or CYCLE after NO") }
            option.name = word(); option.enabled = false; advance()
        case "sequence": try expect("name"); option.name = "sequence name"; option.identifier = try identifier()
        case "restart":
            let with = eat("with")
            if with || symbol("-") { option.number = try integerSpelling() }
            else if case .number = peek().kind { option.number = try integerSpelling() }
        default: preconditionFailure("sequence option")
        }
        return option
    }
    func alterSequence() throws -> SQLAlterSequence {
        let skip = try ifExists(); let name = try qualifiedName()
        if eat("rename") { try expect("to"); return .init(name: name, ifExists: skip, rename: try identifier(), options: []) }
        let options = try sequenceOptions(allowRestart: true)
        guard !options.isEmpty else { throw syntax("expected a sequence option") }
        return .init(name: name, ifExists: skip, options: options)
    }
    func drop() throws -> SQLDrop {
        try expect("drop")
        let object = word()
        guard ["table", "view", "index", "sequence", "type"].contains(object) else { throw syntax("expected keyword 'table'") }
        advance()
        let skip = try object == "index" ? false : ifExists()
        var names = [try qualifiedName()]
        if object != "index" { while eatSymbol(",") { names.append(try qualifiedName()) } }
        let cascading = object == "index" ? false : cascade()
        return SQLDrop(object: object, names: names, ifExists: skip, cascade: cascading)
    }
    func alterTable() throws -> SQLAlterTable {
        let skip = try ifExists(); let name = try qualifiedName()
        var actions: [SQLAlterTableAction] = []
        if eat("rename") {
            if eat("to") { actions.append(.renameTable(try identifier())) }
            else {
                let constraint = eat("constraint"); if !constraint { eat("column") }
                let old = try identifier(); try expect("to"); let new = try identifier()
                actions.append(constraint ? .renameConstraint(old, new) : .renameColumn(old, new))
            }
        } else {
            repeat { actions.append(try alterTableAction()) } while eatSymbol(",")
        }
        return SQLAlterTable(name: name, ifExists: skip, actions: actions)
    }
    func alterTableAction() throws -> SQLAlterTableAction {
        if eat("add") {
            if tableConstraintStart() { return .addConstraint(try tableConstraint()) }
            eat("column"); let skip = try ifNotExists()
            return .addColumn(try columnDefinition(), ifNotExists: skip)
        }
        if eat("drop") {
            if eat("primary") { try expect("key"); return .dropPrimaryKey(cascade: cascade()) }
            let constraint = eat("constraint"); if !constraint { eat("column") }
            let skip = try ifExists(); let name = try identifier(); let cascading = cascade()
            return constraint ? .dropConstraint(name, ifExists: skip, cascade: cascading) : .dropColumn(name, ifExists: skip, cascade: cascading)
        }
        try expect("alter"); eat("column"); let column = try identifier()
        if eat("drop") {
            if eat("default") { return .dropDefault(column) }
            if eat("not") { try expect("null"); return .setNotNull(column, false) }
            try expect("identity"); return .dropIdentity(column, ifExists: try ifExists())
        }
        if eat("add") { try expect("generated"); return .addIdentity(column, try identity()) }
        if eat("set") {
            if eat("default") { return .setDefault(column, try expression()) }
            if eat("not") { try expect("null"); return .setNotNull(column, true) }
            if eat("data") {
                try expect("type"); let type = try typeSyntax()
                return .setType(column, type, using: try eat("using") ? expression() : nil)
            }
            cursor -= 1
        }
        if eat("type") {
            let type = try typeSyntax(); return .setType(column, type, using: try eat("using") ? expression() : nil)
        }
        var always: Bool?; var options: [SQLSequenceOption] = []
        var seen = Set<String>()
        while word() == "set" || word() == "restart" {
            if eat("set") {
                if eat("generated") {
                    if always != nil { throw syntax("multiple SET GENERATED clauses") }
                    always = eat("always"); if always == false { try expect("by"); try expect("default") }
                } else if ["as", "restart", "sequence"].contains(word()) { throw syntax("sequence option \"\(word())\" not supported here") }
                else if let option = try sequenceOption(allowRestart: false) {
                    guard seen.insert(option.name).inserted else { throw syntax("conflicting or redundant options: \(option.name.uppercased())") }
                    options.append(option)
                }
                else { throw syntax("expected a sequence option") }
            } else if let option = try sequenceOption(allowRestart: true) {
                guard seen.insert(option.name).inserted else { throw syntax("conflicting or redundant options: \(option.name.uppercased())") }
                options.append(option)
            }
        }
        guard always != nil || !options.isEmpty else { throw syntax("expected an ALTER COLUMN action") }
        return .alterIdentity(column, always: always, options)
    }
    func viewOptions() throws -> [SQLViewOption] {
        try expectSymbol("(")
        var result: [SQLViewOption] = []
        repeat {
            let name = try identifier()
            var value: SQLText?
            if eatSymbol("=") {
                switch peek().kind {
                case .word(let text, _), .string(let text): advance(); value = text
                case .number, .symbol("-"): value = SQLText(validatedUTF8: Array(try integerSpelling().utf8))
                default: throw syntax("expected a view option value")
                }
            }
            result.append(SQLViewOption(name: name, value: value))
        } while eatSymbol(",")
        try expectSymbol(")"); return result
    }
    func createView(replace: Bool, temporary: Bool, recursive: Bool) throws -> SQLCreateView {
        let skip = try ifNotExists()
        if replace && skip { throw syntax("CREATE OR REPLACE VIEW cannot be combined with IF NOT EXISTS") }
        let name = try qualifiedName()
        let columns = try eatSymbol("(") ? identifierList() : []
        if recursive && columns.isEmpty { throw syntax("CREATE RECURSIVE VIEW requires a column list") }
        let options = try eat("with") ? viewOptions() : []
        try expect("as")
        let oldAllowed = intoAllowed; let oldView = intoView
        intoAllowed = true; intoView = true
        try deepen(); let body = try query(); depth -= 1
        intoAllowed = oldAllowed; intoView = oldView
        var check: String?
        if eat("with") {
            check = eat("local") ? "local" : "cascaded"
            if check == "cascaded" { eat("cascaded") }
            try expect("check"); try expect("option")
            if recursive { throw error("0A000", "WITH CHECK OPTION not supported on recursive views") }
        }
        return SQLCreateView(name: name, replace: replace, temporary: temporary, recursive: recursive,
                             ifNotExists: skip, columns: columns, options: options, query: body, checkOption: check)
    }
    func alterView() throws -> SQLAlterView {
        let skip = try ifExists(); let name = try qualifiedName()
        var actions: [SQLAlterViewAction] = []
        if eat("rename") {
            if eat("to") { actions.append(.rename(try identifier())) }
            else { eat("column"); let old = try identifier(); try expect("to"); actions.append(.renameColumn(old, try identifier())) }
        } else {
            repeat {
                if eat("alter") {
                    eat("column"); let name = try identifier()
                    if eat("set") { try expect("default"); actions.append(.setDefault(name, try expression())) }
                    else { try expect("drop"); try expect("default"); actions.append(.dropDefault(name)) }
                } else if word() == "set" && symbol("(", 1) { advance(); actions.append(.setOptions(try viewOptions())) }
                else if eat("reset") { try expectSymbol("("); actions.append(.resetOptions(try identifierList())) }
                else if ["owner", "set"].contains(word()) { throw error("0A000", "this form of ALTER VIEW is not supported") }
                else { throw syntax("expected an ALTER VIEW action") }
            } while eatSymbol(",")
        }
        return SQLAlterView(name: name, ifExists: skip, actions: actions)
    }
    func explain() throws -> SQLStatement {
        try expect("explain")
        var options = SQLExplain()
        if eat("analyze") { options.analyze = true }
        else if symbol("(") && !queryStart(at: cursor + 1) {
            advance(); var seen = Set<String>()
            repeat {
                let name = word()
                guard ["analyze", "verbose", "costs", "lane"].contains(name) else { throw syntax("unrecognized EXPLAIN option") }
                advance()
                guard seen.insert(name).inserted else { throw syntax("duplicate EXPLAIN option: \(name)") }
                let value: Bool
                if eat("false") || eat("off") { value = false }
                else { if !eat("true") { eat("on") }; value = true }
                switch name {
                case "analyze": options.analyze = value
                case "verbose": options.verbose = value
                case "costs": options.costs = value
                default: options.lane = value
                }
            } while eatSymbol(",")
            try expectSymbol(")")
        }
        let body = try statement(queryOrDMLOnly: word() != "create")
        switch body {
        case .query, .with, .insert, .update, .delete, .merge: break
        case .createTable(let table):
            guard table.query != nil else { throw syntax("EXPLAIN requires a query or DML statement") }
        default: throw syntax("EXPLAIN requires a query or DML statement")
        }
        return .explain(options, body)
    }
    func reindex() throws -> SQLReindex {
        try expect("reindex")
        var concurrent = false; var verbose = false
        if eatSymbol("(") {
            repeat {
                let option = word(); advance()
                if option == "tablespace" { throw error("0A000", "REINDEX TABLESPACE is not supported: jed has no tablespaces") }
                guard ["verbose", "concurrently"].contains(option) else { throw syntax("unrecognized REINDEX option") }
                var enabled = true
                if eat("false") || eat("off") { enabled = false }
                else if !eat("true") && !eat("on") {
                    if case .number(let raw) = peek().kind { advance(); enabled = raw != "0" }
                    else if case .string(let text) = peek().kind { advance(); enabled = !["0", "false", "off", "no"].contains(text.asciiLowercased.description) }
                }
                if option == "verbose" { verbose = enabled } else { concurrent = enabled }
            } while eatSymbol(",")
            try expectSymbol(")")
        }
        let object = word()
        if object == "schema" { throw error("0A000", "REINDEX SCHEMA is not supported: jed has no schemas") }
        guard ["index", "table", "database", "system"].contains(object) else { throw syntax("expected INDEX, TABLE, DATABASE, or SYSTEM") }
        advance()
        if word() == "concurrently" && (!["index", "table"].contains(object) || isIdentifier(1)) { advance(); concurrent = true }
        let name = try isIdentifier() ? qualifiedName() : nil
        if name == nil && ["index", "table"].contains(object) { throw syntax("expected an identifier") }
        return SQLReindex(object: object, name: name, concurrently: concurrent, verbose: verbose)
    }
    func assignmentList() throws -> [SQLAssignment] {
        var result: [SQLAssignment] = []
        repeat {
            let name = try identifier(); try expectSymbol("=")
            let value = try eat("default") ? nil : expression()
            result.append(SQLAssignment(column: name, value: value))
        } while eatSymbol(",")
        return result
    }
    func returning() throws -> SQLReturning? {
        guard eat("returning") else { return nil }
        var oldAlias: SQLText?; var newAlias: SQLText?
        if word() == "with" && symbol("(", 1) && ["old", "new"].contains(word(2)) && word(3) == "as" {
            advance(); advance()
            repeat {
                let old = eat("old"); if !old { try expect("new") }
                try expect("as"); let name = try identifier()
                if old { if oldAlias != nil { throw syntax("duplicate OLD alias") }; oldAlias = name }
                else { if newAlias != nil { throw syntax("duplicate NEW alias") }; newAlias = name }
            } while eatSymbol(",")
            try expectSymbol(")")
        }
        return SQLReturning(oldAlias: oldAlias, newAlias: newAlias, items: try selectItems())
    }
    func overriding() throws -> String? {
        guard eat("overriding") else { return nil }
        let system = eat("system"); if !system { try expect("user") }
        try expect("value"); return system ? "system" : "user"
    }
    func insert() throws -> SQLInsert {
        try expect("insert"); try expect("into")
        let table = try qualifiedName(); let alias = try eat("as") ? identifier() : nil
        var columns: [SQLText] = []
        // A parenthesized SELECT/VALUES source is not the optional column list.
        let parenQuery = symbol("(", 1) || withStart(at: cursor + 1)
            || (word(1) == "values" && symbol("(", 2))
            || (word(1) == "select" && !symbol(",", 2) && !symbol(")", 2))
        if symbol("(") && !parenQuery { advance(); columns = try identifierList() }
        let override = try overriding()
        let source: SQLInsert.Source
        if eat("default") {
            if !columns.isEmpty || override != nil { throw syntax("DEFAULT VALUES cannot be combined with a column list or OVERRIDING") }
            try expect("values"); source = .defaultValues
        }
        else if let rows = try insertValuesSource() { source = .values(rows) }
        else {
            let oldAllowed = intoAllowed; intoAllowed = false; intoNesting += 1
            defer { intoAllowed = oldAllowed; intoNesting -= 1 }
            source = .query(try query())
        }
        var result = SQLInsert(table: table, alias: alias, columns: columns, overriding: override, source: source)
        if eat("on") { try expect("conflict"); result.conflict = try conflict() }
        result.returning = try returning(); return result
    }
    func insertValuesSource() throws -> [[SQLExpression?]]? {
        let start = cursor; let oldDepth = depth; let oldFurthest = furthest
        var parens = 0
        while symbol("(", parens) { parens += 1 }
        guard word(parens) == "values" else { return nil }
        for _ in 0..<parens { advance(); try deepen() }
        try expect("values")
        var rows: [[SQLExpression?]] = []
        repeat {
            try expectSymbol("(")
            var row: [SQLExpression?] = []
            repeat { row.append(try eat("default") ? nil : expression()) } while eatSymbol(",")
            try expectSymbol(")"); rows.append(row)
        } while eatSymbol(",")
        let follow: Set<String> = ["order", "limit", "offset", "union", "intersect", "except"]
        for _ in 0..<parens {
            if follow.contains(word()) {
                cursor = start; depth = oldDepth; furthest = oldFurthest; return nil
            }
            try expectSymbol(")")
        }
        if follow.contains(word()) {
            cursor = start; depth = oldDepth; furthest = oldFurthest; return nil
        }
        depth = oldDepth; return rows
    }
    func conflict() throws -> SQLConflict {
        var columns: [SQLConflictColumn] = []
        var constraint: SQLText?; var predicate: SQLExpression?
        if eatSymbol("(") {
            repeat {
                var column = SQLConflictColumn(name: try identifier())
                if eat("collate") { column.collation = try quotedCollation() }
                if isIdentifier() && !["asc", "desc", "nulls"].contains(word()) { column.operatorClass = try identifier() }
                if eat("asc") { column.descending = false } else if eat("desc") { column.descending = true }
                if eat("nulls") { column.nullsFirst = eat("first"); if column.nullsFirst == false { try expect("last") } }
                columns.append(column)
            } while eatSymbol(",")
            try expectSymbol(")"); if eat("where") { predicate = try expression() }
        } else if eat("on") { try expect("constraint"); constraint = try identifier() }
        try expect("do")
        let nothing = eat("nothing")
        var result = SQLConflict(columns: columns, constraint: constraint, predicate: predicate, doNothing: nothing)
        if !nothing {
            try expect("update"); try expect("set"); result.assignments = try assignmentList()
            if eat("where") { result.updatePredicate = try expression() }
        }
        return result
    }
    func update() throws -> SQLUpdate {
        try expect("update"); let table = try qualifiedName(); try expect("set")
        let assignments = try assignmentList()
        let predicate = try eat("where") ? expression() : nil
        return SQLUpdate(table: table, assignments: assignments, predicate: predicate, returning: try returning())
    }
    func deleteStatement() throws -> SQLDelete {
        try expect("delete"); try expect("from"); let table = try qualifiedName()
        let predicate = try eat("where") ? expression() : nil
        return SQLDelete(table: table, predicate: predicate, returning: try returning())
    }
    func merge() throws -> SQLMerge {
        try expect("merge"); try expect("into"); let table = try qualifiedName()
        let alias = try (eat("as") || mayAlias(stops: ["using"])) ? identifier() : nil
        try expect("using"); let source = try fromItem(); try expect("on"); let condition = try expression()
        var clauses: [SQLMergeClause] = []
        while eat("when") {
            var match = "matched"
            if eat("not") {
                try expect("matched"); match = "not matched by target"
                if eat("by") { if eat("source") { match = "not matched by source" } else { try expect("target") } }
            } else { try expect("matched") }
            let filter = try eat("and") ? expression() : nil
            try expect("then")
            let action: SQLMergeClause.Action
            if eat("do") { try expect("nothing"); action = .nothing }
            else if eat("delete") { action = .delete }
            else if eat("update") { try expect("set"); action = .update(try assignmentList()) }
            else {
                try expect("insert")
                let columns = try eatSymbol("(") ? identifierList() : []
                let override = try overriding()
                var values: [SQLExpression?]?
                if eat("default") { try expect("values") }
                else {
                    try expect("values"); try expectSymbol("(")
                    var row: [SQLExpression?] = []
                    repeat { row.append(try eat("default") ? nil : expression()) } while eatSymbol(",")
                    try expectSymbol(")"); values = row
                }
                action = .insert(columns: columns, overriding: override, values: values)
            }
            switch action {
            case .insert: if match != "not matched by target" { throw syntax("INSERT is not allowed in a MATCHED clause") }
            case .update, .delete: if match == "not matched by target" { throw syntax("UPDATE or DELETE is not allowed in a NOT MATCHED clause") }
            case .nothing: break
            }
            clauses.append(SQLMergeClause(match: match, condition: filter, action: action))
        }
        guard !clauses.isEmpty else { throw syntax("MERGE requires at least one WHEN clause") }
        var unconditional = Set<String>()
        for clause in clauses {
            if unconditional.contains(clause.match) { throw syntax("unreachable WHEN clause specified after unconditional WHEN clause") }
            if clause.condition == nil { unconditional.insert(clause.match) }
        }
        return SQLMerge(table: table, alias: alias, source: source, condition: condition, clauses: clauses, returning: try returning())
    }
}
