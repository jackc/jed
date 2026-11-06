extension ViewRewrite {
    /// An unreferenced dropped column may still occupy an alias-list position.
    /// Remove that alias so subsequent aliases continue to name the same slots.
    static func droppingColumn(in image: inout CatalogImage, table: SQLText, ordinal: Int, domain: SQLText = DomainNames.main) throws {
        for index in image.views.indices where image.views[index].dependencies.contains(where: { $0.kind == 0 && $0.name == table }) {
            let view = image.views[index], query = try ViewDefinition.query(view)
            let tokens = try SQLLexer(utf8: view.definition.utf8).tokenize()
            var edits: [(Int, Int, [UInt8])] = []
            try ViewSyntax.walk(query, from: { from, ctes in
                guard case .table(let reference) = from.source, reference.name == table,
                      reference.database == nil || reference.database == domain,
                      reference.database != nil || !ctes.contains(table), ordinal < from.columnAliases.count else { return }
                let relevant = tokens.filter { $0.byteStart >= reference.span.byteEnd && $0.byteEnd <= from.span.byteEnd && $0.kind != .end }
                guard let open = relevant.first(where: { $0.kind == .symbol("(") }),
                      let close = relevant.last(where: { $0.kind == .symbol(")") }) else { throw JedError("XX003", "stored view column alias list is not closed") }
                var aliases = from.columnAliases; aliases.remove(at: ordinal)
                var bytes: [UInt8] = []
                if !aliases.isEmpty {
                    bytes = [40, 32]
                    for (position, name) in aliases.enumerated() {
                        if position > 0 { bytes += [32, 44, 32] }; bytes += ViewDefinition.quoted(name)
                    }
                    bytes += [32, 41]
                }
                edits.append((open.byteStart, close.byteEnd, bytes))
            })
            if edits.isEmpty { continue }
            var bytes: [UInt8] = [], position = 0
            for edit in edits.sorted(by: { $0.0 < $1.0 }) {
                guard edit.0 >= position else { throw JedError("XX000", "overlapping view alias rewrites") }
                bytes += view.definition.utf8[position..<edit.0]; bytes += edit.2; position = edit.1
            }
            bytes += view.definition.utf8[position...]
            image.views[index].definition = try CatalogSQL.render(bytes)
        }
    }
}
