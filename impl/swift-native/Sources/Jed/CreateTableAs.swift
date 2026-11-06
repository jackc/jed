/// CTAS plans before checking the target name, derives stored column metadata,
/// and fills through ordinary INSERT SELECT in the same statement snapshot.
internal enum CreateTableAs {
    static func execute(_ definition: SQLCreateTable, snapshot: inout DomainSnapshot,
                        settings: SessionSettings, context: ExpressionContext,
                        parameters: [SQLValue]) throws -> MutationResult {
        guard settings.options.allowDDL else { throw JedError("42501", "DDL is not permitted in this session") }
        let name = try CatalogDDL.nameInMain(definition.name)
        try CatalogSQL.reserved(name, kind: "table")
        guard let query = definition.query else { throw JedError("XX000", "CREATE TABLE AS has no query") }
        var planning = settings
        if !definition.withData { planning.privileges = SessionPrivileges(defaults: .allTables) }
        let plan = try SelectPlan.resolve(query, snapshot: snapshot, settings: planning, scalar: context.scalar)
        _ = try ParameterBinding.bind(parameters, types: plan.parameters, timezone: settings.timeZone)
        let result = MutationResult(command: "CREATE TABLE", meter: context.meter)
        if CatalogSQL.relationNames(snapshot.catalog).contains(name) {
            if definition.ifNotExists { return result }
            throw JedError("42P07", "relation \(name) already exists")
        }
        let names = plan.columns.enumerated().map {
            $0.offset < definition.queryColumnNames.count ? definition.queryColumnNames[$0.offset] : $0.element.name
        }
        let derivations = try plan.projections.map { try ExpressionCollation.derive($0) }
        for index in plan.columns.indices where ExpressionCollation.collatable(plan.columns[index].type?.type) {
            _ = try derivations[index].comparisonName()
        }
        guard definition.queryColumnNames.count <= plan.columns.count else { throw JedError("42601", "too many column names were specified") }
        guard definition.onCommit == nil || definition.temporary else { throw JedError("42P16", "ON COMMIT can only be used on temporary tables") }
        var seen: Set<SQLText> = []
        for name in names where !seen.insert(name).inserted { throw JedError("42701", "column \(name) specified more than once") }
        var columns: [CatalogColumn] = []
        for index in plan.columns.indices {
            var type = plan.columns[index].type ?? TypeDescriptor(.scalar(.text))
            switch type.type {
            case .record, .array(.record): throw JedError("42P16", "column \(names[index]) has pseudo-type \(type.type.name)")
            case .scalar(.jsonpath), .array(.array), .array(.range):
                throw JedError("0A000", "a \(type.type.name) column is not supported")
            default: break
            }
            if case .array = type.type { type.decimal = nil; type.textLimit = nil; type.temporal = nil }
            if ExpressionCollation.collatable(type.type) {
                type.collation = derivations[index].name ?? snapshot.catalog.collations.first(where: \.isDefault)?.name
            }
            columns.append(CatalogColumn(name: names[index], type: try CatalogSQL.typeUse(type)))
        }
        guard !definition.temporary else { throw JedError("0A000", "temporary catalog DDL is not yet connected") }
        let table = CatalogTable(name: name, columns: columns, root: PageSealer.nullReference(snapshot.pool.codec))
        var image = snapshot.catalog; image.tables.append(table)
        for column in columns {
            if let name = column.type.collation, !image.collations.contains(where: { $0.name == name }),
               let collation = try settings.environment.unicode.resolve(name) {
                image.collations.append(CatalogCollation(name: name, unicode: collation.unicodeVersion,
                    cldr: collation.cldrVersion, description: collation.description))
            }
        }
        try snapshot.updateCatalog(image, schemaChanged: true)
        snapshot.setTableStore(name, snapshot.emptyStore(types: try columns.map { try $0.type.resolve(snapshot.types) }))
        if definition.withData {
            var filling = settings; filling.privileges = settings.privileges.allowing(.insert, on: name)
            var target = definition.name; target.database = SQLText(validatedUTF8: Array("main".utf8))
            let insert = SQLInsert(table: target, source: .query(query))
            guard let filled = try MutationExecutor.execute(.insert(insert), snapshot: &snapshot, settings: filling,
                context: context, parameters: parameters) else { throw JedError("XX000", "CREATE TABLE AS fill did not execute") }
            result.affected = filled.affected
        }
        return result
    }

    /// The parser has already enforced where INTO may occur. Remove only the
    /// leftmost SELECT's target while preserving every surrounding query tail.
    static func normalize(_ statement: SQLStatement) -> SQLStatement {
        guard case .query(var query) = statement, let target = takeTarget(&query) else { return statement }
        return .createTable(SQLCreateTable(name: target.name, temporary: target.temporary, query: query))
    }
    private static func takeTarget(_ query: inout SQLQuery) -> SQLInto? {
        switch query.body {
        case .select(var select):
            let target = select.into; select.into = nil; query.body = .select(select); return target
        case .nested(var child):
            let target = takeTarget(&child); query.body = .nested(child); return target
        case .set(let operation, let all, var left, let right):
            let target = takeTarget(&left); query.body = .set(operation, all: all, left, right); return target
        case .with(let clause, var child):
            let target = takeTarget(&child); query.body = .with(clause, child); return target
        case .values: return nil
        }
    }
}
