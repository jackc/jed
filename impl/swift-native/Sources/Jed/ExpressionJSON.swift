extension ExpressionResolver {
    func jsonOperation(_ op: String, _ left: SQLExpression, _ right: SQLExpression, position: Int) throws -> ResolvedExpression? {
        guard ["->", "->>", "#>", "#>>", "@>", "<@", "?", "?|", "?&", "||", "-", "#-", "@?", "@@"].contains(op) else { return nil }
        // Most overloaded operators are arithmetic/text/container expressions.
        // Do not recursively resolve their children merely to reject a JSON
        // candidate: repeated speculative resolution makes long chains grow
        // exponentially before the parser's nesting bound is reached.
        if ["@>", "<@", "||", "-"].contains(op), !mayProduceJSON(left) && !mayProduceJSON(right) { return nil }
        var a = try resolve(left), b = try resolve(right)
        if op == "||", a.type?.type == .scalar(.json) || b.type?.type == .scalar(.json) { return nil }
        if op == "||", a.type?.type == .scalar(.text) && !FunctionResolver.bareString(left) || b.type?.type == .scalar(.text) && !FunctionResolver.bareString(right) { return nil }
        let access = ["->", "->>", "#>", "#>>"].contains(op)
        let jsonb = TypeDescriptor(.scalar(.jsonb)), text = TypeDescriptor(.scalar(.text)), textArray = TypeDescriptor(.array(.scalar(.text)))
        let jsonType = a.type?.type == .scalar(.json) || a.type?.type == .scalar(.jsonb)
        if ["@>", "<@", "||", "-"].contains(op), !jsonType && b.type?.type != .scalar(.jsonb) { return nil }
        if a.type == nil || FunctionResolver.bareString(left) { a = try resolve(left, expected: jsonb) }
        guard a.type?.type == .scalar(.jsonb) || access && a.type?.type == .scalar(.json) else { throw JedError("42883", "JSON operator requires a JSON operand") }
        let wanted: TypeDescriptor
        switch op {
        case "#>", "#>>", "#-", "?|", "?&": wanted = textArray
        case "@?", "@@": wanted = TypeDescriptor(.scalar(.jsonpath))
        case "@>", "<@", "||": wanted = jsonb
        case "?": wanted = text
        default:
            if case .scalar(let scalar) = b.type?.type, scalar.integerWidth != nil { wanted = TypeDescriptor(.scalar(.i32)) }
            else if op == "-", b.type?.type == textArray.type { wanted = textArray }
            else { wanted = text }
        }
        b = try resolve(right, expected: wanted)
        if wanted.type == .scalar(.i32), case .scalar(let scalar) = b.type?.type, scalar.integerWidth != nil {
            // Index operators narrow their integer operand with a checked cast.
            // Literal adoption already produced i32 without a runtime cast.
            if scalar != .i32 { b = ResolvedExpression(type: wanted, node: .cast(b, wanted, .assignment), position: b.position) }
        } else if b.type != nil && b.type?.type != wanted.type { throw JedError("42883", "JSON operator argument has incompatible type") }
        let result: TypeDescriptor
        if ["->>", "#>>"].contains(op) { result = text }
        else if ["@>", "<@", "?", "?|", "?&", "@?", "@@"].contains(op) { result = TypeDescriptor(.scalar(.boolean)) }
        else { result = a.type! }
        let function = JsonFunctionResolver.make(op, [a, b], result: result)
        return ResolvedExpression(type: result, node: .call(function), position: position)
    }

    private func mayProduceJSON(_ syntax: SQLExpression) -> Bool {
        func containsJSON(_ type: SQLType?) -> Bool {
            switch type { case .scalar(.json), .scalar(.jsonb): true; case .array(let element): containsJSON(element); default: false }
        }
        switch syntax.kind {
        case .column(let qualifier, let name): return columns.contains { $0.name == name && (qualifier == nil || $0.qualifier == qualifier) && containsJSON($0.type.type) }
        case .parameter(let index): return parameters[index].map { containsJSON($0.type) } ?? true
        case .cast(_, let type), .typedString(let type, _): return ["json", "jsonb"].contains(type.name.description)
        case .parenthesized(let child), .collate(let child, _): return mayProduceJSON(child)
        case .call(let call):
            return call.name.description.hasPrefix("json") || ["to_json", "to_jsonb", "row_to_json", "array_to_json"].contains(call.name.description)
                || hostFunctions.contains { $0.name == call.name && ($0.result == .json || $0.result == .jsonb) }
        case .json(let expression): return expression.name == "json" || expression.name == "json_query" || expression.returning.map { ["json", "jsonb"].contains($0.name.description) } == true
        case .binary(let op, let a, let b): return ["->", "#>"].contains(op) || ["||", "-", "#-"].contains(op) && (mayProduceJSON(a) || mayProduceJSON(b))
        case .conditionalFunction(_, let children): return children.contains(where: mayProduceJSON)
        case .conditional(_, let arms, let fallback): return arms.contains { mayProduceJSON($0.result) } || fallback.map(mayProduceJSON) == true
        case .field, .subscripted: return true
        default: return false
        }
    }

    func jsonPredicate(_ source: SQLExpression, kind: String, unique: Bool, negated: Bool, position: Int) throws -> ResolvedExpression {
        let argument = try resolve(source), result = TypeDescriptor(.scalar(.boolean))
        if let type = argument.type?.type, ![.scalar(.text), .scalar(.json), .scalar(.jsonb)].contains(type) { throw JedError("42804", "IS JSON requires text, json, or jsonb input") }
        let function = JsonFunctionResolver.make("is_json", [argument], result: result, strict: false, metadata: ["json_kind": .text(kind), "unique": .text(unique ? "true" : "false"), "negated": .text(negated ? "true" : "false")])
        return ResolvedExpression(type: result, node: .call(function), position: position)
    }

    func jsonExpression(_ syntax: SQLJSONExpression, position: Int) throws -> ResolvedExpression {
        let output = try syntax.returning.map { try TypeRules.resolve($0, composites: composites) }
            ?? TypeDescriptor(.scalar(syntax.name == "json_exists" ? .boolean : syntax.name == "json_value" ? .text : syntax.name == "json" ? .json : .jsonb))
        guard case .scalar = output.type else { throw JedError("0A000", "SQL/JSON RETURNING requires a supported scalar type") }
        if syntax.name == "json_query" && (![SQLType.scalar(.json), .scalar(.jsonb)].contains(output.type) || !syntax.keepQuotes) {
            throw JedError("0A000", "JSON_QUERY return type or quote mode is not supported")
        }
        let inputHint = TypeDescriptor(.scalar(syntax.name == "json" ? .text : .jsonb))
        let input = try resolve(syntax.context, expected: inputHint)
        try jsonEncoding(syntax.encoding, type: input.type)
        if syntax.name == "json" {
            guard input.type == nil || [.scalar(.text), .scalar(.json), .scalar(.jsonb)].contains(input.type!.type) else { throw JedError("42804", "JSON input must be text or JSON") }
        } else {
            if input.type != nil && ![.scalar(.json), .scalar(.text), .scalar(.jsonb)].contains(input.type!.type) && !(syntax.formatJSON && input.type?.type == .scalar(.bytea)) {
                throw JedError("42804", "JSON context must be JSON, text, or encoded bytea")
            }
        }
        let path = try syntax.path.map { try resolve($0, expected: TypeDescriptor(.scalar(.jsonpath))) }
        if let type = path?.type?.type, type != .scalar(.jsonpath) { throw JedError("42804", "JSON path must have type jsonpath") }
        var passing: [ResolvedJSONExpression.Passing] = []
        for item in syntax.passing {
            let value = try resolve(item.value)
            try jsonEncoding(item.encoding, type: value.type)
            if !item.formatJSON, let type = value.type?.type, [.scalar(.date), .scalar(.timestamp), .scalar(.timestamptz)].contains(type) { throw JedError("0A000", "jsonpath datetime items are not supported") }
            passing.append(.init(name: item.name, value: value, formatJSON: item.formatJSON))
        }
        let empty = try jsonBehavior(syntax.onEmpty, returning: output, exists: syntax.name == "json_exists")
        let failure = try jsonBehavior(syntax.onError, returning: output, exists: syntax.name == "json_exists")
        let resolved = ResolvedJSONExpression(name: syntax.name, context: input, path: path, result: output, passing: passing,
                                              wrapper: syntax.wrapper, uniqueKeys: syntax.uniqueKeys, onEmpty: empty, onError: failure)
        return ResolvedExpression(type: output, node: .json(resolved), position: position)
    }

    func jsonEncoding(_ encoding: SQLText?, type: TypeDescriptor?) throws {
        guard let encoding else { return }
        guard type?.type == .scalar(.bytea) else { throw JedError("42804", "JSON ENCODING clause is only allowed for bytea input type") }
        guard encoding.description == "UTF8" else { throw JedError("22023", "invalid source encoding name \"\(encoding)\"") }
    }

    func jsonBehavior(_ syntax: SQLJSONBehavior?, returning: TypeDescriptor, exists: Bool) throws -> ResolvedJSONExpression.Behavior? {
        guard let syntax else { return nil }
        guard let expression = syntax.value else { return .init(action: syntax.action, value: nil, itemCoercion: false) }
        guard !exists else { throw JedError("42601", "invalid ON ERROR behavior") }
        func shape(_ syntax: SQLExpression) -> Bool {
            switch syntax.kind {
            case .number, .string, .boolean, .null, .typedString, .pattern: true
            case .call(let call): call.over == nil && call.overName == nil
            case .unary(let op, _): op != "not"
            case .binary(let op, _, _): op != "and" && op != "or"
            case .cast(let child, _), .collate(let child, _), .parenthesized(let child): shape(child)
            default: false
            }
        }
        guard shape(expression) else { throw JedError("42804", "can only specify a constant, non-aggregate function, or operator expression for DEFAULT") }
        func inspect(_ syntax: SQLExpression) throws {
            switch syntax.kind {
            case .column: throw JedError("42804", "DEFAULT expression must not contain column references")
            case .parameter: throw JedError("42P02", "there is no parameter in DEFAULT expression")
            case .scalarQuery, .exists: throw JedError("0A000", "cannot use subquery in DEFAULT expression")
            case .call(let call):
                if call.over != nil || call.overName != nil || call.filter != nil || GeneratedSpec.functions.contains(where: { $0.name == call.name.description && $0.kind == "aggregate" }) { throw JedError("42803", "aggregate functions are not allowed in DEFAULT") }
                for argument in call.arguments { try inspect(argument.value) }
            case .cast(let child, _), .collate(let child, _), .parenthesized(let child), .unary(_, let child): try inspect(child)
            case .binary(_, let left, let right): try inspect(left); try inspect(right)
            case .array(let children), .row(let children), .conditionalFunction(_, let children):
                for child in children { try inspect(child) }
            case .field(let child, _), .isNull(let child, _), .isJSON(let child, _, _, _), .extract(_, let child): try inspect(child)
            case .isDistinct(let a, let b, _), .quantified(_, let a, _, let b): try inspect(a); try inspect(b)
            case .member(let child, let children, _): try inspect(child); for child in children { try inspect(child) }
            case .memberQuery, .quantifiedQuery: throw JedError("0A000", "cannot use subquery in DEFAULT expression")
            case .between(let child, let lower, let upper, _): try inspect(child); try inspect(lower); try inspect(upper)
            case .pattern(_, let a, let b, let escape, _): try inspect(a); try inspect(b); if let escape { try inspect(escape) }
            case .subscripted(let child, let indices):
                try inspect(child); for index in indices { if let lower = index.lower { try inspect(lower) }; if let upper = index.upper { try inspect(upper) } }
            case .conditional(let operand, let arms, let fallback):
                if let operand { try inspect(operand) }; for arm in arms { try inspect(arm.condition); try inspect(arm.result) }; if let fallback { try inspect(fallback) }
            case .json(let expression):
                try inspect(expression.context); if let path = expression.path { try inspect(path) }
                for value in expression.passing { try inspect(value.value) }
                if let value = expression.onEmpty?.value { try inspect(value) }; if let value = expression.onError?.value { try inspect(value) }
            default: break
            }
        }
        try inspect(expression)
        func literal(_ syntax: SQLExpression) -> SQLText? { if case .string(let text) = syntax.kind { return text }; if case .parenthesized(let child) = syntax.kind { return literal(child) }; return nil }
        var value: ResolvedExpression
        if let text = literal(expression) {
            var context = scalar; context.meter = nil
            value = ResolvedExpression(type: returning, node: .constant(try ContainerText.input(text, to: returning, explicit: true, context: context)), position: expression.position)
        } else { value = try resolve(expression, expected: returning) }
        var itemCoercion = false
        if let type = value.type?.type, type != returning.type {
            if type == .scalar(.jsonb) || type == .scalar(.boolean) && returning.type != .scalar(.i32) { itemCoercion = true }
            else {
                guard TypeRules.admitsCast(from: type, to: returning.type, mode: .explicit) else { throw JedError("42846", "cannot cast behavior expression to RETURNING type") }
                value = ResolvedExpression(type: returning, node: .cast(value, returning, .explicit), position: value.position)
            }
        }
        return .init(action: syntax.action, value: value, itemCoercion: itemCoercion)
    }
}

