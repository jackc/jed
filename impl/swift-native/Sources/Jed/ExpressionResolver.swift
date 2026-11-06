internal struct ExpressionColumn {
    let name: SQLText
    let qualifier: SQLText?
    let type: TypeDescriptor
    let index: Int
    /// Synthetic relations supply their output derivation explicitly. Ordinary
    /// table columns use the descriptor's frozen name (C when omitted).
    var collation: CollationDerivation?
    var qualifiedOnly = false
    var unresolved = false
    var scalarFunction = false
    var hiddenFromStar = false
}

/// One resolver per statement. Parameter inference is shared across every
/// expression in that statement; a value cannot alter its resolved type.
internal final class ExpressionResolver {
    var columns: [ExpressionColumn]
    var composites: [SQLText: CompositeType]
    var parameters: [Int: TypeDescriptor] = [:]
    var parameterUses: Set<Int> = []
    var scalar: ScalarCasts.Context
    var hostFunctions: [HostFunctionSignature] = []
    var unicode = UnicodeDatabase()
    var storageCollation = false
    var referencedColumns: Set<Int> = []
    var checkFunction: ((SQLText) throws -> Void)?
    var viewExpansion = ViewExpansionState()
    var queryScope: SubqueryScope?
    var outerScopes: [[ExpressionColumn]] = []
    var outerReferences: Set<OuterColumnReference> = []
    var subqueryPlans: [SubqueryPlanKey: ResolvedSubquery] = [:]
    var cteBindings: [SQLText: CTEBinding] = [:]
    var resolveAggregate: ((SQLCall, Int) throws -> ResolvedExpression?)?
    var resolveWindow: ((SQLCall, Int) throws -> ResolvedExpression?)? = WindowQueryBuilder.forbidden
    /// Query stages may substitute an already-materialized expression value.
    /// Input expressions temporarily disable this hook while they resolve.
    var rewriteResolved: ((ResolvedExpression) throws -> ResolvedExpression)?

    init(columns: [ExpressionColumn] = [], composites: [SQLText: CompositeType] = [:], scalar: ScalarCasts.Context = .init()) {
        self.columns = columns; self.composites = composites; self.scalar = scalar
    }

    func resolve(_ syntax: SQLExpression, expected: TypeDescriptor? = nil) throws -> ResolvedExpression {
        do {
            let expression = try finishCollation(resolveInner(syntax, expected: expected))
            return try rewriteResolved?(expression) ?? expression
        }
        catch let error as JedError { throw error.at(syntax.position) }
    }

