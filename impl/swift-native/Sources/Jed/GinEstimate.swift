/// Resident ANALYZE term facts price the same query tree the gather executes.
/// Neither term extraction nor these estimates opens a leaf or invokes SQL.
internal struct GinEstimate {
    let rows: Int64
    let entries: Int64
    let totalEntries: Int64

    func pages(indexNodes: Int64, indexHeight: Int64, totalEntries: Int64? = nil) -> Int64 {
        guard entries > 0, indexNodes > 0 else { return 0 }
        let selected = QueryEstimate.scale(indexNodes, numerator: entries,
            denominator: max(1, totalEntries ?? self.totalEntries))
        return min(indexNodes, max(indexHeight, selected))
    }

    static func totalEntries(columns: [Int], table: CatalogTable, snapshot: DomainSnapshot) -> Int64 {
        var total: Int64 = 0
        for column in columns {
            guard let facts = facts(column: column, table: table, snapshot: snapshot) else { continue }
            total = saturatingAdd(total, QueryEstimate.scale(table.rowCount,
                numerator: facts.termOccurrences, denominator: facts.analyzed))
        }
        return total
    }

    static func resolve(_ query: GinQuery, column: Int, table: CatalogTable,
                        snapshot: DomainSnapshot) -> GinEstimate? {
        guard let facts = facts(column: column, table: table, snapshot: snapshot) else { return nil }
        let population = table.rowCount
        let nonnull = population - min(population, QueryEstimate.scale(population,
            numerator: facts.nulls, denominator: facts.analyzed))
        let total = QueryEstimate.scale(population, numerator: facts.termOccurrences, denominator: facts.analyzed)
        switch query {
        case .empty: return GinEstimate(rows: 0, entries: 0, totalEntries: total)
        case .fullScan: return nil
        case .gather(let tree):
            func frequency(_ bytes: [UInt8]) -> Int64 {
                guard nonnull > 0 else { return 0 }
                if let term = facts.terms.first(where: { $0.bytes == bytes }) {
                    return min(nonnull, QueryEstimate.scale(population, numerator: term.count, denominator: facts.analyzed))
                }
                if facts.untrackedTermRows == 0 && !facts.stale { return 0 }
                let half = facts.untrackedTermRows / 2 + facts.untrackedTermRows % 2
                return max(1, min(nonnull, QueryEstimate.scale(population, numerator: half, denominator: facts.analyzed)))
            }
            func estimate(_ tree: GinQueryTree) -> (rows: Int64, entries: Int64) {
                switch tree {
                case .term(let bytes): let count = frequency(bytes); return (count, count)
                case .any(let children):
                    var rows: Int64 = 0, entries: Int64 = 0
                    for child in children {
                        let value = estimate(child)
                        rows = saturatingAdd(rows, value.rows); entries = saturatingAdd(entries, value.entries)
                    }
                    return (min(nonnull, rows), entries)
                case .all(let children):
                    var leaves: [(bytes: [UInt8], rows: Int64)] = [], rest: [GinQueryTree] = []
                    for child in children {
                        if case .term(let bytes) = child { leaves.append((bytes, frequency(bytes))) }
                        else { rest.append(child) }
                    }
                    leaves.sort { $0.rows == $1.rows ? $0.bytes.lexicographicallyPrecedes($1.bytes) : $0.rows < $1.rows }
                    var rows: Int64 = 0, entries: Int64 = 0, started = false
                    for leaf in leaves {
                        if !started { rows = leaf.rows; entries = leaf.rows; started = true }
                        else {
                            entries = saturatingAdd(entries, rows)
                            rows = population == 0 ? 0 : QueryEstimate.scale(rows, numerator: leaf.rows, denominator: population)
                        }
                    }
                    for child in rest {
                        let value = estimate(child)
                        if !started { rows = value.rows; entries = value.entries; started = true }
                        else {
                            entries = saturatingAdd(entries, value.entries)
                            rows = population == 0 ? 0 : QueryEstimate.scale(rows, numerator: value.rows, denominator: population)
                        }
                    }
                    return (rows, entries)
                }
            }
            let value = estimate(tree)
            return GinEstimate(rows: value.rows, entries: value.entries, totalEntries: total)
        }
    }

    /// SQL OR arms keep their authored contributions; a fallback arm makes the
    /// whole candidate use the structural estimate. A normalized term tree's
    /// own direct leaves have already been deduplicated by query extraction.
    static func disjunction(_ queries: [GinQuery], column: Int, table: CatalogTable,
                            snapshot: DomainSnapshot) -> GinEstimate? {
        guard let facts = facts(column: column, table: table, snapshot: snapshot), !queries.isEmpty else { return nil }
        var rows: Int64 = 0, entries: Int64 = 0, total: Int64 = 0
        for query in queries {
            guard let value = resolve(query, column: column, table: table, snapshot: snapshot) else { return nil }
            rows = saturatingAdd(rows, value.rows); entries = saturatingAdd(entries, value.entries); total = value.totalEntries
        }
        let nonnull = table.rowCount - min(table.rowCount, QueryEstimate.scale(table.rowCount,
            numerator: facts.nulls, denominator: facts.analyzed))
        return GinEstimate(rows: min(nonnull, rows), entries: entries, totalEntries: total)
    }

    private static func facts(column: Int, table: CatalogTable, snapshot: DomainSnapshot) -> CatalogStatistics? {
        snapshot.catalog.statistics.first { $0.table == table.name && $0.column == column && $0.termEligible && $0.analyzed > 0 }
    }
}
