/// Constraint checks replay readable trees in storage-key order. Each pass
/// retains only one row (or the previous unique prefix), independently of size.
extension IntegrityChecker {
    private enum NamedConstraint {
        case check(CatalogCheck), unique(CatalogIndex), foreign(CatalogForeignKey)
        var name: SQLText {
            switch self { case .check(let x): x.name; case .unique(let x): x.name; case .foreign(let x): x.name }
        }
        var rank: Int {
            switch self { case .check: 0; case .unique: 1; case .foreign: 2 }
        }
    }

    func checkConstraints(_ table: CatalogTable) throws {
        var named = table.checks.map(NamedConstraint.check)
        named += table.indexes.filter { $0.kind == 0 && $0.unique }.map(NamedConstraint.unique)
        named += table.foreignKeys.map(NamedConstraint.foreign)
        named.sort { $0.name == $1.name ? $0.rank < $1.rank : $0.name < $1.name }
        if tableUnreadable.contains(table.name) {
            skip(table.name, "table_unreadable")
            for constraint in named { skip(constraint.name, "table_unreadable") }
            return
        }
        let types = try snapshot.tableTypes(table)
        try checkColumns(table, types: types)
        for constraint in named {
            switch constraint {
            case .check(let definition): try checkCheck(table, types: types, definition: definition)
            case .unique(let index): try checkUnique(table, types: types, index: index)
            case .foreign(let foreign): try checkForeignKey(table, types: types, foreign: foreign)
            }
        }
    }

    private func checkColumns(_ table: CatalogTable, types: [TypeDescriptor]) throws {
        let modified = types.map { descriptor in
            guard case .scalar = descriptor.type else { return false }
            return descriptor.decimal != nil || descriptor.textLimit != nil || descriptor.temporal != nil
        }
        try scanRows(root: table.root, types: types) { key, row in
            for ordinal in table.columns.indices {
                let column = table.columns[ordinal], value = row[ordinal]
                if value.isNull {
                    if column.notNull || table.primaryKey.contains(ordinal) {
                        try self.add("not_null_violation", table.name, key: key, "column \(column.name) is NULL")
                    }
                    continue
                }
                guard modified[ordinal] else { continue }
                let type = types[ordinal]
                var detail = "column \(column.name) is changed by its typmod"
                do {
                    let stored = try ScalarCasts.applyTypmod(value, descriptor: type, explicit: false)
                    // Same-type non-NULL bodies compare exact stored bytes.
                    // Raw text avoids the inline u16 cap for overflow values.
                    if try ValueCodec.encodeRawBody(value, type: type) == ValueCodec.encodeRawBody(stored, type: type) { continue }
                } catch let error as JedError {
                    detail = "column \(column.name): \(error)"
                }
                try self.add("typmod_violation", table.name, key: key, detail)
            }
        }
    }

    private func resolveCheck(_ table: CatalogTable, types: [TypeDescriptor], definition: CatalogCheck) throws -> (ResolvedExpression?, String?) {
        guard let syntax = definition.expression.parsed else { return (nil, "unparsable_definition") }
        do { try StoredExpressionDependencies.validateHosts(definition.hosts, environment: environment) }
        catch let error as JedError where error.code == "XX002" { return (nil, "missing_host_function") }
        let context = try freshContext()
        let columns = table.columns.enumerated().map {
            ExpressionColumn(name: $0.element.name, qualifier: table.name, type: types[$0.offset], index: $0.offset)
        }
        let resolver = ExpressionResolver(columns: columns, composites: snapshot.types, scalar: context.scalar)
        resolver.hostFunctions = environment.signatures; resolver.unicode = environment.unicode
        resolver.resolveAggregate = AggregateQueryPlan.forbidden
        let expression: ResolvedExpression
        do { expression = try resolver.resolve(syntax, expected: TypeDescriptor(.scalar(.boolean))) }
        catch let error as JedError {
            switch error.code {
            case "XX003": return (nil, "unparsable_definition")
            case "42883": return (nil, "missing_host_function")
            case "XX002": return (nil, "missing_loaded_data")
            default: throw error
            }
        }
        do { try StoredExpressionDependencies.validateHosts(definition.hosts, expressions: [expression], environment: environment) }
        catch let error as JedError where error.code == "XX002" { return (nil, "missing_host_function") }
        var unavailable = false
        StoredExpressionDependencies.walk([expression]) { node in
            if case .column(let ordinal) = node.node, table.columns.indices.contains(ordinal),
               self.collationUnavailable(table.columns[ordinal].type.collation) { unavailable = true }
        }
        return unavailable ? (nil, "missing_loaded_data") : (expression, nil)
    }