    private func resolveInner(_ syntax: SQLExpression, expected: TypeDescriptor?) throws -> ResolvedExpression {
        func result(_ type: TypeDescriptor?, _ node: ResolvedExpression.Node) -> ResolvedExpression {
            ResolvedExpression(type: type, node: node, position: syntax.position)
        }
        let boolean = TypeDescriptor(.scalar(.boolean))
        switch syntax.kind {
        case .parenthesized(let child): return try resolve(child, expected: expected)
        case .null: return result(expected, .constant(.null))
        case .number(let spelling):
            let decimal = spelling.utf8.contains(46) || (!spelling.lowercased().contains("0x") && spelling.utf8.contains { $0 == 101 || $0 == 69 })
            var type: ScalarType = decimal ? .decimal : .i64
            if case .scalar(let context) = expected?.type {
                if context == .decimal || context == .f32 || context == .f64 || (!decimal && context.integerWidth != nil) { type = context }
            }
            let value: SQLValue
            if decimal {
                let number = try SQLDecimal.parse(spelling)
                value = type == .f32 || type == .f64 ? try FloatText.fromDecimal(number, type: type) : .decimal(number)
            } else {
                let negative = spelling.first == "-", unsigned = negative ? String(spelling.dropFirst()) : spelling
                let magnitude = try TypeRules.unsignedLiteral(unsigned)
                guard magnitude <= 9_223_372_036_854_775_808 else { throw JedError("42601", "integer literal out of range") }
                if type == .decimal || type == .f32 || type == .f64 {
                    // The positive 2^63 token still has no integer value; only
                    // unary-minus folding admits its negative counterpart.
                    guard negative || magnitude <= UInt64(Int64.max) else { throw JedError("22003", "integer literal out of range") }
                    let integer = magnitude == 9_223_372_036_854_775_808 ? Int64.min : negative ? -Int64(magnitude) : Int64(magnitude)
                    value = type == .decimal ? .decimal(SQLDecimal(integer)) : try FloatText.fromDecimal(SQLDecimal(integer), type: type)
                } else {
                    guard negative || magnitude <= UInt64(Int64.max) else { throw JedError("22003", "integer literal out of range") }
                    let integer = magnitude == 9_223_372_036_854_775_808 ? Int64.min : negative ? -Int64(magnitude) : Int64(magnitude)
                    value = .integer(try ScalarCasts.checkedInteger(integer, type: type))
                }
            }
            return result(TypeDescriptor(.scalar(type)), .constant(value))
        case .string(let text):
            if let expected, adaptsString(to: expected.type) {
                return try literalInput(text, type: expected, position: syntax.position)
            }
            return result(TypeDescriptor(.scalar(.text)), .constant(.text(text)))
        case .boolean(let value): return result(boolean, .constant(.boolean(value)))
        case .parameter(let index):
            parameterUses.insert(index)
            if let expected, case .record = expected.type { return result(nil, .parameter(index)) }
            if let expected {
                let inferred = TypeDescriptor(expected.type)
                if let previous = parameters[index], previous.type != inferred.type {
                    if case .scalar(let a) = previous.type, case .scalar(let b) = inferred.type,
                       let aw = a.integerWidth, let bw = b.integerWidth {
                        parameters[index] = aw >= bw ? previous : inferred
                    } else { throw JedError("42804", "inconsistent types inferred for parameter $\(index)") }
                } else { parameters[index] = inferred }
            }
            // Each occurrence keeps its local constraint while admission uses
            // the widest integer constraint across the complete statement.
            return result(expected.map { TypeDescriptor($0.type) } ?? parameters[index], .parameter(index))
        case .column(let qualifier, let name):
            let candidates = columns.filter { $0.name == name && (qualifier == nil ? !$0.qualifiedOnly : $0.qualifier == qualifier) }
            if candidates.isEmpty { return try resolveOuterColumn(qualifier: qualifier, name: name, position: syntax.position) }
            guard !candidates.isEmpty else { throw JedError("42703", "column does not exist: \(name)") }
            guard candidates.count == 1 else { throw JedError("42702", "column reference is ambiguous: \(name)") }
            referencedColumns.insert(candidates[0].index)
            var value = result(candidates[0].unresolved ? nil : candidates[0].type, .column(candidates[0].index))
            if ExpressionCollation.collatable(value.type?.type) {
                value.collation = candidates[0].collation ?? .implicit(candidates[0].type.collation ?? SQLText(validatedUTF8: [67]))
            }
            return value
        case .scalarQuery(let query): return try subqueryExpression(query, kind: .scalar, position: syntax.position)
        case .exists(let query): return try subqueryExpression(query, kind: .exists, position: syntax.position)
        case .memberQuery(let left, let query, let negated):
            return try membershipSubquery(left, query: query, operation: "=", all: false, negated: negated, membership: true, position: syntax.position)
        case .quantifiedQuery(let operation, let left, let all, let query):
            return try membershipSubquery(left, query: query, operation: operation, all: all, negated: false, membership: false, position: syntax.position)
        case .collate(let source, let name): return try collateExpression(source, name: name)
        case .currentTimestamp: return result(TypeDescriptor(.scalar(.timestamptz)), .currentTimestamp)
        case .currentDate: return result(TypeDescriptor(.scalar(.date)), .currentDate)
        case .typedString(let target, let text):
            let type = try TypeRules.resolve(target, composites: composites)
            return try literalInput(text, type: type, position: syntax.position)
        case .cast(let source, let target):
            return try explicitCast(source, to: TypeRules.resolve(target, composites: composites))
        case .unary(let op, let source):
            let child = try resolve(source, expected: op == "not" ? boolean : expected)
            if op == "not" { try requireBoolean(child); return result(boolean, .unary(op, child)) }
            if let type = child.type?.type {
                guard case .scalar(let scalar) = type,
                      scalar.integerWidth != nil || [.decimal, .f32, .f64, .interval].contains(scalar) else {
                    throw JedError("42804", "invalid operand type for \(op)")
                }
            }
            return result(child.type, .unary(op, child))
        case .binary(let op, let left, let right):
            if ["=", "<>", "<", "<=", ">", ">="].contains(op), let arity = syntacticRowArity(left),
               let query = scalarSubquery(right) {
                let b = try subqueryExpression(query, kind: .scalar, rowArity: arity, position: right.position)
                let a = try resolve(left, expected: b.type)
                try TypeRules.comparable(a.type?.type, b.type?.type)
                return result(boolean, .binary(op, a, b, a.type ?? b.type ?? TypeDescriptor(.scalar(.text))))
            }
            if op == "at time zone" {
                let call = SQLCall(name: SQLText(validatedUTF8: Array("timezone".utf8)),
                    arguments: [SQLArgument(value: right), SQLArgument(value: left)])
                let function = try FunctionResolver.resolve(call) { try self.resolve($0, expected: $1) }
                return result(function.resultType, .call(function))
            }
            if ["~", "~*", "!~", "!~*"].contains(op) { return try patternExpression(op, left, right, position: syntax.position) }
            if let json = try jsonOperation(op, left, right, position: syntax.position) { return json }
            if let container = try containerOperation(op, left, right, position: syntax.position) { return container }
            if op == "||" { return try concatenate(left, right, position: syntax.position) }
            if op == "and" || op == "or" {
                let a = try resolve(left, expected: boolean), b = try resolve(right, expected: boolean)
                try requireBoolean(a); try requireBoolean(b)
                return result(boolean, .binary(op, a, b, boolean))
            }
            let (a, b, type) = try pair(left, right, operation: op)
            if ["=", "<>", "<", ">", "<=", ">="].contains(op) {
                try TypeRules.comparable(a.type?.type, b.type?.type)
                return result(boolean, .binary(op, a, b, type))
            }
            guard case .scalar(let scalar) = type.type else { throw JedError("0A000", "container operators are not yet connected") }
            if op == "||" {
                guard scalar == .text || scalar == .bytea else { throw JedError("42804", "invalid concatenation operand") }
            } else {
                guard scalar.integerWidth != nil || [.decimal, .f32, .f64, .date, .timestamp, .timestamptz, .interval].contains(scalar) else { throw JedError("42804", "invalid arithmetic operand") }
                if [.date, .timestamp, .timestamptz, .interval].contains(scalar), temporalResult(op, a.type?.type, b.type?.type) == nil {
                    throw JedError("42804", "invalid temporal arithmetic operands")
                }
                guard ["+", "-", "*", "/", "%"].contains(op) else { throw JedError("42883", "operator does not exist: \(op)") }
            }
            return result(type, .binary(op, a, b, type))
        case .isNull(let source, let negated): return result(boolean, .isNull(try resolve(source), negated))
        case .isJSON(let source, let kind, let unique, let negated):
            return try jsonPredicate(source, kind: kind, unique: unique, negated: negated, position: syntax.position)
        case .pattern(let op, let source, let pattern, let escape, let negated):
            return try patternExpression(op, source, pattern, escape: escape, negated: negated, position: syntax.position)
        case .extract(let unit, let source): return try extractExpression(unit, source, position: syntax.position)
        case .json(let expression): return try jsonExpression(expression, position: syntax.position)
        case .isDistinct(let left, let right, let negated):
            let (a, b, type) = try pair(left, right, operation: "="); try TypeRules.comparable(a.type?.type, b.type?.type)
            return result(boolean, .distinct(a, b, type, negated))
        case .between(let source, let lower, let upper, let negated):
            let low = SQLExpression(kind: .binary(">=", source, lower), span: syntax.span, position: syntax.position)
            let high = SQLExpression(kind: .binary("<=", source, upper), span: syntax.span, position: syntax.position)
            let combined = SQLExpression(kind: .binary("and", low, high), span: syntax.span, position: syntax.position)
            let resolved = try resolve(combined)
            return negated ? result(boolean, .unary("not", resolved)) : resolved
        case .member(let source, let candidates, let negated):
            var accumulated: ResolvedExpression?
            for candidate in candidates {
                let comparison = try resolve(SQLExpression(kind: .binary("=", source, candidate), span: syntax.span, position: syntax.position))
                if let previous = accumulated { accumulated = result(boolean, .binary("or", previous, comparison, boolean)) }
                else { accumulated = comparison }
            }
            let value = accumulated ?? result(boolean, .constant(.boolean(false)))
            return negated ? result(boolean, .unary("not", value)) : value
        case .conditional(let operand, let arms, let fallback):
            let conditions = try arms.map { arm -> ResolvedExpression in
                let condition = try operand.map { try resolve(SQLExpression(kind: .binary("=", $0, arm.condition), span: syntax.span, position: syntax.position)) } ?? resolve(arm.condition, expected: boolean)
                try requireBoolean(condition); return condition
            }
            let sources = arms.map(\.result) + (fallback.map { [$0] } ?? [])
            let outputs = try sources.map { try resolve($0) }
            var type = try common(outputs, keepTypmod: true)
            if fallback == nil { type.decimal = nil; type.textLimit = nil; type.temporal = nil }
            let unified = try zip(sources, outputs).map { source, output in
                try promote(output.type == nil ? resolve(source, expected: type) : output, to: type, floatCast: true)
            }
            let branches = Array(zip(conditions, unified.prefix(arms.count)))
            return result(type, .conditional(branches, fallback == nil ? nil : unified.last))
        case .conditionalFunction(let name, let args):
            if name == "nullif" {
                guard args.count == 2 else { throw JedError("42601", "NULLIF requires two arguments") }
                var (a, b, type) = try pair(args[0], args[1], floatCast: false, operation: "=", promoteValues: false)
                try TypeRules.comparable(a.type?.type, b.type?.type)
                if type.type == .scalar(.decimal), case .scalar(let left) = a.type?.type, left.integerWidth != nil {
                    a = try promote(a, to: type, floatCast: false)
                }
                return result(a.type ?? type, .nullif(a, b, type))
            }
            let children = try args.map { try resolve($0) }, type = try common(children, keepTypmod: true)
            let unified = try zip(args, children).map { source, child in
                try promote(child.type == nil ? resolve(source, expected: type) : child, to: type, floatCast: true)
            }
            if name == "coalesce" { return result(type, .coalesce(unified)) }
            if name == "greatest" || name == "least" { try TypeRules.comparable(type.type, type.type); return result(type, .extreme(unified, greatest: name == "greatest")) }
            throw JedError("42883", "conditional function does not exist: \(name)")
        case .row(let fields):
            let hints = expected.flatMap { TypeRules.fields($0.type) }
            let children = try fields.enumerated().map { index, syntax -> ResolvedExpression in
                let child = try resolve(syntax)
                if child.type == nil, let hints, hints.count == fields.count { return try resolve(syntax, expected: hints[index].descriptor) }
                return child
            }
            let types = children.enumerated().map { index, child in TypeField(name: SQLText(validatedUTF8: Array("f\(index + 1)".utf8)), descriptor: child.type ?? TypeDescriptor(.scalar(.text))) }
            return result(TypeDescriptor(.record(types)), .row(children))
        case .array(let fields): return try arrayConstructor(fields, expected: expected, position: syntax.position)
        case .subscripted(let source, let subscripts):
            let child = try resolve(source)
            guard case .array(let element) = child.type?.type else { throw JedError("42804", "subscripted value is not an array") }
            let indices = try subscripts.map { entry -> ResolvedSubscript in
                func index(_ syntax: SQLExpression?) throws -> ResolvedExpression? {
                    guard let syntax else { return nil }
                    let expression = try resolve(syntax, expected: TypeDescriptor(.scalar(.i64)))
                    if let type = expression.type?.type {
                        guard case .scalar(let scalar) = type, scalar.integerWidth != nil else { throw JedError("42804", "array subscript must be an integer") }
                    }
                    return expression
                }
                return ResolvedSubscript(lower: try index(entry.lower), upper: try index(entry.upper), isSlice: entry.isSlice)
            }
            return result(subscripts.contains(where: \.isSlice) ? child.type : TypeDescriptor(element), .subscriptValue(child, indices))
        case .quantified(let op, let operand, let all, let source):
            var array = try resolve(source)
            if array.type == nil { throw JedError("42P18", "could not determine ANY/ALL array type") }
            guard case .array(let element) = array.type?.type else { throw JedError("42809", "ANY/ALL requires an array") }
            let value = try resolve(operand, expected: TypeDescriptor(element))
            if let type = value.type { array = try resolve(source, expected: TypeDescriptor(.array(type.type))) }
            do { try TypeRules.comparable(value.type?.type, element) }
            catch { throw JedError("42883", "ANY/ALL comparison operator does not exist") }
            let compared = try TypeRules.common(value.type?.type, element) ?? element
            return result(boolean, .quantified(op, all: all, value, array, TypeDescriptor(compared)))
        case .field(let source, let name):
            let child = try resolve(source)
            guard let type = child.type?.type, let fields = TypeRules.fields(type) else { throw JedError("42809", "field selection requires a composite type") }
            guard let name else { throw JedError("0A000", "record wildcard must expand in a target list") }
            guard let index = fields.firstIndex(where: { $0.name == name }) else { throw JedError("42703", "field does not exist: \(name)") }
            return result(fields[index].descriptor, .field(child, index))
        case .call(let call):
            if let window = try resolveWindow?(call, syntax.position) { return window }
            if let aggregate = try resolveAggregate?(call, syntax.position) { return aggregate }
            var function = try JsonFunctionResolver.resolve(call) { try self.resolve($0, expected: $1) }
                ?? ContainerFunctionResolver.resolve(call) { try self.resolve($0, expected: $1) }
                ?? FunctionResolver.resolve(call, hosts: hostFunctions) { try self.resolve($0, expected: $1) }
            function.regex = try ResolvedRegexFunction.prepare(function, unicode: unicode)
            try checkFunction?(call.name)
            return result(function.resultType, .call(function))
        }
    }

