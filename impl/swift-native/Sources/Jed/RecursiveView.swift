/// CREATE RECURSIVE VIEW is stored as the ordinary WITH RECURSIVE query
/// specified by views.md §17. Its synthesized identifiers are always quoted;
/// body token diagnostics continue to point into the original SQL statement.
internal enum RecursiveView {
    static func body(_ syntax: SQLCreateView, source: SQLText) throws -> (query: SQLQuery, source: SQLText) {
        guard syntax.recursive else { return (syntax.query, source) }
        guard !syntax.columns.isEmpty else { throw JedError("42601", "CREATE RECURSIVE VIEW requires a column list") }
        guard syntax.checkOption == nil else { throw JedError("0A000", "WITH CHECK OPTION not supported on recursive views") }
        var columns: [UInt8] = []
        for column in syntax.columns {
            if !columns.isEmpty { columns += [32, 44, 32] }
            columns += ViewDefinition.quoted(column)
        }
        let name = ViewDefinition.quoted(syntax.name.name)
        let prefix = Array("with recursive ".utf8) + name + Array(" ( ".utf8) + columns + Array(" ) as ( ".utf8)
        let suffix = Array(" ) select ".utf8) + columns + Array(" from ".utf8) + name
        let span = syntax.query.span
        guard span.byteStart >= 0, span.byteEnd >= span.byteStart, span.byteEnd <= source.utf8.count else {
            throw JedError("XX000", "recursive view body has an invalid source span")
        }
        let body = Array(source.utf8[span.byteStart..<span.byteEnd])
        let expanded = prefix + body + suffix
        let originals = try SQLLexer(utf8: source.utf8).tokenize()
        let positions = Dictionary(uniqueKeysWithValues: originals.map { ($0.byteStart, $0.position) })
        let after = originals.first(where: { $0.byteStart >= span.byteEnd })?.position ?? span.position
        let remapped = try SQLLexer(utf8: expanded).tokenize().map { token -> Int in
            if token.byteStart < prefix.count { return span.position }
            if token.byteStart >= prefix.count + body.count { return after }
            return positions[span.byteStart + token.byteStart - prefix.count] ?? span.position
        }
        let parser = try SQLParser(utf8: expanded, tokenPositions: remapped)
        let query = try parser.nestedQuery()
        guard parser.peek().kind == .end else { throw JedError("XX000", "recursive view wrapper is not a query") }
        return (query, SQLText(validatedUTF8: expanded))
    }
}