/// Behaviors are retained expressions, never eagerly evaluated function slots.
internal struct ResolvedJSONExpression {
    struct Passing { let name: SQLText; let value: ResolvedExpression; let formatJSON: Bool }
    struct Behavior { let action: String; let value: ResolvedExpression?; let itemCoercion: Bool }
    let name: String
    let context: ResolvedExpression
    let path: ResolvedExpression?
    let result: TypeDescriptor
    let passing: [Passing]
    let wrapper: String?
    let uniqueKeys: Bool
    let onEmpty: Behavior?
    let onError: Behavior?

    var isConstant: Bool {
        inputsConstant
            && (onEmpty?.value.map(FunctionResolver.isConstant) ?? true) && (onError?.value.map(FunctionResolver.isConstant) ?? true)
    }
    private var inputsConstant: Bool {
        FunctionResolver.isConstant(context) && (path.map(FunctionResolver.isConstant) ?? true) && passing.allSatisfy { FunctionResolver.isConstant($0.value) }
    }

    func evaluate(context execution: ExpressionContext, chargePathResults: Bool = true) throws -> SQLValue {
        let value = try execution.evaluate(context)
        if name == "json" {
            if value.isNull { return .null }
            let text: SQLText
            switch value {
            case .text(let t), .json(let t): text = t
            case .jsonb(let node): text = try SQLJSON.render(node, meter: execution.meter, growth: !isConstant)
            default: throw JedError("42804", "JSON input required")
            }
            _ = try JSONDocument(text, decodeStrings: uniqueKeys, uniqueKeys: uniqueKeys)
            return .json(text)
        }
        guard let path else { throw JedError("XX000", "SQL/JSON path missing") }
        let pathValue = try execution.evaluate(path)
        if value.isNull || pathValue.isNull { return .null }
        guard case .jsonpath(let pathText) = pathValue else { throw JedError("42804", "jsonpath input required") }
        let root: SQLJSONValue
        if case .bytea(let bytes) = value {
            execution.meter.charge(1); try execution.meter.step()
            root = try JSONFunctions.node(.text(SQLText(utf8: bytes)))
        } else { root = try JSONFunctions.node(value) }
        var bindings: [(SQLText, SQLJSONValue)] = []
        for binding in passing { bindings.append((binding.name, try Self.passingValue(binding, execution: execution))) }
        // Context/PASSING conversions and dynamic path input belong to their
        // operands and cannot be suppressed by ON ERROR.
        let program = try JSONPathProgram.compile(pathText, meter: execution.meter, regexCompiler: JsonScalarFunctions.regexCompiler(execution))
        let cursor = JSONPathFunctions.query(context: root, program: program, passing: bindings, existenceTest: name == "json_exists", meter: execution.meter)
        defer { cursor.close() }
        var first: SQLJSONValue?, count = 0, items: [SQLJSONValue] = []
        var produced: Int64 = 0
        let collect = name == "json_query" && (wrapper == "unconditional" || wrapper == "conditional")
        let computed: SQLValue?
        do {
            while let item = try cursor.next() {
                if count == 0 { first = item }
                count = min(2, count + 1)
                produced = saturatingAdd(produced, 1)
                if collect { items.append(item) }
                if name == "json_exists" && !program.strict { break }
            }
            if chargePathResults { execution.meter.charge(produced); try execution.meter.guardCost() }
            if name == "json_exists" { return .boolean(first != nil) }
            if count == 0 { computed = nil }
            else if name == "json_value" {
                guard count == 1 else { throw JedError("22034", "JSON path expression must return a single item") }
                guard let first, SQLJSON.kind(first) != "array" && SQLJSON.kind(first) != "object" else { throw JedError("2203F", "JSON path expression must return a scalar item") }
                computed = try Self.coerce(first, to: result, context: execution)
            } else {
                let node: SQLJSONValue
                let wrapped = wrapper == "unconditional" || wrapper == "conditional" && count > 1
                if wrapped { node = .array(items) }
                else { guard count == 1, let first else { throw JedError("22034", "JSON path expression must return a single item") }; node = first }
                try SQLJSON.validateDepth(node)
                let image: SQLValue = result.type == .scalar(.json) ? .json(try SQLJSON.render(node)) : .jsonb(node)
                computed = wrapped ? try JSONFunctions.chargeGrowth(image, constant: inputsConstant, meter: execution.meter) : image
            }
        } catch let error as JedError {
            guard error.code.hasPrefix("22") else { throw error }
            return try behavior(onError, empty: false, error: error, execution: execution)
        }
        if let computed { return computed }
        return try behavior(onEmpty, empty: true, error: JedError("22035", "no SQL/JSON item"), execution: execution)
    }

