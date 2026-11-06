/// Index rows expand to logical entries. Ordered/GiST entries are physical
/// records; GIN entries edit bounded posting runs through TableStore.editGIN.
/// Only from-scratch builds use greedy packing of the complete posting set.
internal struct StoredIndexPlan {
    struct Entry: Equatable {
        let bytes: [UInt8]
        let storageKeyLength: Int
        var term: [UInt8] { Array(bytes.dropLast(storageKeyLength)) }
        var storageKey: [UInt8] { Array(bytes.suffix(storageKeyLength)) }
    }
    struct Change {
        let removed: [Entry]
        let inserted: [Entry]
        init(before: [Entry], after: [Entry]) {
            // Entry lists are byte-sorted. Keep the row's existing entry order
            // while leaving byte-identical postings entirely untouched.
            var removed: [Entry] = [], inserted: [Entry] = [], a = 0, b = 0
            while a < before.count || b < after.count {
                if a == before.count { inserted.append(after[b]); b += 1 }
                else if b == after.count { removed.append(before[a]); a += 1 }
                else if before[a].bytes == after[b].bytes { a += 1; b += 1 }
                else if before[a].bytes.lexicographicallyPrecedes(after[b].bytes) { removed.append(before[a]); a += 1 }
                else { inserted.append(after[b]); b += 1 }
            }
            self.removed = removed; self.inserted = inserted
        }
    }
    let index: CatalogIndex
    let ordered: OrderedIndexPlan
    let opclasses: [GistOpclass]?
    let ginOpclasses: [GinOpclass]?
    var touched: [Int] { ordered.touched }

    static func resolve(table: CatalogTable, index: CatalogIndex, snapshot: DomainSnapshot,
                        settings: SessionSettings, context: ExpressionContext) throws -> StoredIndexPlan {
        guard index.kind <= 2 else { throw JedError.corruption("invalid index kind") }
        let ops: [GistOpclass]?
        let rowTypes = try snapshot.tableTypes(table)
        if index.kind == 2 {
            ops = try index.keys.map { key in
                guard case .column(let column) = key else { throw JedError.corruption("GiST expression key") }
                return try GistOpclass(type: rowTypes[column])
            }
        } else { ops = nil }
        var ordinary = index; ordinary.kind = 0
        let gin: [GinOpclass]?
        if index.kind == 1 {
            guard !index.unique, index.predicate == nil, (1...32).contains(index.keys.count) else {
                throw JedError.corruption("invalid GIN index definition")
            }
            gin = try index.keys.map { key in
                guard case .column(let column) = key else { throw JedError.corruption("GIN expression key") }
                return try GinOpclass(type: rowTypes[column])
            }
        } else { gin = nil }
        let plan = try OrderedIndexPlan.resolve(table: table, index: ordinary, snapshot: snapshot,
            settings: settings, context: context, orderedKeys: gin == nil)
        return StoredIndexPlan(index: index, ordered: plan, opclasses: ops, ginOpclasses: gin)
    }
    func prefix(_ value: @escaping (Int) throws -> SQLValue) throws -> OrderedIndexPlan.Prefix? {
        guard ginOpclasses == nil else { return nil }
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
    func row(_ value: @escaping (Int) throws -> SQLValue, storageKey: [UInt8]) throws -> (prefix: OrderedIndexPlan.Prefix?, entries: [Entry]) {
        if let gin = ginOpclasses {
            var row = ordered.context; row.column = value
            let values = try ordered.keys.map { try row.evaluate($0) }
            let entries = try GinIndex.postings(values: values, opclasses: gin, storageKey: storageKey).map {
                Entry(bytes: $0.key, storageKeyLength: storageKey.count)
            }
            return (nil, entries)
        }
        let prefix = try prefix(value)
        return (prefix, prefix.map { [Entry(bytes: $0.bytes + storageKey, storageKeyLength: storageKey.count)] } ?? [])
    }
    func entries(_ value: @escaping (Int) throws -> SQLValue, storageKey: [UInt8]) throws -> [Entry] {
        try row(value, storageKey: storageKey).entries
    }
    func insert(_ entry: Entry, into store: inout TableStore, netStaging: Bool) throws {
        if ginOpclasses != nil { try store.editGIN(term: entry.term, key: entry.storageKey, insert: true, netStaging: netStaging) }
        else { try store.put(PackedRecord(key: entry.bytes, cells: []), netStaging: netStaging) }
    }
    func remove(_ entry: Entry, from store: inout TableStore, netStaging: Bool) throws {
        if ginOpclasses != nil { try store.editGIN(term: entry.term, key: entry.storageKey, insert: false, netStaging: netStaging) }
        else { try store.remove(entry.bytes) }
    }
    func apply(_ changes: [Change], to store: inout TableStore, twoPass: Bool, netStaging: Bool) throws {
        if twoPass {
            for change in changes {
                for entry in change.removed { try remove(entry, from: &store, netStaging: netStaging) }
            }
        }
        for change in changes {
            if !twoPass {
                for entry in change.removed { try remove(entry, from: &store, netStaging: netStaging) }
            }
            for entry in change.inserted { try insert(entry, into: &store, netStaging: netStaging) }
        }
    }
    func build(_ entries: [Entry], snapshot: DomainSnapshot, netStaging: Bool) throws -> TableStore {
        let sorted = entries.sorted { $0.bytes.lexicographicallyPrecedes($1.bytes) }
        var store = snapshot.emptyStore(types: [])
        if ginOpclasses != nil {
            var position = 0
            while position < sorted.count {
                let term = sorted[position].term
                var keys: [[UInt8]] = []
                repeat {
                    keys.append(sorted[position].storageKey); position += 1
                } while position < sorted.count && sorted[position].term == term
                for run in try GinPostingRun.pack(term: term, keys: keys) {
                    try store.put(PackedRecord(key: run, cells: []), netStaging: netStaging)
                }
            }
        } else {
            for entry in sorted { try store.put(PackedRecord(key: entry.bytes, cells: []), netStaging: netStaging) }
        }
        return store
    }
    func rebuild(_ store: TableStore, snapshot: inout DomainSnapshot) throws {
        guard let ops = opclasses else { return }
        var cursor = store.cursor(); defer { cursor.close() }
        var keys: [[UInt8]] = []
        while let record = try cursor.next() { keys.append(record.key) }
        snapshot.setGistTree(index.name, try GistTree.build(opclasses: ops, leafKeys: keys))
    }
}
