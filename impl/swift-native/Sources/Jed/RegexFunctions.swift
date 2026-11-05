/// Regex scalar kernels and retained SRF producers. Compilation and search costs
/// belong to the call; the surrounding evaluator charges operator_eval.
internal enum RegexFunctions {
    struct Flags { let ignoreCase: Bool; let global: Bool }
    static func flags(_ text: SQLText, function: String) throws -> Flags {
        var insensitive = false, global = false
        for scalar in text.description.unicodeScalars {
            switch scalar.value {
            case 105: insensitive = true
            case 103: global = true
            default: throw JedError("2201B", "invalid regular expression option: \"\(scalar)\"")
            }
        }
        if global && function != "regexp_replace" && function != "regexp_matches" {
            throw JedError("22023", "\(function)() does not support the \"global\" option")
        }
        return Flags(ignoreCase: insensitive, global: global)
    }
    private static func text(_ value: SQLValue) throws -> SQLText {
        guard case .text(let text) = value else { throw JedError("42883", "function does not exist") }; return text
    }
    private static func integer(_ args: [SQLValue], _ index: Int, default fallback: Int64) throws -> Int64 {
        if index >= args.count { return fallback }
        guard case .integer(let n) = args[index] else { throw JedError("42883", "function does not exist") }; return n
    }
    private static func positive(_ value: Int64, _ name: String, zeroAllowed: Bool = false) throws {
        if value < (zeroAllowed ? 0 : 1) { throw JedError("22023", "invalid value for parameter \"\(name)\": \(value)") }
    }
    private struct Prepared {
        let subject: RegexSubject
        let program: RegexProgram
        let flags: Flags
        let occurrence: Int64, subexpression: Int64, endOption: Int64
        var search: Search
    }
    private struct Search {
        let program: RegexProgram
        let scalars: [UInt32]
        let scratch: RegexScratch
        let meter: CostMeter
        var cursor: Int
        mutating func next() throws -> RegexMatch? {
            guard cursor <= scalars.count,
                  let match = try program.search(scalars: scalars, start: cursor, meter: meter, scratch: scratch),
                  let span = match.span() else { cursor = scalars.count + 1; return nil }
            cursor = span.upperBound + (span.isEmpty ? 1 : 0)
            return match
        }
    }
    private static func prepare(name: String, args: [SQLValue], program supplied: RegexProgram?, chargeCompile: Bool,
                                scratch suppliedScratch: RegexScratch?, fold: @escaping @Sendable (UInt32) -> UInt32, meter: CostMeter) throws -> Prepared {
        guard args.count >= 2 else { throw JedError("42883", "function does not exist") }
        let source = try text(args[0]), pattern = try text(args[1])
        var start: Int64 = 1, occurrence: Int64 = 1, subexpression: Int64 = 0, endOption: Int64 = 0
        let flagIndex: Int?
        switch name {
        case "regexp_replace": guard args.count >= 3 else { throw JedError("42883", "function does not exist") }; flagIndex = 3
        case "regexp_count": start = try integer(args, 2, default: 1); flagIndex = 3
        case "regexp_substr":
            start = try integer(args, 2, default: 1); occurrence = try integer(args, 3, default: 1); subexpression = try integer(args, 5, default: 0); flagIndex = 4
        case "regexp_instr":
            start = try integer(args, 2, default: 1); occurrence = try integer(args, 3, default: 1)
            endOption = try integer(args, 4, default: 0); subexpression = try integer(args, 6, default: 0); flagIndex = 5
        case "substring": flagIndex = nil
        default: flagIndex = 2
        }
        try positive(start, "start"); try positive(occurrence, "n"); try positive(subexpression, "subexpr", zeroAllowed: true)
        if endOption != 0 && endOption != 1 { throw JedError("22023", "invalid value for parameter \"endoption\": \(endOption)") }
        let optionText = try flagIndex.flatMap { $0 < args.count ? try text(args[$0]) : nil } ?? SQLText(validatedUTF8: [])
        let flags = try flags(optionText, function: name)
        let program = try supplied ?? RegexProgram.compile(pattern: pattern, options: RegexOptions(ignoreCase: flags.ignoreCase), fold: fold)
        if chargeCompile { try program.chargeCompile(meter) }
        let subject = RegexSubject(source), scalars = program.options.ignoreCase ? subject.scalars.map(program.fold) : subject.scalars
        let cursor = start > Int64(scalars.count) + 1 ? scalars.count + 1 : Int(start - 1)
        return Prepared(subject: subject, program: program, flags: flags, occurrence: occurrence, subexpression: subexpression, endOption: endOption,
                        search: Search(program: program, scalars: scalars, scratch: suppliedScratch ?? RegexScratch(), meter: meter, cursor: cursor))
    }
    static func call(name: String, args: [SQLValue], program supplied: RegexProgram? = nil, chargeCompile: Bool = true, scratch suppliedScratch: RegexScratch? = nil,
                     fold: @escaping @Sendable (UInt32) -> UInt32 = RegexProgram.asciiFold,
                     meter: CostMeter) throws -> SQLValue {
        guard name != "regexp_matches" && name != "regexp_split_to_table" else { throw JedError("42883", "set-returning regex function requires a row producer") }
        if args.contains(where: \.isNull) { return .null }
        var prepared = try prepare(name: name, args: args, program: supplied, chargeCompile: chargeCompile, scratch: suppliedScratch, fold: fold, meter: meter)
        switch name {
        case "regexp_like": return .boolean(try prepared.search.next() != nil)
        case "regexp_count":
            var count: Int64 = 0
            while try prepared.search.next() != nil { count += 1 }
            return .integer(count)
        case "regexp_instr", "regexp_substr", "substring":
            var selected: RegexMatch?, count: Int64 = 0
            while let match = try prepared.search.next() {
                count += 1
                if count == prepared.occurrence { selected = match; break }
            }
            var group = prepared.subexpression
            if name == "substring" { group = prepared.program.groupCount > 0 ? 1 : 0 }
            if prepared.program.groupCount == 0 && group == 1 { group = 0 }
            let span = group <= Int64(prepared.program.groupCount) ? selected?.span(Int(group)) : nil
            if name == "regexp_instr" { return .integer(span.map { Int64((prepared.endOption == 0 ? $0.lowerBound : $0.upperBound) + 1) } ?? 0) }
            return span.map { .text(prepared.subject.slice($0)) } ?? .null
        case "regexp_match":
            guard let match = try prepared.search.next() else { return .null }
            let groups = reportedGroups(prepared.program)
            try admit([match], subject: prepared.subject, groups: groups, meter: meter)
            return try captureArray(match, subject: prepared.subject, groups: groups)
        case "regexp_split_to_array":
            let spans = try splitSpans(&prepared.search)
            return .array(try SQLArray(elements: spans.map { .text(prepared.subject.slice($0)) }))
        case "regexp_replace":
            return .text(try replace(&prepared.search, subject: prepared.subject, replacement: text(args[2]), global: prepared.flags.global, meter: meter))
        default: throw JedError("42883", "function does not exist")
        }
    }

