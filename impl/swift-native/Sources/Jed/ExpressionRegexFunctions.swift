/// Immutable preparation shared by repeated executions of one resolved call.
/// Execution state and VM scratch are deliberately owned by PatternExecution.
internal final class ResolvedRegexFunction {
    let program: RegexProgram?
    let similar: Similar.SubstringProgram?
    init(program: RegexProgram? = nil, similar: Similar.SubstringProgram? = nil) {
        self.program = program; self.similar = similar
    }
    static func prepare(_ function: ResolvedFunction, unicode: UnicodeDatabase) throws -> ResolvedRegexFunction? {
        guard function.hostID == nil, function.arguments.count >= 2 else { return nil }
        let name = function.name, args = function.arguments
        let substring = name == "substring" && args[1].type?.type == .scalar(.text)
        guard substring || name.hasPrefix("regexp_") else { return nil }
        func literal(_ index: Int) -> SQLText? {
            guard args.indices.contains(index), case .constant(.text(let text)) = args[index].node else { return nil }
            return text
        }
        if substring && args.count == 3 {
            if args.contains(where: { if case .constant(.null) = $0.node { true } else { false } }) { return ResolvedRegexFunction() }
            if let pattern = literal(1), let escape = literal(2) {
                return ResolvedRegexFunction(similar: try Similar.compileSubstring(Similar.translate(pattern, escape: escape)))
            }
            return ResolvedRegexFunction()
        }
        let flagIndex: Int?
        switch name {
        case "regexp_replace", "regexp_count": flagIndex = 3
        case "regexp_substr": flagIndex = 4
        case "regexp_instr": flagIndex = 5
        case "substring": flagIndex = nil
        default: flagIndex = 2
        }
        let ignoreCase: Bool?
        if let flagIndex, args.indices.contains(flagIndex) { ignoreCase = literal(flagIndex).map { $0.utf8.contains(105) } }
        else { ignoreCase = false }
        if let pattern = literal(1), let ignoreCase {
            // Validate flag spelling during execution, after numeric arguments;
            // only the literal i bit is needed to prepare the immutable NFA.
            let program = try RegexProgram.compile(pattern: pattern, options: RegexOptions(ignoreCase: ignoreCase), fold: { unicode.simpleLower($0) })
            return ResolvedRegexFunction(program: program)
        }
        return ResolvedRegexFunction()
    }
}

extension PatternExecution {
    final class FunctionState {
        let owner: ResolvedRegexFunction
        var compileCharged = false
        let scratch = RegexScratch()
        let similarScratch = (0..<4).map { _ in RegexScratch() }
        init(_ owner: ResolvedRegexFunction) { self.owner = owner }
    }
    func regexExecution(_ function: ResolvedFunction) -> ScalarFunctions.RegexExecution {
        guard let prepared = function.regex else { return ScalarFunctions.RegexExecution() }
        let state = functionState(for: prepared)
        let charge = prepared.program == nil || !state.compileCharged
        if prepared.program != nil { state.compileCharged = true }
        return ScalarFunctions.RegexExecution(program: prepared.program, chargeCompile: charge, scratch: state.scratch)
    }
    func similarSubstring(_ function: ResolvedFunction, values: [SQLValue], meter: CostMeter) throws -> SQLValue {
        if values[1].isNull || values[2].isNull { return .null }
        guard case .text(let pattern) = values[1], case .text(let escape) = values[2] else {
            throw JedError("42804", "SIMILAR substring operands must be text")
        }
        let state = function.regex.map(functionState)
        let program: Similar.SubstringProgram
        if let compiled = function.regex?.similar {
            program = compiled
            if state?.compileCharged == false { state?.compileCharged = true; try program.chargeCompile(meter) }
        } else {
            let constant = function.constantArguments.dropFirst().allSatisfy { $0 }
            program = try Similar.compileSubstring(Similar.translate(pattern, escape: escape, meter: constant ? nil : meter))
            try program.chargeCompile(meter)
        }
        if values[0].isNull { return .null }
        guard case .text(let source) = values[0] else { throw JedError("42804", "SIMILAR substring subject must be text") }
        return try program.match(source, meter: meter, scratch: state?.similarScratch).map(SQLValue.text) ?? .null
    }
}