    private func checkCheck(_ table: CatalogTable, types: [TypeDescriptor], definition: CatalogCheck) throws {
        let (expression, reason) = try resolveCheck(table, types: types, definition: definition)
        if let reason { skip(definition.name, reason); return }
        guard let expression else { throw JedError.corruption("integrity CHECK resolution produced no expression") }
        try scanRows(root: table.root, types: types) { key, row in
            var context = try self.freshContext()
            context.column = { row[$0] }
            let value: SQLValue
            do { value = try context.evaluate(expression) }
            catch let error as JedError {
                if ["57014", "08006", "58030"].contains(error.code) { throw error }
                try self.add("check_error", definition.name, key: key, String(describing: error)); return
            }
            if case .boolean(false) = value {
                try self.add("check_violation", definition.name, key: key, "the row fails the check")
            }
        }
    }

    private func uniqueTypes(_ table: CatalogTable, types: [TypeDescriptor], index: CatalogIndex) throws -> ([TypeDescriptor], String?) {
        var resolved: StoredIndexPlan?, result: [TypeDescriptor] = []
        for (ordinal, key) in index.keys.enumerated() {
            switch key {
            case .column(let column): result.append(types[column])
            case .expression:
                if resolved == nil {
                    let (plan, reason) = try resolveIndex(table, index: index)
                    if let reason { return ([], reason) }
                    guard let plan else { throw JedError.corruption("integrity index resolution produced no plan") }
                    resolved = plan
                }
                result.append(resolved!.ordered.types[ordinal])
            }
        }
        return (result, nil)
    }

    private func checkUnique(_ table: CatalogTable, types: [TypeDescriptor], index: CatalogIndex) throws {
        let (slots, reason) = try uniqueTypes(table, types: types, index: index)
        if let reason { skip(index.name, reason); return }
        guard indexClean[index.name] == true else { skip(index.name, "index_unreadable"); return }
        var previous: [UInt8]?
        try scanRows(root: index.root, types: []) { entry, _ in
            let split: (count: Int, hasNull: Bool)
            do { split = try IndexKeyBoundary.prefix(entry, types: slots, options: index.keyOptions) }
            catch let error as JedError where error.code == "XX001" {
                previous = nil; return
            }
            if split.hasNull { previous = nil; return }
            let prefix = Array(entry.prefix(split.count))
            if previous == prefix {
                try self.add("unique_violation", index.name, key: Array(entry.dropFirst(split.count)), "the key repeats the previous entry's")
            }
            previous = prefix
        }
    }

    private func checkForeignKey(_ table: CatalogTable, types: [TypeDescriptor], foreign: CatalogForeignKey) throws {
        let plan = try ForeignKeyValidation.Plan(child: table, foreign: foreign, snapshot: snapshot)
        for column in foreign.referencedColumns where collationUnavailable(plan.parent.columns[column].type.collation) {
            skip(foreign.name, "missing_loaded_data"); return
        }
        if tableUnreadable.contains(plan.parent.name) { skip(foreign.name, "table_unreadable"); return }
        if let index = plan.index, indexClean[index.name] != true { skip(foreign.name, "index_unreadable"); return }
        let context = try freshContext()
        try scanRows(root: table.root, types: types) { key, row in
            let nulls = foreign.columns.reduce(0) { $0 + (row[$1].isNull ? 1 : 0) }
            if nulls == foreign.columns.count || (nulls > 0 && !foreign.matchFull) { return }
            var detail = "no referenced row"
            if nulls > 0 { detail = "MATCH FULL key is partly NULL" }
            else {
                let probe: ForeignKeyValidation.Probe
                do { probe = try plan.childProbe({ row[$0] }, context: context) }
                catch let error as JedError {
                    if ["57014", "08006", "58030"].contains(error.code) { throw error }
                    try self.add("foreign_key_violation", foreign.name, key: key, "the key does not encode: \(error)"); return
                }
                switch probe {
                case .exempt: return
                case .missing: break
                case .key(let parentKey):
                    if try plan.parentExists(parentKey, snapshot: self.snapshot, context: context) { return }
                }
            }
            try self.add("foreign_key_violation", foreign.name, key: key, detail)
        }
    }
}
