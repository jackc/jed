/// JSON_TABLE keeps its declared nested tree while exposing one flat relation
/// shape. Sibling nested paths union their rows; only an entirely empty sibling
/// union contributes the parent with NULL child fields.
internal final class JSONTablePlan {
    indirect enum Column {
        case ordinality(Int)
        case value(Int, TypeDescriptor, SQLText, exists: Bool, wrapper: String?,
                   empty: ResolvedJSONExpression.Behavior?, failure: ResolvedJSONExpression.Behavior?)
        case nested(SQLText, [Column])
    }
    let input: ResolvedExpression
    let path: SQLText
    let passing: [ResolvedJSONExpression.Passing]
    let tree: [Column]
    let columns: [ExpressionColumn]
    let expressions: [ResolvedExpression]
    var lateralColumns: Set<Int> {
        var result: Set<Int> = []
        StoredExpressionDependencies.walk(expressions) {
            if case .column(let index) = $0.node { result.insert(index) }
            if case .subquery(let query) = $0.node {
                for reference in query.outerReferences where reference.depth == 1 { result.insert(reference.index) }
            }
        }
        return result
    }
    private init(input: ResolvedExpression, path: SQLText, passing: [ResolvedJSONExpression.Passing],
                 tree: [Column], columns: [ExpressionColumn], expressions: [ResolvedExpression]) {
        self.input = input; self.path = path; self.passing = passing; self.tree = tree
        self.columns = columns; self.expressions = expressions
    }
    static func resolve(_ input: SQLFrom, syntax: SQLJSONTable, resolver: ExpressionResolver) throws -> JSONTablePlan {
        let value = try resolver.resolve(syntax.context, expected: TypeDescriptor(.scalar(.jsonb)))
        guard value.type == nil || [.scalar(.text), .scalar(.json), .scalar(.jsonb)].contains(value.type!.type) else {
            throw JedError("42804", "JSON_TABLE context must be json, jsonb, or text")
        }
        let path = try resolver.resolve(syntax.path, expected: TypeDescriptor(.scalar(.jsonpath)))
        guard path.type?.type == .scalar(.jsonpath) else { throw JedError("42804", "JSON_TABLE path must be a constant jsonpath") }
        guard case .constant(.jsonpath(let rootPath)) = path.node else { throw JedError("0A000", "a non-constant JSON_TABLE path is not supported") }
        let passing = try syntax.passing.map { binding -> ResolvedJSONExpression.Passing in
            let value = try resolver.resolve(binding.value)
            try resolver.jsonEncoding(binding.encoding, type: value.type)
            if !binding.formatJSON, let type = value.type?.type, [.scalar(.date), .scalar(.timestamp), .scalar(.timestamptz)].contains(type) {
                throw JedError("0A000", "jsonpath datetime items are not supported")
            }
            return .init(name: binding.name, value: value, formatJSON: binding.formatJSON)
        }
        var seen: Set<SQLText> = []
        func names(_ columns: [SQLJSONTableColumn]) throws {
            for column in columns {
                let name: SQLText
                switch column {
                case .ordinality(let n), .regular(let n, _, _, _, _, _, _, _, _): name = n
                case .nested(_, _, let children): try names(children); continue
                }
                guard seen.insert(name).inserted else { throw JedError("42712", "duplicate JSON_TABLE column or path name: \(name)") }
            }
        }
        try names(syntax.columns)
        var columns: [ExpressionColumn] = [], expressions = [value] + passing.map(\.value)
        let qualifier = input.alias ?? SQLText(validatedUTF8: Array("json_table".utf8))
        func compile(_ path: SQLText) throws -> SQLText { try JSONPathProgram.compile(path).text }
        func build(_ syntax: [SQLJSONTableColumn]) throws -> [Column] {
            var result: [Column] = [], ordinal = false
            for column in syntax {
                switch column {
                case .ordinality(let name):
                    guard !ordinal else { throw JedError("42601", "only one FOR ORDINALITY column is allowed") }
                    ordinal = true; result.append(.ordinality(columns.count))
                    columns.append(ExpressionColumn(name: name, qualifier: qualifier, type: TypeDescriptor(.scalar(.i32)), index: columns.count))
                case .regular(let name, let syntax, _, let exists, let path, let wrapper, let keepQuotes, let onEmpty, let onError):
                    let type = try TypeRules.resolve(syntax, composites: resolver.composites)
                    guard case .scalar = type.type else { throw JedError("0A000", "JSON_TABLE column must have a supported scalar type") }
                    guard keepQuotes else { throw JedError("0A000", "JSON_TABLE OMIT QUOTES is not supported") }
                    let structural = type.type == .scalar(.json) || type.type == .scalar(.jsonb)
                    guard structural || wrapper == nil || wrapper == "without" else { throw JedError("0A000", "a wrapper on a scalar JSON_TABLE column is not supported") }
                    let source = try path ?? SQLText("$." + SQLJSON.render(.string(name)).description)
                    let compiled = try compile(source)
                    let empty = try resolver.jsonBehavior(onEmpty, returning: type, exists: exists)
                    let failure = try resolver.jsonBehavior(onError, returning: type, exists: exists)
                    expressions += [empty?.value, failure?.value].compactMap { $0 }
                    result.append(.value(columns.count, type, compiled, exists: exists, wrapper: wrapper, empty: empty, failure: failure))
                    columns.append(ExpressionColumn(name: name, qualifier: qualifier, type: type, index: columns.count))
                case .nested(let path, _, let children): result.append(.nested(try compile(path), try build(children)))
                }
            }
            return result
        }
        let tree = try build(syntax.columns)
        return JSONTablePlan(input: value, path: rootPath, passing: passing, tree: tree, columns: columns, expressions: expressions)
    }
    func produce(context: ExpressionContext, visit: ([SQLValue]) throws -> Void) throws {
        var bindings: [(SQLText, SQLJSONValue)] = []
        for binding in passing { bindings.append((binding.name, try ResolvedJSONExpression.passingValue(binding, execution: context))) }
        let value = try context.evaluate(input)
        guard !value.isNull else { return }
        let document = try JSONFunctions.node(value)
        let frozen = bindings.map { ResolvedJSONExpression.Passing(name: $0.0,
            value: ResolvedExpression(type: TypeDescriptor(.scalar(.jsonb)), node: .constant(.jsonb($0.1)), position: 0), formatJSON: false) }
        func sequence(_ path: SQLText, item: SQLJSONValue) throws -> [SQLJSONValue] {
            let program = try JSONPathProgram.compile(path, meter: context.meter, regexCompiler: JsonScalarFunctions.regexCompiler(context))
            let cursor = JSONPathFunctions.query(context: item, program: program, passing: bindings, meter: context.meter)
            defer { cursor.close() }
            var items: [SQLJSONValue] = []
            do {
                while let value = try cursor.next() { items.append(value) }
                return items
            } catch let error as JedError where error.code.hasPrefix("22") { return [] }
        }
        func expand(_ tree: [Column], items: [SQLJSONValue], base: [SQLValue], emit: ([SQLValue]) throws -> Void) throws {
            for (ordinal, item) in items.enumerated() {
                try context.meter.pollStep()
                var row = base, children: [(SQLText, [Column])] = []
                for column in tree {
                    switch column {
                    case .ordinality(let index): row[index] = .integer(Int64(ordinal + 1))
                    case .nested(let path, let child): children.append((path, child))
                    case .value(let index, let type, let path, let exists, let wrapper, let empty, let failure):
                        let structural = type.type == .scalar(.json) || type.type == .scalar(.jsonb)
                        let query = ResolvedJSONExpression(name: exists ? "json_exists" : structural ? "json_query" : "json_value",
                            context: ResolvedExpression(type: TypeDescriptor(.scalar(.jsonb)), node: .column(0), position: 0),
                            path: ResolvedExpression(type: TypeDescriptor(.scalar(.jsonpath)), node: .constant(.jsonpath(path)), position: 0),
                            result: type, passing: frozen, wrapper: wrapper, uniqueKeys: false, onEmpty: empty, onError: failure)
                        var current = context; current.column = { _ in .jsonb(item) }
                        let value = try query.evaluate(context: current, chargePathResults: false)
                        if exists, !value.isNull {
                            guard case .boolean(let truth) = value else { throw JedError("XX000", "JSON_TABLE EXISTS returned a non-boolean") }
                            if case .scalar(let scalar) = type.type, scalar.integerWidth != nil { row[index] = .integer(truth ? 1 : 0) }
                            else if type.type == .scalar(.boolean) { row[index] = value }
                            else { throw JedError("0A000", "an EXISTS JSON_TABLE column must be boolean or integer") }
                        } else { row[index] = value }
                    }
                }
                var emitted = false
                for (path, child) in children {
                    try expand(child, items: sequence(path, item: item), base: row) { value in emitted = true; try emit(value) }
                }
                if !emitted { try emit(row) }
            }
        }
        // This bounded producer completes all paths/defaults before row charges
        // and before the enclosing relation admits its output (memory.md §5.1).
        var rows: [[SQLValue]] = []
        try expand(tree, items: sequence(path, item: document), base: Array(repeating: .null, count: columns.count)) { rows.append($0) }
        for row in rows { context.meter.charge(1); try context.meter.checkpoint(); try visit(row) }
    }
}
