/// One database domain's immutable published state. Dictionaries map catalog
/// names to independent packed trees; they never contain decoded table rows.
internal struct DomainSnapshot: Sendable {
    /// Stored fields live in one copy-on-write box, so passing this value
    /// around costs one retain instead of a field-by-field copy.
    private final class Storage: @unchecked Sendable {
        var identity: StorageRevision
        var catalogID: UInt64
        var pool: NodePool
        var txid: UInt64
        var catalog: CatalogImage
        var types: [SQLText: CompositeType]
        var borrowedTypes: [SQLText: CompositeType]
        var borrowedReferences: CatalogBorrowedReferences
        var tables: [SQLText: TableStore]
        var indexes: [SQLText: TableStore]
        var gistTrees: [SQLText: GistTree]
        var catalogRevision: StorageRevision
        var dataRevision: StorageRevision
        var estimatorRevisions: [SQLText: StorageRevision]
        var nextTypeID: UInt32
        /// The last successful `borrowTypes(from:)`: the lender's identity and
        /// catalog revision, and this catalog's revision at that time.
        var borrowStamp: (StorageRevision, StorageRevision, StorageRevision)?
        init(identity: StorageRevision, catalogID: UInt64, pool: NodePool, txid: UInt64, catalog: CatalogImage, types: [SQLText: CompositeType], borrowedTypes: [SQLText: CompositeType], borrowedReferences: CatalogBorrowedReferences, tables: [SQLText: TableStore], indexes: [SQLText: TableStore], gistTrees: [SQLText: GistTree], catalogRevision: StorageRevision, dataRevision: StorageRevision, estimatorRevisions: [SQLText: StorageRevision], nextTypeID: UInt32) {
            self.identity = identity
            self.catalogID = catalogID
            self.pool = pool
            self.txid = txid
            self.catalog = catalog
            self.types = types
            self.borrowedTypes = borrowedTypes
            self.borrowedReferences = borrowedReferences
            self.tables = tables
            self.indexes = indexes
            self.gistTrees = gistTrees
            self.catalogRevision = catalogRevision
            self.dataRevision = dataRevision
            self.estimatorRevisions = estimatorRevisions
            self.nextTypeID = nextTypeID
        }
        func copy() -> Storage {
            let copy = Storage(identity: identity, catalogID: catalogID, pool: pool, txid: txid, catalog: catalog, types: types, borrowedTypes: borrowedTypes, borrowedReferences: borrowedReferences, tables: tables, indexes: indexes, gistTrees: gistTrees, catalogRevision: catalogRevision, dataRevision: dataRevision, estimatorRevisions: estimatorRevisions, nextTypeID: nextTypeID)
            copy.borrowStamp = borrowStamp
            return copy
        }
    }
    private var storage: Storage
    @inline(__always) private mutating func unique() {
        if !isKnownUniquelyReferenced(&storage) { storage = storage.copy() }
    }
    var identity: StorageRevision { storage.identity }
    var catalogID: UInt64 { storage.catalogID }
    var pool: NodePool { storage.pool }
    private(set) var txid: UInt64 {
        get { storage.txid }
        _modify { unique(); yield &storage.txid }
    }
    private(set) var catalog: CatalogImage {
        get { storage.catalog }
        _modify { unique(); yield &storage.catalog }
    }
    private(set) var types: [SQLText: CompositeType] {
        get { storage.types }
        _modify { unique(); yield &storage.types }
    }
    private(set) var borrowedTypes: [SQLText: CompositeType] {
        get { storage.borrowedTypes }
        _modify { unique(); yield &storage.borrowedTypes }
    }
    private(set) var borrowedReferences: CatalogBorrowedReferences {
        get { storage.borrowedReferences }
        _modify { unique(); yield &storage.borrowedReferences }
    }
    private(set) var tables: [SQLText: TableStore] {
        get { storage.tables }
        _modify { unique(); yield &storage.tables }
    }
    private(set) var indexes: [SQLText: TableStore] {
        get { storage.indexes }
        _modify { unique(); yield &storage.indexes }
    }
    private(set) var gistTrees: [SQLText: GistTree] {
        get { storage.gistTrees }
        _modify { unique(); yield &storage.gistTrees }
    }
    private(set) var catalogRevision: StorageRevision {
        get { storage.catalogRevision }
        _modify { unique(); yield &storage.catalogRevision }
    }
    private(set) var dataRevision: StorageRevision {
        get { storage.dataRevision }
        _modify { unique(); yield &storage.dataRevision }
    }
    private var estimatorRevisions: [SQLText: StorageRevision] {
        get { storage.estimatorRevisions }
        _modify { unique(); yield &storage.estimatorRevisions }
    }
    private var nextTypeID: UInt32 {
        get { storage.nextTypeID }
        _modify { unique(); yield &storage.nextTypeID }
    }

