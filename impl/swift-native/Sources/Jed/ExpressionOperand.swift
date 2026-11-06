extension ExpressionResolver {
    /// A call's resolver families remain lazy and keep their own admission
    /// order. Only an original argument can reuse a local operand; synthesized
    /// defaults and rewritten syntax must resolve in their new context.
    func callArguments(_ call: SQLCall, operands supplied: [Operand]? = nil) -> FunctionResolver.Resolve {
        let operands = supplied ?? call.arguments.map { Operand($0.value, resolver: self) }
        return { syntax, expected in
            if syntax.span.byteStart < syntax.span.byteEnd {
                let matches = operands.filter { $0.syntax.span == syntax.span && $0.syntax.position == syntax.position }
                if matches.count == 1 { return try matches[0].resolve(expected: expected) }
            }
            return try self.resolve(syntax, expected: expected)
        }
    }

    /// Operator overloads inspect the same operands before choosing scalar,
    /// array, range, or JSON behavior. Retain each context-free resolution only
    /// for this operator: resolving a left-deep expression afresh in each probe
    /// would multiply preparation work at every level.
    final class Operand {
        let syntax: SQLExpression
        private unowned let resolver: ExpressionResolver
        private var resolved: ResolvedExpression?
        private lazy var children: [Operand] = {
            let fields: [SQLExpression]
            switch syntax.kind {
            case .array(let values), .row(let values): fields = values
            case .parenthesized(let value), .unary(_, let value): fields = [value]
            default: fields = []
            }
            return fields.map { Operand($0, resolver: resolver) }
        }()

        init(_ syntax: SQLExpression, resolver: ExpressionResolver) {
            self.syntax = syntax
            self.resolver = resolver
        }

        func resolve(expected: TypeDescriptor? = nil) throws -> ResolvedExpression {
            // These syntax forms consume their parent's type hint. Parameters
            // also consult statement-wide inference when the hint is absent.
            // Keep that work live; a compound operator's own result type never
            // depends on its parent's hint.
            if Self.usesHint(syntax) {
                // Rebuild the hint-sensitive wrapper every time, but retain
                // its child operands. A fixed-context call inside ARRAY/ROW
                // must not be rediscovered at each polymorphic probe.
                return try resolver.resolve(syntax, expected: expected, operands: children)
            }
            if let resolved { return resolved }
            let value = try resolver.resolve(syntax)
            resolved = value
            return value
        }

        private static func usesHint(_ syntax: SQLExpression) -> Bool {
            switch syntax.kind {
            case .number, .string, .null, .parameter, .row, .array: true
            case .parenthesized(let child), .unary(_, let child): usesHint(child)
            default: false
            }
        }
    }
}
