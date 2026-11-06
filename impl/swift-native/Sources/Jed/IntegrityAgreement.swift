internal extension IntegrityChecker {
    func collationUnavailable(_ name: SQLText?) -> Bool {
        !CatalogEnvironment.collationMatches(name, image: snapshot.catalog, environment: environment)
    }
    func resolveIndex(_ table: CatalogTable, index: CatalogIndex) throws -> (StoredIndexPlan?, String?) {
        for key in index.keys {
            if case .expression(let definition) = key, definition.parsed == nil { return (nil, "unparsable_definition") }
        }
        if let predicate = index.predicate, predicate.parsed == nil { return (nil, "unparsable_definition") }
        do { try StoredExpressionDependencies.validateHosts(index.hosts, environment: environment) }
        catch let error as JedError where error.code == "XX002" { return (nil, "missing_host_function") }
        do { try IndexDependencies.validateZones(index, environment: environment) }
        catch let error as JedError where error.code == "XX002" { return (nil, "missing_loaded_data") }
        do {
            let plan = try StoredIndexPlan.resolve(table: table, index: index, snapshot: snapshot,
                                                  settings: settings, context: freshContext())
            if plan.ordered.types.contains(where: { collationUnavailable($0.collation) }) ||
                plan.touched.contains(where: { collationUnavailable(table.columns[$0].type.collation) }) {
                return (nil, "missing_loaded_data")
            }
            return (plan, nil)
        } catch let error as JedError {
            switch error.code {
            case "XX003": return (nil, "unparsable_definition")
            case "42883": return (nil, "missing_host_function")
            case "XX002": return (nil, "missing_loaded_data")
            default: throw error
            }
        }
    }
    func checkAgreement(_ table: CatalogTable, index: CatalogIndex) throws {
        let (resolved, reason) = try resolveIndex(table, index: index)
        if let reason { skip(index.name, reason); return }
        if tableUnreadable.contains(table.name) { skip(index.name, "table_unreadable"); return }
        guard indexClean[index.name] == true else { skip(index.name, "index_unreadable"); return }
        let plan = resolved!, types = try snapshot.tableTypes(table)
        var found: UInt64 = 0
        try scanRows(root: table.root, types: types) { key, row in
            let entries: [StoredIndexPlan.Entry]
            do {
                let old = plan.ordered
                let ordered = OrderedIndexPlan(index: old.index, keys: old.keys, types: old.types,
                    predicate: old.predicate, touched: old.touched, context: try self.freshContext())
                let fresh = StoredIndexPlan(index: plan.index, ordered: ordered, opclasses: plan.opclasses, ginOpclasses: plan.ginOpclasses)
                entries = try fresh.entries({ row[$0] }, storageKey: key)
            } catch let error as JedError {
                if ["57014", "08006", "58030"].contains(error.code) { throw error }
                try self.add("index_entry_error", index.name, key: key, error.message); return
            }
            for entry in entries {
                try self.poll()
                let present: Bool
                if let ops = plan.opclasses {
                    present = try self.gistHolds(entry, root: self.gistRoots[index.name]!, ops: ops)
                } else if plan.ginOpclasses != nil {
                    guard let store = self.snapshot.indexes[index.name] else { throw JedError.corruption("missing index store") }
                    var cursor = store.cursor(lower: TreeBound(entry.bytes)); defer { cursor.close() }
                    do {
                        if let run = try cursor.next(), run.key.starts(with: entry.term) {
                            present = try GinPostingRun.decode(termLength: entry.term.count, bytes: run.key).contains(entry.storageKey)
                        } else { present = false }
                    } catch let error as JedError where error.code == "XX001" { present = false }
                } else {
                    guard let store = self.snapshot.indexes[index.name] else { throw JedError.corruption("missing index store") }
                    present = try store.get(entry.bytes) != nil
                }
                if present { found += 1 }
                else { try self.add("index_entry_missing", index.name, key: key) }
            }
        }
        if found != indexEntries[index.name] { try add("index_entry_count_mismatch", index.name) }
    }
}
