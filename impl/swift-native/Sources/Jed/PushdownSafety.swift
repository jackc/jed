/// Catalog-driven proof used only by the logical rewrite. Resource aborts retain
/// their selected-plan timing; ordinary scalar errors may never be moved earlier.
internal enum PushdownSafety {
    static func value(_ expression: ResolvedExpression) -> Bool {
        switch expression.node {
        case .column, .parameter: return true
        case .constant(let value): if case .composite = value { return false }; return true
        case .cast(let source, let target, _), .convert(let source, let target):
            return value(source) && cast(source.type, target, fact: "traps") == false
        case .call(let function):
            return function.hostID == nil && !FunctionResolver.strings(function.metadata.metadata, "arg_families").contains(where: { $0.hasPrefix("any") })
                && function.metadata.metadata["traps"] == .boolean(false)
                && function.arguments.allSatisfy(value)
        default: return false
        }
    }
    static func predicate(_ expression: ResolvedExpression, parameters: [SQLValue]? = nil, planningProbe: PlanningProbe? = nil) -> Bool {
        switch expression.node {
        case .binary(let op, let left, let right, let type):
            if op == "and" || op == "or" { return predicate(left, parameters: parameters, planningProbe: planningProbe) && predicate(right, parameters: parameters, planningProbe: planningProbe) }
            guard ["=", "<>", "!=", "<", "<=", ">", ">="].contains(op), value(left), value(right) else { return false }
            return ["=", "<>", "!="].contains(op) || (type.collation?.description ?? "C") == "C"
        case .unary("not", let child): return predicate(child, parameters: parameters, planningProbe: planningProbe)
        case .isNull(let child, _): return value(child)
        case .distinct(let left, let right, _, _): return value(left) && value(right)
        case .pattern(let pattern):
            guard pattern.like && value(pattern.subject) else { return false }
            let literal: SQLValue
            switch pattern.pattern.node {
            case .constant(let input): literal = input
            case .parameter(let index):
                guard let input = PlanningProbe.parameter(index, parameters: parameters, probe: planningProbe) else { return false }
                literal = input
            default: return false
            }
            if literal.isNull { return true }
            guard case .text(let text) = literal else { return false }
            var escaped = false
            for byte in text.utf8 {
                if escaped { escaped = false }
                else if byte == 92 { escaped = true }
            }
            return !escaped
        default: return value(expression)
        }
    }
    static func preservesEquality(_ expression: ResolvedExpression) -> Bool {
        var preserves = true
        StoredExpressionDependencies.walk([expression]) { node in
            switch node.node {
            case .cast(let source, let target, _), .convert(let source, let target):
                if cast(source.type, target, fact: "preserves_equality") == false { preserves = false }
            case .call(let call):
                if call.metadata.metadata["preserves_equality"] == .boolean(false) { preserves = false }
            default: break
            }
        }
        return preserves
    }
    private static func cast(_ source: TypeDescriptor?, _ target: TypeDescriptor, fact: String) -> Bool? {
        if fact == "traps", target.decimal != nil { return true }
        guard let source else { return fact == "preserves_equality" }
        // Surviving same-family casts may round temporal typmods and trap.
        guard case .scalar(let from) = source.type, case .scalar(let to) = target.type else { return nil }
        let row = TypeRules.rows(GeneratedSpec.casts, "cast").first { $0["from"] == .text(from.name) && $0["to"] == .text(to.name) }
        guard case .boolean(let value) = row?[fact] else { return nil }
        return value
    }
    static func columns(_ expression: ResolvedExpression) -> Set<Int> {
        var columns: Set<Int> = []
        StoredExpressionDependencies.walk([expression]) { if case .column(let index) = $0.node { columns.insert(index) } }
        return columns
    }
    /// All traversed forms are admitted by the proof above. Values outside that
    /// closed set remain untouched and can never become a pushed expression.
    static func rewrite(_ expression: ResolvedExpression, column: (ResolvedExpression, Int) -> ResolvedExpression) -> ResolvedExpression {
        var result = expression
        func child(_ value: ResolvedExpression) -> ResolvedExpression { rewrite(value, column: column) }
        switch expression.node {
        case .column(let index): return column(expression, index)
        case .binary(let op, let a, let b, let type): result.node = .binary(op, child(a), child(b), type)
        case .unary(let op, let input): result.node = .unary(op, child(input))
        case .isNull(let input, let negated): result.node = .isNull(child(input), negated)
        case .distinct(let a, let b, let type, let negated): result.node = .distinct(child(a), child(b), type, negated)
        case .cast(let input, let type, let mode): result.node = .cast(child(input), type, mode)
        case .convert(let input, let type): result.node = .convert(child(input), type)
        case .call(var function): function.arguments = function.arguments.map(child); result.node = .call(function)
        case .pattern(let pattern):
            result.node = .pattern(ResolvedPattern(subject: child(pattern.subject), pattern: child(pattern.pattern),
                like: pattern.like, negated: pattern.negated, insensitive: pattern.insensitive,
                similar: pattern.similar, program: pattern.program))
        default: break
        }
        return result
    }
}