    init(catalogID: UInt64, txid: UInt64, catalog: CatalogImage, pool: NodePool,
         tables: [SQLText: TableStore], indexes: [SQLText: TableStore], gistTrees: [SQLText: GistTree],
         borrowedTypes: [SQLText: CompositeType] = [:], borrowedReferences: CatalogBorrowedReferences = .init()) throws {
        let types = try catalog.resolvedTypes(catalogID: catalogID, borrowedTypes: borrowedTypes)
        guard types.count < UInt32.max else { throw JedError("54000", "composite type identity limit exceeded") }
        storage = Storage(identity: StorageRevision(), catalogID: catalogID, pool: pool, txid: txid, catalog: catalog,
            types: types, borrowedTypes: borrowedTypes, borrowedReferences: borrowedReferences, tables: tables,
            indexes: indexes, gistTrees: gistTrees, catalogRevision: StorageRevision(), dataRevision: StorageRevision(),
            estimatorRevisions: tables.mapValues { _ in StorageRevision() }, nextTypeID: UInt32(types.count + 1))
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

    /// Borrows the lender's composite types before each statement. Types
    /// change only with a schema revision, and borrowing never replaces a name
    /// already held, so repeating it against unchanged catalogs changes
    /// nothing and is skipped.
    mutating func borrowTypes(from lender: DomainSnapshot) throws {
        if let stamp = storage.borrowStamp, stamp.0 === lender.identity, stamp.1 === lender.catalogRevision,
           stamp.2 === catalogRevision { return }
        try borrowTypes(lender.types)
        unique()
        storage.borrowStamp = (lender.identity, lender.catalogRevision, catalogRevision)
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
    func emptyStore(types: [TypeDescriptor], primaryKey: [Int]? = nil) -> TableStore {
        TableStore(tree: BPlusTree(codec: pool.codec, types: types, pool: pool, layout: primaryKey.map { .table(types: types, primaryKey: $0) } ?? .index))
    }
    func estimatorRevision(_ name: SQLText) -> StorageRevision? { estimatorRevisions[name] }
    mutating func advanceEstimatorRevision(_ name: SQLText) { estimatorRevisions[name] = StorageRevision() }
    mutating func setTableStore(_ name: SQLText, _ store: TableStore?) {
        if store == nil { estimatorRevisions[name] = nil }
        else if tables[name]?.revision !== store?.revision { advanceEstimatorRevision(name) }
        tables[name] = store
    }
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
    /// DML changes only a row count and statistics staleness. Neither names
    /// another catalog object, so references and resolved types stand as
    /// validated; this is `updateCatalog(_:schemaChanged: false)` without
    /// repeating that whole-catalog work per statement.
    mutating func recordRowChange(_ table: SQLText, countDelta: Int64) {
        let index = catalog.tables.firstIndex { $0.name == table }!
        catalog.tables[index].rowCount += countDelta
        for statistic in catalog.statistics.indices where catalog.statistics[statistic].table == table
            && !catalog.statistics[statistic].stale {
            catalog.statistics[statistic].stale = true
        }
        dataRevision = StorageRevision()
    }
    /// Explicit storage maintenance is the only unchanged-data commit kind.
    /// It rewrites the catalog/free list without resolving persisted SQL.
    mutating func requireStorageMaintenance() { dataRevision = StorageRevision() }
    mutating func forgetPendingKeys() {
        for name in tables.keys { tables[name]!.forgetPendingKeys() }
        for name in indexes.keys { indexes[name]!.forgetPendingKeys() }
    }
    /// Physical residency changes only after the bytes exist. A page client
    /// re-reads its own prepared body, like the service store's pending writes;
    /// local domains go through the ordinary pool. A failed post-commit read
    /// poisons future writes but does not reverse a known durable commit.
    mutating func finishPersistence(written: Set<UInt32>, remotePages: [UInt32: [UInt8]]? = nil) {
        let pool = self.pool
        func read(_ reference: PageReference, _ types: [TypeDescriptor], _ layout: LeafLayout) throws -> PackedLeaf {
            if let remotePages {
                guard let bytes = remotePages[reference.index] else { throw JedError.corruption("missing committed root page") }
                return try PackedLeaf(bytes: pool.codec.openBody(pageIndex: reference.index,
                    sealed: bytes, expectedTag: reference.tag), types: types, codec: pool.codec, layout: layout)
            }
            return try pool.load(reference, types: types, layout: layout)
        }
        do {
            for name in tables.keys.sorted() {
                var tree = tables[name]!.tree
                try tree.refaultRoot(afterWriting: written, read: read)
                tables[name]!.installPersistedTree(tree)
            }
            for name in indexes.keys.sorted() {
                var tree = indexes[name]!.tree
                try tree.refaultRoot(afterWriting: written, read: read)
                indexes[name]!.installPersistedTree(tree)
            }
        } catch { pool.pager?.poisonWriter() }
    }

    mutating func finishCommit(txid: UInt64, catalog: CatalogImage,
                               tables: [SQLText: TableStore], indexes: [SQLText: TableStore]) {
        self.txid = txid; self.catalog = catalog; self.tables = tables; self.indexes = indexes
        for name in self.tables.keys { self.tables[name]!.clearPending() }
        for name in self.indexes.keys { self.indexes[name]!.clearPending() }
    }
}
