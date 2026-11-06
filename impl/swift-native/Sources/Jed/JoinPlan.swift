/// Physical roles are independent of logical output slots. The initial base
/// candidate inventory contains full scans, PK probes, hash, and nested loop.
internal struct JoinPlan {
    struct Key { let left: Int; let right: Int; let type: TypeDescriptor }
    enum Algorithm { case nested, hash([Key]), indexed(BaseScanPlan) }
    let swapped: Bool
    let algorithm: Algorithm
    let estimate: Int64

    static func choose(left: RelationPlan, right: RelationPlan, kind: String, predicate: ResolvedExpression?) -> JoinPlan {
        let fallback = JoinPlan(swapped: false, algorithm: .nested, estimate: 0)
        guard kind == "inner" || kind == "cross" || kind == "left" else { return fallback }
        let boundary = left.columns.count
        let keyPairs = hashKeys(predicate, columns: left.columns.sorted { $0.index < $1.index } + right.columns.sorted { $0.index < $1.index }, boundary: boundary)
        guard case .base(let leftStore) = left.body, case .base(let rightStore) = right.body,
              let a = left.baseTable, let b = right.baseTable else {
            // The established two-relation path predates costed base-table
            // enumeration. Its authored right input is the build side, even
            // when either relation is a CTE, derived query or resident SRF.
            // Wider join prefixes and lateral/outer barriers keep their lane.
            func eligible(_ input: RelationPlan) -> Bool {
                guard input.lateralColumns.isEmpty else { return false }
                switch input.body {
                case .join, .lateralDerived: return false
                default: return true
                }
            }
            if (kind == "inner" || kind == "left"), eligible(left), eligible(right), let keys = keyPairs {
                return JoinPlan(swapped: false, algorithm: .hash(keys), estimate: 0)
            }
            return fallback
        }
        let stores = [leftStore, rightStore], tables = [a, b]
        let cartesian = QueryEstimate.multiply(a.rowCount, b.rowCount)
        let outputRows = QueryEstimate.rows(predicate, input: cartesian)
        let nodes = QueryEstimate.expression(predicate)
        let scans = saturatingAdd(saturatingAdd(a.rowCount, b.rowCount), Int64(leftStore.tree.nodeCount + rightStore.tree.nodeCount))
        var winner: JoinPlan?
        // Source order precedes access and algorithm ranks on ties.
        for swapped in kind == "left" ? [false] : [false, true] {
            let outer = swapped ? 1 : 0, inner = 1 - outer
            let outerRows = tables[outer].rowCount, innerRows = tables[inner].rowCount
            let offset = inner == 0 ? 0 : boundary
            let bound = (inner == 0 ? left : right).allowsIndexAccess
                ? indexed(predicate, table: tables[inner], types: stores[inner].tree.types,
                          innerOffset: offset, innerWidth: inner == 0 ? boundary : right.columns.count) : nil
            var candidates: [JoinPlan] = []
            if let bound {
                let unique = !tables[inner].primaryKey.isEmpty && tables[inner].primaryKey.allSatisfy { key in
                    bound.terms[key]?.contains(where: { $0.operation == "=" }) ?? false
                }
                var selected = innerRows, pages = Int64(stores[inner].tree.nodeCount)
                if unique { selected = min(1, selected); pages = selected == 0 ? 0 : Int64(stores[inner].tree.height) }
                else {
                    for key in tables[inner].primaryKey {
                        guard let terms = bound.terms[key], !terms.isEmpty else { break }
                        let equality = terms.contains { $0.operation == "=" }
                        selected = QueryEstimate.fraction(equality ? "equality" : "inequality", rows: selected)
                        pages = QueryEstimate.fraction(equality ? "equality" : "inequality", rows: pages)
                        if !equality { break }
                    }
                    pages = selected == 0 ? 0 : min(Int64(stores[inner].tree.nodeCount), max(pages, Int64(stores[inner].tree.height)))
                }
                let perProbe = saturatingAdd(pages, QueryEstimate.multiply(selected, saturatingAdd(1, nodes)))
                let cost = saturatingAdd(saturatingAdd(outerRows, Int64(stores[outer].tree.nodeCount)), QueryEstimate.multiply(outerRows, perProbe))
                candidates.append(JoinPlan(swapped: swapped, algorithm: .indexed(bound), estimate: cost))
            }
            if let keys = keyPairs {
                let width = keys.reduce(Int64(0)) { saturatingAdd($0, Int64(max(1, ValueCodec.fixedWidth($1.type.type) ?? 1))) }
                let framed = saturatingAdd(width, Int64(keys.count * 4))
                let hashes = QueryEstimate.multiply(saturatingAdd(outerRows, innerRows), width)
                let checks = QueryEstimate.multiply(outputRows, saturatingAdd(framed, nodes))
                candidates.append(JoinPlan(swapped: swapped, algorithm: .hash(keys), estimate: saturatingAdd(scans, saturatingAdd(hashes, checks))))
            }
            candidates.append(JoinPlan(swapped: swapped, algorithm: .nested,
                estimate: saturatingAdd(scans, QueryEstimate.multiply(cartesian, nodes))))
            if kind == "left" {
                // The outer-join fence retains its authored legacy preference.
                return candidates[0]
            }
            for candidate in candidates where winner == nil || candidate.estimate < winner!.estimate { winner = candidate }
        }
        return winner ?? fallback
    }