    private func behavior(_ behavior: Behavior?, empty: Bool, error: JedError, execution: ExpressionContext) throws -> SQLValue {
        guard let behavior else { return name == "json_exists" ? .boolean(false) : .null }
        if let expression = behavior.value {
            let value = try execution.evaluate(expression)
            if !behavior.itemCoercion || value.isNull { return value }
            let node: SQLJSONValue
            if case .boolean(let b) = value { node = .boolean(b) } else { node = try JSONFunctions.node(value) }
            do { return try Self.coerce(node, to: result, context: execution) }
            catch let cause as JedError {
                if !cause.code.hasPrefix("22") && cause.code != "42804" { throw cause }
                var failure = JedError("42804", "could not coerce ON \(empty ? "EMPTY" : "ERROR") expression (DEFAULT) to the RETURNING type")
                failure.detail = cause.message; throw failure
            }
        }
        switch behavior.action {
        case "error": throw error
        case "true": return .boolean(true)
        case "false": return .boolean(false)
        case "empty array", "empty object":
            let node: SQLJSONValue = behavior.action == "empty array" ? .array([]) : .object([])
            return result.type == .scalar(.json) ? .json(try SQLJSON.render(node)) : .jsonb(node)
        default: return .null
        }
    }

    static func coerce(_ node: SQLJSONValue, to type: TypeDescriptor, context: ExpressionContext) throws -> SQLValue {
        if case .null = node { return .null }
        if type.type == .scalar(.jsonb) { return .jsonb(node) }
        if type.type == .scalar(.json) { return .json(try SQLJSON.render(node)) }
        if type.type == .scalar(.decimal), case .number(let number) = node { return try ScalarCasts.applyTypmod(.decimal(number), descriptor: type, explicit: false) }
        let text: SQLText
        if case .string(let value) = node { text = value }
        else { text = try SQLJSON.render(node) }
        return try ContainerText.input(text, to: type, explicit: false, context: context.scalar)
    }

