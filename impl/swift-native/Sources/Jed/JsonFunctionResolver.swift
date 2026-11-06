/// JSON signatures with recursive arguments and non-integer defaults are
/// resolved by hand. The scalar catalog still owns ordinary fixed signatures.
internal enum JsonFunctionResolver {
    static let pathNames: Set<String> = ["jsonb_path_exists", "jsonb_path_match", "jsonb_path_query_array", "jsonb_path_query_first", "jsonb_path_exists_tz", "jsonb_path_match_tz", "jsonb_path_query_array_tz", "jsonb_path_query_first_tz"]
    static func resolve(_ call: SQLCall, setReturning: Bool = false, resolve: FunctionResolver.Resolve) throws -> ResolvedFunction? {
        let name = call.name.description
        let isPath = pathNames.contains(name) || (setReturning && ["jsonb_path_query", "jsonb_path_query_tz"].contains(name))
        let mutator = ["jsonb_set", "jsonb_insert", "jsonb_set_lax"].contains(name)
        let object = ["json_object", "jsonb_object"].contains(name)
        let strip = ["json_strip_nulls", "jsonb_strip_nulls"].contains(name)
        guard mutator || object || strip || isPath || name == "row_to_json" || name == "array_to_json" else { return nil }
        if !mutator && !isPath, call.arguments.contains(where: { $0.name != nil }) { throw JedError("42883", "named arguments are not supported by this JSON function") }
        guard !call.star, !call.distinct, call.orderBy.isEmpty, call.withinGroup.isEmpty, call.filter == nil,
              call.over == nil, call.overName == nil, call.arguments.allSatisfy({ !$0.variadic }) else { throw JedError("42883", "invalid JSON function arguments") }
        let json = TypeDescriptor(.scalar(name.hasPrefix("jsonb") ? .jsonb : .json))
        let text = TypeDescriptor(.scalar(.text)), boolean = TypeDescriptor(.scalar(.boolean))
        let array = TypeDescriptor(.array(.scalar(.text)))
        var names: [String], targets: [TypeDescriptor?], defaults: [SQLValue], minimum: Int
        if mutator {
            names = ["jsonb_in", "path", "replacement", name == "jsonb_insert" ? "insert_after" : "create_if_missing"]
            targets = [json, array, json, boolean]; defaults = [.boolean(name != "jsonb_insert")]; minimum = 3
            if name == "jsonb_set_lax" { names.append("null_value_treatment"); targets.append(text); defaults.append(.text(SQLText(validatedUTF8: Array("use_json_null".utf8)))) }
        } else if isPath {
            names = ["target", "path", "vars", "silent"]; targets = [json, TypeDescriptor(.scalar(.jsonpath)), json, boolean]
            defaults = [.jsonb(.object([])), .boolean(false)]; minimum = 2
        } else if object {
            names = call.arguments.count <= 1 ? ["keys"] : ["keys", "values"]; targets = names.map { _ in array }; defaults = []; minimum = names.count
        } else if strip {
            names = ["target", "strip_in_arrays"]; targets = [json, boolean]; defaults = [.boolean(false)]; minimum = 1
        } else {
            names = ["row", "pretty_bool"]; targets = [nil, boolean]; defaults = [.boolean(false)]; minimum = 1
        }
        guard call.arguments.count >= minimum && call.arguments.count <= names.count else { throw JedError("42883", "JSON function does not exist") }
        var slots = [SQLExpression?](repeating: nil, count: names.count), named = false
        for (source, argument) in call.arguments.enumerated() {
            let index: Int
            if let name = argument.name { named = true; guard let found = names.firstIndex(of: name.description) else { throw JedError("42883", "unknown argument name") }; index = found }
            else { guard !named else { throw JedError("42601", "positional argument cannot follow named argument") }; index = source }
            guard slots[index] == nil else { throw JedError("42601", "argument name used more than once") }; slots[index] = argument.value
        }
        var arguments: [ResolvedExpression] = []
        for index in names.indices {
            if let syntax = slots[index] {
                var argument = try resolve(syntax, targets[index])
                if name == "array_to_json", index == 0, let type = argument.type?.type {
                    guard case .array = type else { throw JedError("42883", "array_to_json requires an array") }
                }
                if targets[index]?.type == .scalar(.boolean), FunctionResolver.bareString(syntax), case .constant(.text(let text)) = argument.node {
                    argument = ResolvedExpression(type: targets[index], node: .constant(try ScalarCasts.input(text, to: targets[index]!)), position: argument.position)
                }
                if let target = targets[index], let actual = argument.type, actual.type != target.type { throw JedError("42883", "JSON function argument has incompatible type") }
                arguments.append(argument)
            } else {
                guard index >= minimum else { throw JedError("42883", "required JSON function argument missing") }
                arguments.append(ResolvedExpression(type: targets[index], node: .constant(defaults[index - minimum]), position: 0))
            }
        }
        if name == "row_to_json", FunctionResolver.bareString(call.arguments[0].value) { throw JedError("0A000", "input of anonymous composite types is not implemented") }
        if name == "row_to_json", let type = arguments[0].type?.type {
            switch type { case .record, .composite: break; default: throw JedError("42883", "row_to_json requires a record") }
        }
        let result = pathNames.contains(name) && (name.contains("exists") || name.contains("match")) ? boolean : json
        return make(name, arguments, result: result, strict: name != "jsonb_set_lax")
    }

