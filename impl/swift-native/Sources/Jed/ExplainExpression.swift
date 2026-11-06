/// Plan spelling consumes resolved nodes. An explicit work stack keeps a flat
/// SQL operator chain from turning into recursive host calls during rendering.
internal enum ExplainExpression {
    private enum Work { case node(ResolvedExpression), text(String) }

    static func render(_ expression: ResolvedExpression) throws -> String {
        var pending: [Work] = [.node(expression)], output = ""
        func list(_ expressions: [ResolvedExpression], start: String, end: String) -> [Work] {
            var result: [Work] = [.text(start)]
            for (index, expression) in expressions.enumerated() {
                if index > 0 { result.append(.text(", ")) }
                result.append(.node(expression))
            }
            result.append(.text(end)); return result
        }
        func binary(_ op: String, _ left: ResolvedExpression, _ right: ResolvedExpression) -> [Work] {
            [.text("("), .node(left), .text(" \(op) "), .node(right), .text(")")]
        }
        while let work = pending.popLast() {
            guard case .node(let expression) = work else {
                if case .text(let text) = work { output += text }; continue
            }
            let pieces: [Work]
            switch expression.node {
            case .constant(let value): pieces = [.text(try literal(value, type: expression.type))]
            case .column(let slot): pieces = [.text("#\(slot)")]
            case .outerColumn(let reference): pieces = [.text("outer(\(reference.depth),\(reference.index))")]
            case .parameter(let index): pieces = [.text("$\(index)")]
            case .currentTimestamp: pieces = [.text("current_timestamp")]
            case .currentDate: pieces = [.text("current_date")]
            case .dateInput(let text): pieces = [.text("cast(\(quote(text.description)) as date)")]
            case .cast(let source, let target, _):
                pieces = [.text("cast("), .node(source), .text(" as \(SystemRelationPlan.typeText(target)))")]
            case .convert(let source, _): pieces = [.node(source)]
            case .unary(let op, let source): pieces = [.text("(\(op) "), .node(source), .text(")")]
            case .binary(let op, let a, let b, _), .rangeOperation(let op, let a, let b, _): pieces = binary(op, a, b)
            case .isNull(let source, let negated): pieces = [.text("("), .node(source), .text(negated ? " is not null)" : " is null)")]
            case .distinct(let a, let b, _, let negated): pieces = binary(negated ? "is not distinct from" : "is distinct from", a, b)
            case .conditional(let arms, let fallback):
                var result: [Work] = [.text("(case")]
                for (condition, value) in arms { result += [.text(" when "), .node(condition), .text(" then "), .node(value)] }
                if let fallback { result += [.text(" else "), .node(fallback)] }
                result.append(.text(" end)")); pieces = result
            case .coalesce(let values): pieces = list(values, start: "coalesce(", end: ")")
            case .extreme(let values, let greatest): pieces = list(values, start: greatest ? "greatest(" : "least(", end: ")")
            case .nullif(let a, let b, _): pieces = list([a, b], start: "nullif(", end: ")")
            case .row(let values): pieces = list(values, start: "row(", end: ")")
            case .field(let source, let slot): pieces = [.text("field("), .node(source), .text(", \(slot))")]
            case .array(let values, _): pieces = list(values, start: "array[", end: "]")
            case .subscriptValue(let source, let subscripts):
                var result: [Work] = [.node(source)]
                for item in subscripts {
                    result.append(.text("["))
                    if item.isSlice {
                        if let lower = item.lower { result.append(.node(lower)) }
                        result.append(.text(":"))
                    }
                    if let upper = item.upper { result.append(.node(upper)) }
                    result.append(.text("]"))
                }
                pieces = result
            case .quantified(let op, let all, let a, let b, _):
                pieces = [.text("("), .node(a), .text(" \(op) \(all ? "all" : "any")("), .node(b), .text("))")]
            case .call(let call):
                if call.arguments.count == 2, ["||", "->", "->>", "#>", "#>>", "@>", "<@", "?", "?|", "?&", "-", "#-", "@?", "@@", "&&"].contains(call.name) {
                    pieces = binary(call.name, call.arguments[0], call.arguments[1])
                } else {
                    let name = ["substring": "substr", "position": "strpos"][call.name] ?? call.name
                    pieces = list(call.arguments, start: name + "(", end: ")")
                }
            case .pattern(let pattern):
                let op: String
                if pattern.like { op = (pattern.negated ? "not " : "") + (pattern.insensitive ? "ilike" : "like") }
                else if pattern.similar { op = pattern.negated ? "not similar to" : "similar to" }
                else { op = (pattern.negated ? "!" : "") + "~" + (pattern.insensitive ? "*" : "") }
                pieces = binary(op, pattern.subject, pattern.pattern)
            case .subquery(let query):
                switch query.kind {
                case .scalar: pieces = [.text("(subquery)")]
                case .exists: pieces = [.text("exists(subquery)")]
                case .membership(let op, let all, let negated, let membership, let operand):
                    pieces = [.text("("), .node(operand), .text(membership ? (negated ? " not in (subquery))" : " in (subquery))") : " \(op) \(all ? "all" : "any")(subquery))")]
                }
            case .json(let json):
                var result: [Work] = [.text(json.name + "("), .node(json.context)]
                if let path = json.path { result += [.text(", "), .node(path)] }
                for (index, passing) in json.passing.enumerated() {
                    result.append(.text(index == 0 ? " passing " : ", "))
                    if passing.value.type?.type == .scalar(.json) { result += [.text("cast("), .node(passing.value), .text(" as jsonb)")] }
                    else { result.append(.node(passing.value)) }
                    result.append(.text(" as " + identifier(passing.name)))
                }
                if json.name == "json" {
                    if json.uniqueKeys { result.append(.text(" with unique keys")) }
                } else {
                    result.append(.text(" returning " + SystemRelationPlan.typeText(json.result)))
                    if json.name == "json_query" {
                        result.append(.text(" " + (json.wrapper.map { "with " + $0 } ?? "without") + " array wrapper keep quotes on scalar string"))
                    }
                    for (name, behavior) in [("empty", json.onEmpty), ("error", json.onError)] {
                        if json.name == "json_exists", name == "empty" { continue }
                        result.append(.text(" " + (behavior?.action ?? (json.name == "json_exists" ? "false" : "null"))))
                        if let value = behavior?.value { result += [.text(" "), .node(value)] }
                        result.append(.text(" on " + name))
                    }
                }
                result.append(.text(")")); pieces = result
            }
            pending.append(contentsOf: pieces.reversed())
        }
        return output
    }
    static func identifier(_ value: SQLText) -> String {
        let bytes = Array(value.description.utf8)
        let plain = !bytes.isEmpty && bytes.enumerated().allSatisfy { index, byte in
            (byte >= 97 && byte <= 122) || byte == 95 || (index > 0 && byte >= 48 && byte <= 57)
        }
        if plain { return value.description }
        var output: [UInt8] = [34]
        for byte in bytes { output.append(byte); if byte == 34 { output.append(byte) } }
        output.append(34); return String(decoding: output, as: UTF8.self)
    }
    static func quote(_ text: String) -> String {
        var output: [UInt8] = [39]
        for byte in text.utf8 { output.append(byte); if byte == 39 || byte == 92 { output.append(byte) } }
        output.append(39); return String(decoding: output, as: UTF8.self)
    }
    static func literal(_ value: SQLValue, type: TypeDescriptor?) throws -> String {
        switch value {
        case .null: return "null"
        case .boolean(let value): return value ? "true" : "false"
        case .integer(let value): return String(value)
        case .decimal(let value): return value.description
        case .f32, .f64: return try FloatText.render(value).description
        default:
            guard let type else { throw JedError("XX000", "typed EXPLAIN constant has no descriptor") }
            return quote(try ContainerText.render(value, type: type).description)
        }
    }
}
