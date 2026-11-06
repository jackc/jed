extension CatalogDDL {
    static func addAlterConstraint(_ syntax: SQLConstraint, table: inout CatalogTable, image: CatalogImage,
                                   source: SQLText, snapshot: DomainSnapshot, settings: SessionSettings,
                                   context: ExpressionContext) throws -> SQLText {
        let local = CatalogSQL.constraints(table)
        let schema = image.tables.reduce(into: Set<SQLText>()) { $0.formUnion(CatalogSQL.constraints($1)) }
        func name(_ columns: [SQLText], label: String, relation: Bool) throws -> SQLText {
            let name = syntax.name ?? CatalogSQL.objectName(table.name, columns: columns, label: label,
                taken: relation ? schema.union(CatalogSQL.relationNames(image)) : schema)
            if relation {
                try CatalogSQL.reserved(name, kind: "constraint")
                guard !CatalogSQL.relationNames(image).contains(name) else { throw JedError("42P07", "relation already exists: \(name)") }
            }
            guard !local.contains(name) else { throw JedError("42710", "constraint already exists: \(name)") }
            return name
        }
        switch syntax.kind {
        case .check(let expression):
            try CatalogExpressionPolicy.validate(expression, isDefault: false)
            let resolver = try resolver(snapshot: snapshot, settings: settings, context: context, table: table)
            let resolved = try resolver.resolve(expression, expected: TypeDescriptor(.scalar(.boolean)))
            guard resolved.type == nil || resolved.type?.type == .scalar(.boolean) else { throw JedError("42804", "argument of CHECK must be boolean") }
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
        case .primaryKey: throw JedError("0A000", "ALTER TABLE primary-key rewrite is not yet connected")
        case .foreignKey: throw JedError("0A000", "ALTER TABLE foreign-key validation is not yet connected")
        case .exclude: throw JedError("0A000", "ALTER TABLE exclusion validation is not yet connected")
        default: throw JedError("42601", "invalid table constraint")
        }
    }

    /// Constraint additions share one final-state scan. Existing CHECKs are not
    /// resolved: stale or unparsable old checks must not block adding another one.
    static func validateAlterConstraints(_ table: inout CatalogTable, added: Set<SQLText>, store: TableStore,
                                        snapshot: DomainSnapshot, settings: SessionSettings,
                                        context: ExpressionContext) throws -> [SQLText: TableStore] {
        guard !added.isEmpty else { return [:] }
        var checks: [(SQLText, ResolvedExpression)] = []
        for ordinal in table.checks.indices where added.contains(table.checks[ordinal].name) {
            let check = table.checks[ordinal]
            guard let syntax = check.expression.parsed else { throw JedError("XX003", "stored check expression cannot be parsed") }
            let resolver = try resolver(snapshot: snapshot, settings: settings, context: context, table: table)
            let resolved = try resolver.resolve(syntax, expected: TypeDescriptor(.scalar(.boolean)))
            table.checks[ordinal].hosts = try StoredExpressionDependencies.hosts([resolved], environment: settings.environment)
            checks.append((check.name, resolved))
        }
        let plans = try table.indexes.filter { added.contains($0.name) }.map {
            try OrderedIndexPlan.resolve(table: table, index: $0, snapshot: snapshot, settings: settings, context: context)
        }
        var entries = [[[UInt8]]](repeating: [], count: plans.count)
        var unique = Array(repeating: Set<[UInt8]>(), count: plans.count)
        try ScanCost.charge(store, touched: Array(table.columns.indices), meter: context.meter)
        var scan = store.cursor(); defer { scan.close() }
        while let record = try scan.next() {
            context.meter.charge(1); try context.meter.step()
            let row = ScanRecord(record, pager: store.tree.pool?.pager)
            for ordinal in table.columns.indices { _ = try row.value(ordinal) }
            var execution = context; execution.column = row.value
            for (name, expression) in checks {
                if case .boolean(false) = try execution.evaluate(expression) { throw JedError("23514", "check constraint \(name) is violated by some row") }
            }
            for ordinal in plans.indices {
                guard let prefix = try plans[ordinal].prefix(row.value) else { continue }
                if plans[ordinal].index.unique && !prefix.hasNull && !unique[ordinal].insert(prefix.bytes).inserted {
                    throw JedError("23505", "duplicate key value violates unique constraint \(plans[ordinal].index.name)")
                }
                entries[ordinal].append(prefix.bytes + record.key)
            }
        }
        var built: [SQLText: TableStore] = [:]
        for ordinal in plans.indices {
            let keys = entries[ordinal].sorted { $0.lexicographicallyPrecedes($1) }
            var index = snapshot.emptyStore(types: [])
            for key in keys { try index.put(PackedRecord(key: key, cells: []), netStaging: settings.options.maxQueryMemoryBytes > 0) }
            built[plans[ordinal].index.name] = index
        }
        return built
    }
}
