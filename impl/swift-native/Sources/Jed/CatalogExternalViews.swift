/// Private views can depend on main objects, while main catalogs never import
/// private entries. Name maintenance edits the private catalog independently.
internal enum CatalogExternalViews {
    static func privateViews(_ settings: SessionSettings) -> [CatalogView] {
        guard settings.targetDomain == DomainNames.main else { return [] }
        return settings.domains?.snapshots[DomainNames.temporary]?.catalog.views ?? []
    }
    // ALTER's action-order guards use a private metadata projection. The actual
    // stored text is rewritten once the owning catalog has validated its frame.
    static func rename(_ views: inout [CatalogView], name: SQLText, renamed: SQLText? = nil,
                       column: (SQLText, SQLText)? = nil) {
        for index in views.indices {
            for dependency in views[index].dependencies.indices
                where views[index].dependencies[dependency].kind == 0 && views[index].dependencies[dependency].name == name {
                if let renamed { views[index].dependencies[dependency].name = renamed }
                if let column {
                    views[index].dependencies[dependency].columns = views[index].dependencies[dependency].columns.map {
                        $0 == column.0 ? column.1 : $0
                    }.sorted()
                }
            }
        }
    }
    static func drop(_ views: inout [CatalogView], table: SQLText, column: SQLText, sequences: Set<SQLText>) {
        let direct = Set(views.filter { view in
            view.dependencies.contains { $0.kind == 0 && $0.name == table && $0.columns.contains(column)
                || $0.kind == 2 && sequences.contains($0.name) }
        }.map(\.name))
        remove(&views, names: direct)
    }
    private static func remove(_ views: inout [CatalogView], names: Set<SQLText>) {
        var removed = names, changing = true
        while changing {
            changing = false
            for view in views where !removed.contains(view.name) {
                if view.dependencies.contains(where: { $0.kind == 1 && removed.contains($0.name) }) {
                    removed.insert(view.name); changing = true
                }
            }
        }
        views.removeAll { removed.contains($0.name) }
    }
    static func apply(_ statement: SQLStatement, before: QueryDomains, after: inout QueryDomains,
                      settings: SessionSettings, context: ExpressionContext) throws {
        guard settings.targetDomain == DomainNames.main,
              let old = before.snapshots[DomainNames.main], let main = after.snapshots[DomainNames.main],
              let prior = before.snapshots[DomainNames.temporary],
              var temporary = after.snapshots[DomainNames.temporary] else { return }
        var image = temporary.catalog, changedName: SQLText?, replacement = false
        func viewEdits(_ name: SQLText, _ actions: [SQLAlterViewAction]) throws {
            guard let view = old.catalog.views.first(where: { $0.name == name }) else { return }
            var name = name, columns = view.columns.map(\.name)
            changedName = name
            for action in actions {
                switch action {
                case .rename(let new):
                    try ViewRewrite.references(in: &image, name: name, kind: 1, renamed: new, columns: columns)
                    name = new
                case .renameColumn(let from, let to):
                    guard let ordinal = columns.firstIndex(of: from) else { continue }
                    try ViewRewrite.references(in: &image, name: name, kind: 1, column: (ordinal, to), columns: columns)
                    columns[ordinal] = to
                default: break
                }
            }
        }
        switch statement {
        case .alterTable(let value):
            if let table = old.catalog.tables.first(where: { $0.name == value.name.name }) {
                changedName = table.name
                var name = table.name, columns = table.columns.map(\.name)
                var sequences: [SQLText: Set<SQLText>] = [:]
                for sequence in old.catalog.sequences {
                    if let owner = sequence.owner, owner.table == table.name, columns.indices.contains(owner.column) {
                        sequences[columns[owner.column], default: []].insert(sequence.name)
                    }
                }
                for action in value.actions {
                    switch action {
                    case .renameTable(let new):
                        try ViewRewrite.references(in: &image, name: name, kind: 0, renamed: new, columns: columns)
                        name = new
                    case .renameColumn(let from, let to):
                        guard let ordinal = columns.firstIndex(of: from) else { continue }
                        try ViewRewrite.references(in: &image, name: name, kind: 0, column: (ordinal, to), columns: columns)
                        columns[ordinal] = to; sequences[to] = sequences.removeValue(forKey: from)
                    case .addColumn(let column, _): if !columns.contains(column.name) { columns.append(column.name) }
                    case .dropColumn(let column, _, let cascade):
                        guard let ordinal = columns.firstIndex(of: column) else { continue }
                        let owned = sequences[column] ?? []
                        let dependent = Set(image.views.filter { view in
                            view.dependencies.contains { $0.kind == 0 && $0.name == name && $0.columns.contains(column)
                                || $0.kind == 2 && owned.contains($0.name) }
                        }.map(\.name))
                        guard cascade || dependent.isEmpty else { throw JedError("2BP01", "cannot drop column because a private view depends on it").hinted(DependencyDiagnostics.cascade) }
                        remove(&image.views, names: dependent)
                        try ViewRewrite.droppingColumn(in: &image, table: name, ordinal: ordinal)
                        columns.remove(at: ordinal); sequences[column] = nil
                    default: break
                    }
                }
            } else {
                let actions: [SQLAlterViewAction] = value.actions.compactMap {
                    switch $0 {
                    case .renameTable(let name): return .rename(name)
                    case .renameColumn(let old, let new): return .renameColumn(old, new)
                    default: return nil
                    }
                }
                try viewEdits(value.name.name, actions)
            }
        case .alterView(let value): try viewEdits(value.name.name, value.actions)
        case .createView(let value) where value.replace:
            changedName = value.name.name; replacement = true
        case .alterSequence(let value):
            if let renamed = value.rename, old.catalog.sequences.contains(where: { $0.name == value.name.name }) {
                for table in image.tables.indices {
                    for column in image.tables[table].columns.indices {
                        guard case .expression(let expression) = image.tables[table].columns[column].defaultValue else { continue }
                        if let text = try SequenceReferences.replacing(in: expression.text, from: value.name.name, to: renamed) {
                            image.tables[table].columns[column].defaultValue = .expression(CatalogExpression(text))
                        }
                    }
                }
                for index in image.views.indices {
                    if let text = try SequenceReferences.replacing(in: image.views[index].definition, from: value.name.name, to: renamed) {
                        image.views[index].definition = text
                    }
                    for column in image.views[index].columns.indices {
                        if let old = image.views[index].columns[column].defaultText,
                           let text = try SequenceReferences.replacing(in: old, from: value.name.name, to: renamed) {
                            image.views[index].columns[column].defaultText = text
                        }
                    }
                    for dependency in image.views[index].dependencies.indices where image.views[index].dependencies[dependency].kind == 2
                        && image.views[index].dependencies[dependency].name == value.name.name {
                        image.views[index].dependencies[dependency].name = renamed
                    }
                    image.views[index].dependencies.sort { $0.name == $1.name ? $0.kind < $1.kind : $0.name < $1.name }
                }
            }
        default: return
        }
        let changed = zip(image.views, temporary.catalog.views).contains { left, right in
            left.name != right.name || left.definition != right.definition || left.dependencies.count != right.dependencies.count
                || left.columns.count != right.columns.count
                || !zip(left.columns, right.columns).allSatisfy { $0.name == $1.name && $0.defaultText == $1.defaultText }
                || !zip(left.dependencies, right.dependencies).allSatisfy { $0.kind == $1.kind && $0.name == $1.name && $0.columns == $1.columns }
        } || image.views.count != temporary.catalog.views.count || zip(image.tables, temporary.catalog.tables).contains { left, right in
            zip(left.columns, right.columns).contains { a, b in
                switch (a.defaultValue, b.defaultValue) {
                case (.expression(let a), .expression(let b)): return a.text != b.text
                default: return false
                }
            }
        }
        try temporary.borrow(main)
        if changed { try temporary.updateCatalog(image, schemaChanged: true) }
        after.snapshots[DomainNames.temporary] = temporary
        if let changedName {
            var scoped = settings; scoped.targetDomain = DomainNames.temporary; scoped.domains = after
            try CatalogDDL.revalidateViews(dependingOn: changedName, old: prior, new: temporary,
                settings: scoped, context: context, replacement: replacement, previousDomains: before)
        }
    }
}
