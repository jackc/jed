/// Domain selection happens at the session boundary. These checks preserve the
/// owning catalog and the main/temp namespace without merging any stored roots.
internal enum CatalogDomain {
    static func name(_ name: SQLName, settings: SessionSettings) throws -> SQLText {
        guard name.database == nil || name.database == settings.targetDomain else {
            throw JedError("42P01", "relation does not exist in database \(settings.targetDomain): \(name.name)")
        }
        return name.name
    }
    static func relations(_ image: CatalogImage, settings: SessionSettings) -> Set<SQLText> {
        var result = CatalogSQL.relationNames(image)
        if !DomainNames.isAttachment(settings.targetDomain), let domains = settings.domains {
            for name in [DomainNames.main, DomainNames.temporary] where name != settings.targetDomain {
                if let snapshot = domains.snapshots[name] { result.formUnion(CatalogSQL.relationNames(snapshot.catalog)) }
            }
        }
        return result
    }
    static func constraints(_ image: CatalogImage, settings: SessionSettings) -> Set<SQLText> {
        // Derived CHECK/FK names walk only their own schema. Relation names
        // (indexes, UNIQUE, EXCLUDE) retain the shared main/temp namespace.
        image.tables.reduce(into: Set<SQLText>()) { $0.formUnion(CatalogSQL.constraints($1)) }
    }

    static func requireDDL(_ settings: SessionSettings, statement: SQLStatement? = nil) throws {
        var temporary = settings.targetDomain == DomainNames.temporary
        if case .createView(let value) = statement { temporary = value.temporary || value.name.database == DomainNames.temporary }
        let permitted = temporary ? (settings.options.allowTemporaryDDL ?? settings.options.allowDDL) : settings.options.allowDDL
        guard permitted else { throw JedError("42501", "DDL is not permitted in this session") }
    }
    static func validateTable(_ table: CatalogTable, sequences: [CatalogSequence] = [],
                              settings: SessionSettings, types: [SQLText: CompositeType]) throws {
        let attached = DomainNames.isAttachment(settings.targetDomain)
        if attached {
            guard table.foreignKeys.isEmpty, table.exclusions.isEmpty, sequences.isEmpty,
                  table.columns.allSatisfy({ $0.identity == nil }) else {
                throw JedError("0A000", "this constraint or sequence column is not supported on an attached table")
            }
            func composite(_ type: SQLType) -> Bool {
                switch type { case .composite: return true; case .array(let element), .range(let element): return composite(element); default: return false }
            }
            for column in table.columns where try composite(column.type.resolve(types).type) {
                throw JedError("0A000", "composite columns are not supported on an attached table")
            }
        }
        if attached || settings.targetDomain == DomainNames.temporary {
            guard table.columns.allSatisfy({ $0.type.collation == nil }) else {
                throw JedError("0A000", "COLLATE on temporary or attached table columns is not yet supported")
            }
        }
    }
    static func foreignParent(_ name: SQLName, child: CatalogTable, image: CatalogImage,
                              snapshot: DomainSnapshot, settings: SessionSettings) throws -> CatalogTable {
        let domains = QueryDomains.statement(snapshot: snapshot, settings: settings)
        if let qualifier = name.database { _ = try domains.snapshot(qualifier) }
        if DomainNames.isAttachment(settings.targetDomain) || name.database.map(DomainNames.isAttachment) == true {
            throw JedError("0A000", "foreign keys cannot cross attached database boundaries")
        }
        if name.name == child.name, name.database == nil || name.database == settings.targetDomain { return child }
        let owner = try domains.relation(name)
        let catalog = owner.name == settings.targetDomain ? image : owner.snapshot.catalog
        guard let parent = catalog.tables.first(where: { $0.name == name.name }) else {
            if CatalogSQL.relationNames(catalog).contains(name.name) { throw JedError("42809", "referenced relation \(name.name) is not a table") }
            throw JedError("42P01", "table does not exist: \(name.name)")
        }
        guard owner.name == settings.targetDomain else {
            throw JedError("42P16", settings.targetDomain == DomainNames.temporary
                ? "constraints on temporary tables may reference only temporary tables"
                : "constraints on permanent tables may reference only permanent tables")
        }
        return parent
    }
}
