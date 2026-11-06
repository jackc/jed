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
        case .foreignKey(let columns, let reference):
            let local = try keyColumns(columns, own: nil, table: table, kind: "foreign key", keyable: false, types: snapshot.types)
            let parentName = try nameInMain(reference.table)
            let parent: CatalogTable
            if parentName == table.name { parent = table }
            else if let found = image.tables.first(where: { $0.name == parentName }) { parent = found }
            else if CatalogSQL.relationNames(image).contains(parentName) { throw JedError("42809", "\(parentName) is not a table") }
            else { throw JedError("42P01", "table does not exist: \(parentName)") }
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
            try StoredIndexPlan.resolve(table: table, index: $0, snapshot: snapshot, settings: settings, context: context)
        }
        var entries = [[[UInt8]]](repeating: [], count: plans.count)
        var unique = Array(repeating: Set<[UInt8]>(), count: plans.count)
        let foreign = try table.foreignKeys.filter { added.contains($0.name) }.sorted { $0.name < $1.name }.map {
            try ForeignKeyValidation.Plan(child: table, foreign: $0, snapshot: snapshot,
                parent: $0.referencedTable == table.name ? table : nil)
        }
        let exclusions = try table.exclusions.filter { added.contains($0.name) }.sorted { $0.name < $1.name }.map {
            try ExclusionValidation(table: table, definition: $0, snapshot: snapshot, settings: settings, context: context)
        }
        var rows: [ScanRecord] = []
        try ScanCost.charge(store, touched: Array(table.columns.indices), meter: context.meter)
        var scan = store.cursor(); defer { scan.close() }
        while let record = try scan.next() {
            context.meter.charge(1); try context.meter.step()
            let row = ScanRecord(record, pager: store.tree.pool?.pager)
            for ordinal in table.columns.indices { _ = try row.value(ordinal) }
            if !foreign.isEmpty || !exclusions.isEmpty { rows.append(row) }
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
        // ALTER validates FKs after the complete CHECK/uniqueness pass, in FK
        // name order. Self-references use the scanned final state and charge
        // each candidate comparison, including the successful one.
        for plan in foreign {
            for row in rows {
                let probe = try plan.childProbe(row.value, context: context)
                switch probe {
                case .exempt: continue
                case .missing: throw plan.violation()
                case .key(let key):
                    if plan.parent.name == table.name {
                        var found = false
                        for candidate in rows {
                            context.meter.charge(1); try context.meter.step()
                            if try plan.parentProbe(candidate.value, context: context) == key { found = true; break }
                        }
                        guard found else { throw plan.violation() }
                    } else {
                        guard try plan.parentExists(key, snapshot: snapshot, context: context) else { throw plan.violation() }
                    }
                }
            }
        }
        for exclusion in exclusions {
            let members = try rows.compactMap { try exclusion.member($0.value) }
            for second in members.indices {
                for first in 0..<second {
                    context.meter.charge(1); try context.meter.step()
                    if let a = members[second].probe, let b = members[first].probe, try exclusion.conflicts(a, b) {
                        throw exclusion.violation(a, other: b.values, alter: true)
                    }
                }
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
