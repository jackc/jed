/// Session-owned ON COMMIT metadata travels with the same private frame as the
/// temporary roots. The caller records only successful statements and invokes
/// apply only for a successful read-write commit, before storage preflight.
internal enum TemporaryCommitActions {
    static func record(_ statement: SQLStatement, before: CatalogImage, after: CatalogImage,
                       actions: inout [SQLText: String]) {
        let previous = Set(before.tables.map(\.name)), surviving = Set(after.tables.map(\.name))
        if case .alterTable(let syntax) = statement, previous.contains(syntax.name.name) {
            var renamed = syntax.name.name
            for action in syntax.actions { if case .renameTable(let name) = action { renamed = name } }
            if renamed != syntax.name.name, surviving.contains(renamed), let action = actions.removeValue(forKey: syntax.name.name) {
                actions[renamed] = action
            }
        }
        actions = actions.filter { surviving.contains($0.key) }
        if case .createTable(let syntax) = statement,
           !previous.contains(syntax.name.name), surviving.contains(syntax.name.name), let action = syntax.onCommit {
            actions[syntax.name.name] = action
        }
    }

    @discardableResult static func apply(snapshot: inout DomainSnapshot, actions: inout [SQLText: String],
                                        settings: SessionSettings) throws -> Bool {
        let tables = snapshot.catalog.tables.sorted { $0.name < $1.name }
        let truncates = Set(tables.filter { actions[$0.name] == "delete rows" }.map(\.name))
        let drops = Set(tables.filter { actions[$0.name] == "drop" }.map(\.name))
        // Every inbound pair is checked before truncation or drop, even for an
        // empty child or a child that this same commit would subsequently drop.
        for parent in truncates.sorted() {
            if let child = tables.first(where: { !truncates.contains($0.name) && $0.foreignKeys.contains { $0.referencedTable == parent } }) {
                var error = JedError("0A000", "unsupported ON COMMIT and foreign key combination")
                error.detail = "Table \"\(child.name)\" references \"\(parent)\", but they do not have the same ON COMMIT setting."
                throw error
            }
        }
        var work = snapshot, image = snapshot.catalog, changed = false
        let zero = PageSealer.nullReference(snapshot.pool.codec)
        for name in truncates.sorted() {
            guard let ordinal = image.tables.firstIndex(where: { $0.name == name }), let store = work.tables[name] else {
                throw JedError.corruption("temporary action references a missing table store")
            }
            let table = image.tables[ordinal]
            let hasRows = store.tree.root != nil || table.indexes.contains { work.indexes[$0.name]?.tree.root != nil }
            guard hasRows else { continue }
            work.setTableStore(name, TableStore(tree: BPlusTree(codec: work.pool.codec, types: store.tree.types, pool: work.pool, layout: store.tree.layout),
                                               nextRowID: store.nextRowID))
            image.tables[ordinal].root = zero; image.tables[ordinal].rowCount = 0
            for index in table.indexes.indices {
                let definition = table.indexes[index]
                guard let old = work.indexes[definition.name] else { throw JedError.corruption("temporary index store is missing") }
                work.setIndexStore(definition.name, TableStore(tree: BPlusTree(codec: work.pool.codec, types: [], pool: work.pool), nextRowID: old.nextRowID))
                image.tables[ordinal].indexes[index].root = zero
                if definition.kind == 2 {
                    guard let old = work.gistTrees[definition.name] else { throw JedError.corruption("temporary GiST tree is missing") }
                    work.setGistTree(definition.name, try GistTree.build(opclasses: old.opclasses, leafKeys: []))
                }
            }
            for index in image.statistics.indices where image.statistics[index].table == name { image.statistics[index].stale = true }
            changed = true
        }
        if !drops.isEmpty {
            let sequences = Set(image.sequences.filter { $0.owner.map { drops.contains($0.table) } ?? false }.map(\.name))
            var views: Set<SQLText> = [], grew = true
            while grew {
                grew = false
                for view in image.views where !views.contains(view.name) {
                    if view.dependencies.contains(where: {
                        $0.kind == 0 && drops.contains($0.name) || $0.kind == 2 && sequences.contains($0.name)
                            || $0.kind == 1 && views.contains($0.name)
                    }) { views.insert(view.name); grew = true }
                }
            }
            for table in image.tables where drops.contains(table.name) {
                work.setTableStore(table.name, nil)
                for index in table.indexes { work.setIndexStore(index.name, nil); work.setGistTree(index.name, nil) }
            }
            image.tables.removeAll { drops.contains($0.name) }
            for index in image.tables.indices { image.tables[index].foreignKeys.removeAll { drops.contains($0.referencedTable) } }
            image.sequences.removeAll { sequences.contains($0.name) }
            image.views.removeAll { views.contains($0.name) }
            image.statistics.removeAll { drops.contains($0.table) }
            changed = true
        }
        if changed { try work.updateCatalog(image, schemaChanged: !drops.isEmpty); snapshot = work }
        actions = actions.filter { name, _ in image.tables.contains { $0.name == name } }
        return changed
    }
}
