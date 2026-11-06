/// Layout migration rebuilds record disposition and insertion shape while
/// retaining at most one decoded row plus the current staged-write batch.
/// Interior metadata and canonical GiST trees retain their existing resident
/// contract. This internal primitive does not expose a page-size change API.
internal enum DomainRebuilder {
    static func rebuild(_ source: DomainSnapshot, scratch: inout ScratchDomain,
                        stageBytes: Int64) throws -> DomainSnapshot {
        guard stageBytes > 0 else { throw JedError("22023", "scratch stage threshold must be positive") }
        guard scratch.stages == 0 else { throw JedError("55006", "scratch domain has already been used") }
        guard Set(source.catalog.tables.map(\.name)) == Set(source.tables.keys),
              Set(source.catalog.tables.flatMap { $0.indexes.map(\.name) }) == Set(source.indexes.keys) else {
            throw JedError.corruption("source catalog and stores disagree")
        }
        let codec = scratch.pool.codec
        var target = try DomainSnapshot(catalogID: source.catalogID, txid: 0, catalog: source.catalog,
            pool: scratch.pool, tables: [:], indexes: [:], gistTrees: [:],
            borrowedTypes: source.borrowedTypes, borrowedReferences: source.borrowedReferences)
        for table in source.catalog.tables {
            guard let old = source.tables[table.name] else { throw JedError.corruption("missing source table store") }
            let types = try source.tableTypes(table)
            target.setTableStore(table.name, TableStore(tree: BPlusTree(codec: codec, types: types, pool: scratch.pool),
                nextRowID: old.nextRowID))
        }
        for name in source.indexes.keys {
            target.setIndexStore(name, TableStore(tree: BPlusTree(codec: codec, types: [], pool: scratch.pool),
                nextRowID: source.indexes[name]!.nextRowID))
        }
        // The source root is pinned by the caller. Its pager can advance in a
        // different transaction, but every inherited page remains protected.
        let sourcePages = source.pool.pager?.selected.meta.pageCount
        for isIndex in [false, true] {
            let stores = isIndex ? source.indexes : source.tables
            for name in stores.keys.sorted() {
                var cursor = stores[name]!.cursor()
                while let record = try cursor.next() {
                    var destination = (isIndex ? target.indexes[name] : target.tables[name])!
                    let values = try record.cells.indices.map {
                        RecordInput.value(try record.materialize(column: $0, pager: source.pool.pager, pageCount: sourcePages))
                    }
                    let rebuilt = try RecordDisposition.plan(key: record.key, values: values,
                        types: destination.tree.types, codec: codec)
                    try destination.put(rebuilt, netStaging: true)
                    if isIndex { target.setIndexStore(name, destination) }
                    else { target.setTableStore(name, destination) }
                    if target.pendingBytes >= stageBytes { try scratch.stage(&target) }
                }
            }
        }
        try scratch.stage(&target)
        for table in source.catalog.tables {
            for index in table.indexes where index.kind == 2 {
                let rowTypes = try target.tableTypes(table)
                let ops = try index.keys.map { key -> GistOpclass in
                    guard case .column(let ordinal) = key, rowTypes.indices.contains(ordinal) else {
                        throw JedError.corruption("invalid GiST rebuild column")
                    }
                    return try GistOpclass(type: rowTypes[ordinal])
                }
                guard let store = target.indexes[index.name] else { throw JedError.corruption("missing GiST rebuild store") }
                var cursor = store.cursor(), keys: [[UInt8]] = []
                while let record = try cursor.next() { keys.append(record.key) }
                target.setGistTree(index.name, try GistTree.build(opclasses: ops, leafKeys: keys))
            }
        }
        // Full catalog/target layout validation still precedes final image
        // entropy and output. Scratch bytes may be discarded on any refusal.
        _ = try DomainCommitPlanner.layout(target, highWater: scratch.pager.selected.meta.pageCount, free: [])
        return target
    }
}
