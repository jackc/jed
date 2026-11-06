/// Edits only FROM bindings and their positional aliases. Expressions keep the
/// names they originally resolved, so a rename cannot capture another column.
internal enum ViewRewrite {
    static func references(in image: inout CatalogImage, name: SQLText, kind: UInt8,
                           renamed: SQLText? = nil, column: (ordinal: Int, name: SQLText)? = nil,
                           columns: [SQLText], domain: SQLText = DomainNames.main) throws {
        for index in image.views.indices where image.views[index].dependencies.contains(where: { $0.kind == kind && $0.name == name }) {
            var view = image.views[index]
            let query = try ViewDefinition.query(view)
            let tokens = try SQLLexer(utf8: view.definition.utf8).tokenize()
            var edits: [(start: Int, end: Int, bytes: [UInt8])] = []
            try ViewSyntax.walk(query, from: { from, ctes in
                guard case .table(let reference) = from.source, reference.name == name,
                      reference.database == nil || reference.database == domain,
                      reference.database != nil || !ctes.contains(name) else { return }
                if let renamed {
                    guard let token = tokens.last(where: { $0.byteStart >= reference.span.byteStart && $0.byteEnd <= reference.span.byteEnd && $0.kind != .end }) else {
                        throw JedError("XX003", "stored view relation has no name token")
                    }
                    var replacement = ViewDefinition.quoted(renamed)
                    if from.alias == nil { replacement += Array(" as ".utf8) + ViewDefinition.quoted(name) }
                    edits.append((token.byteStart, token.byteEnd, replacement))
                } else if let column, from.columnAliases.count <= column.ordinal {
                    guard columns.indices.contains(column.ordinal) else { throw JedError("XX000", "view rewrite column is outside its relation") }
                    let added = columns[from.columnAliases.count...column.ordinal]
                    var names: [UInt8] = []
                    for name in added {
                        if !names.isEmpty { names += [32, 44, 32] }
                        names += ViewDefinition.quoted(name)
                    }
                    if from.columnAliases.isEmpty {
                        var suffix: [UInt8] = []
                        if from.alias == nil { suffix += Array(" as ".utf8) + ViewDefinition.quoted(name) }
                        suffix += [32, 40, 32] + names + [32, 41]
                        edits.append((from.span.byteEnd, from.span.byteEnd, suffix))
                    } else {
                        guard let close = tokens.last(where: { $0.byteStart >= reference.span.byteEnd && $0.byteEnd <= from.span.byteEnd && $0.kind == .symbol(")") }) else {
                            throw JedError("XX003", "stored view column alias list is not closed")
                        }
                        edits.append((close.byteStart, close.byteStart, [32, 44, 32] + names + [32]))
                    }
                }
            })
            var bytes: [UInt8] = [], cursor = 0
            for edit in edits.sorted(by: { $0.start < $1.start }) {
                guard edit.start >= cursor else { throw JedError("XX000", "overlapping view rename substitutions") }
                bytes += view.definition.utf8[cursor..<edit.start]; bytes += edit.bytes; cursor = edit.end
            }
            bytes += view.definition.utf8[cursor...]
            view.definition = try CatalogSQL.render(bytes)
            for dependency in view.dependencies.indices where view.dependencies[dependency].kind == kind && view.dependencies[dependency].name == name {
                if let renamed { view.dependencies[dependency].name = renamed }
                if kind == 0, let column {
                    let old = columns[column.ordinal]
                    view.dependencies[dependency].columns = view.dependencies[dependency].columns.map { $0 == old ? column.name : $0 }.sorted()
                }
            }
            view.dependencies.sort { $0.name == $1.name ? $0.kind < $1.kind : $0.name < $1.name }
            image.views[index] = view
        }
    }
}
