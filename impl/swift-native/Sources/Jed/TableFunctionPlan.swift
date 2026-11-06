/// FROM functions have their own catalog mold. Arguments resolve once; the
/// relation producer charges generated rows before admitting their buffers.
internal final class TableFunctionPlan {
    let function: ResolvedFunction
    let columns: [ExpressionColumn]
    let recordFields: [TypeField]?
    var expressions: [ResolvedExpression] { function.arguments }
    var lateralColumns: Set<Int> {
        var result: Set<Int> = []
        StoredExpressionDependencies.walk(expressions) { expression in
            if case .column(let index) = expression.node { result.insert(index) }
            if case .subquery(let query) = expression.node {
                for reference in query.outerReferences where reference.depth == 1 { result.insert(reference.index) }
            }
        }
        return result
    }
    init(function: ResolvedFunction, columns: [ExpressionColumn], recordFields: [TypeField]?) {
        self.function = function; self.columns = columns; self.recordFields = recordFields
    }
    static func resolve(_ input: SQLFrom, syntax: SQLExpression, resolver: ExpressionResolver) throws -> TableFunctionPlan {
        guard case .call(let call) = syntax.kind else { throw JedError("42883", "FROM function does not exist") }
        let name = call.name.description
        guard !call.star, !call.distinct, call.orderBy.isEmpty, call.withinGroup.isEmpty,
              call.filter == nil, call.over == nil, call.overName == nil,
              call.arguments.allSatisfy({ !$0.variadic }) else { throw JedError("42883", "invalid table function arguments") }
        if JSONRecordPlan.names.contains(name) { return try JSONRecordPlan.resolve(input, call: call, resolver: resolver) }
        let path = name == "jsonb_path_query" || name == "jsonb_path_query_tz"
        let facts = GeneratedSpec.functions.filter { $0.kind == "set_returning" && $0.name == (path ? "jsonb_path_query" : name) }
        guard let fact = facts.first(where: { path || FunctionResolver.strings($0.metadata, "arg_families").count == call.arguments.count }) else {
            throw JedError("42883", "table function does not exist: \(call.name)")
        }
        if !path, call.arguments.contains(where: { $0.name != nil }) { throw JedError("42883", "named arguments are not supported by this table function") }
        var arguments: [ResolvedExpression], resultType: TypeDescriptor
        if path {
            guard let resolved = try JsonFunctionResolver.resolve(call, setReturning: true, resolve: { try resolver.resolve($0, expected: $1) }) else {
                throw JedError("XX000", "missing JSONPath table function resolver")
            }
            arguments = resolved.arguments; resultType = resolved.resultType
        } else if name == "generate_series" {
            arguments = try call.arguments.map { try resolver.resolve($0.value) }
            var scalar: ScalarType = .i16
            for argument in arguments {
                guard let type = argument.type?.type else { continue }
                guard case .scalar(let integer) = type, let width = integer.integerWidth else { throw JedError("42883", "generate_series requires integer arguments") }
                if width > scalar.integerWidth! { scalar = integer }
            }
            if arguments.allSatisfy({ $0.type == nil }) { scalar = .i64 }
            resultType = TypeDescriptor(.scalar(scalar))
            arguments = try call.arguments.map { try resolver.resolve($0.value, expected: resultType) }
        } else if name == "unnest" {
            arguments = try call.arguments.map { try resolver.resolve($0.value) }
            guard arguments[0].type != nil else { throw JedError("42P18", "could not determine polymorphic type") }
            guard case .array(let element) = arguments[0].type?.type else { throw JedError("42883", "unnest requires an array") }
            resultType = TypeDescriptor(element)
        } else {
            let families = FunctionResolver.strings(fact.metadata, "arg_families")
            arguments = try call.arguments.enumerated().map { index, argument in
                guard let scalar = FunctionResolver.familyType(families[index]) else { throw JedError("XX000", "unknown table function family") }
                let target = TypeDescriptor(.scalar(scalar))
                let value = try resolver.resolve(argument.value, expected: target)
                guard value.type?.type == nil || value.type?.type == target.type else { throw JedError("42883", "table function argument has incompatible type") }
                return value
            }
            let spelling = FunctionResolver.text(fact.metadata, "result") ?? "text"
            if spelling == "text[]" { resultType = TypeDescriptor(.array(.scalar(.text))) }
            else if let scalar = ScalarType.allCases.first(where: { $0.name == spelling }) { resultType = TypeDescriptor(.scalar(scalar)) }
            else { throw JedError("XX000", "unknown table function result") }
        }
        try resolver.checkFunction?(call.name)
        var function = ResolvedFunction(metadata: FunctionSpecFact(category: fact.category, name: name, kind: fact.kind, metadata: fact.metadata),
            arguments: arguments, resultType: resultType, evaluationOrder: Array(arguments.indices), constantArguments: arguments.map(FunctionResolver.isConstant))
        function.regex = try ResolvedRegexFunction.prepare(function, unicode: resolver.unicode)
        let qualifier = input.alias ?? call.name
        let columnName = FunctionResolver.text(fact.metadata, "column_source") == "function" ? qualifier
            : SQLText(validatedUTF8: Array((FunctionResolver.text(fact.metadata, "column") ?? name).utf8))
        var fields: [TypeField]?, columns: [ExpressionColumn]
        if name == "unnest", case .record(let actual) = resultType.type {
            guard !input.columnDefinitions.isEmpty else { throw JedError("42601", "a column definition list is required for functions returning record") }
            var seen: Set<SQLText> = []
            let declared = try input.columnDefinitions.map { column -> TypeField in
                guard seen.insert(column.name).inserted else { throw JedError("42701", "column name specified more than once") }
                return TypeField(name: column.name, descriptor: try TypeRules.resolve(column.type, composites: resolver.composites))
            }
            guard declared.count == actual.count, zip(actual, declared).allSatisfy({ recordCompatible($0.descriptor.type, $1.descriptor.type) }) else {
                throw JedError("42804", "function return row and column definition list do not match")
            }
            fields = declared
            columns = declared.enumerated().map { ExpressionColumn(name: $0.element.name, qualifier: qualifier, type: $0.element.descriptor, index: $0.offset) }
        } else {
            guard input.columnDefinitions.isEmpty else { throw JedError("42601", "column definition list is redundant for a function with a known result type") }
            let each = name == "json_each" || name == "jsonb_each" || name == "json_each_text" || name == "jsonb_each_text"
            columns = each ? [ExpressionColumn(name: SQLText(validatedUTF8: Array("key".utf8)), qualifier: qualifier, type: TypeDescriptor(.scalar(.text)), index: 0)] : []
            columns.append(ExpressionColumn(name: columnName, qualifier: qualifier, type: resultType, index: columns.count, scalarFunction: !each))
        }
        return TableFunctionPlan(function: function, columns: columns, recordFields: fields)
    }
    private static func recordCompatible(_ actual: SQLType, _ declared: SQLType) -> Bool {
        if actual == declared { return true }
        switch (actual, declared) {
        case (.scalar(let a), .scalar(let b)): return a.integerWidth != nil && b.integerWidth != nil
        case (.array(let a), .array(let b)): return recordCompatible(a, b)
        default: return false
        }
    }
    func produce(context: ExpressionContext, visit: ([SQLValue]) throws -> Void) throws {
        let meter = context.meter, name = function.name
        let kernel = name.hasPrefix("regexp_") || name.hasPrefix("jsonb_path_query")
        if kernel { meter.charge(1); try meter.step() }
        let values = try function.arguments.map { try context.evaluate($0) }
        if JSONRecordPlan.names.contains(name) { try JSONRecordPlan.produce(self, values: values, context: context, visit: visit); return }
        guard !values.contains(where: \.isNull) else { return }
        func emit(_ row: [SQLValue]) throws { meter.charge(1); try meter.checkpoint(); try visit(row) }
        if name == "generate_series" {
            guard case .integer(let start) = values[0], case .integer(let stop) = values[1] else { throw JedError("42804", "series values must be integers") }
            let step: Int64
            if values.count == 3, case .integer(let value) = values[2] { step = value } else { step = 1 }
            guard step != 0 else { throw JedError("22023", "step size cannot be equal to zero") }
            var value = start
            while step > 0 ? value <= stop : value >= stop {
                try emit([.integer(value)])
                let next = value.addingReportingOverflow(step)
                if next.overflow { break }; value = next.partialValue
            }
        } else if name == "unnest" {
            guard case .array(let array) = values[0] else { throw JedError("42804", "unnest argument must be an array") }
            for element in array.elements {
                if let fields = recordFields {
                    if element.isNull { try emit(Array(repeating: .null, count: fields.count)) }
                    else {
                        guard case .composite(let row) = element else { throw JedError("42804", "record array element is invalid") }
                        let converted = try row.enumerated().map { index, value in
                            try ContainerCasts.store(value, to: fields[index].descriptor, context: context.scalar)
                        }
                        try emit(converted)
                    }
                } else { try emit([element]) }
            }
        } else if name.hasPrefix("regexp_") {
            let state = context.patterns.regexExecution(function), unicode = context.functions.unicode
            let rows = try RegexFunctions.rows(name: name, args: values, program: state.program, chargeCompile: state.chargeCompile,
                scratch: state.scratch, fold: { unicode.simpleLower($0) }, meter: meter)
            defer { rows.close() }
            while let value = try rows.next() { try visit([value]) }
        } else if name.hasPrefix("jsonb_path_query") {
            guard case .jsonpath(let path) = values[1] else { throw JedError("42804", "JSONPath argument required") }
            let silent = if case .boolean(let flag) = values[3] { flag } else { false }
            let rows = try JSONPathFunctions.query(context: JSONFunctions.node(values[0]), path: path,
                variables: JSONFunctions.node(values[2]), silent: silent, meter: meter, regexCompiler: JsonScalarFunctions.regexCompiler(context))
            defer { rows.close() }
            var items: [SQLJSONValue] = []
            while let value = try rows.next() { items.append(value) }
            // Relation generation follows complete path evaluation. The cursor
            // remains the retained kernel seam used by other streaming callers.
            for value in items { try emit([.jsonb(value)]) }
        } else {
            let array = name.contains("array_elements"), keys = name.contains("object_keys"), text = name.hasSuffix("_text")
            try JSONFunctions.each(values[0], array: array, asText: text, keysOnly: keys, meter: meter,
                constant: function.constantArguments[0], beforeElement: { meter.charge(1); try meter.checkpoint() }) { key, value in
                try visit(keys ? [.text(key!)] : array ? [value] : [.text(key!), value]); return true
            }
        }
    }
}