    /// A constructor cast is a typed ROW whose fields are ordinary explicit
    /// casts. It adds no synthetic outer operation or dynamic record coercion.
    private func explicitCast(_ source: SQLExpression, to type: TypeDescriptor) throws -> ResolvedExpression {
        if case .parenthesized(let child) = source.kind { return try explicitCast(child, to: type) }
        func result(_ type: TypeDescriptor?, _ node: ResolvedExpression.Node) -> ResolvedExpression {
            ResolvedExpression(type: type, node: node, position: source.position)
        }
        if let text = stringLiteral(source) {
            return try literalInput(text, type: type, position: source.position)
        }
        if nullLiteral(source) {
            if case .scalar(let scalar) = type.type, scalar.integerWidth != nil || [.decimal, .f32, .f64, .boolean].contains(scalar) {
                return result(type, .cast(result(nil, .constant(.null)), type, .explicit))
            }
            return result(type, .constant(.null))
        }
        if case .composite(let composite) = type.type, case .row(let fields) = source.kind {
            guard fields.count == composite.fields.count else { throw JedError("42846", "cannot cast type record to \(composite.name)") }
            let children = try zip(fields, composite.fields).map { try finishCollation(explicitCast($0, to: $1.descriptor)) }
            return result(type, .row(children))
        }
        let hint = type.type == .scalar(.boolean) && numberLiteral(source) ? TypeDescriptor(.scalar(.i32)) : type
        let child = try resolve(source, expected: hint)
        if child.type?.type == type.type {
            switch type.type {
            case .range, .composite, .array: return child
            default: break
            }
        }
        if let from = child.type?.type, !TypeRules.admitsCast(from: from, to: type.type, mode: .explicit) {
            if case .array = from, case .array = type.type { throw JedError("0A000", "this array element cast is not supported") }
            if case .range = from { throw JedError("0A000", "range casts are not supported") }
            throw JedError(type.type == .scalar(.jsonpath) || TypeRules.fields(from) != nil || TypeRules.fields(type.type) != nil ? "42846" : "42804", "cannot cast \(from.name) to \(type.type.name)")
        }
        return result(type, .cast(child, type, .explicit))
    }

