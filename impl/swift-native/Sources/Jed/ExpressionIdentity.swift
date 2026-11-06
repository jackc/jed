/// Structural identity ignores diagnostic positions. It is used for SQL92
/// output-name disambiguation, never for evaluating or simplifying expressions.
internal enum ExpressionIdentity {
    static func same(_ a: ResolvedExpression, _ b: ResolvedExpression) -> Bool {
        guard a.type == b.type else { return false }
        func list(_ a: [ResolvedExpression], _ b: [ResolvedExpression]) -> Bool {
            a.count == b.count && zip(a, b).allSatisfy(same)
        }
        func optional(_ a: ResolvedExpression?, _ b: ResolvedExpression?) -> Bool {
            if let a, let b { return same(a, b) }; return a == nil && b == nil
        }
        switch (a.node, b.node) {
        case (.constant(let x), .constant(let y)):
            if x.isNull || y.isNull { return x.isNull && y.isNull }
            guard let type = a.type else { return false }
            guard let x = try? ValueCodec.encodeRawBody(x, type: type), let y = try? ValueCodec.encodeRawBody(y, type: type) else { return false }
            return x == y
        case (.column(let x), .column(let y)), (.parameter(let x), .parameter(let y)): return x == y
        case (.currentTimestamp, .currentTimestamp), (.currentDate, .currentDate): return true
        case (.dateInput(let x), .dateInput(let y)): return x == y
        case (.cast(let x, let xt, let xm), .cast(let y, let yt, let ym)): return xt == yt && xm == ym && same(x, y)
        case (.convert(let x, let xt), .convert(let y, let yt)): return xt == yt && same(x, y)
        case (.unary(let xo, let x), .unary(let yo, let y)): return xo == yo && same(x, y)
        case (.binary(let xo, let xa, let xb, let xt), .binary(let yo, let ya, let yb, let yt)):
            return xo == yo && xt == yt && same(xa, ya) && same(xb, yb)
        case (.isNull(let x, let xn), .isNull(let y, let yn)): return xn == yn && same(x, y)
        case (.distinct(let xa, let xb, let xt, let xn), .distinct(let ya, let yb, let yt, let yn)):
            return xn == yn && xt == yt && same(xa, ya) && same(xb, yb)
        case (.conditional(let x, let xf), .conditional(let y, let yf)):
            return x.count == y.count && zip(x, y).allSatisfy { same($0.0, $1.0) && same($0.1, $1.1) } && optional(xf, yf)
        case (.coalesce(let x), .coalesce(let y)), (.row(let x), .row(let y)): return list(x, y)
        case (.extreme(let x, let xg), .extreme(let y, let yg)): return xg == yg && list(x, y)
        case (.nullif(let xa, let xb, let xt), .nullif(let ya, let yb, let yt)):
            return xt == yt && same(xa, ya) && same(xb, yb)
        case (.array(let x, let xn), .array(let y, let yn)): return xn == yn && list(x, y)
        case (.field(let x, let xi), .field(let y, let yi)): return xi == yi && same(x, y)
        case (.call(let x), .call(let y)):
            return x.name == y.name && x.hostID == y.hostID && x.resultType == y.resultType && x.evaluationOrder == y.evaluationOrder && list(x.arguments, y.arguments)
        default: return false
        }
    }
}
