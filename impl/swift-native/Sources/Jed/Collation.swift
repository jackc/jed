/// Immutable UCA data and executor. Tables are supplied through a JUCD bundle or
/// JCOLL artifact; there is no host locale or implicit Unicode-data fallback.
internal struct CollationElement: Sendable, Equatable {
    let flags: UInt8
    let primary: UInt16, secondary: UInt16, tertiary: UInt16
}
internal struct CollationSingle: Sendable, Equatable {
    let codepoint: UInt32
    let elements: [CollationElement]
}
internal struct CollationContraction: Sendable, Equatable {
    let codepoints: [UInt32]
    let elements: [CollationElement]
}
internal struct CollationTable: Sendable, Equatable {
    let singles: [CollationSingle]
    let contractions: [CollationContraction]

    init(singles: [CollationSingle], contractions: [CollationContraction]) {
        self.singles = singles; self.contractions = contractions
    }
    init(_ bytes: [UInt8], delta: Bool = false) throws {
        var reader = ByteReader(bytes)
        guard try reader.u8() == 1 else { throw JedError.corruption("unsupported collation table layout") }
        let singleCount = Int(try reader.u32())
        // Frozen JUCD deltas use the same counts-first header as full tables.
        let contractionCount = Int(try reader.u32())
        guard singleCount <= reader.remaining / 12 else { throw JedError.corruption("invalid collation single count") }
        var singles: [CollationSingle] = []
        for _ in 0..<singleCount {
            let codepoint = try Self.scalar(&reader)
            guard singles.last.map({ $0.codepoint < codepoint }) ?? true else { throw JedError.corruption("collation singles are not strictly ordered") }
            singles.append(CollationSingle(codepoint: codepoint, elements: try Self.elements(&reader)))
        }
        let count = contractionCount
        guard count <= reader.remaining / 17 else { throw JedError.corruption("invalid collation contraction count") }
        var contractions: [CollationContraction] = []
        for _ in 0..<count {
            let length = Int(try reader.u8())
            guard length >= 2 && length <= reader.remaining / 4 else { throw JedError.corruption("invalid collation contraction length") }
            var sequence: [UInt32] = []
            for _ in 0..<length { sequence.append(try Self.scalar(&reader)) }
            guard contractions.last.map({ $0.codepoints.lexicographicallyPrecedes(sequence) }) ?? true else { throw JedError.corruption("collation contractions are not strictly ordered") }
            contractions.append(CollationContraction(codepoints: sequence, elements: try Self.elements(&reader)))
        }
        try reader.requireEnd()
        self.singles = singles; self.contractions = contractions
    }
    static func scalar(_ reader: inout ByteReader) throws -> UInt32 {
        let value = try reader.u32()
        guard value <= 0x10ffff && !(0xd800...0xdfff).contains(value) else { throw JedError.corruption("invalid Unicode scalar in collation data") }
        return value
    }
    private static func elements(_ reader: inout ByteReader) throws -> [CollationElement] {
        let count = Int(try reader.u8())
        guard count > 0 && count <= reader.remaining / 7 else { throw JedError.corruption("invalid collation element count") }
        var result: [CollationElement] = []
        for _ in 0..<count {
            let flags = try reader.u8()
            guard flags & 0xfe == 0 else { throw JedError.corruption("reserved collation element flags") }
            result.append(CollationElement(flags: flags, primary: try reader.u16(), secondary: try reader.u16(), tertiary: try reader.u16()))
        }
        return result
    }
    func encoded(delta: Bool = false) -> [UInt8] {
        var writer = ByteWriter(); writer.u8(1); writer.u32(UInt32(singles.count))
        writer.u32(UInt32(contractions.count))
        func writeElements(_ elements: [CollationElement], to writer: inout ByteWriter) {
            writer.u8(UInt8(elements.count))
            for element in elements { writer.u8(element.flags); writer.u16(element.primary); writer.u16(element.secondary); writer.u16(element.tertiary) }
        }
        for single in singles { writer.u32(single.codepoint); writeElements(single.elements, to: &writer) }
        for contraction in contractions {
            writer.u8(UInt8(contraction.codepoints.count))
            for scalar in contraction.codepoints { writer.u32(scalar) }
            writeElements(contraction.elements, to: &writer)
        }
        return writer.bytes
    }
    func merging(_ delta: CollationTable) -> CollationTable {
        // Merge sorted arrays directly: a sparse delta adds or replaces one key.
        var singles: [CollationSingle] = [], contractions: [CollationContraction] = []
        var old = 0, new = 0
        while old < self.singles.count || new < delta.singles.count {
            if new == delta.singles.count || old < self.singles.count && self.singles[old].codepoint < delta.singles[new].codepoint {
                singles.append(self.singles[old]); old += 1
            } else {
                if old < self.singles.count && self.singles[old].codepoint == delta.singles[new].codepoint { old += 1 }
                singles.append(delta.singles[new]); new += 1
            }
        }
        old = 0; new = 0
        while old < self.contractions.count || new < delta.contractions.count {
            if new == delta.contractions.count || old < self.contractions.count && self.contractions[old].codepoints.lexicographicallyPrecedes(delta.contractions[new].codepoints) {
                contractions.append(self.contractions[old]); old += 1
            } else {
                if old < self.contractions.count && self.contractions[old].codepoints == delta.contractions[new].codepoints { old += 1 }
                contractions.append(delta.contractions[new]); new += 1
            }
        }
        return CollationTable(singles: singles, contractions: contractions)
    }
    private func single(_ codepoint: UInt32) -> [CollationElement]? {
        var lower = 0, upper = singles.count
        while lower < upper {
            let middle = lower + (upper - lower) / 2
            if singles[middle].codepoint < codepoint { lower = middle + 1 } else { upper = middle }
        }
        return lower < singles.count && singles[lower].codepoint == codepoint ? singles[lower].elements : nil
    }
    func sortKey(_ text: SQLText) throws -> [UInt8] {
        let scalars = text.description.unicodeScalars.map(\.value)
        var primary = ByteWriter(), secondary = ByteWriter(), tertiary = ByteWriter(), position = 0
        while position < scalars.count {
            var lower = 0, upper = contractions.count
            while lower < upper {
                let middle = lower + (upper - lower) / 2
                if contractions[middle].codepoints[0] < scalars[position] { lower = middle + 1 } else { upper = middle }
            }
            var length = 1, elements: [CollationElement]?
            while lower < contractions.count && contractions[lower].codepoints[0] == scalars[position] {
                let candidate = contractions[lower]
                if candidate.codepoints.count > length && candidate.codepoints.count <= scalars.count - position &&
                    candidate.codepoints.elementsEqual(scalars[position..<(position + candidate.codepoints.count)]) {
                    length = candidate.codepoints.count; elements = candidate.elements
                }
                lower += 1
            }
            if elements == nil { elements = single(scalars[position]) }
            guard let elements else { throw JedError("0A000", "collation has no mapping for code point U+\(String(scalars[position], radix: 16).uppercased())") }
            for element in elements {
                if element.primary != 0 { primary.u16(element.primary) }
                if element.secondary != 0 { secondary.u16(element.secondary) }
                if element.tertiary != 0 { tertiary.u16(element.tertiary) }
            }
            position += length
        }
        primary.u16(0); secondary.u16(0); tertiary.u16(0)
        primary.append(secondary.bytes); primary.append(tertiary.bytes)
        primary.append(try KeyEncoding.encode(.text(text), type: TypeDescriptor(.scalar(.text))))
        return primary.bytes
    }
}

