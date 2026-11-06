import Jed

/// Build-time Unicode definitions. This target is separate from the production
/// engine; all input is explicit text, with no host locale or Unicode lookup.
public enum UnicodeCompiler {
    public struct Casing: Sendable {
        public let version: String
        public let payload: [UInt8]
        public init(version: String, payload: [UInt8]) throws {
            try UnicodeArtifactCodec.validateCasing(payload, version: version)
            self.version = version; self.payload = payload
        }
    }
    public static func compile(name: String, definitions: [String], description: String = "") throws -> [UInt8] {
        var definition = UnicodeDefinition()
        for text in definitions { try definition.append(text) }
        return try UnicodeArtifactCodec.artifact(name: name, unicodeVersion: definition.version,
                                                 description: description, table: definition.encoded())
    }
    public static func compileCasing(_ source: String) throws -> Casing {
        var version = "", special = false
        var simple: [UInt32: [UInt32]] = [:], full: [UInt32: [[UInt32]]] = [:]
        for line in DefinitionSyntax.lines(source) {
            if line.first == "@" {
                let fields = DefinitionSyntax.words(line)
                if fields.first == "@version" { version = fields.count > 1 ? fields[1] : "" }
                if fields.first == "@special" { special = true }
                continue
            }
            let fields = line.split(separator: ";", omittingEmptySubsequences: false).map { DefinitionSyntax.trim(String($0)) }
            guard fields.count <= 4 else { throw DefinitionSyntax.error("too many casing fields") }
            let codepoint = try DefinitionSyntax.codepoint(fields[0])
            var mappings: [[UInt32]] = []
            for index in 1...3 {
                if index >= fields.count || fields[index].isEmpty || fields[index] == "-" { mappings.append([codepoint]) }
                else { mappings.append(try DefinitionSyntax.words(fields[index]).map(DefinitionSyntax.codepoint)) }
            }
            if special {
                guard full[codepoint] == nil else { throw DefinitionSyntax.error("duplicate special casing mapping") }
                guard mappings.allSatisfy({ $0.count <= 255 }) else { throw JedError("54000", "casing expansion is too large") }
                full[codepoint] = mappings
            } else {
                guard simple[codepoint] == nil && mappings.allSatisfy({ $0.count == 1 }) else {
                    throw DefinitionSyntax.error("invalid or duplicate simple casing mapping")
                }
                simple[codepoint] = mappings.map { $0[0] }
            }
        }
        var bytes = DefinitionBytes(); bytes.byte(1); try bytes.count(simple.count)
        for codepoint in simple.keys.sorted() {
            bytes.word(codepoint); for target in simple[codepoint]! { bytes.word(target) }
        }
        try bytes.count(full.count)
        for codepoint in full.keys.sorted() {
            bytes.word(codepoint)
            for targets in full[codepoint]! { bytes.byte(UInt8(targets.count)); for target in targets { bytes.word(target) } }
        }
        return try Casing(version: version, payload: bytes.bytes)
    }
    public static func bundle(root: [UInt8]?, tailorings: [[UInt8]] = [], casing: Casing? = nil,
                              description: String = "") throws -> [UInt8] {
        try UnicodeArtifactCodec.bundle(rootArtifact: root, tailoringArtifacts: tailorings,
                                        casing: casing?.payload, casingVersion: casing?.version, description: description)
    }
}

internal struct DefinitionWeight: Equatable {
    var flags: UInt8 = 0
    var levels: [UInt16]
}
internal struct UnicodeDefinition {
    var version = ""
    var mappings: [[UInt32]: [DefinitionWeight]] = [:]