    static func make(_ name: String, _ arguments: [ResolvedExpression], result: TypeDescriptor,
                     strict: Bool = true, metadata: [String: SpecDataValue] = [:]) -> ResolvedFunction {
        var metadata = metadata; metadata["null"] = .text(strict ? "propagates" : "custom")
        metadata["volatility"] = .text(name.hasSuffix("_tz") ? "stable" : "immutable")
        return ResolvedFunction(metadata: FunctionSpecFact(category: "json", name: name, kind: "function", metadata: metadata), arguments: arguments,
                                resultType: result, evaluationOrder: Array(arguments.indices), constantArguments: arguments.map(FunctionResolver.isConstant))
    }
}

internal enum JsonScalarFunctions {
    static func regexCompiler(_ context: ExpressionContext) -> JSONPathRegexCompiler {
        let unicode = context.functions.unicode
        return { pattern, flags, meter in try JSONPathFunctions.regex(pattern, flags, meter, fold: { unicode.simpleLower($0) }) }
    }
    static func textPath(_ array: SQLArray) throws -> [SQLText?] {
        try array.elements.map { value in if value.isNull { return nil }; guard case .text(let text) = value else { throw JedError("42804", "text array required") }; return text }
    }
    static func render(_ value: SQLValue, type: TypeDescriptor, context: ExpressionContext, meter: CostMeter, zonePrecharged: Bool = false) throws -> SQLText {
        switch value {
        case .timestamp, .timestamptz, .date: return try Temporal.render(value, timezone: context.scalar.timezone, iso: true, meter: zonePrecharged ? nil : meter)
        default:
            if let render = context.functions.render { return try render(value, type, meter) }
            var scalar = context.scalar; scalar.meter = zonePrecharged ? nil : meter
            return try ContainerText.render(value, type: type, context: scalar, growth: false)
        }
    }
    static func evaluate(_ function: ResolvedFunction, values: [SQLValue], context: ExpressionContext) throws -> SQLValue {
        let name = function.name, meter = context.meter, constant = function.constantArguments.allSatisfy { $0 }
        if name == "is_json" {
            let result = JSONFunctions.isJSON(values[0], kind: FunctionResolver.text(function.metadata.metadata, "json_kind") ?? "value", unique: FunctionResolver.text(function.metadata.metadata, "unique") == "true")
            if case .boolean(let value) = result, FunctionResolver.text(function.metadata.metadata, "negated") == "true" { return .boolean(!value) }; return result
        }
        func path(_ index: Int) throws -> SQLArray { guard case .array(let array) = values[index] else { throw JedError("42804", "text array required") }; return array }
        func text(_ index: Int) throws -> SQLText { switch values[index] { case .text(let value), .jsonpath(let value): return value; default: throw JedError("42804", "text required") } }
        func bool(_ index: Int) -> Bool { if case .boolean(let value) = values[index] { value } else { false } }
        switch name {
        case "->", "->>":
            if case .integer(let n) = values[1] { guard let index = Int32(exactly: n) else { throw JedError("22003", "integer out of range") }; return try JSONFunctions.element(values[0], index: index, asText: name == "->>", constant: constant, meter: meter) }
            return try JSONFunctions.field(values[0], key: text(1), asText: name == "->>", constant: constant, meter: meter)
        case "#>", "#>>": return try JSONFunctions.extractPath(values[0], path: textPath(path(1)), asText: name == "#>>", constant: constant, meter: meter)
        case "@>", "<@": return .boolean(try JSONFunctions.contains(JSONFunctions.node(values[name == "@>" ? 0 : 1]), JSONFunctions.node(values[name == "@>" ? 1 : 0])))
        case "?": return .boolean(try JSONFunctions.exists(JSONFunctions.node(values[0]), key: text(1)))
        case "?|", "?&":
            let root = try JSONFunctions.node(values[0]), keys = try textPath(path(1)).compactMap { $0 }
            return .boolean(name == "?|" ? keys.contains { JSONFunctions.exists(root, key: $0) } : keys.allSatisfy { JSONFunctions.exists(root, key: $0) })
        case "||": return try JSONFunctions.concatenate(JSONFunctions.node(values[0]), JSONFunctions.node(values[1]), constant: constant, meter: meter)
        case "-":
            let root = try JSONFunctions.node(values[0])
            if case .integer(let n) = values[1] { guard let index = Int32(exactly: n) else { throw JedError("22003", "integer out of range") }; return .jsonb(try JSONFunctions.delete(root, index: index)) }
            if case .array(let keys) = values[1] { return .jsonb(try JSONFunctions.delete(root, keys: textPath(keys), dimensions: keys.dimensions.count)) }
            return .jsonb(try JSONFunctions.delete(root, keys: [text(1)]))
        case "#-": let keys = try path(1); return try JSONFunctions.mutate(JSONFunctions.node(values[0]), path: textPath(keys), dimensions: keys.dimensions.count, operation: .delete, constant: constant, meter: meter)
        case "jsonb_set_lax":
            return try JSONFunctions.setLax(values[0], path: { if case .array(let a) = values[1] { a } else { nil } }(), replacement: values[2], create: values[3].isNull ? nil : bool(3), treatment: values[4].isNull ? nil : text(4), constant: constant, meter: meter)
        case "jsonb_set", "jsonb_insert":
            let keys = try path(1)
            return try JSONFunctions.mutate(JSONFunctions.node(values[0]), path: textPath(keys), dimensions: keys.dimensions.count, replacement: JSONFunctions.node(values[2]), operation: name == "jsonb_set" ? .set(create: bool(3)) : .insert(after: bool(3)), constant: constant, meter: meter)
        case "json_object", "jsonb_object": return try JSONFunctions.object(path(0), values: values.count > 1 ? path(1) : nil, binary: name == "jsonb_object", constant: constant, meter: meter)
        case "json_strip_nulls", "jsonb_strip_nulls": return try JSONFunctions.stripNulls(values[0], arrays: bool(1))
        case "row_to_json", "array_to_json":
            return try JSONFunctions.chargeGrowth(JSONFunctions.image(values[0], type: function.arguments[0].type ?? TypeDescriptor(.record([])), binary: false, meter: meter, scalarRender: { try render($0, type: $1, context: context, meter: $2) }, pretty: bool(1)), constant: constant, meter: meter)
        case "@?", "@@": return try JSONPathFunctions.call(name, context: JSONFunctions.node(values[0]), path: text(1), constant: constant, operatorForm: true, meter: meter, regexCompiler: regexCompiler(context))
        default:
            if JsonFunctionResolver.pathNames.contains(name) { return try JSONPathFunctions.call(name, context: JSONFunctions.node(values[0]), path: text(1), variables: JSONFunctions.node(values[2]), silent: bool(3), constant: constant, meter: meter, regexCompiler: regexCompiler(context)) }
            throw JedError("XX000", "unconnected JSON scalar kernel")
        }
    }
}
