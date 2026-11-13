/// Structural identity ignores diagnostic positions. It is used for SQL92
/// output-name disambiguation, never for evaluating or simplifying expressions.
internal enum ExpressionIdentity {
    static func same(_ a: ResolvedExpression, _ b: ResolvedExpression) -> Bool {
        same(a, b, columns: nil)
    }
    /// Query results propagate collation more broadly than stored expressions.
    /// Derived collation does not change a value's bytes; operations which order
    /// values still compare their effective collation below.
    static func sameIndexKey(_ a: ResolvedExpression, _ b: ResolvedExpression) -> Bool {
        same(a, b, columns: nil, storageKey: true)
    }
    static func same(_ a: ResolvedExpression, _ b: ResolvedExpression, columns: ((Int, Int) -> Bool)?, storageKey: Bool = false) -> Bool {
        func same(_ a: ResolvedExpression, _ b: ResolvedExpression) -> Bool { Self.same(a, b, columns: columns, storageKey: storageKey) }
        func type(_ a: TypeDescriptor?, _ b: TypeDescriptor?, collation: Bool = true) -> Bool {
            guard storageKey, var left = a, var right = b else { return a == b }
            if ExpressionCollation.collatable(left.type) {
                left.collation = collation ? left.collation ?? SQLText(validatedUTF8: [67]) : nil
            }
            if ExpressionCollation.collatable(right.type) {
                right.collation = collation ? right.collation ?? SQLText(validatedUTF8: [67]) : nil
            }
            return left == right
        }
        guard type(a.type, b.type, collation: false) else { return false }
        func list(_ a: [ResolvedExpression], _ b: [ResolvedExpression]) -> Bool {
            a.count == b.count && zip(a, b).allSatisfy(same)
        }
        func optional(_ a: ResolvedExpression?, _ b: ResolvedExpression?) -> Bool {
            if let a, let b { return same(a, b) }; return a == nil && b == nil
        }
        guard a.rowPresence == b.rowPresence else { return false }
        switch (a.node, b.node) {
        case (.outerColumn(let x), .outerColumn(let y)): return x == y
        case (.subquery(let x), .subquery(let y)): return x === y
        case (.constant(let x), .constant(let y)):
            if x.isNull || y.isNull { return x.isNull && y.isNull }
            guard let type = a.type else { return false }
            guard let x = try? ValueCodec.encodeRawBody(x, type: type), let y = try? ValueCodec.encodeRawBody(y, type: type) else { return false }
            return x == y
        case (.column(let x), .column(let y)): return columns?(x, y) ?? (x == y)
        case (.parameter(let x), .parameter(let y)): return x == y
        case (.currentTimestamp, .currentTimestamp), (.currentDate, .currentDate): return true
        case (.dateInput(let x), .dateInput(let y)): return x == y
        case (.cast(let x, let xt, let xm), .cast(let y, let yt, let ym)): return type(xt, yt, collation: false) && xm == ym && same(x, y)
        case (.convert(let x, let xt), .convert(let y, let yt)): return type(xt, yt, collation: false) && same(x, y)
        case (.unary(let xo, let x), .unary(let yo, let y)): return xo == yo && same(x, y)
        case (.binary(let xo, let xa, let xb, let xt), .binary(let yo, let ya, let yb, let yt)):
            return xo == yo && type(xt, yt, collation: ["<", "<=", ">", ">="].contains(xo)) && same(xa, ya) && same(xb, yb)
        case (.isNull(let x, let xn), .isNull(let y, let yn)): return xn == yn && same(x, y)
        case (.distinct(let xa, let xb, let xt, let xn), .distinct(let ya, let yb, let yt, let yn)):
            return xn == yn && type(xt, yt, collation: false) && same(xa, ya) && same(xb, yb)
        case (.conditional(let x, let xf), .conditional(let y, let yf)):
            return x.count == y.count && zip(x, y).allSatisfy { same($0.0, $1.0) && same($0.1, $1.1) } && optional(xf, yf)
        case (.coalesce(let x), .coalesce(let y)), (.row(let x), .row(let y)): return list(x, y)
        case (.extreme(let x, let xg), .extreme(let y, let yg)): return xg == yg && type(a.type, b.type) && list(x, y)
        case (.nullif(let xa, let xb, let xt), .nullif(let ya, let yb, let yt)):
            return type(xt, yt, collation: false) && same(xa, ya) && same(xb, yb)
        case (.array(let x, let xn), .array(let y, let yn)): return xn == yn && list(x, y)
        case (.field(let x, let xi), .field(let y, let yi)): return xi == yi && same(x, y)
        case (.call(let x), .call(let y)):
            // Unlike text-returning kernels, the threshold-array overload uses
            // its arguments' collation to choose a numeric result.
            if storageKey, x.name == "width_bucket", x.arguments.count == 2 {
                do {
                    let left = try ExpressionCollation.combine(x.arguments).comparisonName()
                    let right = try ExpressionCollation.combine(y.arguments).comparisonName()
                    if (left?.description ?? "C") != (right?.description ?? "C") { return false }
                } catch { return false }
            }
            return x.name == y.name && x.hostID == y.hostID && type(x.resultType, y.resultType, collation: false) && x.evaluationOrder == y.evaluationOrder && list(x.arguments, y.arguments)
        default: return false
        }
    }
}
