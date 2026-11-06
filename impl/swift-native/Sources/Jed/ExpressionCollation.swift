/// Query provenance is separate from the persisted descriptor's concrete name.
/// An indeterminate expression differs from a synthetic column which has lost
/// its collation: the latter yields when an ordinary expression combines it.
internal enum CollationDerivation: Equatable {
    case none
    case implicit(SQLText)
    case explicit(SQLText)
    case indeterminate(SQLText, SQLText)
    case conflicted

    var name: SQLText? {
        switch self { case .implicit(let name), .explicit(let name): name; default: nil }
    }
    var asColumn: CollationDerivation {
        switch self {
        case .implicit(let name), .explicit(let name): .implicit(name)
        case .indeterminate, .conflicted: .conflicted
        case .none: .none
        }
    }
    static func combine(_ left: Self, _ right: Self) throws -> Self {
        switch (left, right) {
        case (.conflicted, let other), (let other, .conflicted): return other
        case (.explicit(let a), .explicit(let b)):
            guard a == b else { throw JedError("42P21", "collation mismatch between explicit collations \"\(a)\" and \"\(b)\"") }
            return left
        case (.explicit, _): return left
        case (_, .explicit): return right
        case (.indeterminate, _): return left
        case (_, .indeterminate): return right
        case (.implicit(let a), .implicit(let b)): return a == b ? left : .indeterminate(a, b)
        case (.implicit, .none): return left
        case (.none, .implicit): return right
        case (.none, .none): return .none
        }
    }
    /// Comparisons demand a determined collation even for byte-identity equality.
    func comparisonName() throws -> SQLText? {
        switch self {
        case .indeterminate, .conflicted: throw JedError("42P22", "could not determine which collation to use for string comparison").hinted(ErrorHints.indeterminateCollation)
        default: return name
        }
    }
    /// A sort/group expression diagnoses an implicit conflict at planning time;
    /// reading a conflicted synthetic column still diagnoses missing collation.
    func orderingName() throws -> SQLText? {
        if case .indeterminate(let a, let b) = self {
            throw JedError("42P21", "collation mismatch between implicit collations \"\(a)\" and \"\(b)\"").hinted(ErrorHints.implicitCollation)
        }
        return try comparisonName()
    }
}

internal enum ExpressionCollation {
    static func collatable(_ type: SQLType?) -> Bool {
        type == .scalar(.text) || type == .array(.scalar(.text))
    }
    static func combine(_ expressions: [ResolvedExpression], storage: Bool = false) throws -> CollationDerivation {
        try expressions.reduce(.none) { try CollationDerivation.combine($0, derive($1, storage: storage)) }
    }
    static func derive(_ expression: ResolvedExpression, storage: Bool = false) throws -> CollationDerivation {
        if let cached = expression.collation { return cached }
        guard collatable(expression.type?.type) else { return .none }
        switch expression.node {
        case .binary("||", let a, let b, _): return try combine([a, b], storage: storage)
        default: if storage { return .none }
        }
        switch expression.node {
        case .cast(let child, let target, _):
            if target.textLimit != nil { return try combine([child], storage: storage) }
            return try derive(child, storage: storage)
        case .convert(let child, _): return try derive(child, storage: storage)
        case .conditional(let branches, let fallback): return try combine(branches.map(\.1) + (fallback.map { [$0] } ?? []), storage: storage)
        case .coalesce(let values), .extreme(let values, _), .array(let values, _): return try combine(values, storage: storage)
        case .nullif(let a, let b, _): return try combine([a, b], storage: storage)
        case .subscriptValue(let array, _): return try combine([array], storage: storage)
        case .call(let function):
            let carries = GeneratedSpec.functions.contains { row in
                row.kind == "function" && row.name == function.name && ["text", "anyarray", "anycompatiblearray", "text[]"].contains(FunctionResolver.text(row.metadata, "result") ?? "")
            }
            return carries ? try combine(function.arguments, storage: storage) : .none
        default: return .none
        }
    }
}

extension ExpressionResolver {
    func finishCollation(_ input: ResolvedExpression) throws -> ResolvedExpression {
        var result = input
        let derivation = try ExpressionCollation.derive(input, storage: storageCollation)
        result.collation = derivation
        if ExpressionCollation.collatable(result.type?.type) { result.type?.collation = derivation.name }
        switch result.node {
        case .binary(let op, let a, let b, var type) where ["=", "<>", "<", "<=", ">", ">="].contains(op):
            if ExpressionCollation.collatable(type.type) { type.collation = try demandCollation([a, b]) }
            result.node = .binary(op, a, b, type)
        case .distinct(let a, let b, var type, let negated):
            if ExpressionCollation.collatable(type.type) { type.collation = try demandCollation([a, b]) }
            result.node = .distinct(a, b, type, negated)
        case .nullif(let a, let b, var type):
            if ExpressionCollation.collatable(type.type) { type.collation = try demandCollation([a, b]) }
            result.node = .nullif(a, b, type)
        case .quantified(let op, let all, let a, let array, var type):
            if ExpressionCollation.collatable(type.type) { type.collation = try demandCollation([a, array]) }
            result.node = .quantified(op, all: all, a, array, type)
        case .extreme(let values, _):
            if ExpressionCollation.collatable(result.type?.type) { result.type?.collation = try demandCollation(values) }
        case .call(let function) where ["@>", "<@", "&&"].contains(function.name) || function.name == "width_bucket" && function.arguments.count == 2:
            _ = try demandCollation(function.arguments)
        default: break
        }
        return result
    }
    func demandCollation(_ arguments: [ResolvedExpression]) throws -> SQLText? {
        let name = try ExpressionCollation.combine(arguments, storage: storageCollation).comparisonName()
        if let name { _ = try unicode.resolve(name) }
        return name
    }
    func collateExpression(_ source: SQLExpression, name: SQLText) throws -> ResolvedExpression {
        var value = try resolve(source)
        guard value.type == nil || ExpressionCollation.collatable(value.type?.type) else { throw JedError("42804", "collations are not supported by this type") }
        _ = try unicode.resolve(name)
        value.collation = .explicit(name); value.type?.collation = name
        return value
    }
}
