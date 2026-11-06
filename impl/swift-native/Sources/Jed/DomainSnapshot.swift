/// One database domain's immutable published state. Dictionaries map catalog
/// names to independent packed trees; they never contain decoded table rows.
internal struct DomainSnapshot: Sendable {
    let identity: StorageRevision
    let catalogID: UInt64
    let pool: NodePool
    private(set) var txid: UInt64
    private(set) var catalog: CatalogImage
    private(set) var types: [SQLText: CompositeType]
    private(set) var borrowedTypes: [SQLText: CompositeType] = [:]
    private(set) var borrowedReferences = CatalogBorrowedReferences()
    private(set) var tables: [SQLText: TableStore]
    private(set) var indexes: [SQLText: TableStore]
    private(set) var gistTrees: [SQLText: GistTree]
    private(set) var catalogRevision = StorageRevision()
    private(set) var dataRevision = StorageRevision()
    private var nextTypeID: UInt32

    init(catalogID: UInt64, txid: UInt64, catalog: CatalogImage, pool: NodePool,
         tables: [SQLText: TableStore], indexes: [SQLText: TableStore], gistTrees: [SQLText: GistTree],
         borrowedTypes: [SQLText: CompositeType] = [:], borrowedReferences: CatalogBorrowedReferences = .init()) throws {
        identity = StorageRevision(); self.catalogID = catalogID; self.txid = txid; self.catalog = catalog
        self.pool = pool; self.tables = tables; self.indexes = indexes; self.gistTrees = gistTrees
        self.borrowedTypes = borrowedTypes; self.borrowedReferences = borrowedReferences
        types = try catalog.resolvedTypes(catalogID: catalogID, borrowedTypes: borrowedTypes)
        guard types.count < UInt32.max else { throw JedError("54000", "composite type identity limit exceeded") }
        nextTypeID = UInt32(types.count + 1)
    }
    /// Temp catalogs borrow immutable main type trees without importing main
    /// catalog entries or page ownership. Retain names already used by temp
    /// stores even after another session drops their persistent definition.
    mutating func borrowTypes(_ available: [SQLText: CompositeType]) throws {
        guard catalog.composites.isEmpty else { throw JedError.corruption("a borrowed-type domain cannot define composite types") }
        borrowedTypes.merge(available) { _, current in current }
        types = borrowedTypes
        try catalog.validateReferences(catalogID: catalogID, borrowedTypes: borrowedTypes, borrowedReferences: borrowedReferences)
    }

    /// Private views may name main objects without importing their catalog or
    /// page roots. Retained facts validate unrelated private commits after a
    /// different session removes a main object; using that view still replans
    /// against the current named roots and reports the ordinary missing name.
    mutating func borrow(_ main: DomainSnapshot) throws {
        borrowedReferences.include(main.catalog)
        try borrowTypes(main.types)
    }


    /// Existing stores own fully resolved nominal identities. A same-named
    /// composite recreated in main cannot reinterpret previously stored rows.
    /// A prospective ALTER layout falls back to resolving its new declaration.
    func tableTypes(_ table: CatalogTable) throws -> [TypeDescriptor] {
        if !borrowedTypes.isEmpty, let stored = tables[table.name]?.tree.types, stored.count == table.columns.count {
            let matches = try zip(stored, table.columns).allSatisfy { descriptor, column in
                let raw = try CatalogSQL.typeUse(descriptor), declared = column.type
                return raw.type == declared.type && raw.decimal == declared.decimal
                    && raw.textLimit == declared.textLimit && raw.temporal == declared.temporal
                    && raw.collation == declared.collation
            }
            if matches { return stored }
        }
        return try table.columns.map { try $0.type.resolve(types) }
    }