    static func indexed(_ predicate: ResolvedExpression?, table: CatalogTable, types: [TypeDescriptor],
                                innerOffset: Int, innerWidth: Int) -> BaseScanPlan? {
        func local(_ index: Int) -> Bool { index >= innerOffset && index < innerOffset + innerWidth }
        var bounds: [ResolvedExpression] = []
        for term in QueryEstimate.conjuncts(predicate) {
            guard case .binary(let op, let a, let b, let type) = term.node, ["=", "<", "<=", ">", ">="].contains(op),
                  case .column(let l) = a.node, case .column(let r) = b.node, local(l) != local(r) else { continue }
            func mapped(_ value: ResolvedExpression, _ index: Int) -> ResolvedExpression {
                var result = value
                result.node = local(index) ? .column(index - innerOffset) : .outerColumn(OuterColumnReference(depth: 1, index: index))
                return result
            }
            bounds.append(ResolvedExpression(type: term.type, node: .binary(op, mapped(a, l), mapped(b, r), type), position: term.position))
        }
        let bound = BaseScanPlan(predicate: QueryEstimate.and(bounds), keys: table.primaryKey, types: types)
        guard let first = table.primaryKey.first, bound.terms[first] != nil else { return nil }
        return bound
    }

    static func keyable(_ type: SQLType) -> Bool {
        switch type {
        case .scalar(let scalar): return ![.json, .jsonb, .jsonpath].contains(scalar)
        case .array(.scalar(let scalar)), .range(.scalar(let scalar)): return ![.json, .jsonb, .jsonpath].contains(scalar)
        default: return false
        }
    }

    private static func hashKeys(_ predicate: ResolvedExpression?, columns: [ExpressionColumn], boundary: Int) -> [Key]? {
        var keys: [Key] = []
        for term in QueryEstimate.conjuncts(predicate) {
            guard case .binary(let op, let a, let b, _) = term.node, ["=", "<>", "<", "<=", ">", ">="].contains(op),
                  case .column(let left) = a.node, case .column(let right) = b.node else { return nil }
            if op != "=" || (left < boundary) == (right < boundary) { continue }
            let l = min(left, right), r = max(left, right)
            guard columns.indices.contains(l), columns.indices.contains(r), columns[l].type.type == columns[r].type.type else { return nil }
            let type = columns[l].type
            guard keyable(type.type) else { return nil }
            keys.append(Key(left: l, right: r - boundary, type: type))
        }
        return keys.isEmpty ? nil : keys
    }
}
