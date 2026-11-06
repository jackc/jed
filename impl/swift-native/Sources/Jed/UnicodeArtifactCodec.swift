/// Package-only bridge for the separate Unicode build tool. No definition
/// parser or host Unicode access belongs to the production Jed target.
package enum UnicodeArtifactCodec {
    package static func artifact(name: String, unicodeVersion: String, cldrVersion: String = "",
                                 description: String = "", table: [UInt8]) throws -> [UInt8] {
        _ = try CollationTable(table)
        guard !name.isEmpty && name != "C" else { throw JedError("22023", "invalid collation name") }
        let compressed = LZ4.compress(table)
        var writer = ByteWriter(); writer.append([74, 67, 79, 76, 76, 0]); writer.u16(1)
        for value in [name, unicodeVersion, cldrVersion, description] { try string(value, into: &writer) }
        writer.u32(Checksums.crc32(table)); try length(table.count, into: &writer); try length(compressed.count, into: &writer)
        writer.append(compressed)
        return writer.bytes
    }

    package static func validateCasing(_ payload: [UInt8], version: String) throws {
        _ = try UnicodeProperties(payload, version: SQLText(version))
    }

    package static func bundle(rootArtifact: [UInt8]?, tailoringArtifacts: [[UInt8]],
                               casing: [UInt8]?, casingVersion: String?, description: String = "") throws -> [UInt8] {
        let root = try rootArtifact.map { try SQLCollation(artifact: $0) }
        let tailorings = try tailoringArtifacts.map { try SQLCollation(artifact: $0) }
        guard root != nil || tailorings.isEmpty else { throw JedError("22023", "a tailoring requires a root collation") }
        let version = root?.unicodeVersion.description ?? casingVersion ?? ""
        if casing != nil, let casingVersion, casingVersion != version { throw JedError("22023", "Unicode source versions do not match") }
        struct Section {
            let kind: UInt8, name: String, payload: [UInt8], compressed: [UInt8]
            init(_ kind: UInt8, _ name: String, _ payload: [UInt8]) {
                self.kind = kind; self.name = name; self.payload = payload; compressed = LZ4.compress(payload)
            }
        }
        var sections: [Section] = [], names = Set<SQLText>()
        if let casing { try validateCasing(casing, version: version); sections.append(Section(0, "", casing)) }
        if let root {
            names.insert(root.name); sections.append(Section(1, root.name.description, root.table.encoded()))
            let singleMap = Dictionary(uniqueKeysWithValues: root.table.singles.map { ($0.codepoint, $0.elements) })
            let contractionMap = Dictionary(uniqueKeysWithValues: root.table.contractions.map { ($0.codepoints, $0.elements) })
            for tailoring in tailorings {
                guard names.insert(tailoring.name).inserted else { throw JedError("22023", "duplicate collation name") }
                guard tailoring.unicodeVersion == root.unicodeVersion && tailoring.cldrVersion == root.cldrVersion else {
                    throw JedError("22023", "collation versions do not match")
                }
                let delta = CollationTable(singles: tailoring.table.singles.filter { singleMap[$0.codepoint] != $0.elements },
                                           contractions: tailoring.table.contractions.filter { contractionMap[$0.codepoints] != $0.elements })
                guard root.table.merging(delta) == tailoring.table else { throw JedError("0A000", "collation delta cannot remove mappings") }
                sections.append(Section(2, tailoring.name.description, delta.encoded(delta: true)))
            }
        }
        guard sections.count <= Int(UInt16.max) else { throw JedError("54000", "too many Unicode bundle sections") }
        var writer = ByteWriter(); writer.append([74, 85, 67, 68, 0, 0]); writer.u16(1)
        for value in [version, root?.cldrVersion.description ?? "", description] { try string(value, into: &writer) }
        writer.u16(UInt16(sections.count))
        var offset = writer.bytes.count
        for section in sections { offset += 19 + section.name.utf8.count }
        for section in sections {
            writer.u8(section.kind); try string(section.name, into: &writer)
            writer.u32(Checksums.crc32(section.payload)); try length(section.payload.count, into: &writer)
            try length(section.compressed.count, into: &writer); try length(offset, into: &writer)
            guard offset <= Int(UInt32.max) - section.compressed.count else { throw JedError("54000", "Unicode bundle is too large") }
            offset += section.compressed.count
        }
        for section in sections { writer.append(section.compressed) }
        writer.u32(Checksums.crc32(writer.bytes))
        // Reuse runtime validation, including exact manifest/body boundaries.
        _ = try UnicodeBundle(writer.bytes)
        return writer.bytes
    }
    private static func length(_ count: Int, into writer: inout ByteWriter) throws {
        guard count >= 0 && count <= Int(UInt32.max) else { throw JedError("54000", "Unicode artifact is too large") }
        writer.u32(UInt32(count))
    }
    private static func string(_ value: String, into writer: inout ByteWriter) throws {
        let text = try SQLText(value)
        guard text.utf8.count <= Int(UInt16.max) else { throw JedError("54000", "Unicode artifact metadata is too long") }
        writer.u16(UInt16(text.utf8.count)); writer.append(text.utf8)
    }
}