    var pendingBytes: Int64 {
        tables.values.reduce(indexes.values.reduce(Int64(0)) { saturatingAdd($0, $1.pendingBytes) }) {
            saturatingAdd($0, $1.pendingBytes)
        }
    }
    func hasChanges(comparedTo old: DomainSnapshot) -> Bool {
        guard identity === old.identity, catalogRevision === old.catalogRevision, dataRevision === old.dataRevision,
              tables.count == old.tables.count, indexes.count == old.indexes.count else { return true }
        for (name, store) in tables where old.tables[name]?.revision !== store.revision { return true }
        for (name, store) in indexes where old.indexes[name]?.revision !== store.revision { return true }
        return false
    }
    func emptyStore(types: [TypeDescriptor]) -> TableStore {
        TableStore(tree: BPlusTree(codec: pool.codec, types: types, pool: pool))
    }
    mutating func setTableStore(_ name: SQLText, _ store: TableStore?) { tables[name] = store }
    mutating func setIndexStore(_ name: SQLText, _ store: TableStore?) { indexes[name] = store }
    mutating func setGistTree(_ name: SQLText, _ tree: GistTree?) {
        gistTrees[name] = tree; dataRevision = StorageRevision()
    }
    /// Call only for an actual metadata mutation. Schema revisions invalidate
    /// prepared plans; sequence/statistics metadata uses the separate data token.
    mutating func updateCatalog(_ image: CatalogImage, schemaChanged: Bool) throws {
        try image.validateReferences(catalogID: catalogID, borrowedTypes: borrowedTypes, borrowedReferences: borrowedReferences)
        var next = nextTypeID, ids: [SQLText: UInt32] = [:]
        for definition in image.composites.sorted(by: { $0.name < $1.name }) {
            if let old = types[definition.name] { ids[definition.name] = old.id }
            else {
                guard next < UInt32.max else { throw JedError("54000", "composite type identity limit exceeded") }
                ids[definition.name] = next; next += 1
            }
        }
        let definitions = Dictionary(uniqueKeysWithValues: image.composites.map { ($0.name, $0) })
        var resolved = borrowedTypes, active: Set<SQLText> = [], depths: [SQLText: Int] = [:]
        func visit(_ name: SQLText) throws {
            if resolved[name] != nil { return }
            guard let definition = definitions[name], active.insert(name).inserted, active.count <= 32 else {
                throw JedError.corruption("invalid composite type dependency")
            }
            var depth = 1
            func dependency(_ type: CatalogType) throws {
                switch type {
                case .composite(let name): try visit(name); depth = max(depth, 1 + depths[name]!)
                case .array(let element): try dependency(element)
                default: break
                }
            }
            for field in definition.fields { try dependency(field.type.type) }
            guard depth <= 32 else { throw JedError.corruption("composite type graph exceeds nesting limit") }
            depths[name] = depth
            let fields = try definition.fields.map {
                TypeField(name: $0.name, descriptor: try $0.type.resolve(resolved), notNull: $0.notNull)
            }
            resolved[name] = CompositeType(catalogID: catalogID, id: ids[name]!, name: name, fields: fields)
            active.remove(name)
        }
        for name in definitions.keys.sorted() { try visit(name) }
        catalog = image; types = resolved; nextTypeID = next
        if schemaChanged { catalogRevision = StorageRevision() } else { dataRevision = StorageRevision() }
    }
    /// Explicit storage maintenance is the only unchanged-data commit kind.
    /// It rewrites the catalog/free list without resolving persisted SQL.
    mutating func requireStorageMaintenance() { dataRevision = StorageRevision() }
    mutating func forgetPendingKeys() {
        for name in tables.keys { tables[name]!.forgetPendingKeys() }
        for name in indexes.keys { indexes[name]!.forgetPendingKeys() }
    }
    mutating func finishCommit(txid: UInt64, catalog: CatalogImage,
                               tables: [SQLText: TableStore], indexes: [SQLText: TableStore]) {
        self.txid = txid; self.catalog = catalog; self.tables = tables; self.indexes = indexes
        for name in self.tables.keys { self.tables[name]!.clearPending() }
        for name in self.indexes.keys { self.indexes[name]!.clearPending() }
    }
}