internal struct SQLCollation: Sendable {
    let name: SQLText
    let unicodeVersion: SQLText
    let cldrVersion: SQLText
    let description: SQLText
    let contentHash: UInt32
    let table: CollationTable
    private let artifact: [UInt8]?

    init(name: SQLText, unicodeVersion: SQLText, cldrVersion: SQLText, description: SQLText, table: CollationTable) {
        self.name = name; self.unicodeVersion = unicodeVersion; self.cldrVersion = cldrVersion; self.description = description
        self.table = table; contentHash = Checksums.crc32(table.encoded()); artifact = nil
    }
    init(artifact bytes: [UInt8]) throws {
        var reader = ByteReader(bytes)
        guard Array(try reader.take(6)) == [74, 67, 79, 76, 76, 0], try reader.u16() == 1 else { throw JedError.corruption("invalid JCOLL artifact header") }
        name = try Self.string(&reader); unicodeVersion = try Self.string(&reader)
        cldrVersion = try Self.string(&reader); description = try Self.string(&reader)
        guard !name.utf8.isEmpty && name.utf8 != [67] else { throw JedError.corruption("invalid bundled collation name") }
        contentHash = try reader.u32()
        let uncompressed = Int(try reader.u32()), compressed = Int(try reader.u32())
        let raw = try LZ4.decompress(Array(reader.take(compressed)), rawLength: uncompressed)
        try reader.requireEnd()
        guard Checksums.crc32(raw) == contentHash else { throw JedError.corruption("collation table checksum mismatch") }
        table = try CollationTable(raw); artifact = bytes
    }
    static func string(_ reader: inout ByteReader) throws -> SQLText {
        let count = Int(try reader.u16()); return try reader.text(count)
    }
    func sortKey(_ text: SQLText) throws -> [UInt8] { try table.sortKey(text) }
    /// The original artifact is retained for exact round-trip inspection. A
    /// merged runtime table has no fabricated artifact or implicit compiler.
    var originalArtifact: [UInt8]? { artifact }
}