    private func requireBoolean(_ expression: ResolvedExpression) throws {
        if let type = expression.type, type.type != .scalar(.boolean) { throw JedError("42804", "boolean expression required") }
    }
    private func numberLiteral(_ expression: SQLExpression) -> Bool {
        switch expression.kind { case .number: true; case .parenthesized(let child): numberLiteral(child); default: false }
    }
    private func nullLiteral(_ expression: SQLExpression) -> Bool {
        switch expression.kind { case .null: true; case .parenthesized(let child): nullLiteral(child); default: false }
    }
    private func literalInput(_ text: SQLText, type: TypeDescriptor, position: Int) throws -> ResolvedExpression {
        if type.type == .scalar(.date), ["today", "tomorrow", "yesterday", "now"].contains(String(decoding: Temporal.trimmed(text.utf8).map(Temporal.asciiLower), as: UTF8.self)) {
            return ResolvedExpression(type: type, node: .dateInput(text), position: position)
        }
        var context = scalar
        context.meter = nil // literal conversion is preparation, not per-row work
        var nonimmutable = false
        context.sessionDependency = { nonimmutable = true }
        let value = try ContainerText.input(text, to: type, explicit: true, context: context)
        return ResolvedExpression(type: type, node: .constant(value), position: position, nonimmutable: nonimmutable)
    }
    private func stringLiteral(_ expression: SQLExpression) -> SQLText? {
        switch expression.kind { case .string(let text): text; case .parenthesized(let child): stringLiteral(child); default: nil }
    }
    private func adaptable(_ expression: SQLExpression) -> Bool {
        switch expression.kind {
        case .number, .string, .null, .parameter: true
        case .parenthesized(let child): adaptable(child)
        default: false
        }
    }
    private func adaptsString(to type: SQLType) -> Bool {
        switch type {
        case .scalar(let scalar): [.bytea, .uuid, .timestamp, .timestamptz, .date, .interval, .f32, .f64, .json, .jsonb, .jsonpath].contains(scalar)
        case .array, .range, .composite: true
        case .record: false
        }
    }
    private func pair(_ left: SQLExpression, _ right: SQLExpression, floatCast: Bool = true, operation: String? = nil,
                      promoteValues: Bool = true) throws -> (ResolvedExpression, ResolvedExpression, TypeDescriptor) {
        var a: ResolvedExpression, b: ResolvedExpression
        func hint(_ sibling: TypeDescriptor?, _ literal: SQLExpression) -> TypeDescriptor? {
            if (operation == "*" || operation == "/") && sibling?.type == .scalar(.interval) {
                return TypeDescriptor(.scalar(.decimal))
            }
            if operation == "+", stringLiteral(literal) != nil,
               sibling?.type == .scalar(.timestamp) || sibling?.type == .scalar(.timestamptz) { return TypeDescriptor(.scalar(.interval)) }
            return sibling
        }
        if adaptable(left) && !adaptable(right) { b = try resolve(right); a = try resolve(left, expected: hint(b.type, left)) }
        else { a = try resolve(left); b = try resolve(right, expected: adaptable(right) ? hint(a.type, right) : nil) }
        if a.type == nil { a = try resolve(left, expected: b.type) }
        if b.type == nil { b = try resolve(right, expected: a.type) }
        if let operation, ["=", "<>", "<", ">", "<=", ">="].contains(operation),
           a.type?.type == .scalar(.json) || b.type?.type == .scalar(.json) { throw JedError("42883", "json has no comparison operator") }
        if let l = a.type, let r = b.type, TypeRules.fields(l.type) != nil && TypeRules.fields(r.type) != nil {
            a = try resolve(left, expected: r)
            b = try resolve(right, expected: a.type)
        }
        if let operation, let temporal = temporalResult(operation, a.type?.type, b.type?.type) { return (a, b, TypeDescriptor(.scalar(temporal))) }
        let type: TypeDescriptor
        if let l = a.type, let r = b.type, TypeRules.fields(l.type) != nil && TypeRules.fields(r.type) != nil {
            try TypeRules.comparable(l.type, r.type); type = l
        } else { type = try common([a, b]) }
        if !promoteValues { return (a, b, type) }
        return (try promote(a, to: type, floatCast: floatCast), try promote(b, to: type, floatCast: floatCast), type)
    }
    private func temporalResult(_ op: String, _ left: SQLType?, _ right: SQLType?) -> ScalarType? {
        guard case .scalar(let a) = left, case .scalar(let b) = right else { return nil }
        if (op == "*" || op == "/") && a == .interval && (b.integerWidth != nil || b == .decimal) { return .interval }
        if op == "*" && b == .interval && (a.integerWidth != nil || a == .decimal) { return .interval }
        guard op == "+" || op == "-" else { return nil }
        if a == .interval && b == .interval { return .interval }
        if (a == .timestamp || a == .timestamptz) && b == .interval { return a }
        if op == "+" && a == .interval && (b == .timestamp || b == .timestamptz) { return b }
        if a == .date && b == .interval || op == "+" && a == .interval && b == .date { return .timestamp }
        if a == .date && b.integerWidth != nil || op == "+" && a.integerWidth != nil && b == .date { return .date }
        if op == "-" && a == b && (a == .timestamp || a == .timestamptz) { return .interval }
        if op == "-" && a == .date && b == .date { return .i32 }
        return nil
    }
    private func concatenate(_ left: SQLExpression, _ right: SQLExpression, position: Int) throws -> ResolvedExpression {
        var a = try resolve(left), b = try resolve(right)
        let text = TypeDescriptor(.scalar(.text))
        let typedTextA = a.type?.type == text.type && stringLiteral(left) == nil
        let typedTextB = b.type?.type == text.type && stringLiteral(right) == nil
        for scalar in [ScalarType.jsonb, .bytea] {
            let type = TypeDescriptor(.scalar(scalar))
            if a.type?.type == type.type && !typedTextB || b.type?.type == type.type && !typedTextA {
                a = try resolve(left, expected: type); b = try resolve(right, expected: type)
                guard (a.type == nil || a.type?.type == type.type) && (b.type == nil || b.type?.type == type.type) else {
                    throw JedError("42883", "operator does not exist for concatenation operands")
                }
                return ResolvedExpression(type: type, node: .binary("||", a, b, type), position: position)
            }
        }
        guard a.type == nil || b.type == nil || a.type?.type == text.type || b.type?.type == text.type else {
            throw JedError("42883", "operator does not exist for concatenation operands")
        }
        func asText(_ syntax: SQLExpression, _ value: ResolvedExpression) throws -> ResolvedExpression {
            if value.type == nil { return try resolve(syntax, expected: text) }
            if value.type?.type == text.type { return value }
            guard case .scalar = value.type?.type else { throw JedError("0A000", "container text concatenation is not supported") }
            return ResolvedExpression(type: text, node: .cast(value, text, .explicit), position: syntax.position)
        }
        return ResolvedExpression(type: text, node: .binary("||", try asText(left, a), try asText(right, b), text), position: position)
    }
    private func common(_ expressions: [ResolvedExpression], keepTypmod: Bool = false) throws -> TypeDescriptor {
        var type: SQLType?
        for expression in expressions { type = try TypeRules.common(type, expression.type?.type) }
        var descriptor = TypeDescriptor(type ?? .scalar(.text))
        if keepTypmod, let first = expressions.first?.type, expressions.allSatisfy({
            $0.type?.type == first.type && $0.type?.decimal == first.decimal
                && $0.type?.textLimit == first.textLimit && $0.type?.temporal == first.temporal
        }) {
            descriptor.decimal = first.decimal; descriptor.textLimit = first.textLimit; descriptor.temporal = first.temporal
        }
        if ExpressionCollation.collatable(descriptor.type) {
            descriptor.collation = try ExpressionCollation.combine(expressions, storage: storageCollation).name
        }
        return descriptor
    }
    private func promote(_ expression: ResolvedExpression, to target: TypeDescriptor, floatCast: Bool) throws -> ResolvedExpression {
        if expression.type == nil { var output = expression; output.type = target; return output }
        if expression.type?.type == target.type { return expression }
        if TypeRules.fields(expression.type!.type) != nil && TypeRules.fields(target.type) != nil { return expression }
        if expression.type?.type == .scalar(.f32) && target.type == .scalar(.f64) && floatCast {
            return ResolvedExpression(type: target, node: .cast(expression, target, .implicitCast), position: expression.position)
        }
        return ResolvedExpression(type: target, node: .convert(expression, target), position: expression.position)
    }
}