    static func passingValue(_ binding: Passing, execution: ExpressionContext) throws -> SQLJSONValue {
        let value = try execution.evaluate(binding.value)
        if value.isNull { return .null }
        if binding.formatJSON {
            switch value {
            case .text:
                execution.meter.charge(1); try execution.meter.step(); return try JSONFunctions.node(value)
            case .bytea(let bytes):
                execution.meter.charge(1); try execution.meter.step(); return try JSONFunctions.node(.text(SQLText(utf8: bytes)))
            default: break
            }
        } else {
            switch value {
            case .boolean(let b): return .boolean(b)
            case .integer(let n): return .number(SQLDecimal(n))
            case .decimal(let n): return .number(n)
            case .text(let t): return .string(t)
            case .f32(let bits):
                guard Float(bitPattern: bits).isFinite else { throw JedError("0A000", "non-finite jsonpath numeric items are not supported") }
                let exact = try BinaryFloatDecimal.exact(bits32: bits)
                return .number(Self.floatItem(exact, digits: 6))
            case .f64(let bits):
                guard Double(bitPattern: bits).isFinite else { throw JedError("0A000", "non-finite jsonpath numeric items are not supported") }
                let exact = try BinaryFloatDecimal.exact(bits64: bits)
                return .number(Self.floatItem(exact, digits: 15))
            default: break
            }
        }
        if case .jsonb(let node) = value { return node }
        if case .json = value { execution.meter.charge(1); try execution.meter.step(); return try JSONFunctions.node(value) }
        execution.meter.charge(1); try execution.meter.step()
        let image = try JSONFunctions.image(value, type: binding.value.type ?? TypeDescriptor(.scalar(.text)), binary: true, meter: execution.meter,
                                            scalarRender: { try JsonScalarFunctions.render($0, type: $1, context: execution, meter: $2) })
        _ = try JSONFunctions.chargeGrowth(image, constant: FunctionResolver.isConstant(binding.value), meter: execution.meter)
        return try JSONFunctions.node(image)
    }
    private static func floatItem(_ exact: SQLDecimal, digits: Int) -> SQLDecimal {
        let value = BinaryFloatDecimal.roundedEven(exact, scale: digits - (exact.precision - exact.scale))
        var magnitude = value.magnitude, scale = value.scale
        while scale > 0 { let divided = magnitude.divided(by: UInt32(10)); if divided.remainder != 0 { break }; magnitude = divided.quotient; scale -= 1 }
        return SQLDecimal(negative: value.negative, magnitude: magnitude, scale: scale)
    }
}
