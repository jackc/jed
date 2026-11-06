/// Aggregate and window result slots are evaluator addresses for computed
/// ORDER expressions. Their values still get a separate retained sort slot.
internal extension SelectPlan {
    func materializesOrderKey(_ term: ResolvedOrderTerm) -> Bool {
        if term.materialized { return true }
        guard case .column(let index) = term.expression.node else { return true }
        if index < 0 || index >= WindowQueryPlan.keyBase { return true }
        if let aggregation, index >= aggregation.sourceWidth { return true }
        return false
    }
}

internal enum OrderSyntax {
    static func materialized(_ syntax: SQLExpression) -> Bool {
        switch syntax.kind {
        case .column: return false
        case .parenthesized(let child), .collate(let child, _): return materialized(child)
        default: return true
        }
    }
}
