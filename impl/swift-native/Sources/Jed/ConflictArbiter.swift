/// ON CONFLICT chooses one arbiter at resolution. Its probes are unmetered
/// validation against committed rows plus the caller's sequential overlay.
internal enum ConflictArbiter {
    case primary, index(Int), exclusion(Int), all

    static func resolve(_ syntax: SQLConflict, relation: MutationExecutor.Relation,
                        indexes: [StoredIndexPlan], exclusions: [ExclusionValidation],
                        resolver: ExpressionResolver) throws -> ConflictArbiter {
        if let name = syntax.constraint {
            guard relation.view == nil else { throw JedError("42704", "constraint does not exist on view: \(name)") }
            let primaryName = CatalogSQL.objectName(relation.table.name, label: "pkey")
            if !relation.table.primaryKey.isEmpty, name == primaryName { return .primary }
            if let index = indexes.firstIndex(where: { $0.index.name == name && $0.index.unique && $0.index.isConstraint }) { return .index(index) }
            if let exclusion = exclusions.firstIndex(where: { $0.definition.name == name }) {
                guard syntax.doNothing else { throw JedError("42809", "ON CONFLICT DO UPDATE not supported with exclusion constraints") }
                return .exclusion(exclusion)
            }
            throw JedError("42704", "constraint does not exist: \(name)")
        }
        if syntax.columns.isEmpty {
            guard syntax.doNothing else { throw JedError("42601", "ON CONFLICT DO UPDATE requires inference specification or constraint name") }
            return .all
        }
        var columns: [Int?] = [], decorated = true
        // Names/decorations are resolved in list order; a readonly view output
        // merely prevents inference after every name has been checked.
        for item in syntax.columns {
            guard item.descending == nil, item.nullsFirst == nil else { throw noMatch() }
            let visible = try relation.view?.column(item.name) ?? relation.column(item.name)
            let column: Int?
            if let view = relation.view { column = try? view.baseColumn(layer: 0, column: visible) }
            else { column = visible }
            columns.append(column)
            if let name = item.collation { _ = try resolver.unicode.resolve(name) }
            var requested: String?
            if let name = item.operatorClass {
                let spelling = name.description == "varchar_ops" ? "text_ops" : name.description
                guard classes.contains(spelling) else { throw JedError("42704", "operator class does not exist for access method btree: \(name)") }
                requested = spelling
            }
            if let column {
                let type = relation.types[column]
                if let name = item.collation {
                    decorated = decorated && ExpressionCollation.collatable(type.type)
                        && (type.collation ?? SQLText(validatedUTF8: [67])) == name
                }
                if let requested { decorated = decorated && opclass(type.type) == requested }
            } else { decorated = false }
        }
        var premise: ResolvedExpression?
        if let predicate = syntax.predicate {
            guard relation.view == nil else { throw JedError("0A000", "partial index inference through a view is not supported") }
            try CatalogExpressionPolicy.validate(predicate, isDefault: false)
            premise = try resolver.resolve(predicate)
        }
        guard decorated, columns.allSatisfy({ $0 != nil }) else { throw noMatch() }
        let wanted = Set(columns.compactMap { $0 })
        if !relation.table.primaryKey.isEmpty, Set(relation.table.primaryKey) == wanted { return .primary }
        var partials: [Int] = []
        for ordinal in indexes.indices where indexes[ordinal].index.unique {
            let index = indexes[ordinal]
            var keyed: [Int] = []
            for key in index.index.keys { if case .column(let column) = key { keyed.append(column) } }
            guard keyed.count == index.index.keys.count, Set(keyed) == wanted else { continue }
            if index.ordered.predicate == nil { return .index(ordinal) }
            partials.append(ordinal)
        }
        if let premise {
            for ordinal in partials {
                if PredicateImplication.implies([premise], predicate: indexes[ordinal].ordered.predicate!) { return .index(ordinal) }
            }
        }
        throw noMatch()
    }
    private static func noMatch() -> JedError { JedError("42P10", "there is no unique or exclusion constraint matching the ON CONFLICT specification") }
    private static let classes: Set<String> = Set(GeneratedSpec.scalars.compactMap {
        if case .text(let value) = $0.metadata["btree_opclass"] { return value }; return nil
    } + ["array_ops", "range_ops", "record_ops"])
    private static func opclass(_ type: SQLType) -> String? {
        switch type {
        case .scalar(let scalar):
            if let fact = GeneratedSpec.scalars.first(where: { $0.id == scalar.name }),
               case .text(let value) = fact.metadata["btree_opclass"] { return value }; return nil
        case .array: return "array_ops"
        case .range: return "range_ops"
        case .composite, .record: return "record_ops"
        }
    }
    func key(_ values: [SQLValue], relation: MutationExecutor.Relation, indexes: [StoredIndexPlan],
             context: ExpressionContext) throws -> [UInt8]? {
        switch self {
        case .primary:
            var bytes: [UInt8] = []
            for column in relation.table.primaryKey {
                if values[column].isNull { return nil }
                bytes += try KeyEncoding.encode(values[column], type: relation.types[column], collatedKey: context.collatedKey)
            }
            return bytes
        case .index(let ordinal):
            guard let prefix = try indexes[ordinal].prefix({ values[$0] }), !prefix.hasNull else { return nil }
            return prefix.bytes
        case .all, .exclusion: return nil
        }
    }
    func existing(_ key: [UInt8], relation: MutationExecutor.Relation, indexes: [StoredIndexPlan],
                  snapshot: DomainSnapshot, vacated: Set<[UInt8]>) throws -> PackedRecord? {
        switch self {
        case .primary: return try vacated.contains(key) ? nil : relation.store.get(key)
        case .index(let ordinal):
            guard let store = snapshot.indexes[indexes[ordinal].index.name] else { throw JedError.corruption("missing arbiter index") }
            var cursor = store.cursor(lower: TreeBound(key), upper: OrderedIndexPlan.successor(key).map { TreeBound($0, inclusive: false) })
            defer { cursor.close() }
            while let record = try cursor.next() {
                let storageKey = Array(record.key.dropFirst(key.count))
                if !vacated.contains(storageKey) { return try relation.store.get(storageKey) }
            }
            return nil
        case .all, .exclusion: return nil
        }
    }
}
