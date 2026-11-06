/// A prepared pattern is immutable. Its compile-charge flag and VM scratch live
/// on the execution, so reusing a prepared plan starts a fresh charge schedule.
internal final class ResolvedPattern {
    let subject: ResolvedExpression
    let pattern: ResolvedExpression
    let like: Bool
    let negated: Bool
    let insensitive: Bool
    let similar: Bool
    let program: RegexProgram?

    init(subject: ResolvedExpression, pattern: ResolvedExpression, like: Bool,
         negated: Bool, insensitive: Bool, similar: Bool, program: RegexProgram?) {
        self.subject = subject; self.pattern = pattern; self.like = like
        self.negated = negated; self.insensitive = insensitive; self.similar = similar; self.program = program
    }
    var isConstant: Bool { FunctionResolver.isConstant(subject) && FunctionResolver.isConstant(pattern) }

    func evaluate(context: ExpressionContext) throws -> SQLValue {
        let source = try context.evaluate(subject), value = try context.evaluate(pattern)
        if source.isNull || value.isNull { return .null }
        guard case .text(let sourceText) = source, case .text(let patternText) = value else {
            throw JedError("42804", "pattern operands must be text")
        }
        let matched: Bool
        if like {
            let unicode = context.functions.unicode
            let fold: (UInt32) -> UInt32 = insensitive ? { unicode.simpleLower($0) } : { $0 }
            matched = try Self.likeMatch(sourceText.description.unicodeScalars.map { fold($0.value) },
                                         patternText.description.unicodeScalars.map { fold($0.value) })
        } else {
            let state = context.patterns.state(for: self)
            if let program {
                if !state.compileCharged { state.compileCharged = true; try program.chargeCompile(context.meter) }
                matched = try program.isMatch(sourceText, meter: context.meter, scratch: state.scratch)
            } else {
                let unicode = context.functions.unicode
                let compiled = try RegexProgram.compile(pattern: patternText,
                    options: RegexOptions(ignoreCase: insensitive, dotAll: similar), fold: { unicode.simpleLower($0) })
                try compiled.chargeCompile(context.meter)
                matched = try compiled.isMatch(sourceText, meter: context.meter, scratch: state.scratch)
            }
        }
        return .boolean(matched != negated)
    }

    /// The last percent is the only backtrack point. This uses bounded stack
    /// space and matches the specified escape-error timing of the shared core.
    private static func likeMatch(_ source: [UInt32], _ pattern: [UInt32]) throws -> Bool {
        var input = 0, token = 0, star: Int?, absorbed = 0
        while input < source.count {
            if token < pattern.count && pattern[token] == 92 {
                guard token + 1 < pattern.count else { throw JedError("22025", "LIKE pattern must not end with escape character") }
                if source[input] == pattern[token + 1] { input += 1; token += 2; continue }
            } else if token < pattern.count && pattern[token] == 95 {
                input += 1; token += 1; continue
            } else if token < pattern.count && pattern[token] == 37 {
                star = token; absorbed = input; token += 1; continue
            } else if token < pattern.count && pattern[token] == source[input] {
                input += 1; token += 1; continue
            }
            guard let star else { return false }
            token = star + 1; absorbed += 1; input = absorbed
        }
        while token < pattern.count && pattern[token] == 37 { token += 1 }
        return token == pattern.count
    }
}

internal final class PatternExecution {
    final class State {
        // Retain the owner as well as its identity, preventing identity reuse.
        let owner: ResolvedPattern
        var compileCharged = false
        let scratch = RegexScratch()
        init(_ owner: ResolvedPattern) { self.owner = owner }
    }
    private var states: [ObjectIdentifier: State] = [:]
    private var functions: [ObjectIdentifier: FunctionState] = [:]
    func functionState(for function: ResolvedRegexFunction) -> FunctionState {
        let key = ObjectIdentifier(function)
        if let state = functions[key] { return state }
        let state = FunctionState(function); functions[key] = state; return state
    }
    func state(for pattern: ResolvedPattern) -> State {
        let key = ObjectIdentifier(pattern)
        if let state = states[key] { return state }
        let state = State(pattern); states[key] = state; return state
    }
}

