/// Shared diagnostic metadata is selected at the innermost resolution site.
/// It never evaluates an expression or contributes SQL work.
internal enum ErrorHints {
    static let function = "No function matches the given name and argument types. You might need to add explicit type casts."
    static let ambiguousFunction = "Could not choose a best candidate function. You might need to add explicit type casts."
    static let operatorBinary = "No operator matches the given name and argument types. You might need to add explicit type casts."
    static let operatorPrefix = "No operator matches the given name and argument type. You might need to add an explicit type cast."
    static let ambiguousOperator = "Could not choose a best candidate operator. You might need to add explicit type casts."
    static let ordering = "Use an explicit ordering operator or modify the query."
    static let assignment = "You will need to rewrite or cast the expression."
    static let recursiveType = "Cast the output of the non-recursive term to the correct type."
    static let recursiveCollation = "Use the COLLATE clause to set the collation of the non-recursive term."
    static let implicitCollation = "You can choose the collation by applying the COLLATE clause to one or both expressions."
    static let indeterminateCollation = "Use the COLLATE clause to set the collation explicitly."
    static let lateralDetail = "The combining JOIN type must be INNER or LEFT for a LATERAL reference."
}

extension JedError {
    func hinted(_ text: String?) -> JedError {
        var error = self
        if !error.hintDecided { error.hint = text; error.hintDecided = true }
        return error
    }
    func detailed(_ text: String) -> JedError {
        var error = self
        if error.detail == nil { error.detail = text }
        return error
    }
}

extension SQLExpression {
    /// PostgreSQL exprLocation: grouping parentheses are transparent. A CAST
    /// keyword over a nonliteral deliberately has no syntactic location.
    var expressionLocation: Int {
        var current = self
        while true {
            switch current.kind {
            case .parenthesized(let child), .collate(let child, _), .field(let child, _), .subscripted(let child, _),
                 .isNull(let child, _), .isJSON(let child, _, _, _): current = child
            case .binary("at time zone", _, _): return current.position
            case .binary(_, let left, _), .isDistinct(let left, _, _), .member(let left, _, _),
                 .memberQuery(let left, _, _), .between(let left, _, _, _), .pattern(_, let left, _, _, _),
                 .quantified(_, let left, _, _), .quantifiedQuery(_, let left, _, _): current = left
            case .cast(let child, _):
                if current.position != current.span.position || child.unknownLiteral { current = child }
                else { return 0 }
            default: return current.position
            }
        }
    }
    var unknownLiteral: Bool {
        switch kind {
        case .string, .null: true
        case .parenthesized(let child): child.unknownLiteral
        default: false
        }
    }
}

internal enum ResolutionDiagnostics {
    static func locate(_ input: JedError, syntax: SQLExpression) -> JedError {
        guard !input.locationDecided else { return input }
        var error = input, position = 0, hint: String?
        let operatorError = error.code == "42804" || error.code == "42883"
        switch syntax.kind {
        case .column:
            if ["42703", "42702", "42P01", "42P09", "42P10"].contains(error.code) { position = syntax.position }
        case .call:
            if ["42883", "42725"].contains(error.code) {
                position = syntax.position
                hint = error.code == "42725" ? ErrorHints.ambiguousFunction : ErrorHints.function
            }
        case .binary(let op, let left, let right) where operatorError && op != "and" && op != "or":
            position = syntax.position
            hint = op == "at time zone" ? ErrorHints.function
                : left.unknownLiteral && right.unknownLiteral ? ErrorHints.ambiguousOperator : ErrorHints.operatorBinary
        case .unary("-", let child) where operatorError:
            position = syntax.position; hint = child.unknownLiteral ? ErrorHints.ambiguousOperator : ErrorHints.operatorPrefix
        case .isDistinct, .pattern, .member, .between, .memberQuery, .quantified, .quantifiedQuery:
            if operatorError { position = syntax.position; hint = ErrorHints.operatorBinary }
        case .conditionalFunction("nullif", _) where operatorError:
            position = syntax.position; hint = ErrorHints.operatorBinary
        case .string: position = syntax.position
        case .cast(_, let type):
            if error.code == "42704" { position = type.span.position }
            else if error.code == "42804" || error.code == "42846" { position = syntax.position }
        case .typedString(let type, _): if error.code == "42704" { position = type.span.position }
        case .extract where error.code == "42883": position = syntax.position; hint = ErrorHints.function
        default: break
        }
        if let hint { error = error.hinted(hint) }
        return error.at(position)
    }
    static func common(_ left: SQLType?, _ right: ResolvedExpression) throws -> SQLType? {
        do { return try TypeRules.common(left, right.type?.type) }
        catch let error as JedError { throw error.at(right.location) }
    }
    static func comparable(_ type: SQLType?, at position: Int, ordering: Bool) throws {
        do { try TypeRules.comparable(type, type) }
        catch let error as JedError { throw error.hinted(ordering ? ErrorHints.ordering : nil).at(position) }
    }
}
