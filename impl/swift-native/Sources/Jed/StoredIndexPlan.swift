/// The resident leaf store has one maintenance interface for ordered and GiST
/// indexes. GiST's bound prefix is followed by the same table storage key; its
/// canonical R-tree is rebuilt once from the final leaf set of a statement.
internal struct StoredIndexPlan {
    let index: CatalogIndex
    let ordered: OrderedIndexPlan
    let opclasses: [GistOpclass]?
    var touched: [Int] { ordered.touched }

    static func resolve(table: CatalogTable, index: CatalogIndex, snapshot: DomainSnapshot,
                        settings: SessionSettings, context: ExpressionContext) throws -> StoredIndexPlan {
        guard index.kind == 0 || index.kind == 2 else { throw JedError("0A000", "inverted index maintenance is not yet connected") }
        let ops: [GistOpclass]?
        if index.kind == 2 {
            ops = try index.keys.map { key in
                guard case .column(let column) = key else { throw JedError.corruption("GiST expression key") }
                return try GistOpclass(type: table.columns[column].type.resolve(snapshot.types))
            }
        } else { ops = nil }
        var ordinary = index; ordinary.kind = 0
        let plan = try OrderedIndexPlan.resolve(table: table, index: ordinary, snapshot: snapshot, settings: settings, context: context)
        return StoredIndexPlan(index: index, ordered: plan, opclasses: ops)
    }
    func prefix(_ value: @escaping (Int) throws -> SQLValue) throws -> OrderedIndexPlan.Prefix? {
        guard let ops = opclasses else { return try ordered.prefix(value) }
        var row = ordered.context; row.column = value
        if let predicate = ordered.predicate, try !row.matches(predicate) { return nil }
        var bounds: [GistBound] = []
        for ordinal in ops.indices {
            guard let bound = try ops[ordinal].leafBound(row.evaluate(ordered.keys[ordinal])) else { return nil }
            bounds.append(bound)
        }
        return OrderedIndexPlan.Prefix(bytes: try GistTuple.encode(bounds, ops: ops), hasNull: false)
    }
    func conflicts(_ prefix: OrderedIndexPlan.Prefix, store: TableStore, vacated: Set<[UInt8]> = []) throws -> Bool {
        try ordered.conflicts(prefix, store: store, vacated: vacated)
    }
    func rebuild(_ store: TableStore, snapshot: inout DomainSnapshot) throws {
        guard let ops = opclasses else { return }
        var cursor = store.cursor(); defer { cursor.close() }
        var keys: [[UInt8]] = []
        while let record = try cursor.next() { keys.append(record.key) }
        snapshot.setGistTree(index.name, try GistTree.build(opclasses: ops, leafKeys: keys))
    }
}
