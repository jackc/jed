extension CatalogDDL {
    /// Apply the catalog frame left-to-right, then validate the final nullability
    /// in one scan. The caller publishes this working snapshot only on success.
    static func alterTable(_ definition: SQLAlterTable, source: SQLText, snapshot: inout DomainSnapshot,
                           settings: SessionSettings, context: ExpressionContext) throws {
        try SystemRelationPlan.rejectTarget(definition.name.name)
        let name = try CatalogDomain.name(definition.name, settings: settings)
        if snapshot.catalog.views.contains(where: { $0.name == name }) {
            let actions = try definition.actions.map { action -> SQLAlterViewAction in
                switch action {
                case .renameTable(let name): return .rename(name)
                case .renameColumn(let old, let new): return .renameColumn(old, new)
                case .setDefault(let name, let value): return .setDefault(name, value)
                case .dropDefault(let name): return .dropDefault(name)
                default: throw JedError("42809", "\(name) is not a table")
                }
            }
            return try alterView(SQLAlterView(name: definition.name, ifExists: definition.ifExists, actions: actions),
                                 source: source, snapshot: &snapshot, settings: settings, context: context)
        }
        guard let position = snapshot.catalog.tables.firstIndex(where: { $0.name == name }),
              let originalStore = snapshot.tables[name] else {
            if CatalogSQL.relationNames(snapshot.catalog).contains(name) { throw JedError("42809", "\(name) is not a table") }
            if definition.ifExists { return }
            throw JedError("42P01", "table does not exist: \(name)")
        }
        let original = snapshot.catalog.tables[position]
        if DomainNames.isAttachment(settings.targetDomain), snapshot.catalog.views.contains(where: {
            $0.dependencies.contains { $0.kind == 0 && $0.name == name }
        }) { throw JedError("0A000", "ALTER of an attached table referenced by a view is not supported") }
        let before = snapshot
        try context.sequences?.apply(to: &snapshot)
        context.sequences?.synchronizeDefinitions(from: snapshot.catalog, domain: settings.targetDomain)
        var image = snapshot.catalog, table = original
        var removedIndexes: Set<SQLText> = [], renamedIndexes: [(SQLText, SQLText)] = []
        var addedConstraints: Set<SQLText> = []
        var sources = original.columns.indices.map(AlterRowSource.original)
        var steps: [AlterRowStep] = []
        var changedTypes: Set<Int> = [], changedViewColumns: Set<SQLText> = []
        var clearStatistics = false
        var preparationCost: Int64 = 0
        func column(_ name: SQLText) throws -> Int {
            guard let found = table.columns.firstIndex(where: { $0.name == name }) else { throw JedError("42703", "column does not exist: \(name)") }
            return found
        }
        func ownedSequence(_ ordinal: Int) throws -> Int {
            guard case .expression(let expression) = table.columns[ordinal].defaultValue,
                  let found = image.sequences.firstIndex(where: {
                      $0.owner?.table == table.name && $0.owner?.column == ordinal
                          && SequenceReferences.contains($0.name, in: expression.text)
                  }) else { throw JedError.corruption("identity column has no owned sequence") }
            return found
        }
        for action in definition.actions {
            switch action {
            case .addColumn(let syntax, let ifNotExists):
                if table.columns.contains(where: { $0.name == syntax.name }) {
                    if ifNotExists { continue }
                    throw JedError("42701", "column already exists: \(syntax.name)")
                }
                let captured = try AlterColumnDDL.add(syntax, table: &table, image: &image, source: source,
                    snapshot: snapshot, settings: settings, context: context, added: &addedConstraints)
                sources.append(.added(captured))
                steps.append(try AlterRowRewrite.append(captured, snapshot: snapshot, settings: settings, context: context))
            case .renameTable(let new):
                try CatalogSQL.reserved(new, kind: "table")
                guard !CatalogDomain.relations(image, settings: settings).contains(new) else { throw JedError("42P07", "relation already exists: \(new)") }
                let oldName = table.name
                try ViewRewrite.references(in: &image, name: oldName, kind: 0, renamed: new, columns: table.columns.map(\.name))
                try CatalogRewrite.tableExpressions(&table, renamedTable: new)
                table.name = new
                for child in image.tables.indices {
                    for foreign in image.tables[child].foreignKeys.indices where image.tables[child].foreignKeys[foreign].referencedTable == oldName {
                        image.tables[child].foreignKeys[foreign].referencedTable = new
                    }
                }
                for foreign in table.foreignKeys.indices where table.foreignKeys[foreign].referencedTable == oldName { table.foreignKeys[foreign].referencedTable = new }
                for index in image.sequences.indices where image.sequences[index].owner?.table == oldName { image.sequences[index].owner?.table = new }
                for index in image.statistics.indices where image.statistics[index].table == oldName { image.statistics[index].table = new }
            case .renameColumn(let old, let new):
                let ordinal = try column(old)
                guard !table.columns.contains(where: { $0.name == new }) else { throw JedError("42701", "column already exists: \(new)") }
                try ViewRewrite.references(in: &image, name: table.name, kind: 0, column: (ordinal, new), columns: table.columns.map(\.name))
                try CatalogRewrite.tableExpressions(&table, column: (old, new))
                table.columns[ordinal].name = new
            case .renameConstraint(let old, let new):
                guard CatalogSQL.constraints(table).contains(old) else { throw JedError("42704", "constraint does not exist: \(old)") }
                guard !CatalogSQL.constraints(table).contains(new) else { throw JedError("42710", "constraint already exists: \(new)") }
                if let index = table.indexes.firstIndex(where: { $0.name == old && ($0.isConstraint || table.exclusions.contains { $0.index == old }) }) {
                    try CatalogSQL.reserved(new, kind: "constraint")
                    guard !CatalogDomain.relations(image, settings: settings).contains(new) else { throw JedError("42P07", "relation already exists: \(new)") }
                    table.indexes[index].name = new; renamedIndexes.append((old, new))
                }
                for i in table.checks.indices where table.checks[i].name == old { table.checks[i].name = new }
                for i in table.foreignKeys.indices where table.foreignKeys[i].name == old { table.foreignKeys[i].name = new }
                for i in table.exclusions.indices where table.exclusions[i].name == old {
                    table.exclusions[i].name = new; table.exclusions[i].index = new
                }
            case .setDefault(let name, let expression):
                let ordinal = try column(name)
                guard table.columns[ordinal].identity == nil else { throw JedError("42601", "identity column cannot have an explicit default") }
                let resolver = try resolver(snapshot: snapshot, settings: settings, context: context)
                table.columns[ordinal].defaultValue = try defaultValue(expression, target: table.columns[ordinal].type.resolve(snapshot.types), source: source, resolver: resolver, context: context)
            case .dropDefault(let name):
                let ordinal = try column(name)
                guard table.columns[ordinal].identity == nil else { throw JedError("42601", "identity column cannot drop its default") }
                table.columns[ordinal].defaultValue = nil
            case .setNotNull(let name, let enabled):
                let ordinal = try column(name)
                if !enabled {
                    guard table.columns[ordinal].identity == nil else { throw JedError("42601", "identity column cannot drop not null") }
                    guard !table.primaryKey.contains(ordinal) else { throw JedError("42P16", "column is in a primary key") }
                }
                table.columns[ordinal].notNull = enabled
            case .addIdentity(let name, let identity):
                let ordinal = try column(name), target = try table.columns[ordinal].type.resolve(snapshot.types).type
                guard case .scalar(let scalar) = target, scalar.integerWidth != nil else { throw JedError("22023", "identity column type must be smallint, integer, or bigint") }
                let sequence = try SequenceDefinition.owned(table: table.name, column: name, ordinal: ordinal,
                    type: scalar, options: identity.options, taken: CatalogSQL.relationNames(image), composites: snapshot.types)
                guard table.columns[ordinal].notNull, table.columns[ordinal].identity == nil, table.columns[ordinal].defaultValue == nil else {
                    throw JedError("55000", "identity requires a NOT NULL column without an identity or default")
                }
                image.sequences.append(sequence); table.columns[ordinal].identity = identity.always
                table.columns[ordinal].defaultValue = .expression(SequenceName.defaultExpression(sequence.name))
            case .alterIdentity(let name, let always, let options):
                let ordinal = try column(name)
                guard table.columns[ordinal].identity != nil else { throw JedError("55000", "column is not an identity column") }
                let sequence = try ownedSequence(ordinal)
                guard case .scalar(let scalar) = table.columns[ordinal].type.type else { throw JedError.corruption("identity column is not an integer") }
                if let always { table.columns[ordinal].identity = always }
                if !options.isEmpty { image.sequences[sequence] = try SequenceDefinition.alter(image.sequences[sequence], options: options, type: scalar) }
            case .dropIdentity(let name, let ifExists):
                let ordinal = try column(name)
                if table.columns[ordinal].identity == nil {
                    if ifExists { continue }
                    throw JedError("55000", "column is not an identity column")
                }
                let owned = try ownedSequence(ordinal), sequenceName = image.sequences[owned].name
                guard !image.views.contains(where: { $0.dependencies.contains { $0.kind == 2 && $0.name == sequenceName } }) else {
                    throw JedError("2BP01", "cannot drop identity because other objects depend on its sequence")
                }
                image.sequences.remove(at: owned)
                table.columns[ordinal].identity = nil; table.columns[ordinal].defaultValue = nil
            case .dropColumn(let name, let ifExists, let cascade):
                guard let ordinal = table.columns.firstIndex(where: { $0.name == name }) else {
                    if ifExists { continue }
                    throw JedError("42703", "column does not exist: \(name)")
                }
                try AlterColumnDDL.drop(ordinal, table: &table, image: &image, cascade: cascade,
                    removedIndexes: &removedIndexes, added: &addedConstraints)
                sources.remove(at: ordinal); steps.append(.remove(ordinal)); clearStatistics = true
                changedTypes = Set(changedTypes.filter { $0 != ordinal }.map { $0 > ordinal ? $0 - 1 : $0 })
            case .dropConstraint(let name, let ifExists, let cascade):
                guard CatalogSQL.constraints(table).contains(name) else {
                    if ifExists { continue }
                    throw JedError("42704", "constraint does not exist: \(name)")
                }
                if let index = table.indexes.first(where: { $0.name == name && ($0.isConstraint || table.exclusions.contains { $0.index == name }) }) {
                    removedIndexes.insert(index.name); table.indexes.removeAll { $0.name == index.name }
                }
                table.checks.removeAll { $0.name == name }; table.foreignKeys.removeAll { $0.name == name }; table.exclusions.removeAll { $0.name == name }
                addedConstraints.remove(name)
                image.tables[position] = table
                for child in image.tables.indices {
                    var kept: [CatalogForeignKey] = []
                    for foreign in image.tables[child].foreignKeys {
                        if foreign.referencedTable == table.name && !uniqueCover(foreign.referencedColumns, table: table) {
                            guard cascade else { throw JedError("2BP01", "foreign key depends on the constraint") }
                        } else { kept.append(foreign) }
                    }
                    image.tables[child].foreignKeys = kept
                }
                table.foreignKeys = image.tables[position].foreignKeys
            case .addConstraint(let syntax):
                if case .primaryKey(let names) = syntax.kind {
                    guard table.primaryKey.isEmpty else { throw JedError("42P16", "multiple primary keys are not allowed") }
                    table.primaryKey = try keyColumns(names, own: nil, table: table, kind: "primary key", keyable: true, types: snapshot.types)
                    for ordinal in table.primaryKey { table.columns[ordinal].notNull = true }
                } else {
                    let name = try addAlterConstraint(syntax, table: &table, image: image, source: source,
                        snapshot: snapshot, settings: settings, context: context)
                    addedConstraints.insert(name)
                }
            case .dropPrimaryKey(let cascade):
                guard !table.primaryKey.isEmpty else { throw JedError("42704", "table has no primary key") }
                table.primaryKey = []
                image.tables[position] = table
                for child in image.tables.indices {
                    var kept: [CatalogForeignKey] = []
                    for foreign in image.tables[child].foreignKeys {
                        if foreign.referencedTable == table.name && !uniqueCover(foreign.referencedColumns, table: table) {
                            guard cascade else { throw JedError("2BP01", "foreign key depends on the primary key") }
                            if child == position { addedConstraints.remove(foreign.name) }
                        } else { kept.append(foreign) }
                    }
                    image.tables[child].foreignKeys = kept
                }
                table.foreignKeys = image.tables[position].foreignKeys
            case .setType(let name, let target, let using):
                let ordinal = try column(name)
                if case .original(let old) = sources[ordinal] { changedViewColumns.insert(original.columns[old].name) }
                let value = try AlterColumnType.apply(ordinal, target: target, using: using, table: &table,
                    image: &image, source: source, snapshot: snapshot, settings: settings, context: context, preparationCost: &preparationCost)
                changedTypes.insert(ordinal); clearStatistics = true; steps.append(.convert(ordinal, value))
                addedConstraints.formUnion(table.checks.map(\.name)); addedConstraints.formUnion(table.indexes.map(\.name))
                addedConstraints.formUnion(table.foreignKeys.map(\.name)); addedConstraints.formUnion(table.exclusions.map(\.name))
            }
            image.tables[position] = table
        }
        for view in image.views {
            if view.dependencies.contains(where: { $0.kind == 0 && $0.name == table.name && !$0.columns.filter(changedViewColumns.contains).isEmpty }) {
                throw JedError("0A000", "cannot alter type of a column used by a view")
            }
        }
        if !changedTypes.isEmpty { try AlterColumnType.validateEndState(table: table, changed: changedTypes, image: image, snapshot: snapshot) }
        let rekey = table.primaryKey.count != original.primaryKey.count || table.primaryKey.enumerated().contains { position, ordinal in
            guard case .original(let old) = sources[ordinal] else { return true }
            return old != original.primaryKey[position] || changedTypes.contains(ordinal)
        }
        let rewriting = rekey || !steps.isEmpty
        // Frozen cores charge each saved-default conversion to a session meter,
        // then transfer its cost after action/end-state validation. This also
        // preserves their current lifetime double accrual for that preparation.
        try CatalogDomain.validateTable(table, sequences: image.sequences.filter { $0.owner?.table == table.name }, settings: settings, types: snapshot.types)
        context.meter.charge(preparationCost); try context.meter.checkpoint()
        if clearStatistics { image.statistics.removeAll { $0.table == table.name } }
        context.sequences?.synchronizeDefinitions(from: image, domain: settings.targetDomain)
        var actionSnapshot = snapshot
        try actionSnapshot.updateCatalog(image, schemaChanged: false)
        let rewritten: (rows: [AlterRowRewrite.Row], nextID: Int64)?
        if rewriting {
            rewritten = try AlterRowRewrite.materialize(table: table, steps: steps, original: originalStore,
                rekey: rekey, snapshot: actionSnapshot, context: context)
        } else { rewritten = nil }
        // The frozen contract validates newly NOT NULL original columns with a
        // second original-row scan even if a preceding rewrite checked its rows.
        let notNull = table.columns.indices.compactMap { index -> (Int, SQLText)? in
            guard table.columns[index].notNull, case .original(let ordinal) = sources[index], !original.columns[ordinal].notNull else { return nil }
            return (ordinal, table.columns[index].name)
        }
        if !notNull.isEmpty {
            try ScanCost.charge(originalStore, touched: notNull.map { $0.0 }, meter: context.meter)
            var scan = originalStore.cursor(); defer { scan.close() }
            while let record = try scan.next() {
                try context.meter.step(); context.meter.charge(1)
                let row = ScanRecord(record, pager: originalStore.tree.pool?.pager)
                for (ordinal, name) in notNull where try row.value(ordinal).isNull {
                    throw JedError("23502", "column \(name) contains null values")
                }
            }
        }
        let built: [SQLText: TableStore], tableStore: TableStore
        if let rewritten {
            let validation = try AlterConstraintValidation(table: &table, added: addedConstraints,
                rebuilt: rekey ? Set(table.indexes.map(\.name)) : [], snapshot: actionSnapshot, settings: settings, context: context)
            for row in rewritten.rows {
                try context.meter.step()
                try validation.consume(key: row.key, value: { row.values[$0] })
            }
            built = try validation.finish()
            tableStore = try AlterRowRewrite.publish(rewritten.rows, nextID: rewritten.nextID,
                types: table.columns.map { try $0.type.resolve(snapshot.types) }, original: originalStore, settings: settings)
        } else {
            tableStore = originalStore
            built = try validateAlterConstraints(&table, added: addedConstraints, store: originalStore,
                snapshot: actionSnapshot, settings: settings, context: context)
        }
        table.checks.sort { $0.name < $1.name }; table.indexes.sort { $0.name < $1.name }
        table.foreignKeys.sort { $0.name < $1.name }; table.exclusions.sort { $0.name < $1.name }
        image.tables[position] = table
        try snapshot.updateCatalog(image, schemaChanged: true)
        if table.name != name { snapshot.setTableStore(name, nil) }
        if rewriting || table.name != name { snapshot.setTableStore(table.name, tableStore) }
        for name in removedIndexes { snapshot.setIndexStore(name, nil); snapshot.setGistTree(name, nil) }
        for (old, new) in renamedIndexes {
            snapshot.setIndexStore(new, snapshot.indexes[old]); snapshot.setIndexStore(old, nil)
            if let tree = snapshot.gistTrees[old] { snapshot.setGistTree(new, tree); snapshot.setGistTree(old, nil) }
        }
        for name in built.keys.sorted() {
            let store = built[name]!
            snapshot.setIndexStore(name, store)
            if let index = table.indexes.first(where: { $0.name == name }), index.kind == 2 {
                let plan = try StoredIndexPlan.resolve(table: table, index: index, snapshot: snapshot, settings: settings, context: context)
                try plan.rebuild(store, snapshot: &snapshot)
            }
        }
        try context.sequences?.apply(to: &snapshot)
        try revalidateViews(dependingOn: name, old: before, new: snapshot, settings: settings, context: context, replacement: false)
        try context.meter.syncPending(snapshot.pendingBytes)
    }
}
