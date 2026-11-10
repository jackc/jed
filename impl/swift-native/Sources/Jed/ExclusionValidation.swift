/// Exclusion probes are trusted, unmetered constraint maintenance. The ALTER
/// caller alone charges its prescribed pairwise validation comparisons.
internal struct ExclusionValidation {
    struct Probe {
        let prefix: [UInt8]
        let bounds: [GistBound]
        let queries: [GistQuery]
        let values: [SQLValue]
    }
    /// Predicate-TRUE rows participate in ALTER's pair schedule even when a
    /// strict operator's NULL/empty operand makes the actual probe exempt.
    struct Member { let probe: Probe? }
    let table: CatalogTable
    let definition: CatalogExclusion
    let plan: StoredIndexPlan
    let strategies: [GistStrategy]
    let settings: SessionSettings

    init(table: CatalogTable, definition: CatalogExclusion, snapshot: DomainSnapshot,
         settings: SessionSettings, context: ExpressionContext) throws {
        guard let index = table.indexes.first(where: { $0.name == definition.index }) else { throw JedError.corruption("missing exclusion backing index") }
        self.table = table; self.definition = definition
        self.settings = context.errorDetail?.settings ?? settings
        plan = try StoredIndexPlan.resolve(table: table, index: index, snapshot: snapshot, settings: settings, context: context)
        strategies = definition.elements.map { $0.strategy == 0 ? .overlaps : .equal }
    }
    func member(_ value: @escaping (Int) throws -> SQLValue) throws -> Member? {
        var row = plan.ordered.context; row.column = value
        if let predicate = plan.ordered.predicate, try !row.matches(predicate) { return nil }
        var prefix: [UInt8] = [], bounds: [GistBound] = [], queries: [GistQuery] = [], values: [SQLValue] = []
        for ordinal in definition.elements.indices {
            let value = try value(definition.elements[ordinal].column)
            if value.isNull { return Member(probe: nil) }
            values.append(value)
            if let ops = plan.opclasses {
                if definition.elements[ordinal].strategy == 0, case .range(let range) = value, range.isEmpty { return Member(probe: nil) }
                guard let bound = try ops[ordinal].leafBound(value), let query = try ops[ordinal].query(value) else { return Member(probe: nil) }
                bounds.append(bound); queries.append(query)
            } else {
                prefix += try KeyEncoding.encodeSlot(value, type: plan.ordered.types[ordinal], descending: false,
                    nullsFirst: false, collatedKey: row.collatedKey)
            }
        }
        return Member(probe: Probe(prefix: prefix, bounds: bounds, queries: queries, values: values))
    }
    func conflicts(_ a: Probe, _ b: Probe) throws -> Bool {
        guard let ops = plan.opclasses else { return a.prefix == b.prefix }
        for ordinal in ops.indices {
            if try !ops[ordinal].matches(a.bounds[ordinal], query: b.queries[ordinal], strategy: strategies[ordinal], leaf: true) { return false }
        }
        return true
    }
    func existing(_ probe: Probe, snapshot: DomainSnapshot, vacated: Set<[UInt8]>) throws -> [UInt8]? {
        if plan.opclasses != nil {
            guard let tree = snapshot.gistTrees[definition.index] else { throw JedError.corruption("missing exclusion GiST tree") }
            return try tree.search(queries: probe.queries, strategies: strategies).storageKeys.first { !vacated.contains($0) }
        }
        guard let store = snapshot.indexes[definition.index] else { throw JedError.corruption("missing exclusion index store") }
        var cursor = store.cursor(lower: TreeBound(probe.prefix),
            upper: OrderedIndexPlan.successor(probe.prefix).map { TreeBound($0, inclusive: false) })
        defer { cursor.close() }
        while let record = try cursor.next() {
            let key = Array(record.key.dropFirst(probe.prefix.count))
            if !vacated.contains(key) { return key }
        }
        return nil
    }
    func violation(_ probe: Probe, other: [SQLValue]?, alter: Bool = false) -> JedError {
        var error = JedError("23P01", "conflicting key value violates exclusion constraint: \(definition.name)")
        error.tableName = table.name.description; error.constraintName = definition.name.description
        // Diagnostics are unmetered and cannot replace the chosen violation.
        if let other, (try? settings.privileges.require(.select, on: table.name)) != nil {
            func render(_ values: [SQLValue]) throws -> String {
                try values.enumerated().map { ordinal, value in
                    let text = try value.isNull ? SQLText("null") : settings.render(value, type: plan.ordered.types[ordinal])
                    var count = min(64, text.utf8.count)
                    while count < text.utf8.count && count > 0 && text.utf8[count] & 0xc0 == 0x80 { count -= 1 }
                    return SQLText(validatedUTF8: Array(text.utf8.prefix(count))).description + (count < text.utf8.count ? "..." : "")
                }.joined(separator: ", ")
            }
            let names = definition.elements.map { TextFunctions.quoteIdentifier(table.columns[$0.column].name).description }.joined(separator: ", ")
            if let a = try? render(probe.values), let b = try? render(other) {
                error.detail = "Key (\(names))=(\(a)) conflicts with \(alter ? "" : "existing ")key (\(names))=(\(b))."
            }
        }
        return error
    }
    static func validate(_ candidates: [ForeignKeyValidation.Candidate], table: CatalogTable, snapshot: DomainSnapshot,
                         settings: SessionSettings, context: ExpressionContext, vacated: Set<[UInt8]> = []) throws {
        for definition in table.exclusions.sorted(by: { $0.name < $1.name }) {
            let validation = try ExclusionValidation(table: table, definition: definition, snapshot: snapshot, settings: settings, context: context)
            var preceding: [Probe] = []
            for candidate in candidates {
                guard let probe = try validation.member(candidate.value)?.probe else { continue }
                if let key = try validation.existing(probe, snapshot: snapshot, vacated: vacated) {
                    var other: [SQLValue]?
                    if let store = snapshot.tables[table.name], let record = try? store.get(key) {
                        let row = ScanRecord(record, types: store.tree.types, pager: store.tree.pool?.pager)
                        other = try? definition.elements.map { try row.value($0.column) }
                    }
                    throw validation.violation(probe, other: other)
                }
                for other in preceding where try validation.conflicts(probe, other) { throw validation.violation(probe, other: other.values) }
                preceding.append(probe)
            }
        }
    }
}
