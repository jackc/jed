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
                            settings: SessionSettings) throws {
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
        var entries: [[(before: [UInt8]?, after: [UInt8]?)]] = []
        for index in indexes {
            guard let existing = snapshot.indexes[index.index.name] else { throw JedError.corruption("missing writable CTE index") }
            var batch: Set<[UInt8]> = [], edits: [(before: [UInt8]?, after: [UInt8]?)] = []
            for change in changes {
                // The pin's old version may have been replaced or deleted by
                // an earlier part. Remove the entry the working tree owns.
                var before: [UInt8]?, after: [UInt8]?
                if let key = change.old?.key, let record = try store.get(key) {
                    let row = ScanRecord(record, pager: store.tree.pool?.pager)
                    before = try index.prefix(row.value).map { $0.bytes + key }
                }
                if let record = change.new {
                    let row = ScanRecord(record, pager: store.tree.pool?.pager)
                    if let prefix = try index.prefix(row.value) {
                        if index.index.unique && !prefix.hasNull {
                            guard batch.insert(prefix.bytes).inserted,
                                  try !index.conflicts(prefix, store: existing, vacated: vacated) else { throw duplicateIndex(index.index.name) }
                        }
                        after = prefix.bytes + record.key
                    }
                }
                edits.append((before, after))
            }
            entries.append(edits)
        }
        var delta: Int64 = 0
        for change in changes {
            if let old = change.old, try store.remove(old.key) != nil { delta -= 1 }
        }
        for change in changes {
            if let new = change.new, try store.put(new, netStaging: settings.options.maxQueryMemoryBytes > 0) == nil { delta += 1 }
        }
        if !changes.isEmpty {
            for ordinal in indexes.indices {
                let index = indexes[ordinal]
                guard var target = snapshot.indexes[index.index.name] else { throw JedError.corruption("missing writable CTE index") }
                for edit in entries[ordinal] where edit.before != edit.after {
                    if let key = edit.before { try target.remove(key) }
                }
                for edit in entries[ordinal] where edit.before != edit.after {
                    if let key = edit.after { try target.put(PackedRecord(key: key, cells: []), netStaging: settings.options.maxQueryMemoryBytes > 0) }
                }
                snapshot.setIndexStore(index.index.name, target); try index.rebuild(target, snapshot: &snapshot)
            }
            try install(store, relation: relation, countDelta: delta, snapshot: &snapshot)
        }
    }
}
