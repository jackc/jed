internal struct UnicodeSimpleCase: Sendable, Equatable {
    let codepoint: UInt32
    let upper: UInt32, lower: UInt32, title: UInt32
}
internal struct UnicodeSpecialCase: Sendable, Equatable {
    let codepoint: UInt32
    let upper: [UInt32], lower: [UInt32], title: [UInt32]
}

/// Only pinned simple and unconditional full casing is supported by JUCD v1.
/// Neither Swift's host Unicode tables nor contextual casing participate.
internal struct UnicodeProperties: Sendable {
    let unicodeVersion: SQLText
    let simple: [UnicodeSimpleCase]
    let special: [UnicodeSpecialCase]

    init(_ bytes: [UInt8], version: SQLText) throws {
        var reader = ByteReader(bytes)
        guard try reader.u8() == 1 else { throw JedError.corruption("unsupported Unicode property layout") }
        let count = Int(try reader.u32())
        guard count <= reader.remaining / 16 else { throw JedError.corruption("invalid simple casing count") }
        var simple: [UnicodeSimpleCase] = []
        for _ in 0..<count {
            let codepoint = try CollationTable.scalar(&reader)
            guard simple.last.map({ $0.codepoint < codepoint }) ?? true else { throw JedError.corruption("simple casing entries are not strictly ordered") }
            let upper = try CollationTable.scalar(&reader), lower = try CollationTable.scalar(&reader), title = try CollationTable.scalar(&reader)
            guard codepoint == 0 || upper != 0 && lower != 0 && title != 0 else { throw JedError.corruption("casing introduces a NUL character") }
            simple.append(UnicodeSimpleCase(codepoint: codepoint, upper: upper, lower: lower, title: title))
        }
        let specialCount = Int(try reader.u32())
        guard specialCount <= reader.remaining / 7 else { throw JedError.corruption("invalid special casing count") }
        var special: [UnicodeSpecialCase] = []
        func mapping(_ reader: inout ByteReader) throws -> [UInt32] {
            let count = Int(try reader.u8())
            guard count <= reader.remaining / 4 else { throw JedError.corruption("invalid special casing sequence length") }
            var result: [UInt32] = []
            for _ in 0..<count { result.append(try CollationTable.scalar(&reader)) }
            return result
        }
        for _ in 0..<specialCount {
            let codepoint = try CollationTable.scalar(&reader)
            guard special.last.map({ $0.codepoint < codepoint }) ?? true else { throw JedError.corruption("special casing entries are not strictly ordered") }
            let upper = try mapping(&reader), lower = try mapping(&reader), title = try mapping(&reader)
            guard codepoint == 0 || !upper.contains(0) && !lower.contains(0) && !title.contains(0) else { throw JedError.corruption("casing introduces a NUL character") }
            special.append(UnicodeSpecialCase(codepoint: codepoint, upper: upper, lower: lower, title: title))
        }
        try reader.requireEnd()
        self.unicodeVersion = version; self.simple = simple; self.special = special
    }
    private func simpleMapping(_ codepoint: UInt32) -> UnicodeSimpleCase? {
        var lower = 0, upper = simple.count
        while lower < upper {
            let middle = lower + (upper - lower) / 2
            if simple[middle].codepoint < codepoint { lower = middle + 1 } else { upper = middle }
        }
        return lower < simple.count && simple[lower].codepoint == codepoint ? simple[lower] : nil
    }
    private func specialMapping(_ codepoint: UInt32) -> UnicodeSpecialCase? {
        var lower = 0, upper = special.count
        while lower < upper {
            let middle = lower + (upper - lower) / 2
            if special[middle].codepoint < codepoint { lower = middle + 1 } else { upper = middle }
        }
        return lower < special.count && special[lower].codepoint == codepoint ? special[lower] : nil
    }
    func simpleLower(_ codepoint: UInt32) -> UInt32 { simpleMapping(codepoint)?.lower ?? codepoint }
    func fold(_ text: SQLText, upper: Bool) -> SQLText {
        var output: [UInt8] = []
        for scalar in text.description.unicodeScalars {
            if let special = specialMapping(scalar.value) {
                for mapped in upper ? special.upper : special.lower { output.append(contentsOf: String(Unicode.Scalar(mapped)!).utf8) }
            } else if let simple = simpleMapping(scalar.value) {
                output.append(contentsOf: String(Unicode.Scalar(upper ? simple.upper : simple.lower)!).utf8)
            } else { output.append(contentsOf: String(scalar).utf8) }
        }
        return SQLText(validatedUTF8: output)
    }
    func encoded() -> [UInt8] {
        var writer = ByteWriter(); writer.u8(1); writer.u32(UInt32(simple.count))
        for entry in simple { writer.u32(entry.codepoint); writer.u32(entry.upper); writer.u32(entry.lower); writer.u32(entry.title) }
        writer.u32(UInt32(special.count))
        for entry in special {
            writer.u32(entry.codepoint)
            for mapping in [entry.upper, entry.lower, entry.title] {
                writer.u8(UInt8(mapping.count)); for codepoint in mapping { writer.u32(codepoint) }
            }
        }
        return writer.bytes
    }
}

internal struct UnicodeBundle: Sendable {
    struct Section: Sendable {
        let kind: UInt8
        let name: SQLText
        let contentHash: UInt32
        let payload: [UInt8]
    }
    let unicodeVersion: SQLText
    let cldrVersion: SQLText
    let description: SQLText
    let sections: [Section]
    let collations: [SQLCollation]
    let properties: UnicodeProperties?
    private let original: [UInt8]