    mutating func append(_ source: String) throws {
        for line in DefinitionSyntax.lines(source) {
            if line.first == "@" {
                let fields = DefinitionSyntax.words(line)
                if fields.first == "@version" { version = fields.count > 1 ? fields[1] : "" }
                continue
            }
            if line.first == "&" { try applyRule(line); continue }
            guard let separator = line.firstIndex(of: ";") else { throw DefinitionSyntax.error("mapping has no semicolon") }
            let sequence = try DefinitionSyntax.words(String(line[..<separator])).map(DefinitionSyntax.codepoint)
            guard !sequence.isEmpty else { throw DefinitionSyntax.error("mapping has no code points") }
            guard sequence.count <= 255 else { throw JedError("54000", "collation contraction is too long") }
            guard mappings[sequence] == nil else { throw DefinitionSyntax.error("duplicate collation mapping") }
            let source = Array(line[line.index(after: separator)...].unicodeScalars.map(\.value))
            var cursor = 0, weights: [DefinitionWeight] = []
            func skipSpace() { while cursor < source.count && DefinitionSyntax.space(source[cursor]) { cursor += 1 } }
            skipSpace()
            while cursor < source.count {
                guard source[cursor] == 91 else { throw DefinitionSyntax.error("collation element must begin with '['") }; cursor += 1
                guard cursor < source.count && (source[cursor] == 46 || source[cursor] == 42) else { throw DefinitionSyntax.error("invalid collation element marker") }
                let flags: UInt8 = source[cursor] == 42 ? 1 : 0; cursor += 1
                var components: [UInt16] = []
                while true {
                    let start = cursor
                    while cursor < source.count && source[cursor] != 46 && source[cursor] != 93 { cursor += 1 }
                    guard cursor < source.count else { throw DefinitionSyntax.error("unterminated collation element") }
                    if components.count < 3 {
                        let token = String(String.UnicodeScalarView(source[start..<cursor].compactMap(Unicode.Scalar.init)))
                        guard let weight = UInt16(DefinitionSyntax.trim(token), radix: 16) else { throw DefinitionSyntax.error("invalid collation weight") }
                        components.append(weight)
                    } else {
                        // Quaternary information is ignored by the frozen
                        // three-level contract, including values above u16.
                        components.append(0)
                    }
                    let end = source[cursor] == 93; cursor += 1
                    if end { break }
                }
                guard components.count >= 3 else { throw DefinitionSyntax.error("collation element requires three weights") }
                weights.append(DefinitionWeight(flags: flags, levels: Array(components.prefix(3))))
                guard weights.count <= 255 else { throw JedError("54000", "collation expansion is too long") }
                skipSpace()
            }
            guard !weights.isEmpty else { throw DefinitionSyntax.error("mapping has no collation elements") }
            mappings[sequence] = weights
        }
    }
    func encoded() throws -> [UInt8] {
        let singles = mappings.keys.filter { $0.count == 1 }.sorted { $0[0] < $1[0] }
        let contractions = mappings.keys.filter { $0.count > 1 }.sorted { $0.lexicographicallyPrecedes($1) }
        var output = DefinitionBytes(); output.byte(1); try output.count(singles.count); try output.count(contractions.count)
        for sequence in singles + contractions {
            if sequence.count > 1 { output.byte(UInt8(sequence.count)) }
            for codepoint in sequence { output.word(codepoint) }
            let weights = mappings[sequence]!
            output.byte(UInt8(weights.count))
            for weight in weights { output.byte(weight.flags); for level in weight.levels { output.half(level) } }
        }
        return output.bytes
    }
}

internal enum DefinitionSyntax {
    static func error(_ message: String) -> JedError { JedError("42601", "Unicode definition: " + message) }
    // Fixed Unicode White_Space set; no host Unicode property version enters a
    // compiler result. Canonical DUCET/LDML/casing sources use ASCII whitespace.
    static func space(_ scalar: UInt32) -> Bool {
        switch scalar {
        case 9...13, 32, 0x85, 0xa0, 0x1680, 0x2000...0x200a, 0x2028, 0x2029, 0x202f, 0x205f, 0x3000: true
        default: false
        }
    }
    static func trim(_ source: String) -> String {
        let scalars = source.unicodeScalars
        var start = scalars.startIndex, end = scalars.endIndex
        while start < end && space(scalars[start].value) { start = scalars.index(after: start) }
        while start < end && space(scalars[scalars.index(before: end)].value) { end = scalars.index(before: end) }
        return String(scalars[start..<end])
    }
    static func words(_ source: String) -> [String] {
        source.unicodeScalars.split { space($0.value) }.map { String(String.UnicodeScalarView($0)) }
    }
    static func lines(_ source: String) -> [String] {
        source.unicodeScalars.split(omittingEmptySubsequences: false, whereSeparator: { $0.value == 10 }).compactMap { raw in
            let end = raw.firstIndex(where: { $0.value == 35 }) ?? raw.endIndex, line = trim(String(raw[..<end]))
            return line.isEmpty ? nil : line
        }
    }
    static func codepoint(_ token: String) throws -> UInt32 {
        guard let value = UInt32(token, radix: 16), Unicode.Scalar(value) != nil else { throw error("invalid code point " + token) }
        return value
    }
}
internal struct DefinitionBytes {
    var bytes: [UInt8] = []
    mutating func byte(_ value: UInt8) { bytes.append(value) }
    mutating func half(_ value: UInt16) { bytes.append(UInt8(value >> 8)); bytes.append(UInt8(truncatingIfNeeded: value)) }
    mutating func word(_ value: UInt32) {
        for shift in [24, 16, 8, 0] { bytes.append(UInt8(truncatingIfNeeded: value >> shift)) }
    }
    mutating func count(_ count: Int) throws {
        guard count <= Int(UInt32.max) else { throw JedError("54000", "Unicode definition has too many entries") }
        word(UInt32(count))
    }
}
