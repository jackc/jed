/// CTAS plans before checking the target name, derives stored column metadata,
/// and fills through ordinary INSERT SELECT in the same statement snapshot.
internal enum CreateTableAs {
    static func execute(_ definition: SQLCreateTable, snapshot: inout DomainSnapshot,
                        settings: SessionSettings, context: ExpressionContext,
                        parameters: [SQLValue]) throws -> MutationResult {
        var working = QueryDomains.statement(snapshot: snapshot, settings: settings)
        let result = try execute(definition, working: &working, settings: settings, context: context,
            parameters: parameters, requireWritable: { owner in
                guard owner == settings.targetDomain else {
                    throw JedError("0A000", "this CREATE TABLE AS caller needs multiple database domains")
                }
            })
        snapshot = try working.snapshot(settings.targetDomain)
        return result
    }

    static func execute(_ definition: SQLCreateTable, working: inout QueryDomains,
                        settings: SessionSettings, context: ExpressionContext,
                        parameters: [SQLValue], requireWritable: (SQLText) throws -> Void) throws -> MutationResult {
        try CatalogDomain.requireDDL(settings)
        try requireWritable(settings.targetDomain)
        var snapshot = try working.snapshot(settings.targetDomain)
        if settings.targetDomain == DomainNames.temporary, let main = working.snapshots[DomainNames.main] {
            try snapshot.borrow(main)
        }
        var planning = settings
        planning.domains = working
        planning.domains?.snapshots[settings.targetDomain] = snapshot
        let name = try CatalogDomain.name(definition.name, settings: planning)
        try CatalogSQL.reserved(name, kind: "table")
        guard let query = definition.query else { throw JedError("XX000", "CREATE TABLE AS has no query") }
        if !definition.withData { planning.privileges = SessionPrivileges(defaults: .allTables) }
        let writable = try WritableCTEPlan.resolve(.query(query), snapshot: snapshot, settings: planning, context: context)
        let plan: SelectPlan
        if let writable {
            guard let output = writable.query else { throw JedError("42601", "CREATE TABLE AS requires a query") }
            plan = output
        } else { plan = try SelectPlan.resolve(query, snapshot: snapshot, settings: planning, scalar: context.scalar) }
        _ = try ParameterBinding.bind(parameters, types: writable?.parameters ?? plan.parameters, timezone: settings.timeZone)
        let result = MutationResult(command: "CREATE TABLE", meter: context.meter)
        if CatalogDomain.relations(snapshot.catalog, settings: planning).contains(name) {
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
        guard definition.onCommit == nil || settings.targetDomain == DomainNames.temporary else { throw JedError("42P16", "ON COMMIT can only be used on temporary tables") }
        var seen: Set<SQLText> = []
        for name in names where !seen.insert(name).inserted { throw JedError("42701", "column \(name) specified more than once") }
        var columns: [CatalogColumn] = [], storedTypes: [TypeDescriptor] = []
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
                let derived = derivations[index].name
                type.collation = derived?.description == "C" ? nil : derived ?? snapshot.catalog.collations.first(where: \.isDefault)?.name
            }
            storedTypes.append(type)
            columns.append(CatalogColumn(name: names[index], type: try CatalogSQL.typeUse(type)))
        }
        let table = CatalogTable(name: name, columns: columns, root: PageSealer.nullReference(snapshot.pool.codec))
        try CatalogDomain.validateTable(table, settings: planning, types: snapshot.types)
        var image = snapshot.catalog; image.tables.append(table)
        for column in columns {
            if let name = column.type.collation, !image.collations.contains(where: { $0.name == name }),
               let collation = try settings.environment.unicode.resolve(name) {
                image.collations.append(CatalogCollation(name: name, unicode: collation.unicodeVersion,
                    cldr: collation.cldrVersion, description: collation.description))
            }
        }
        try snapshot.updateCatalog(image, schemaChanged: true)
        // A temporary source can retain an older nominal composite identity
        // after another session recreates its main type name. CTAS captures the
        // resolved output tree, including that identity, with its new store.
        snapshot.setTableStore(name, snapshot.emptyStore(types: storedTypes))
        working.snapshots[settings.targetDomain] = snapshot
        if definition.withData {
            var filling = settings; filling.domains = working
            filling.privileges = settings.privileges.allowing(.insert, on: name)
            var target = definition.name; target.database = settings.targetDomain
            let filled: MutationResult?
            if writable != nil, let part = WritableCTEPlan.extract(.query(query)), let source = part.body.queryBody {
                let insert = SQLInsert(table: target, source: .query(source))
                filled = try WritableCTEExecution.execute(.with(part.clause, .insert(insert)), working: &working,
                    settings: filling, context: context, parameters: parameters, requireWritable: requireWritable)
            } else {
                let insert = SQLInsert(table: target, source: .query(query))
                let oldPendingBase = context.meter.pendingDomainBase
                let oldPendingExcluded = context.meter.pendingDomainExcluded
                let excluded = settings.targetDomain == DomainNames.temporary
                context.meter.pendingDomainBase = working.pendingBytes - (excluded ? 0 : snapshot.pendingBytes)
                context.meter.pendingDomainExcluded = excluded
                defer {
                    context.meter.pendingDomainBase = oldPendingBase
                    context.meter.pendingDomainExcluded = oldPendingExcluded
                }
                filled = try MutationExecutor.execute(.insert(insert), snapshot: &snapshot, settings: filling,
                    context: context, parameters: parameters)
                working.snapshots[settings.targetDomain] = snapshot
            }
            guard let filled else { throw JedError("XX000", "CREATE TABLE AS fill did not execute") }
            defer { filled.close() }
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
