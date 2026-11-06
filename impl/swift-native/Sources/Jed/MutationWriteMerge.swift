/// Writable CTE sub-statements prepare against their common read pin, then
/// merge records into the current working roots. Earlier parts' unrelated
/// writes must survive, and a hidden inserted key still raises 23505 here.
extension MutationExecutor {
    struct WriteChange {
        let old: PackedRecord?
        let new: PackedRecord?
    }
    static func mergeWrites(_ changes: [WriteChange], relation: Relation, indexes: [StoredIndexPlan],
                            allocated: TableStore, snapshot: inout DomainSnapshot,
                            settings: SessionSettings, twoPass: Bool = false) throws {
        var store = allocated
        let vacated = Set(changes.compactMap { $0.old?.key })
        var primary: Set<[UInt8]> = []
        for change in changes {
            guard let new = change.new else { continue }
            guard primary.insert(new.key).inserted else { throw duplicate(relation.table.name) }
            // A row this same sub-statement vacates is free (including an
            // upsert insert at a key its earlier proposal moved away from).
            if !vacated.contains(new.key), try store.get(new.key) != nil { throw duplicate(relation.table.name) }
        }
        var entries: [[StoredIndexPlan.Change]] = []
        for index in indexes {
            guard let existing = snapshot.indexes[index.index.name] else { throw JedError.corruption("missing writable CTE index") }
            var batch: Set<[UInt8]> = [], edits: [StoredIndexPlan.Change] = []
            for change in changes {
                // The pin's old version may have been replaced or deleted by
                // an earlier part. Remove the entry the working tree owns.
                var before: [StoredIndexPlan.Entry] = [], after: [StoredIndexPlan.Entry] = []
                if let key = change.old?.key, let record = try store.get(key) {
                    let row = ScanRecord(record, pager: store.tree.pool?.pager)
                    before = try index.entries(row.value, storageKey: key)
                }
                if let record = change.new {
                    let row = ScanRecord(record, pager: store.tree.pool?.pager)
                    let indexed = try index.row(row.value, storageKey: record.key)
                    if let prefix = indexed.prefix {
                        if index.index.unique && !prefix.hasNull {
                            guard batch.insert(prefix.bytes).inserted,
                                  try !index.conflicts(prefix, store: existing, vacated: vacated) else { throw duplicateIndex(index.index.name) }
                        }
                    }
                    after = indexed.entries
                }
                edits.append(StoredIndexPlan.Change(before: before, after: after))
            }
            entries.append(edits)
        }
        var delta: Int64 = 0
        for change in changes {
            if let old = change.old, twoPass || change.new == nil || old.key != change.new?.key,
               try store.remove(old.key) != nil { delta -= 1 }
        }
        for change in changes {
            if let new = change.new, try store.put(new, netStaging: settings.options.maxQueryMemoryBytes > 0) == nil { delta += 1 }
        }
        if !changes.isEmpty {
            for ordinal in indexes.indices {
                let index = indexes[ordinal]
                guard var target = snapshot.indexes[index.index.name] else { throw JedError.corruption("missing writable CTE index") }
                try index.apply(entries[ordinal], to: &target, twoPass: twoPass,
                    netStaging: settings.options.maxQueryMemoryBytes > 0)
                snapshot.setIndexStore(index.index.name, target); try index.rebuild(target, snapshot: &snapshot)
            }
            try install(store, relation: relation, countDelta: delta, snapshot: &snapshot)
        }
    }
}
