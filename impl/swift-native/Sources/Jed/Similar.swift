/// SQL SIMILAR translation and its gated four-search substring decomposition.
/// Each search retains the ordinary VM instruction and cost contracts.
internal enum Similar {
    struct Translation: Sendable { let text: SQLText; let parts: [SQLText] }
    static func translate(_ pattern: SQLText, escape: SQLText? = nil, meter: CostMeter? = nil) throws -> Translation {
        let escapeScalars = (escape?.description ?? "\\").unicodeScalars.map(\.value)
        guard escapeScalars.count <= 1 else { throw JedError("22025", "invalid escape string") }
        let marker = escapeScalars.first
        var escaped = false, inClass = false, classPosition = 0, parts = [String()], text = "^(?:"
        func string(_ c: UInt32) -> String { String(UnicodeScalar(c)!) }
        for c in pattern.description.unicodeScalars.map(\.value) {
            var emitted = ""
            if escaped {
                escaped = false
                if c == 34 && !inClass {
                    guard parts.count < 3 else { throw JedError("2200C", "SQL regular expression may not contain more than two escape-double-quote separators") }
                    text += parts.count == 1 ? "){1,1}?(" : "){1,1}(?:"
                    parts.append(""); continue
                }
                emitted = "\\" + string(c)
                if inClass { classPosition = 3 }
            } else if c == marker { escaped = true; continue }
            else if inClass {
                emitted = c == 92 ? "\\\\" : string(c)
                if c == 93 && classPosition > 2 { inClass = false }
                else if c == 94 { classPosition += 1 }
                else { classPosition = 3 }
            } else {
                switch c {
                case 91: emitted = "["; inClass = true; classPosition = 1
                case 37: emitted = ".*"
                case 95: emitted = "."
                case 40: emitted = "(?:"
                case 92, 46, 94, 36: emitted = "\\" + string(c)
                default: emitted = string(c)
                }
            }
            parts[parts.count - 1] += emitted; text += emitted
        }
        text += ")$"
        meter?.charge(Int64(text.utf8.count)); try meter?.guardCost()
        return Translation(text: SQLText(validatedUTF8: Array(text.utf8)), parts: parts.map { SQLText(validatedUTF8: Array($0.utf8)) })
    }
    struct SubstringProgram: Sendable {
        let programs: [RegexProgram]
        var instructionCount: Int { programs.reduce(0) { $0 + $1.instructionCount } }
        func chargeCompile(_ meter: CostMeter) throws { meter.charge(Int64(instructionCount)); try meter.guardCost() }
        func match(_ subject: SQLText, meter: CostMeter) throws -> SQLText? {
            let scalars = subject.description.unicodeScalars.map(\.value)
            if programs.count == 1 { return try programs[0].search(scalars: scalars, meter: meter) != nil ? subject : nil }
            let firstEnds = try programs[0].ends(scalars: scalars, gates: [0], meter: meter)
            guard let first = try programs[1].search(scalars: scalars, gates: firstEnds, meter: meter)?.span()?.lowerBound else { return nil }
            if programs.count == 2 { return RegexSubject(subject).slice(first..<scalars.count) }
            let middleEnds = try programs[2].ends(scalars: scalars, gates: [first], meter: meter)
            guard let last = try programs[3].search(scalars: scalars, gates: middleEnds, meter: meter)?.span()?.lowerBound else { return nil }
            return RegexSubject(subject).slice(first..<last)
        }
    }
    static func compileSubstring(_ translation: Translation) throws -> SubstringProgram {
        let options = RegexOptions(dotAll: true)
        if translation.parts.count == 1 { return SubstringProgram(programs: [try RegexProgram.compile(pattern: translation.text, options: options)]) }
        func wrapped(_ index: Int) -> String { "(?:" + translation.parts[index].description + ")" }
        var programs = [try RegexProgram.compile(pattern: SQLText(wrapped(0)), options: options, wrapper: .gatedLeftmost),
                        try RegexProgram.compile(pattern: SQLText(wrapped(1) + (translation.parts.count == 3 ? wrapped(2) : "") + "$"), options: options, wrapper: .gatedLeftmost)]
        if translation.parts.count == 3 {
            programs.append(try RegexProgram.compile(pattern: SQLText(wrapped(1)), options: options, wrapper: .gatedLeftmost))
            programs.append(try RegexProgram.compile(pattern: SQLText(wrapped(2) + "$"), options: options, wrapper: .gatedRightmost))
        }
        return SubstringProgram(programs: programs)
    }
}

/// One UTF-8 offset index per call lets capture sizing/slicing read O(1) spans
/// without rescanning the entire subject for each group or each global match.
internal struct RegexSubject {
    let text: SQLText
    let scalars: [UInt32]
    let offsets: [Int]
    init(_ text: SQLText) {
        self.text = text
        scalars = text.description.unicodeScalars.map(\.value)
        var offsets = [0], total = 0
        for scalar in scalars {
            total += scalar < 128 ? 1 : scalar < 2048 ? 2 : scalar < 65536 ? 3 : 4
            offsets.append(total)
        }
        self.offsets = offsets
    }
    func byteCount(_ span: Range<Int>) -> Int { offsets[span.upperBound] - offsets[span.lowerBound] }
    func slice(_ span: Range<Int>) -> SQLText { SQLText(validatedUTF8: Array(text.utf8[offsets[span.lowerBound]..<offsets[span.upperBound]])) }
}
