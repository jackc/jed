/// One database domain's immutable published state. Dictionaries map catalog
/// names to independent packed trees; they never contain decoded table rows.
internal struct DomainSnapshot: Sendable {
    let identity: StorageRevision
    let catalogID: UInt64
    let pool: NodePool
    private(set) var txid: UInt64
    private(set) var catalog: CatalogImage
    private(set) var types: [SQLText: CompositeType]
    private(set) var tables: [SQLText: TableStore]
    private(set) var indexes: [SQLText: TableStore]
    private(set) var gistTrees: [SQLText: GistTree]
    private(set) var catalogRevision = StorageRevision()
    private(set) var dataRevision = StorageRevision()
    private var nextTypeID: UInt32

    init(catalogID: UInt64, txid: UInt64, catalog: CatalogImage, pool: NodePool,
         tables: [SQLText: TableStore], indexes: [SQLText: TableStore], gistTrees: [SQLText: GistTree]) throws {
        identity = StorageRevision(); self.catalogID = catalogID; self.txid = txid; self.catalog = catalog
        self.pool = pool; self.tables = tables; self.indexes = indexes; self.gistTrees = gistTrees
        types = try catalog.resolvedTypes(catalogID: catalogID)
        guard types.count < UInt32.max else { throw JedError("54000", "composite type identity limit exceeded") }
        nextTypeID = UInt32(types.count + 1)
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
        try image.validateReferences(catalogID: catalogID)
        var next = nextTypeID, ids: [SQLText: UInt32] = [:]
        for definition in image.composites.sorted(by: { $0.name < $1.name }) {
            if let old = types[definition.name] { ids[definition.name] = old.id }
            else {
                guard next < UInt32.max else { throw JedError("54000", "composite type identity limit exceeded") }
                ids[definition.name] = next; next += 1
            }
        }
        let definitions = Dictionary(uniqueKeysWithValues: image.composites.map { ($0.name, $0) })
        var resolved: [SQLText: CompositeType] = [:], active: Set<SQLText> = []
        func visit(_ name: SQLText) throws {
            if resolved[name] != nil { return }
            guard let definition = definitions[name], active.insert(name).inserted, active.count <= 256 else {
                throw JedError.corruption("invalid composite type dependency")
            }
            func dependency(_ type: CatalogType) throws {
                switch type {
                case .composite(let name): try visit(name)
                case .array(let element): try dependency(element)
                default: break
                }
            }
            for field in definition.fields { try dependency(field.type.type) }
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
