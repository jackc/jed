/// Persisted definitions bind host signatures to an explicit component/version.
/// Walking resolved nodes preserves the selected overload and adopted literals.
internal enum StoredExpressionDependencies {
    static func hosts(_ expressions: [ResolvedExpression], environment: Environment,
                      immutable: Bool = false) throws -> [CatalogHostDependency] {
        var result: [CatalogHostDependency] = [], seen: Set<UInt64> = []
        try walk(expressions) { expression in
            guard case .call(let call) = expression.node, let id = call.hostID, seen.insert(id).inserted else { return }
            guard id < environment.functions.count else { throw JedError("XX002", "stored host function is unavailable") }
            let host = environment.functions[Int(id)]
            guard let component = host.componentID else { throw JedError("42P17", "persisted expression requires host function component identity") }
            guard !immutable || host.volatility == .immutable else { throw JedError("42P17", "functions in index expressions must be marked IMMUTABLE") }
            result.append(CatalogHostDependency(name: host.name, arguments: host.arguments, result: host.result,
                                                component: component, version: host.semanticVersion ?? 0))
        }
        return result.sorted(by: precedes)
    }
    /// With expressions, verify calls only after ordinary resolution: an absent
    /// function must retain resolution's 42883, while changed registrations are
    /// XX002. Without expressions, inspect persisted metadata for host reporting.
    static func validateHosts(_ stored: [CatalogHostDependency], expressions: [ResolvedExpression]? = nil,
                              environment: Environment) throws {
        func mismatch() -> JedError { JedError("XX002", "stored expression host function dependency does not match the environment") }
        if let expressions {
            let actual: [CatalogHostDependency]
            do { actual = try hosts(expressions, environment: environment) }
            catch { throw mismatch() }
            for dependency in actual {
                guard stored.contains(where: {
                    $0.name == dependency.name && $0.arguments == dependency.arguments
                        && $0.result == dependency.result && $0.component == dependency.component
                        && $0.version == dependency.version
                }) else { throw mismatch() }
            }
            return
        }
        for dependency in stored {
            guard let host = environment.functions.first(where: { $0.name == dependency.name && $0.arguments == dependency.arguments }),
                  host.result == dependency.result, host.componentID == dependency.component,
                  (host.semanticVersion ?? 0) == dependency.version else { throw mismatch() }
        }
    }
    private static func precedes(_ a: CatalogHostDependency, _ b: CatalogHostDependency) -> Bool {
        a.name != b.name ? a.name < b.name : a.arguments.map(\.rawValue).lexicographicallyPrecedes(b.arguments.map(\.rawValue))
    }
    static func walk(_ expressions: [ResolvedExpression], visit: (ResolvedExpression) throws -> Void) rethrows {
        var work = Array(expressions.reversed())
        while let expression = work.popLast() {
            try visit(expression)
            work.append(contentsOf: children(expression).reversed())
        }
    }
    static func children(_ expression: ResolvedExpression) -> [ResolvedExpression] {
        switch expression.node {
        case .cast(let value, _, _), .convert(let value, _), .unary(_, let value), .isNull(let value, _), .field(let value, _): return [value]
        case .binary(_, let a, let b, _), .distinct(let a, let b, _, _), .nullif(let a, let b, _),
             .rangeOperation(_, let a, let b, _), .quantified(_, _, let a, let b, _): return [a, b]
        case .conditional(let arms, let fallback): return arms.flatMap { [$0.0, $0.1] } + (fallback.map { [$0] } ?? [])
        case .coalesce(let items), .row(let items), .extreme(let items, _), .array(let items, _): return items
        case .call(let call): return call.arguments
        case .subscriptValue(let value, let bounds): return [value] + bounds.flatMap { [$0.lower, $0.upper].compactMap { $0 } }
        case .json(let value): return [value.context] + (value.path.map { [$0] } ?? []) + value.passing.map(\.value)
            + [value.onEmpty?.value, value.onError?.value].compactMap { $0 }
        case .pattern(let value): return [value.subject, value.pattern]
        default: return []
        }
    }
}
