/// A host signature is an injected fact. Registration, privilege checks, and the
/// callback's lifetime remain the owning database/session's responsibility.
internal struct HostFunctionSignature {
    let id: UInt64
    let name: SQLText
    let arguments: [ScalarType]
    let result: ScalarType
    var cost: Int64 = 1
    var nullBehavior: String = "propagates"
    var volatility: String = "volatile"
}

internal struct ResolvedFunction {
    let metadata: FunctionSpecFact
    var arguments: [ResolvedExpression]
    let resultType: TypeDescriptor
    /// Slots in execution order, including synthesized defaults. Named notation
    /// is normalized to positional notation before execution (functions.md §11).
    var evaluationOrder: [Int]
    var constantArguments: [Bool]
    var hostID: UInt64?
    var variadicArray = false
    var name: String { metadata.name }
    var nullBehavior: String { FunctionResolver.text(metadata.metadata, "null") ?? "propagates" }
    var cost: Int64 { if case .integer(let n) = metadata.metadata["cost"] { n } else { 1 } }
}

/// The selection pass is pure: only the winning signature may resolve an
/// unknown/literal argument in context, so rejected candidates cannot infer a
/// parameter type or invoke a type's input parser.
internal enum FunctionResolver {
    typealias Resolve = (SQLExpression, TypeDescriptor?) throws -> ResolvedExpression
    private struct Placement {
        let syntax: [SQLExpression]
        let source: [Int?]
        let families: [String]
    }
    private struct Candidate {
        let fact: FunctionSpecFact
        let placement: Placement
        let targets: [ScalarType?]
        let score: Int
        let host: HostFunctionSignature?
    }
    static func text(_ row: [String: SpecDataValue], _ key: String) -> String? {
        guard case .text(let value) = row[key] else { return nil }; return value
    }
    static func strings(_ row: [String: SpecDataValue], _ key: String) -> [String] {
        guard case .array(let values) = row[key] else { return [] }
        return values.compactMap { if case .text(let value) = $0 { value } else { nil } }
    }
    static func familyType(_ family: String) -> ScalarType? {
        if family == "integer" { return .i64 }
        if family == "float" { return .f64 }
        return ScalarType.allCases.first { $0.name == family }
    }
    static func resolve(_ call: SQLCall, hosts: [HostFunctionSignature] = [], resolve: Resolve) throws -> ResolvedFunction {
        guard !call.star, !call.distinct, call.orderBy.isEmpty, call.withinGroup.isEmpty,
              call.filter == nil, call.over == nil, call.overName == nil else {
            throw JedError("42809", "aggregate or window modifiers require an aggregate or window function")
        }
        if let specialized = try special(call, resolve: resolve) { return specialized }
        let rows = GeneratedSpec.functions.filter { $0.kind == "function" && $0.name.utf8.elementsEqual(call.name.utf8) }
        try validateNotation(call)
        let original = try call.arguments.map { try resolve($0.value, nil) }
        let hostRows = hosts.filter { $0.name == call.name && $0.arguments.count == original.count }
        var placements: [(FunctionSpecFact, Placement)] = []
        for row in rows {
            if let placed = try place(call, row: row) { placements.append((row, placed)) }
        }
        func candidates(coercing: Bool, stringsUnknown: Bool) throws -> [Candidate] {
            var result: [Candidate] = []
            for (row, placement) in placements {
                var targets: [ScalarType?] = [], score = 0, matches = true
                for index in placement.syntax.indices {
                    let family = placement.families[index]
                    let sourceIndex = placement.source[index]
                    let argument = sourceIndex.map { original[$0] }
                    let unknownString = stringsUnknown && bareString(placement.syntax[index])
                    if unknownString && !stringFamilies.contains(family) { matches = false; break }
                    if sourceIndex == nil || argument?.type == nil || unknownString {
                        targets.append(familyType(family)); continue
                    }
                    if family == "any" { targets.append(nil); continue }
                    guard case .scalar(let source) = argument!.type!.type else { matches = false; break }
                    if TypeRules.family(source) == family { targets.append(source); continue }
                    guard coercing else { matches = false; break }
                    let literal = numericLiteral(placement.syntax[index])
                    let choices = ScalarType.allCases.compactMap { target -> (Int, ScalarType)? in
                        guard TypeRules.family(target) == family,
                              let cost = TypeRules.argumentCoercion(from: source, to: target, literal: literal),
                              literalFits(argument!, target: target, literal: literal) else { return nil }
                        return (cost, target)
                    }
                    guard let best = choices.min(by: { $0.0 < $1.0 }) else { matches = false; break }
                    targets.append(best.1); score += best.0
                }
                if matches { result.append(Candidate(fact: row, placement: placement, targets: targets, score: score, host: nil)) }
            }
            return result
        }
        func hostsMatching(coercing: Bool) -> [Candidate] {
            guard call.arguments.allSatisfy({ $0.name == nil && !$0.variadic }) else { return [] }
            return hostRows.compactMap { host in
                var score = 0
                for (index, target) in host.arguments.enumerated() {
                    guard case .scalar(let source) = original[index].type?.type else { return nil }
                    if source == target { continue }
                    let literal = numericLiteral(call.arguments[index].value)
                    guard coercing, let cost = TypeRules.argumentCoercion(from: source, to: target, literal: literal),
                          literalFits(original[index], target: target, literal: literal) else { return nil }
                    score += cost
                }
                let fact = FunctionSpecFact(category: "host", name: call.name.description, kind: "function", metadata: [
                    "result": .text(host.result.name), "null": .text(host.nullBehavior), "cost": .integer(host.cost), "volatility": .text(host.volatility)])
                let placed = Placement(syntax: call.arguments.map(\.value), source: original.indices.map { Optional($0) }, families: host.arguments.map { TypeRules.family($0) })
                return Candidate(fact: fact, placement: placed, targets: host.arguments.map { Optional($0) }, score: score, host: host)
            }
        }
        let selected: Candidate
        let exact = try candidates(coercing: false, stringsUnknown: false)
        if !exact.isEmpty { selected = try choose(exact, original: original, stringsUnknown: false) }
        else {
            let exactHosts = hostsMatching(coercing: false)
            if !exactHosts.isEmpty { selected = try choose(exactHosts, original: original, stringsUnknown: false) }
            else {
                let converted = try candidates(coercing: true, stringsUnknown: false) + hostsMatching(coercing: true)
                if !converted.isEmpty { selected = try choose(converted, original: original, stringsUnknown: false) }
                else {
                    let fallback = try candidates(coercing: false, stringsUnknown: true)
                    guard !fallback.isEmpty else { throw JedError("42883", "function does not exist: \(call.name)") }
                    selected = try choose(fallback, original: original, stringsUnknown: true)
                }
            }
        }
        var arguments: [ResolvedExpression] = []
        for index in selected.placement.syntax.indices {
            let syntax = selected.placement.syntax[index], target = selected.targets[index]
            let initial = selected.placement.source[index].map { original[$0] }
            var argument: ResolvedExpression
            if let target, initial == nil || initial?.type == nil || bareString(syntax) && initial?.type?.type != .scalar(target) || numericLiteral(syntax) != nil && target != .decimal && initial?.type?.type != .scalar(target) {
                argument = try resolve(syntax, TypeDescriptor(.scalar(target)))
            } else if let initial { argument = initial }
            else { argument = try resolve(syntax, target.map { TypeDescriptor(.scalar($0)) }) }
            if let target, argument.type?.type != .scalar(target) {
                let descriptor = TypeDescriptor(.scalar(target))
                argument = ResolvedExpression(type: descriptor, node: .convert(argument, descriptor), position: argument.position)
            }
            arguments.append(argument)
        }
        if call.arguments.last?.variadic == true {
            guard let last = arguments.last, case .array = last.type?.type else { throw JedError("42804", "VARIADIC argument must be an array") }
        }
        let result = try resultType(selected.fact, arguments: arguments)
        // The mixed-width-float rule is an ordinary, charged implicit cast only
        // for a built-in float slot returning f64. Coercion conversions are free.
        if selected.host == nil && result.type == .scalar(.f64) {
            for index in arguments.indices where selected.placement.families[index] == "float" && arguments[index].type?.type == .scalar(.f32) {
                let target = TypeDescriptor(.scalar(.f64)), argument = arguments[index]
                arguments[index] = ResolvedExpression(type: target, node: .cast(argument, target, .implicitCast), position: argument.position)
            }
        }
        return ResolvedFunction(metadata: selected.fact, arguments: arguments, resultType: result,
                                evaluationOrder: Array(arguments.indices), constantArguments: arguments.map(isConstant),
                                hostID: selected.host?.id, variadicArray: call.arguments.last?.variadic == true)
    }
    /// These signatures are explicitly resolver-routed in the spec; their
    /// results depend on argument types, so no scalar catalog row exists.
    private static func special(_ call: SQLCall, resolve: Resolve) throws -> ResolvedFunction? {
        let name = call.name.description
        guard ["date_trunc", "mod", "div", "gcd", "lcm"].contains(name) || name == "width_bucket" && call.arguments.count == 4 else { return nil }
        guard call.arguments.allSatisfy({ $0.name == nil && !$0.variadic }) else { throw JedError("42883", "function has no matching named or variadic signature") }
        let syntax = call.arguments.map(\.value)
        var arguments: [ResolvedExpression] = []
        let result: TypeDescriptor
        var volatility = "immutable"
        if name == "date_trunc" {
            guard syntax.count == 2 || syntax.count == 3 else { throw JedError("42883", "function does not exist: date_trunc") }
            arguments = [try resolve(syntax[0], TypeDescriptor(.scalar(.text))), try resolve(syntax[1], nil)]
            guard arguments[0].type?.type == .scalar(.text), let source = arguments[1].type,
                  [.scalar(.timestamp), .scalar(.timestamptz), .scalar(.interval)].contains(source.type) else { throw JedError("42883", "function does not exist: date_trunc") }
            result = source
            if syntax.count == 3 {
                guard source.type == .scalar(.timestamptz) else { throw JedError("42883", "function does not exist: date_trunc") }
                let zone = try resolve(syntax[2], TypeDescriptor(.scalar(.text)))
                guard zone.type?.type == .scalar(.text) else { throw JedError("42883", "function does not exist: date_trunc") }; arguments.append(zone)
            } else if source.type == .scalar(.timestamptz) { volatility = "stable" }
        } else {
            guard syntax.count == (name == "width_bucket" ? 4 : 2) else { throw JedError("42883", "function does not exist: \(name)") }
            arguments = try syntax.map { try resolve($0, nil) }
            let count = name == "width_bucket" ? 3 : 2
            let hasFloat = arguments.prefix(count).contains { $0.type?.type == .scalar(.f32) || $0.type?.type == .scalar(.f64) }
            if hasFloat {
                guard name == "mod" || name == "width_bucket" else { throw JedError("42883", "function does not exist: \(name)") }
                let float: ScalarType = name == "width_bucket" || arguments.prefix(count).contains { $0.type?.type == .scalar(.f64) } ? .f64 : .f32
                for index in 0..<count where numericLiteral(syntax[index]) != nil && arguments[index].type?.type != .scalar(float) {
                    arguments[index] = try resolve(syntax[index], TypeDescriptor(.scalar(float)))
                }
            }
            var common: SQLType?
            for argument in arguments.prefix(count) {
                if let type = argument.type?.type {
                    guard case .scalar(let scalar) = type, scalar.integerWidth != nil || scalar == .decimal || hasFloat && (scalar == .f32 || scalar == .f64) else { throw JedError("42883", "function does not exist: \(name)") }
                    if hasFloat && scalar != .f32 && scalar != .f64 { throw JedError(name == "mod" ? "42804" : "42883", "numeric argument types cannot be matched") }
                }
                do { common = try TypeRules.common(common, argument.type?.type) }
                catch { throw JedError(name == "mod" ? "42804" : "42883", "numeric argument types cannot be matched") }
            }
            if name == "div" { common = .scalar(.decimal) }
            if name == "width_bucket" { common = .scalar(hasFloat ? .f64 : .decimal) }
            let target = TypeDescriptor(common ?? .scalar(.i64))
            for index in 0..<count {
                if arguments[index].type == nil { arguments[index] = try resolve(syntax[index], target) }
                else if arguments[index].type?.type != target.type {
                    let original = arguments[index]
                    let cast = name == "mod" && original.type?.type == .scalar(.f32) && target.type == .scalar(.f64)
                    arguments[index] = ResolvedExpression(type: target, node: cast ? .cast(original, target, .implicitCast) : .convert(original, target), position: original.position)
                }
            }
            if name == "width_bucket" {
                if arguments[3].type == nil { arguments[3] = try resolve(syntax[3], TypeDescriptor(.scalar(.i64))) }
                guard case .scalar(let scalar) = arguments[3].type?.type, scalar.integerWidth != nil else { throw JedError("42883", "width_bucket count must be an integer") }
                result = TypeDescriptor(.scalar(.i32))
            } else { result = target }
        }
        let metadata = FunctionSpecFact(category: "special", name: name, kind: "function", metadata: ["result": .text(result.type.name), "null": .text("propagates"), "volatility": .text(volatility)])
        return ResolvedFunction(metadata: metadata, arguments: arguments, resultType: result, evaluationOrder: Array(arguments.indices), constantArguments: arguments.map(isConstant))
    }
    private static let stringFamilies: Set<String> = ["text", "any", "bytea", "uuid", "date", "timestamp", "timestamptz", "interval", "json", "jsonb", "jsonpath"]
    private static func choose(_ all: [Candidate], original: [ResolvedExpression], stringsUnknown: Bool) throws -> Candidate {
        let minimum = all.map(\.score).min()!
        var candidates = all.filter { $0.score == minimum }
        if candidates.contains(where: { $0.host == nil }) { candidates.removeAll { $0.host != nil } }
        if candidates.count == 1 { return candidates[0] }
        // Candidates may differ in trailing defaults. Examine each supplied
        // unknown at its normalized slot, never depend on catalog iteration order.
        for source in original.indices where original[source].type == nil || stringsUnknown && candidates.contains(where: { candidate in
            candidate.placement.source.enumerated().contains { $0.element == source && bareString(candidate.placement.syntax[$0.offset]) }
        }) {
            let families = Set(candidates.compactMap { candidate -> String? in
                guard let slot = candidate.placement.source.firstIndex(where: { $0 == source }) else { return nil }
                return candidate.placement.families[slot]
            })
            let family: String
            if families.contains("text") { family = "text" }
            else if families.count == 1 { family = families.first! }
            else { throw JedError("42725", "function call is ambiguous") }
            candidates.removeAll { candidate in
                guard let slot = candidate.placement.source.firstIndex(where: { $0 == source }) else { return true }
                return candidate.placement.families[slot] != family
            }
        }
        guard candidates.count == 1 else { throw JedError("42725", "function call is ambiguous") }
        return candidates[0]
    }
    private static func validateNotation(_ call: SQLCall) throws {
        var names = Set<SQLText>(), named = false
        for (index, argument) in call.arguments.enumerated() {
            if let name = argument.name {
                named = true
                guard names.insert(name).inserted else { throw JedError("42601", "argument name used more than once: \(name)") }
            } else if named { throw JedError("42601", "positional argument cannot follow named argument") }
            if argument.variadic && index != call.arguments.count - 1 { throw JedError("42601", "VARIADIC argument must be last") }
        }
    }
    private static func place(_ call: SQLCall, row: FunctionSpecFact) throws -> Placement? {
        let families = strings(row.metadata, "arg_families"), names = strings(row.metadata, "arg_names"), defaults = strings(row.metadata, "arg_defaults")
        let variadic = row.metadata["variadic"] == .boolean(true)
        if variadic {
            guard call.arguments.allSatisfy({ $0.name == nil }), call.arguments.count >= max(0, families.count - 1) else { return nil }
            // num_nulls/nonulls require at least one spread argument; JSON and
            // string builders permit an empty variadic tail.
            if ["num_nulls", "num_nonnulls", "concat"].contains(row.name) && call.arguments.isEmpty { return nil }
            if row.name == "concat_ws" && call.arguments.count < 2 { return nil }
            if call.arguments.contains(where: \.variadic), call.arguments.count != families.count { return nil }
            let expanded = call.arguments.indices.map { families[min($0, families.count - 1)] }
            return Placement(syntax: call.arguments.map(\.value), source: call.arguments.indices.map { Optional($0) }, families: expanded)
        }
        guard !call.arguments.contains(where: \.variadic), call.arguments.count <= families.count else { return nil }
        var slots = [Int?](repeating: nil, count: families.count)
        for (source, argument) in call.arguments.enumerated() {
            let slot: Int
            if let name = argument.name {
                guard let found = names.firstIndex(where: { $0.utf8.elementsEqual(name.utf8) }) else { return nil }; slot = found
            } else { slot = source }
            guard slots[slot] == nil else { throw JedError("42601", "argument specified more than once") }
            slots[slot] = source
        }
        var syntax: [SQLExpression] = []
        let defaultStart = families.count - defaults.count
        for index in slots.indices {
            if let source = slots[index] { syntax.append(call.arguments[source].value) }
            else {
                guard index >= defaultStart else { return nil }
                let span = SQLSourceSpan(byteStart: 0, byteEnd: 0, position: 1)
                syntax.append(SQLExpression(kind: .number(defaults[index - defaultStart]), span: span, position: 1))
            }
        }
        return Placement(syntax: syntax, source: slots, families: families)
    }
    static func resultType(_ row: FunctionSpecFact, arguments: [ResolvedExpression]) throws -> TypeDescriptor {
        guard let result = text(row.metadata, "result") else { throw JedError("XX000", "function result metadata is missing") }
        if result == "promoted" {
            var type: SQLType?
            for argument in arguments { type = try TypeRules.common(type, argument.type?.type) }
            guard let type else { throw JedError("42P18", "could not determine function result type") }
            return TypeDescriptor(type)
        }
        if let scalar = ScalarType.allCases.first(where: { $0.name == result }) { return TypeDescriptor(.scalar(scalar)) }
        if result.hasSuffix("[]"), let scalar = ScalarType.allCases.first(where: { $0.name == result.dropLast(2) }) { return TypeDescriptor(.array(.scalar(scalar))) }
        throw JedError("0A000", "function requires its polymorphic resolver: \(row.name)")
    }
    static func bareString(_ syntax: SQLExpression) -> Bool {
        switch syntax.kind { case .string: true; case .parenthesized(let child): bareString(child); default: false }
    }
    static func numericLiteral(_ syntax: SQLExpression) -> String? {
        switch syntax.kind {
        case .number(let spelling): spelling.utf8.contains(46) || (!spelling.lowercased().hasPrefix("0x") && spelling.utf8.contains { $0 == 101 || $0 == 69 }) ? "decimal_literal" : "integer_literal"
        case .parenthesized(let child): numericLiteral(child)
        case .unary(let op, let child) where op == "+" || op == "-": numericLiteral(child)
        default: nil
        }
    }
    private static func literalFits(_ argument: ResolvedExpression, target: ScalarType, literal: String?) -> Bool {
        guard literal == "integer_literal", target.integerWidth != nil else { return true }
        if case .constant(.integer(let value)) = argument.node { return target.admits(value) }
        return false
    }
    static func isConstant(_ expression: ResolvedExpression) -> Bool {
        switch expression.node {
        case .constant, .parameter: return true
        case .cast(let child, _, _), .convert(let child, _), .unary(_, let child), .isNull(let child, _), .field(let child, _): return isConstant(child)
        case .binary(_, let a, let b, _), .distinct(let a, let b, _, _), .nullif(let a, let b, _): return isConstant(a) && isConstant(b)
        case .coalesce(let children), .row(let children), .extreme(let children, _), .array(let children, _): return children.allSatisfy(isConstant)
        case .rangeOperation(_, let a, let b, _), .quantified(_, _, let a, let b, _): return isConstant(a) && isConstant(b)
        case .subscriptValue(let child, let indices): return isConstant(child) && indices.allSatisfy { ($0.lower.map(isConstant) ?? true) && ($0.upper.map(isConstant) ?? true) }
        case .conditional(let arms, let fallback): return arms.allSatisfy { isConstant($0.0) && isConstant($0.1) } && (fallback.map(isConstant) ?? true)
        case .call(let function):
            return function.hostID == nil && text(function.metadata.metadata, "volatility") != "volatile" && !["now", "current_date", "current_setting", "localtimestamp", "current_timestamp"].contains(function.name) && function.arguments.allSatisfy(isConstant)
        default: return false
        }
    }
}
