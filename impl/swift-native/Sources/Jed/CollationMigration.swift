/// Host-only whole-database re-keying. The source remains immutable, and each
/// scan holds only the current decoded row. Tentative output is packed trees.
internal enum CollationMigration {
    static func references(_ image: CatalogImage) -> [SQLText] {
        var names = Set(image.collations.filter(\.isDefault).map(\.name))
        for table in image.tables {
            for column in table.columns {
                if let name = column.type.collation, name.utf8 != [67] { names.insert(name) }
            }
        }
        return names.sorted()
    }

    static func upgrade(_ snapshot: inout DomainSnapshot, environment: Environment) throws -> Int {
        let names = references(snapshot.catalog)
        var skewed = Set<SQLText>()
        for name in names {
            guard let loaded = try environment.unicode.resolve(name) else { continue }
            if let pin = snapshot.catalog.collations.first(where: { $0.name == name }),
               pin.unicode != loaded.unicodeVersion || pin.cldr != loaded.cldrVersion { skewed.insert(name) }
        }
        guard !skewed.isEmpty else { return 0 }
        var working = snapshot, image = snapshot.catalog
        let collated: KeyEncoding.CollatedKey = { try environment.unicode.key($0, collation: $1) }
        func affected(_ column: CatalogColumn) -> Bool { column.type.collation.map { skewed.contains($0) } ?? false }
        for table in image.tables.sorted(by: { $0.name < $1.name }) {
            let primary = table.primaryKey.contains { affected(table.columns[$0]) }
            let indexes = table.indexes.filter { index in
                primary || index.keys.contains { if case .column(let column) = $0 { return affected(table.columns[column]) }; return false }
            }.sorted { $0.name < $1.name }
            image.statistics.removeAll { $0.table == table.name && affected(table.columns[$0.column]) }
            guard primary || !indexes.isEmpty else { continue }
            guard let old = snapshot.tables[table.name] else { throw JedError.corruption("collation upgrade table store is absent") }
            let types = try snapshot.tableTypes(table)
            func values(_ record: PackedRecord) throws -> [SQLValue] {
                try types.indices.map { try record.materialize(column: $0, type: types[$0], pager: snapshot.pool.pager) }
            }
            func storageKey(_ record: PackedRecord, _ row: [SQLValue]) throws -> [UInt8] {
                guard primary else { return record.key }
                var key: [UInt8] = []
                for column in table.primaryKey { key += try KeyEncoding.encode(row[column], type: types[column], collatedKey: collated) }
                return key
            }
            // Baseline admission first materializes every row, then rekeys the
            // PK, then visits each index. Separate bounded scans retain that
            // error precedence without collecting decoded table contents.
            do {
                var cursor = old.cursor(); defer { cursor.close() }
                while let record = try cursor.next() { _ = try values(record) }
            }
            if primary {
                // Encoding every PK precedes record disposition in the frozen
                // contract. Validate in a bounded pass before inserting rows.
                do {
                    var cursor = old.cursor(); defer { cursor.close() }
                    while let record = try cursor.next() { _ = try storageKey(record, values(record)) }
                }
                var rebuilt = snapshot.emptyStore(types: types, primaryKey: table.primaryKey), cursor = old.cursor(); defer { cursor.close() }
                while let record = try cursor.next() {
                    let row = try values(record), key = try storageKey(record, row)
                    let replacement = try RecordDisposition.plan(key: key, values: row.map(RecordInput.value), types: types, codec: snapshot.pool.codec, layout: rebuilt.tree.layout)
                    try rebuilt.put(replacement, netStaging: true)
                }
                working.setTableStore(table.name, rebuilt)
            }
            for index in indexes {
                guard index.keys.allSatisfy({ if case .column = $0 { return true }; return false }) else {
                    throw JedError("0A000", "collation upgrade of a table with an expression index is not supported yet")
                }
                guard index.predicate == nil else {
                    throw JedError("0A000", "collation upgrade of a table with a partial index is not supported yet")
                }
                let columns = index.keys.map { if case .column(let column) = $0 { return column }; return -1 }
                let gin = try index.kind == 1 ? columns.map { try GinOpclass(type: types[$0]) } : []
                let gist = try index.kind == 2 ? columns.map { try GistOpclass(type: types[$0]) } : []
                func indexKeys(_ record: PackedRecord) throws -> [[UInt8]] {
                    let row = try values(record), key = try storageKey(record, row)
                    switch index.kind {
                    case 0:
                        var prefix: [UInt8] = []
                        for ordinal in columns.indices {
                            let column = columns[ordinal], options = index.keyOptions.isEmpty ? 0 : index.keyOptions[ordinal]
                            prefix += try KeyEncoding.encodeSlot(row[column], type: types[column], descending: options & 1 != 0,
                                nullsFirst: options & 2 != 0, collatedKey: collated)
                        }
                        return [prefix + key]
                    case 1:
                        return try GinIndex.postings(values: columns.map { row[$0] }, opclasses: gin, storageKey: key).map(\.key)
                    case 2:
                        var bounds: [GistBound] = []
                        for ordinal in columns.indices {
                            guard let bound = try gist[ordinal].leafBound(row[columns[ordinal]]) else { return [] }
                            bounds.append(bound)
                        }
                        return [try GistTuple.encode(bounds, ops: gist) + key]
                    default: throw JedError.corruption("unknown index kind in collation upgrade")
                    }
                }
                // Finish key validation before a size/layout failure from the
                // packed sorter can take precedence over a later encoding error.
                do {
                    var cursor = old.cursor(); defer { cursor.close() }
                    while let record = try cursor.next() { _ = try indexKeys(record) }
                }
                var sorted = BPlusTree(codec: snapshot.pool.codec, types: [])
                do {
                    var cursor = old.cursor(); defer { cursor.close() }
                    while let record = try cursor.next() {
                        for key in try indexKeys(record) { _ = try sorted.insert(PackedRecord(key: key, cells: [])) }
                    }
                }
                var source = TreeCursor(sorted); defer { source.close() }
                var final = snapshot.emptyStore(types: [])
                if index.kind == 1 {
                    let built = try GinIndex.buildSorted(codec: snapshot.pool.codec, pool: snapshot.pool) {
                        guard let record = try source.next() else { return nil }
                        let prefix = gin.count == 1 ? 0 : 1
                        let column = prefix == 0 ? 0 : Int(record.key[0])
                        guard gin.indices.contains(column) else { throw JedError.corruption("invalid GIN upgrade column tag") }
                        let length = prefix + (try gin[column].termLength(in: record.key, start: prefix))
                        return GinPosting(term: Array(record.key.prefix(length)), storageKey: Array(record.key.dropFirst(length)))
                    }
                    // Stage final physical runs only; sorting nodes never
                    // enter the transaction's pending-write balance.
                    var runs = TreeCursor(built.tree); defer { runs.close() }
                    while let record = try runs.next() { try final.put(record, netStaging: true) }
                } else {
                    while let record = try source.next() { try final.put(record, netStaging: true) }
                }
                working.setIndexStore(index.name, final)
                if index.kind == 2 {
                    var keys: [[UInt8]] = [], leaves = final.cursor(); defer { leaves.close() }
                    while let record = try leaves.next() { keys.append(record.key) }
                    // GiST retains its resident R-tree under the supported
                    // storage contract; table rows remain streamed above.
                    working.setGistTree(index.name, try GistTree.build(opclasses: gist, leafKeys: keys))
                }
            }
        }
        for index in image.collations.indices where skewed.contains(image.collations[index].name) {
            guard let loaded = try environment.unicode.resolve(image.collations[index].name) else { throw JedError.corruption("C collation pin") }
            image.collations[index].unicode = loaded.unicodeVersion; image.collations[index].cldr = loaded.cldrVersion
            image.collations[index].description = loaded.description
        }
        try working.updateCatalog(image, schemaChanged: true)
        snapshot = working
        return skewed.count
    }
}