    /// The spec requires the complete search and capture-array admission before
    /// any regexp_matches row is produced. This factory retains only capture
    /// state or split spans; next() creates one SQL value, never a nested result
    /// array. VM/offset/capture scratch has no separate Q1–Q4 logical charge.
    /// Rows.next() owns generated_row; its relation adapter must not charge that
    /// unit again. A materializing relation or host collector admits rows it keeps.
    static func rows(name: String, args: [SQLValue], program supplied: RegexProgram? = nil, chargeCompile: Bool = true, scratch suppliedScratch: RegexScratch? = nil,
                     fold: @escaping @Sendable (UInt32) -> UInt32 = RegexProgram.asciiFold,
                     meter: CostMeter) throws -> Rows {
        guard name == "regexp_matches" || name == "regexp_split_to_table" else { throw JedError("42883", "function is not a regex row producer") }
        if args.contains(where: \.isNull) { return Rows(meter: meter) }
        var prepared = try prepare(name: name, args: args, program: supplied, chargeCompile: chargeCompile, scratch: suppliedScratch, fold: fold, meter: meter)
        if name == "regexp_split_to_table" {
            return Rows(subject: prepared.subject, splits: try splitSpans(&prepared.search), meter: meter)
        }
        var matches: [RegexMatch?] = []
        while let match = try prepared.search.next() {
            matches.append(match)
            if !prepared.flags.global { break }
        }
        let groups = reportedGroups(prepared.program)
        try admit(matches, subject: prepared.subject, groups: groups, meter: meter)
        return Rows(subject: prepared.subject, matches: matches, groups: groups, meter: meter)
    }

    final class Rows {
        private var subject: RegexSubject?
        private var matches: [RegexMatch?]?
        private var splits: [Range<Int>]?
        private var position = 0
        private let groups: ClosedRange<Int>
        private let meter: CostMeter
        fileprivate init(meter: CostMeter) { self.meter = meter; groups = 0...0 }
        fileprivate init(subject: RegexSubject, matches: [RegexMatch?], groups: ClosedRange<Int>, meter: CostMeter) {
            self.subject = subject; self.matches = matches; self.groups = groups; self.meter = meter
        }
        fileprivate init(subject: RegexSubject, splits: [Range<Int>], meter: CostMeter) {
            self.subject = subject; self.splits = splits; groups = 0...0; self.meter = meter
        }
        func next() throws -> SQLValue? {
            guard let subject, position < (matches?.count ?? splits?.count ?? 0) else { close(); return nil }
            do {
                meter.charge(1); try meter.checkpoint() // generated_row
                let value: SQLValue
                if let match = matches?[position] {
                    value = try RegexFunctions.captureArray(match, subject: subject, groups: groups)
                    matches?[position] = nil
                } else if let split = splits?[position] { value = .text(subject.slice(split)) }
                else { throw JedError("XX000", "missing regex producer state") }
                position += 1
                if position == (matches?.count ?? splits?.count ?? 0) { close() }
                return value
            } catch { close(); throw error }
        }
        func close() { subject = nil; matches = nil; splits = nil; position = 0 }
    }

