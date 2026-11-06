/// Rebuilds use the same definition admission and scan as CREATE INDEX. Each
/// tentative catalog stays private until every selected owner has succeeded.
internal enum ReindexExecution {
    static func execute(_ syntax: SQLReindex, working: inout QueryDomains, settings: SessionSettings,
                        context: ExpressionContext, requireWritable: (SQLText) throws -> Void) throws -> CommandTag {
        guard settings.options.allowDDL else { throw JedError("42501", "DDL is not permitted in this session") }
        if syntax.object == "system" && syntax.concurrently { throw JedError("0A000", "cannot reindex system catalogs concurrently") }
        if syntax.tablespace != nil { throw JedError("0A000", "tablespaces are not supported") }
        if syntax.object == "schema" { throw JedError("0A000", "schemas are not supported") }
        var targets: [(SQLText, CatalogTable, CatalogIndex)] = []
        if syntax.object == "database" || syntax.object == "system" {
            let name = syntax.name?.name ?? DomainNames.main
            guard syntax.name?.database == nil, name != DomainNames.temporary, working.snapshots[name] != nil else {
                throw JedError("0A000", "can only reindex the currently open database")
            }
            try requireWritable(name)
            if syntax.object == "database" {
                let owners = name == DomainNames.main ? [DomainNames.main, DomainNames.temporary] : [name]
                for owner in owners {
                    guard let snapshot = working.snapshots[owner] else { continue }
                    for table in snapshot.catalog.tables.sorted(by: { $0.name < $1.name }) {
                        for index in table.indexes.sorted(by: { $0.name < $1.name }) { targets.append((owner, table, index)) }
                    }
                }
            }
        } else {
            guard let name = syntax.name else { throw JedError("42601", "REINDEX requires a relation name") }
            let resolved = try working.relation(name), catalog = resolved.snapshot.catalog
            try requireWritable(resolved.name)
            if SystemRelationPlan.contains(name.name) { throw JedError("42809", "cannot modify system relation") }
            if syntax.object == "index" {
                guard let table = catalog.tables.first(where: { $0.indexes.contains { $0.name == name.name } }),
                      let index = table.indexes.first(where: { $0.name == name.name }) else {
                    throw JedError(CatalogSQL.relationNames(catalog).contains(name.name) ? "42809" : "42P01", "relation is not an index: \(name.name)")
                }
                if resolved.name != DomainNames.temporary { try settings.privileges.require(.select, on: table.name) }
                targets.append((resolved.name, table, index))
            } else {
                guard let table = catalog.tables.first(where: { $0.name == name.name }) else {
                    throw JedError(CatalogSQL.relationNames(catalog).contains(name.name) ? "42809" : "42P01", "relation is not a table: \(name.name)")
                }
                if resolved.name != DomainNames.temporary { try settings.privileges.require(.select, on: table.name) }
                for index in table.indexes.sorted(by: { $0.name < $1.name }) { targets.append((resolved.name, table, index)) }
            }
        }
        var frame = working
        let meter = context.meter, oldBase = context.meter.pendingDomainBase, oldExcluded = context.meter.pendingDomainExcluded
        defer { meter.pendingDomainBase = oldBase; meter.pendingDomainExcluded = oldExcluded }
        for (owner, table, index) in targets {
            try requireWritable(owner)
            let source = try definition(index, table: table, owner: owner)
            let statement: SQLStatement
            do { statement = try SQLParser(utf8: source.utf8).parse() }
            catch { throw JedError("XX003", "index definition cannot be parsed: \(index.name)") }
            guard case .createIndex(let syntax) = statement else { throw JedError("XX003", "invalid stored index definition") }
            var snapshot = try frame.snapshot(owner), scoped = settings
            scoped.targetDomain = owner; scoped.domains = frame
            meter.pendingDomainExcluded = owner == DomainNames.temporary
            meter.pendingDomainBase = frame.pendingBytes - (meter.pendingDomainExcluded ? 0 : snapshot.pendingBytes)
            try CatalogDDL.createIndex(syntax, source: source, snapshot: &snapshot, settings: scoped, context: context, replacing: index)
            frame.snapshots[owner] = snapshot
        }
        working = frame
        return CommandTag(command: "REINDEX", rowsAffected: 0, cost: meter.accrued)
    }

    private static func definition(_ index: CatalogIndex, table: CatalogTable, owner: SQLText) throws -> SQLText {
        func quote(_ name: SQLText) -> String { String(decoding: ViewDefinition.quoted(name), as: UTF8.self) }
        var keys: [String] = []
        for (ordinal, key) in index.keys.enumerated() {
            var text: String
            switch key {
            case .column(let position): text = quote(table.columns[position].name)
            case .expression(let expression):
                guard expression.parsed != nil else { throw JedError("XX003", "index definition cannot be parsed: \(index.name)") }
                text = "(" + expression.text.description + ")"
            }
            if index.kind == 0 {
                let options = ordinal < index.keyOptions.count ? index.keyOptions[ordinal] : 0
                text += options & 1 == 0 ? " ASC" : " DESC"
                text += options & 2 == 0 ? " NULLS LAST" : " NULLS FIRST"
            }
            keys.append(text)
        }
        let method = index.kind == 2 ? "gist" : index.kind == 1 ? "gin" : "btree"
        var text = "CREATE " + (index.unique ? "UNIQUE " : "") + "INDEX " + quote(index.name)
        text += " ON " + quote(owner) + "." + quote(table.name) + " USING " + method + " (" + keys.joined(separator: ", ") + ")"
        if let predicate = index.predicate {
            guard predicate.parsed != nil else { throw JedError("XX003", "index predicate cannot be parsed: \(index.name)") }
            text += " WHERE " + predicate.text.description
        }
        return try SQLText(text)
    }
}
