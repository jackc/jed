extension CatalogDDL {
    static func addAlterConstraint(_ syntax: SQLConstraint, table: inout CatalogTable, image: CatalogImage,
                                   source: SQLText, snapshot: DomainSnapshot, settings: SessionSettings,
                                   context: ExpressionContext) throws -> SQLText {
        let local = CatalogSQL.constraints(table)
        let schema = CatalogDomain.constraints(image, settings: settings)
        func name(_ columns: [SQLText], label: String, relation: Bool) throws -> SQLText {
            let name = syntax.name ?? CatalogSQL.objectName(table.name, columns: columns, label: label,
                taken: relation ? schema.union(CatalogDomain.relations(image, settings: settings)) : schema)
            if relation {
                try CatalogSQL.reserved(name, kind: "constraint")
                guard !CatalogDomain.relations(image, settings: settings).contains(name) else { throw JedError("42P07", "relation already exists: \(name)") }
            }
            guard !local.contains(name) else { throw JedError("42710", "constraint already exists: \(name)") }
            return name
        }
        switch syntax.kind {
        case .check(let expression):
            try CatalogExpressionPolicy.validate(expression, isDefault: false)
            let resolver = try resolver(snapshot: snapshot, settings: settings, context: context, table: table)
            let resolved = try resolver.resolve(expression, expected: TypeDescriptor(.scalar(.boolean)))
            guard resolved.type == nil || resolved.type?.type == .scalar(.boolean) else { throw JedError("42804", "argument of CHECK must be boolean").at(expression.expressionLocation) }
            let referenced = resolver.referencedColumns.sorted()
            let chosen = try name(referenced.count == 1 ? [table.columns[referenced[0]].name] : [], label: "check", relation: false)
            table.checks.append(CatalogCheck(name: chosen, expression: try CatalogSQL.expression(expression, source: source),
                hosts: try StoredExpressionDependencies.hosts([resolved], environment: settings.environment)))
            return chosen
        case .unique(let columns):
            let keys = try keyColumns(columns, own: nil, table: table, kind: "unique", keyable: true, types: snapshot.types)
            let chosen = try name(keys.map { table.columns[$0].name }, label: "key", relation: true)
            table.indexes.append(CatalogIndex(name: chosen, keys: keys.map(CatalogIndexKey.column), unique: true,
                isConstraint: true, root: PageSealer.nullReference(snapshot.pool.codec)))
            return chosen
        case .primaryKey(let columns):
            guard table.primaryKey.isEmpty else { throw JedError("42P16", "multiple primary keys are not allowed") }
            table.primaryKey = try keyColumns(columns, own: nil, table: table, kind: "primary key", keyable: true, types: snapshot.types)
            for ordinal in table.primaryKey { table.columns[ordinal].notNull = true }
            return CatalogSQL.objectName(table.name, label: "pkey")
        case .foreignKey(let columns, let reference):
            let local = try keyColumns(columns, own: nil, table: table, kind: "foreign key", keyable: false, types: snapshot.types)
            let parent = try CatalogDomain.foreignParent(reference.table, child: table, image: image,
                snapshot: snapshot, settings: settings)
            let parentName = parent.name
            let remote: [Int]
            if reference.columns.isEmpty {
                guard !parent.primaryKey.isEmpty else { throw JedError("42704", "there is no primary key for referenced table \(parentName)") }
                remote = parent.primaryKey
            } else {
                remote = try keyColumns(reference.columns, own: nil, table: parent, kind: "foreign key", keyable: false, types: snapshot.types)
            }
            guard local.count == remote.count else { throw JedError("42830", "number of referencing and referenced columns for foreign key disagree") }
            guard uniqueCover(remote, table: parent) else { throw JedError("42830", "there is no unique constraint matching given keys for referenced table \(parentName)") }
            for (a, b) in zip(local, remote) {
                guard try foreignKeyPair(table.columns[a].type.resolve(snapshot.types).type, parent.columns[b].type.resolve(snapshot.types).type) else {
                    throw JedError("42804", "foreign key constraint cannot be implemented: incompatible key column types")
                }
            }
            let chosen = try name(local.map { table.columns[$0].name }, label: "fkey", relation: false)
            let actions: [String: UInt8] = ["no action": 0, "restrict": 1, "cascade": 2, "set null": 3, "set default": 4]
            guard let onDelete = actions[reference.onDelete], let onUpdate = actions[reference.onUpdate] else {
                throw JedError("XX000", "unresolved foreign-key action")
            }
            table.foreignKeys.append(CatalogForeignKey(name: chosen, columns: local, referencedTable: parentName,
                referencedColumns: remote, onDelete: onDelete, onUpdate: onUpdate, matchFull: reference.match == "full"))
            return chosen
        case .exclude(let method, let elements, let predicate):
            let definition = try exclusionDefinition(method: method, elements: elements, predicate: predicate,
                table: table, source: source, snapshot: snapshot, settings: settings, context: context)
            let chosen = try name(definition.columns.map { table.columns[$0].name }, label: "excl", relation: true)
            table.indexes.append(definition.index(chosen, root: PageSealer.nullReference(snapshot.pool.codec)))
            table.exclusions.append(CatalogExclusion(name: chosen, index: chosen, elements: definition.elements))
            return chosen
        default: throw JedError("42601", "invalid table constraint")
        }
    }

    /// Constraint additions validate final metadata in one full table scan.
    static func validateAlterConstraints(_ table: inout CatalogTable, added: Set<SQLText>, store: TableStore,
                                        snapshot: DomainSnapshot, settings: SessionSettings,
                                        context: ExpressionContext) throws -> [SQLText: TableStore] {
        guard !added.isEmpty else { return [:] }
        let validation = try AlterConstraintValidation(table: &table, added: added, snapshot: snapshot, settings: settings, context: context)
        try ScanCost.charge(store, touched: Array(table.columns.indices), meter: context.meter)
        var scan = store.cursor(); defer { scan.close() }
        while let record = try scan.next() {
            context.meter.charge(1); try context.meter.step()
            let row = ScanRecord(record, pager: store.tree.pool?.pager)
            for ordinal in table.columns.indices { _ = try row.value(ordinal) }
            try validation.consume(key: record.key, value: row.value)
        }
        return try validation.finish()
    }
}
