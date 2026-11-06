extension CatalogDDL {
    static func createSequence(_ definition: SQLCreateSequence, snapshot: inout DomainSnapshot) throws {
        let name = try nameInMain(definition.name)
        try CatalogSQL.reserved(name, kind: "sequence")
        if CatalogSQL.relationNames(snapshot.catalog).contains(name) {
            if definition.ifNotExists { return }
            throw JedError("42P07", "relation already exists: \(name)")
        }
        let sequence = try SequenceDefinition.create(name: name, options: definition.options, composites: snapshot.types)
        var image = snapshot.catalog; image.sequences.append(sequence)
        try snapshot.updateCatalog(image, schemaChanged: true)
    }
    static func alterSequence(_ definition: SQLAlterSequence, snapshot: inout DomainSnapshot) throws {
        let name = try nameInMain(definition.name)
        guard let position = snapshot.catalog.sequences.firstIndex(where: { $0.name == name }) else {
            if CatalogSQL.relationNames(snapshot.catalog).contains(name) { throw JedError("42809", "\(name) is not a sequence") }
            if definition.ifExists { return }
            throw JedError("42P01", "sequence does not exist: \(name)")
        }
        var image = snapshot.catalog
        if let renamed = definition.rename {
            try CatalogSQL.reserved(renamed, kind: "sequence")
            guard !CatalogSQL.relationNames(image).contains(renamed) else { throw JedError("42P07", "relation already exists: \(renamed)") }
            image.sequences[position].name = renamed
            for table in image.tables.indices {
                for column in image.tables[table].columns.indices {
                    guard case .expression(let expression) = image.tables[table].columns[column].defaultValue else { continue }
                    if let text = try SequenceReferences.replacing(in: expression.text, from: name, to: renamed) {
                        image.tables[table].columns[column].defaultValue = .expression(CatalogExpression(text))
                    }
                }
            }
            // View definitions and defaults retain the same regclass-like literal
            // dependency; computed text arguments deliberately remain unchanged.
            for view in image.views.indices {
                if let text = try SequenceReferences.replacing(in: image.views[view].definition, from: name, to: renamed) {
                    image.views[view].definition = text
                }
                for column in image.views[view].columns.indices {
                    if let old = image.views[view].columns[column].defaultText,
                       let text = try SequenceReferences.replacing(in: old, from: name, to: renamed) {
                        image.views[view].columns[column].defaultText = text
                    }
                }
                for dependency in image.views[view].dependencies.indices where image.views[view].dependencies[dependency].kind == 2
                    && image.views[view].dependencies[dependency].name == name {
                    image.views[view].dependencies[dependency].name = renamed
                }
                image.views[view].dependencies.sort { $0.kind == $1.kind ? $0.name < $1.name : $0.kind < $1.kind }
            }
        } else {
            image.sequences[position] = try SequenceDefinition.alter(image.sequences[position], options: definition.options)
        }
        try snapshot.updateCatalog(image, schemaChanged: true)
    }
    static func dropSequences(_ definition: SQLDrop, snapshot: inout DomainSnapshot) throws {
        var targets: Set<SQLText> = []
        let catalog = snapshot.catalog, relations = CatalogSQL.relationNames(snapshot.catalog)
        for input in definition.names {
            let name = try nameInMain(input)
            if catalog.sequences.contains(where: { $0.name == name }) { targets.insert(name) }
            else if relations.contains(name) { throw JedError("42809", "\(name) is not a sequence") }
            else if !definition.ifExists { throw JedError("42P01", "sequence does not exist: \(name)") }
        }
        if targets.isEmpty { return }
        for sequence in catalog.sequences.sorted(by: { $0.name < $1.name }) where targets.contains(sequence.name) {
            guard sequence.owner == nil else {
                throw JedError(definition.cascade ? "0A000" : "2BP01", "cannot drop owned sequence \(sequence.name)")
            }
            for table in catalog.tables.sorted(by: { $0.name < $1.name }) {
                for column in table.columns {
                    if case .expression(let expression) = column.defaultValue,
                       SequenceReferences.contains(sequence.name, in: expression.text) {
                        throw JedError(definition.cascade ? "0A000" : "2BP01", "default value for column \(column.name) of table \(table.name) depends on sequence \(sequence.name)")
                    }
                }
            }
            for view in catalog.views.sorted(by: { $0.name < $1.name }) {
                for column in view.columns {
                    if let text = column.defaultText, SequenceReferences.contains(sequence.name, in: text) {
                        throw JedError(definition.cascade ? "0A000" : "2BP01", "default value for column \(column.name) of view \(view.name) depends on sequence \(sequence.name)")
                    }
                }
            }
        }
        var views: Set<SQLText> = [], changed = true
        while changed {
            changed = false
            for view in catalog.views where !views.contains(view.name) {
                if view.dependencies.contains(where: { $0.kind == 2 && targets.contains($0.name) || $0.kind == 1 && views.contains($0.name) }) {
                    guard definition.cascade else { throw JedError("2BP01", "view \(view.name) depends on sequence") }
                    views.insert(view.name); changed = true
                }
            }
        }
        var image = catalog
        image.sequences.removeAll { targets.contains($0.name) }
        image.views.removeAll { views.contains($0.name) }
        try snapshot.updateCatalog(image, schemaChanged: true)
    }
}

/// Persisted definitions may no longer parse in a future binary. Sequence
/// reference maintenance intentionally needs only their stable token spelling.
internal enum SequenceReferences {
    private static func literals(_ text: SQLText, name: SQLText) -> [SQLToken] {
        guard let tokens = try? SQLLexer(utf8: text.utf8).tokenize(), tokens.count >= 4 else { return [] }
        var found: [SQLToken] = []
        for index in 0..<(tokens.count - 3) {
            guard case .word(let function, _) = tokens[index].kind,
                  ["nextval", "currval", "setval"].contains(function.description),
                  tokens[index + 1].kind == .symbol("("),
                  case .string(let reference) = tokens[index + 2].kind,
                  [.symbol(")"), .symbol(",")].contains(tokens[index + 3].kind),
                  (try? SequenceName.parse(reference)) == name else { continue }
            found.append(tokens[index + 2])
        }
        return found
    }
    static func contains(_ name: SQLText, in text: SQLText) -> Bool { !literals(text, name: name).isEmpty }
    static func replacing(in text: SQLText, from old: SQLText, to new: SQLText) throws -> SQLText? {
        let matches = literals(text, name: old)
        guard !matches.isEmpty else { return nil }
        let literal: [UInt8] = [39] + SequenceName.quotedReference(new).utf8.flatMap { byte -> [UInt8] in byte == 39 ? [39, 39] : [byte] } + [39]
        var output: [UInt8] = [], cursor = 0
        for token in matches {
            output += text.utf8[cursor..<token.byteStart]; output += literal; cursor = token.byteEnd
        }
        output += text.utf8[cursor...]
        return try CatalogSQL.render(output)
    }
}
