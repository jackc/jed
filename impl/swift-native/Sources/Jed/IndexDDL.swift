extension CatalogDDL {
    static func createIndex(_ definition: SQLCreateIndex, source: SQLText, snapshot: inout DomainSnapshot,
                            settings: SessionSettings, context: ExpressionContext, replacing: CatalogIndex? = nil) throws {
        try SystemRelationPlan.rejectTarget(definition.table.name)
        let tableName = try CatalogDomain.name(definition.table, settings: settings)
        guard let tablePosition = snapshot.catalog.tables.firstIndex(where: { $0.name == tableName }),
              let store = snapshot.tables[tableName] else {
            if CatalogSQL.relationNames(snapshot.catalog).contains(tableName) { throw JedError("42809", "\(tableName) is not a table") }
            throw JedError("42P01", "table does not exist: \(tableName)")
        }
        let table = snapshot.catalog.tables[tablePosition]
        try CatalogEnvironment.checkTableCollations(table, image: snapshot.catalog, environment: settings.environment)
        let method = definition.method?.description ?? "btree"
        let kind: UInt8
        switch method {
        case "btree": kind = 0
        case "gist": kind = 2
        case "gin": kind = 1
        default: throw JedError("42704", "access method does not exist: \(method)")
        }
        let resolver = try resolver(snapshot: snapshot, settings: settings, context: context, table: table)
        resolver.storageCollation = true
        var keys: [CatalogIndexKey] = [], expressions: [ResolvedExpression] = [], parts: [SQLText] = [], options: [UInt8] = []
        func unwrapped(_ expression: SQLExpression) -> SQLExpression {
            if case .parenthesized(let child) = expression.kind { return unwrapped(child) }
            return expression
        }
        for key in definition.keys {
            let expression = unwrapped(key.expression)
            if kind != 0, key.indexExpression { throw JedError("0A000", "an expression key on a \(method) index is not supported yet") }
            try CatalogExpressionPolicy.validate(expression, isDefault: false)
            let resolved = try resolver.resolve(expression)
            var part: SQLText
            if case .column(let ordinal) = resolved.node, !key.indexExpression {
                keys.append(.column(ordinal)); part = table.columns[ordinal].name
            } else {
                keys.append(.expression(try CatalogSQL.expression(expression, source: source)))
                if case .call(let call) = expression.kind { part = call.name }
                else { part = SQLText(validatedUTF8: Array("expr".utf8)) }
            }
            _ = try IndexDependencies.resolve([resolved], environment: settings.environment)
            guard let type = resolved.type else { throw JedError("42P18", "could not determine index expression type") }
            if kind != 0 {
                if kind == 1 { _ = try GinOpclass(type: type) }
                else { _ = try GistOpclass(type: type) }
                if key.directionSpecified { throw JedError("0A000", "access method \(method) does not support ASC/DESC options") }
                if key.nullsFirst != nil { throw JedError("0A000", "access method \(method) does not support NULLS FIRST/LAST options") }
            } else { try KeyEncoding.validateType(type.type) }
            _ = try ExpressionCollation.derive(resolved, storage: true).orderingName()
            let base = part; var suffix = 1
            while parts.contains(part) {
                let digits = Array(String(suffix).utf8)
                var count = min(base.utf8.count, 63 - digits.count)
                while count > 0 && count < base.utf8.count && base.utf8[count] & 0xc0 == 0x80 { count -= 1 }
                part = SQLText(validatedUTF8: Array(base.utf8.prefix(count)) + digits); suffix += 1
            }
            parts.append(part); expressions.append(resolved)
            options.append((key.descending ? 1 : 0) | ((key.nullsFirst ?? key.descending) ? 2 : 0))
        }
        if kind == 1 {
            guard !definition.unique else { throw JedError("0A000", "access method gin does not support unique indexes") }
            guard keys.count <= 32 else { throw JedError("54011", "cannot use more than 32 columns in an index") }
        } else if kind == 2 {
            guard !definition.unique else { throw JedError("0A000", "access method gist does not support unique indexes") }
            guard keys.count == 1 || replacing != nil else { throw JedError("0A000", "a multi-column gist index is not supported yet") }
            guard settings.targetDomain != DomainNames.temporary else { throw JedError("0A000", "GiST indexes on temporary tables are not supported") }
        }
        if DomainNames.isAttachment(settings.targetDomain), kind != 0 { throw JedError("0A000", "non-btree indexes on attachments are not yet supported") }
        var predicate: CatalogExpression?
        if let syntax = definition.predicate {
            guard kind == 0 || kind == 2 && replacing != nil else { throw JedError("0A000", "a partial (WHERE) \(method) index is not supported yet") }
            try CatalogExpressionPolicy.validate(syntax, isDefault: false)
            let resolved = try resolver.resolve(syntax, expected: TypeDescriptor(.scalar(.boolean)))
            guard resolved.type == nil || resolved.type?.type == .scalar(.boolean) else { throw JedError("42804", "argument of WHERE must be boolean").at(syntax.expressionLocation) }
            expressions.append(resolved); predicate = try CatalogSQL.expression(syntax, source: source)
        }
        let dependencies = try IndexDependencies.resolve(expressions, environment: settings.environment)
        var relations = CatalogDomain.relations(snapshot.catalog, settings: settings)
        if let replacing { relations.remove(replacing.name) }
        let name: SQLText
        if let explicit = definition.name {
            try CatalogSQL.reserved(explicit, kind: "index")
            if relations.contains(explicit) {
                if definition.ifNotExists { return }
                throw JedError("42P07", "relation already exists: \(explicit)")
            }
            name = explicit
        } else { name = CatalogSQL.objectName(tableName, columns: parts, label: "idx", taken: relations) }
        var index = CatalogIndex(name: name, keys: keys, unique: definition.unique, kind: kind,
                                 root: PageSealer.nullReference(snapshot.pool.codec), predicate: predicate)
        index.hosts = dependencies.hosts; index.dynamicZones = dependencies.dynamicZones; index.zones = dependencies.zones
        if let replacing { index.isConstraint = replacing.isConstraint }
        if options.contains(where: { $0 != 0 }) { index.keyOptions = options }
        let plan = try StoredIndexPlan.resolve(table: table, index: index, snapshot: snapshot, settings: settings, context: context)
        try ScanCost.charge(store, touched: plan.touched, meter: context.meter)
        var cursor = store.cursor(); defer { cursor.close() }
        var entries: [StoredIndexPlan.Entry] = [], unique: Set<[UInt8]> = []
        while let record = try cursor.next() {
            context.meter.charge(1); try context.meter.step()
            let row = ScanRecord(record, types: store.tree.types, pager: store.tree.pool?.pager)
            for column in plan.touched { _ = try row.value(column) }
            let indexed = try plan.row(row.value, storageKey: record.key)
            if index.unique, let prefix = indexed.prefix, !prefix.hasNull && !unique.insert(prefix.bytes).inserted {
                throw JedError("23505", "duplicate key value violates unique constraint: \(name)")
                    .object(table: table.name, constraint: name, detail: context.errorDetail?.index(table, plan: plan, value: row.value, ending: "is duplicated."))
            }
            entries += indexed.entries
        }
        // Entry-key order fixes right-edge splits and persisted tree shape.
        let built = try plan.build(entries, snapshot: snapshot, netStaging: settings.options.maxQueryMemoryBytes > 0)
        var image = snapshot.catalog
        if let replacing, let position = image.tables[tablePosition].indexes.firstIndex(where: { $0.name == replacing.name }) {
            image.tables[tablePosition].indexes[position] = index
        } else { image.tables[tablePosition].indexes.append(index) }
        image.tables[tablePosition].indexes.sort { $0.name < $1.name }
        try snapshot.updateCatalog(image, schemaChanged: true)
        snapshot.setIndexStore(name, built)
        try plan.rebuild(built, snapshot: &snapshot)
        try context.meter.syncPending(snapshot.pendingBytes)
    }

    static func dropIndexes(_ definition: SQLDrop, snapshot: inout DomainSnapshot, settings: SessionSettings) throws {
        var targets: Set<SQLText> = []
        let catalog = snapshot.catalog, relations = CatalogSQL.relationNames(snapshot.catalog)
        for input in definition.names {
            let name = try CatalogDomain.name(input, settings: settings)
            if let table = catalog.tables.first(where: { $0.indexes.contains { $0.name == name } }),
               let index = table.indexes.first(where: { $0.name == name }) {
                guard !index.isConstraint, !table.exclusions.contains(where: { $0.index == name }) else {
                    throw JedError("2BP01", "cannot drop index \(name) because a constraint requires it").hinted(DependencyDiagnostics.ownedIndex(name, table: table))
                }
                targets.insert(name)
            } else if relations.contains(name) { throw JedError("42809", "\(name) is not an index") }
            else if !definition.ifExists { throw JedError("42704", "index does not exist: \(name)") }
        }
        if targets.isEmpty { return }
        var image = catalog
        for position in image.tables.indices {
            image.tables[position].indexes.removeAll { targets.contains($0.name) }
        }
        for child in image.tables {
            for foreign in child.foreignKeys {
                guard let parent = image.tables.first(where: { $0.name == foreign.referencedTable }) else { continue }
                guard uniqueCover(foreign.referencedColumns, table: parent) else {
                    throw JedError("2BP01", "cannot drop index because other objects depend on it: constraint \(foreign.name) on table \(child.name)").hinted(DependencyDiagnostics.cascade)
                }
            }
        }
        try snapshot.updateCatalog(image, schemaChanged: true)
        for name in targets.sorted() { snapshot.setIndexStore(name, nil); snapshot.setGistTree(name, nil) }
    }
}