    init(_ bytes: [UInt8]) throws {
        guard bytes.count >= 16 else { throw JedError.corruption("truncated JUCD bundle") }
        var checksum = ByteReader(bytes.suffix(4))
        guard try Checksums.crc32(bytes.dropLast(4)) == checksum.u32() else { throw JedError.corruption("Unicode bundle checksum mismatch") }
        var reader = ByteReader(bytes.dropLast(4))
        guard Array(try reader.take(6)) == [74, 85, 67, 68, 0, 0], try reader.u16() == 1 else { throw JedError.corruption("invalid JUCD bundle header") }
        unicodeVersion = try SQLCollation.string(&reader); cldrVersion = try SQLCollation.string(&reader); description = try SQLCollation.string(&reader)
        let count = Int(try reader.u16())
        guard count <= reader.remaining / 19 else { throw JedError.corruption("invalid Unicode bundle section count") }
        struct Manifest {
            let kind: UInt8, name: SQLText, hash: UInt32
            let rawLength: Int, compressedLength: Int, offset: Int
        }
        var manifest: [Manifest] = [], names = Set<SQLText>(), hasRoot = false, hasProperties = false
        for _ in 0..<count {
            let kind = try reader.u8(), name = try SQLCollation.string(&reader)
            guard kind <= 2 else { throw JedError.corruption("unknown Unicode bundle section kind") }
            if kind == 0 {
                guard !hasProperties && name.utf8.isEmpty else { throw JedError.corruption("invalid duplicate or named property section") }
                hasProperties = true
            } else {
                guard !name.utf8.isEmpty && name.utf8 != [67] && names.insert(name).inserted else { throw JedError.corruption("invalid duplicate collation name") }
                if kind == 1 {
                    guard !hasRoot else { throw JedError.corruption("multiple Unicode root sections") }; hasRoot = true
                }
            }
            manifest.append(Manifest(kind: kind, name: name, hash: try reader.u32(), rawLength: Int(try reader.u32()), compressedLength: Int(try reader.u32()), offset: Int(try reader.u32())))
        }
        var sections: [Section] = []
        for item in manifest {
            guard item.offset == reader.offset && item.compressedLength <= reader.remaining else { throw JedError.corruption("invalid Unicode section body offset") }
            let payload = try LZ4.decompress(Array(reader.take(item.compressedLength)), rawLength: item.rawLength)
            guard Checksums.crc32(payload) == item.hash else { throw JedError.corruption("Unicode section checksum mismatch") }
            sections.append(Section(kind: item.kind, name: item.name, contentHash: item.hash, payload: payload))
        }
        try reader.requireEnd()
        var root: CollationTable?
        if let section = sections.first(where: { $0.kind == 1 }) { root = try CollationTable(section.payload) }
        var collations: [SQLCollation] = [], properties: UnicodeProperties?
        for section in sections {
            if section.kind == 0 { properties = try UnicodeProperties(section.payload, version: unicodeVersion); continue }
            guard let root else { throw JedError.corruption("Unicode tailoring has no root section") }
            let table = section.kind == 1 ? root : try root.merging(CollationTable(section.payload, delta: true))
            collations.append(SQLCollation(name: section.name, unicodeVersion: unicodeVersion, cldrVersion: cldrVersion, description: description, table: table))
        }
        self.sections = sections; self.collations = collations; self.properties = properties; original = bytes
    }
    func encoded() -> [UInt8] { original }
}

/// Value semantics make a built environment immutable; a derived builder shares
/// its arrays and tables until another explicit load. Earlier names/properties
/// win, and a malformed later bundle changes no state.
internal struct UnicodeDatabase: Sendable {
    private(set) var loadedCollations: [SQLCollation] = []
    private(set) var properties: UnicodeProperties?
    mutating func load(_ bytes: [UInt8]) throws {
        let bundle = try UnicodeBundle(bytes)
        for collation in bundle.collations where !loadedCollations.contains(where: { $0.name == collation.name }) { loadedCollations.append(collation) }
        if properties == nil { properties = bundle.properties }
    }
    /// C is always available and has no table or version dependency.
    func resolve(_ name: SQLText) throws -> SQLCollation? {
        if name.utf8 == [67] { return nil }
        guard let collation = loadedCollations.first(where: { $0.name == name }) else { throw JedError("42704", "collation \"\(name)\" does not exist") }
        return collation
    }
    func key(_ text: SQLText, collation name: SQLText) throws -> [UInt8] {
        if let collation = try resolve(name) { return try collation.sortKey(text) }
        return try KeyEncoding.encode(.text(text), type: TypeDescriptor(.scalar(.text)))
    }
    func simpleLower(_ scalar: UInt32) -> UInt32 {
        if let properties { return properties.simpleLower(scalar) }
        return (65...90).contains(scalar) ? scalar + 32 : scalar
    }
    func lower(_ text: SQLText) -> SQLText { properties?.fold(text, upper: false) ?? text.asciiLowercased }
    func upper(_ text: SQLText) -> SQLText {
        properties?.fold(text, upper: true) ?? SQLText(validatedUTF8: text.utf8.map { (97...122).contains($0) ? $0 - 32 : $0 })
    }
}