extension ExpressionResolver {
    func patternExpression(_ operation: String, _ source: SQLExpression, _ syntax: SQLExpression,
                           escape: SQLExpression? = nil, negated: Bool = false, position: Int) throws -> ResolvedExpression {
        let text = TypeDescriptor(.scalar(.text)), boolean = TypeDescriptor(.scalar(.boolean))
        func operand(_ syntax: SQLExpression, code: String = "42804") throws -> ResolvedExpression {
            let value = try resolve(syntax, expected: text)
            guard value.type == nil || value.type?.type == text.type else { throw JedError(code, "pattern operands must be text").hinted(code == "42883" ? ErrorHints.function : ErrorHints.operatorBinary) }
            return value
        }
        let subject = try operand(source)
        var pattern = try operand(syntax)
        let like = operation == "like" || operation == "ilike", similar = operation == "similar"
        if similar || escape != nil {
            var arguments = [pattern]
            if let escape { arguments.append(try operand(escape, code: "42883")) }
            if similar, arguments.allSatisfy({ if case .constant(.text) = $0.node { true } else { false } }) {
                let values = arguments.map { expression -> SQLText in if case .constant(.text(let text)) = expression.node { return text }; preconditionFailure() }
                pattern = ResolvedExpression(type: text, node: .constant(.text(try Similar.translate(values[0], escape: values.count > 1 ? values[1] : nil).text)), position: syntax.position)
            } else {
                let name = similar ? "similar_to_escape" : "like_escape"
                let metadata = FunctionSpecFact(category: "string", name: name, kind: "function", metadata: ["null": .text("propagates"), "volatility": .text("immutable")])
                pattern = ResolvedExpression(type: text, node: .call(ResolvedFunction(metadata: metadata, arguments: arguments, resultType: text,
                    evaluationOrder: Array(arguments.indices), constantArguments: arguments.map(FunctionResolver.isConstant))), position: syntax.position)
            }
        }
        let insensitive = operation == "ilike" || operation == "~*" || operation == "!~*"
        let program: RegexProgram?
        if !like, case .constant(.text(let text)) = pattern.node {
            let data = unicode
            program = try RegexProgram.compile(pattern: text, options: RegexOptions(ignoreCase: insensitive, dotAll: similar), fold: { data.simpleLower($0) })
        } else { program = nil }
        return ResolvedExpression(type: boolean, node: .pattern(ResolvedPattern(subject: subject, pattern: pattern, like: like,
            negated: negated || operation == "!~" || operation == "!~*", insensitive: insensitive, similar: similar, program: program)), position: position)
    }

    func extractExpression(_ field: SQLText, _ source: SQLExpression, position: Int) throws -> ResolvedExpression {
        let argument = try resolve(source)
        guard argument.type == nil || [.scalar(.date), .scalar(.timestamp), .scalar(.timestamptz), .scalar(.interval)].contains(argument.type!.type) else {
            throw JedError("42883", "function extract does not exist for this source type")
        }
        let probe: SQLValue?
        switch argument.type?.type {
        case .scalar(.date): probe = .date(0)
        case .scalar(.timestamp): probe = .timestamp(0)
        case .scalar(.timestamptz): probe = .timestamptz(0)
        case .scalar(.interval): probe = .interval(SQLInterval(months: 0, days: 0, microseconds: 0))
        default: probe = nil
        }
        if let probe { _ = try TemporalFunctions.extract(unit: field, value: probe) }
        let unit = ResolvedExpression(type: TypeDescriptor(.scalar(.text)), node: .constant(.text(field)), position: position)
        let result = TypeDescriptor(.scalar(.decimal))
        let metadata = FunctionSpecFact(category: "temporal", name: "extract", kind: "function", metadata: ["null": .text("propagates"), "volatility": .text(argument.type?.type == .scalar(.timestamptz) && field.asciiLowercased.description != "epoch" ? "stable" : "immutable")])
        try checkFunction?(SQLText("extract"))
        return ResolvedExpression(type: result, node: .call(ResolvedFunction(metadata: metadata, arguments: [unit, argument], resultType: result,
            evaluationOrder: [0, 1], constantArguments: [true, FunctionResolver.isConstant(argument)])), position: position)
    }
}