    private static func reportedGroups(_ program: RegexProgram) -> ClosedRange<Int> { program.groupCount == 0 ? 0...0 : 1...program.groupCount }
    private static func admit(_ matches: [RegexMatch?], subject: RegexSubject, groups: ClosedRange<Int>, meter: CostMeter) throws {
        var size: Int64 = 0
        for optional in matches {
            guard let match = optional else { continue }
            var rowSize = GeneratedSpec.memoryArrayDimension
            for group in groups {
                rowSize = saturatingAdd(rowSize, GeneratedSpec.memoryValue)
                if let span = match.span(group) { rowSize = saturatingAdd(rowSize, Int64(subject.byteCount(span))) }
            }
            meter.charge(rowSize); try meter.guardCost(); size = saturatingAdd(size, rowSize)
        }
        try meter.reserveScalar(size)
    }
    private static func captureArray(_ match: RegexMatch, subject: RegexSubject, groups: ClosedRange<Int>) throws -> SQLValue {
        .array(try SQLArray(elements: groups.map { group in match.span(group).map { .text(subject.slice($0)) } ?? .null }))
    }
    private static func splitSpans(_ search: inout Search) throws -> [Range<Int>] {
        var pieces: [Range<Int>] = [], previousEnd = 0, copiedThrough = 0
        while let match = try search.next() {
            let span = match.span()!
            if span.lowerBound != search.scalars.count && span.upperBound > previousEnd {
                pieces.append(copiedThrough..<span.lowerBound); copiedThrough = span.upperBound
            }
            previousEnd = span.upperBound
        }
        pieces.append(copiedThrough..<search.scalars.count)
        return pieces
    }

    private enum Piece { case literal(Range<Int>), capture(Int), slash }
    private struct Replacement { let pieces: [Piece]; let groups: [Int] }
    private static func template(_ bytes: [UInt8]) -> Replacement {
        var pieces: [Piece] = [], groups = [0], position = 0, literalStart = 0
        while position + 1 < bytes.count {
            if bytes[position] != 92 { position += 1; continue }
            let next = bytes[position + 1]
            let piece: Piece?
            if (49...57).contains(next) || next == 38 {
                let group = next == 38 ? 0 : Int(next - 48)
                if !groups.contains(group) { groups.append(group) }
                piece = .capture(groups.firstIndex(of: group)!)
            } else if next == 92 { piece = .slash }
            else { piece = nil }
            guard let piece else { position += 1; continue }
            if literalStart < position { pieces.append(.literal(literalStart..<position)) }
            pieces.append(piece); position += 2; literalStart = position
        }
        if literalStart < bytes.count { pieces.append(.literal(literalStart..<bytes.count)) }
        return Replacement(pieces: pieces, groups: groups)
    }
    private static func replace(_ search: inout Search, subject: RegexSubject, replacement: SQLText, global: Bool, meter: CostMeter) throws -> SQLText {
        let recipe = template(replacement.utf8)
        // A replacement can reference only groups 0...9. Drop every other
        // capture tree immediately after each search, retaining compact spans.
        // Full search precedes sizing as required by cost.md §8.1; no rescan or
        // extra metering/admission point is introduced by this representation.
        var matches: [[Range<Int>?]] = []
        while let match = try search.next() {
            matches.append(recipe.groups.map { match.span($0) })
            if !global { break }
        }
        var size: Int64 = 0, copiedThrough = 0
        for captures in matches {
            meter.charge(Int64(replacement.utf8.count)); try meter.guardCost()
            let span = captures[0]!
            size = saturatingAdd(size, Int64(subject.byteCount(copiedThrough..<span.lowerBound)))
            copiedThrough = span.upperBound
            for part in recipe.pieces {
                switch part {
                case .literal(let range): size = saturatingAdd(size, Int64(range.count))
                case .slash: size = saturatingAdd(size, 1)
                case .capture(let group): if let span = captures[group] { size = saturatingAdd(size, Int64(subject.byteCount(span))) }
                }
            }
        }
        size = saturatingAdd(size, Int64(subject.byteCount(copiedThrough..<subject.scalars.count)))
        guard size <= 1_073_741_823 else { throw JedError("54000", "requested length too large") }
        meter.charge(size); try meter.guardCost(); try meter.reserveScalar(size)
        var output: [UInt8] = []; output.reserveCapacity(Int(size)); copiedThrough = 0
        for captures in matches {
            let span = captures[0]!
            output.append(contentsOf: subject.text.utf8[subject.offsets[copiedThrough]..<subject.offsets[span.lowerBound]])
            for part in recipe.pieces {
                switch part {
                case .literal(let range): output.append(contentsOf: replacement.utf8[range])
                case .slash: output.append(92)
                case .capture(let group): if let span = captures[group] { output.append(contentsOf: subject.text.utf8[subject.offsets[span.lowerBound]..<subject.offsets[span.upperBound]]) }
                }
            }
            copiedThrough = span.upperBound
        }
        output.append(contentsOf: subject.text.utf8[subject.offsets[copiedThrough]...])
        return SQLText(validatedUTF8: output)
    }
}
