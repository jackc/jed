/// Planner §3.1's deliberately small proof grammar. It reads only authored
/// literals or bound parameters, never folded subqueries, calls or row values.
internal enum PredicateContradiction {
    private struct Bound { let operation: String; let source: ResolvedExpression }
    private static func literalShape(_ expression: ResolvedExpression) -> Bool {
        switch expression.node { case .constant(.composite): false; case .constant, .parameter: true; default: false }
    }
    static func literal(_ expression: ResolvedExpression, parameters: [SQLValue]?, planningProbe: PlanningProbe? = nil) -> SQLValue? {
        guard literalShape(expression) else { return nil }
        return PlanningProbe.known(expression, parameters: parameters, probe: planningProbe)
    }
    static func proves(_ predicate: ResolvedExpression?, parameters: [SQLValue]? = nil, planningProbe: PlanningProbe? = nil) -> Bool {
        var columns: [Int: [Bound]] = [:]
        func value(_ source: ResolvedExpression) -> SQLValue? {
            literal(source, parameters: parameters, planningProbe: planningProbe)
        }
        func isNull(_ source: ResolvedExpression) -> Bool {
            literalShape(source) && PlanningProbe.isNull(source, parameters: parameters, probe: planningProbe)
        }
        for term in QueryEstimate.conjuncts(predicate) {
            if let known = value(term) {
                if known.isNull { return true }
                if case .boolean(false) = known { return true }
            }
            guard case .binary(let operation, let left, let right, _) = term.node,
                  ["=", "<", "<=", ">", ">="].contains(operation) else { continue }
            // Only a two-literal comparison can inspect both values here. In a
            // lone column=$1 bound the only observable assumption is non-NULL.
            if literalShape(left), literalShape(right) {
                if isNull(left) || isNull(right) { return true }
                if let a = value(left), let b = value(right),
                   (!isText(a) && !isText(b) || operation == "="),
                   let order = order(a, b), !matches(order, operation) { return true }
            }
            let column: Int, bound: Bound
            if case .column(let slot) = left.node, literalShape(right) {
                column = slot; bound = Bound(operation: operation, source: right)
            } else if case .column(let slot) = right.node, literalShape(left) {
                column = slot
                bound = Bound(operation: ["<": ">", "<=": ">=", ">": "<", ">=": "<="][operation] ?? operation, source: left)
            } else { continue }
            if isNull(bound.source) { return true }
            for prior in columns[column] ?? [] {
                if let a = value(prior.source), let b = value(bound.source),
                   bounds(prior.operation, a, bound.operation, b) { return true }
            }
            columns[column, default: []].append(bound)
        }
        return false
    }
    static func bounds(_ first: String, _ a: SQLValue, _ second: String, _ b: SQLValue) -> Bool {
        guard ["=", "<", "<=", ">", ">="].contains(first), ["=", "<", "<=", ">", ">="].contains(second),
              (!isText(a) && !isText(b) || first == "=" && second == "="), let comparison = order(a, b) else { return false }
        if first == "=" { return !matches(comparison, second) }
        if second == "=" { return !matches(-comparison, first) }
        let lower = first == ">" || first == ">=", otherLower = second == ">" || second == ">="
        guard lower != otherLower else { return false }
        let ordered = lower ? comparison : -comparison
        return ordered > 0 || ordered == 0 && (first == ">" || first == "<" || second == ">" || second == "<")
    }
    private static func isText(_ value: SQLValue) -> Bool { if case .text = value { return true }; return false }
    static func matches(_ order: Int, _ operation: String) -> Bool {
        switch operation { case "=": order == 0; case "<": order < 0; case "<=": order <= 0; case ">": order > 0; case ">=": order >= 0; default: true }
    }
    /// Same representation kinds only. Open/container literals deliberately have
    /// no proof. Text callers must enforce byte-equality-only admission above.
    static func order(_ left: SQLValue, _ right: SQLValue) -> Int? {
        func compare<T: Comparable>(_ a: T, _ b: T) -> Int { a == b ? 0 : a < b ? -1 : 1 }
        func bytes(_ a: [UInt8], _ b: [UInt8]) -> Int { a == b ? 0 : a.lexicographicallyPrecedes(b) ? -1 : 1 }
        func floating<T: BinaryFloatingPoint>(_ a: T, _ b: T) -> Int {
            if a.isNaN { return b.isNaN ? 0 : 1 }; if b.isNaN { return -1 }; return compare(a, b)
        }
        switch (left, right) {
        case (.integer(let a), .integer(let b)), (.timestamp(let a), .timestamp(let b)),
             (.timestamptz(let a), .timestamptz(let b)): return compare(a, b)
        case (.date(let a), .date(let b)): return compare(a, b)
        case (.boolean(let a), .boolean(let b)): return a == b ? 0 : a ? 1 : -1
        case (.text(let a), .text(let b)): return bytes(a.utf8, b.utf8)
        case (.uuid(let a), .uuid(let b)): return bytes(a.bytes, b.bytes)
        case (.bytea(let a), .bytea(let b)): return bytes(a, b)
        case (.decimal(let a), .decimal(let b)): return a.compare(to: b)
        case (.interval(let a), .interval(let b)): return compare(a.comparisonSpan, b.comparisonSpan)
        case (.f32(let a), .f32(let b)): return floating(Float(bitPattern: a), Float(bitPattern: b))
        case (.f64(let a), .f64(let b)): return floating(Double(bitPattern: a), Double(bitPattern: b))
        default: return nil
        }
    }
}
