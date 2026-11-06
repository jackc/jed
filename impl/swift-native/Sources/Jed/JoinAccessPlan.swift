/// A selected two-base join keeps the precise access identities used by both
/// execution and EXPLAIN. Column slots remain in authored source order.
internal struct JoinAccessPlan {
    struct Key { let outer: Int; let inner: Int; let type: TypeDescriptor }
    enum Algorithm { case indexed, hash([Key]), nested }
    struct Estimate {
        let outer: PlanEstimate
        let inner: PlanEstimate
        let total: PlanEstimate
        let outerCalls: Int64
    }
    let left: RelationPlan
    let right: RelationPlan
    let kind: String
    let predicate: ResolvedExpression?
    let swapped: Bool
    let outerAccess: ScanCandidate
    let innerAccess: ScanCandidate
    let algorithm: Algorithm
    var outer: RelationPlan { swapped ? right : left }
    var inner: RelationPlan { swapped ? left : right }
    var outerOffset: Int { swapped ? left.columns.count : 0 }
    var innerOffset: Int { swapped ? 0 : left.columns.count }
    var indexed: Bool { if case .indexed = algorithm { return true }; return false }

    static func candidates(_ plan: SelectPlan, parameters: [SQLValue]?,
                           context: ExpressionContext) throws -> [JoinAccessPlan]? {
        guard let relation = plan.relation, case .join(let left, let right, let kind, let predicate) = relation.body,
              ["inner", "cross", "left"].contains(kind), left.lateralColumns.isEmpty, right.lateralColumns.isEmpty,
              case .base = left.body, case .base = right.body else { return nil }
        if kind == "left" {
            // An outer edge is a fixed authored step: retain the established
            // ordinary input preference and first admitted INL path. It does
            // not enter the inner-join cost search or receive its order credit.
            func ordinary(_ input: RelationPlan, offset: Int) throws -> ScanCandidate? {
                let local = QueryEstimate.and(QueryEstimate.conjuncts(plan.completePredicate).compactMap {
                    RelationAccess.local($0, offset: offset, width: input.columns.count, earlier: 0..<0)
                })
                return RelationAccess.legacy(try RelationAccess.inventory(input, predicate: local, context: context))
            }
            guard let outer = try ordinary(left, offset: 0),
                  let ordinaryInner = try ordinary(right, offset: left.columns.count) else { return nil }
            let indexed = try RelationAccess.indexedInventory(right, on: predicate, whereFilter: plan.completePredicate,
                offset: left.columns.count, earlier: Set(0..<left.columns.count), context: context).first
            let algorithm: Algorithm
            if indexed != nil { algorithm = .indexed }
            else if let keys = hashKeys(predicate, left: left, right: right, swapped: false) { algorithm = .hash(keys) }
            else { algorithm = .nested }
            return [JoinAccessPlan(left: left, right: right, kind: kind, predicate: relation.effectiveOn, swapped: false,
                outerAccess: outer, innerAccess: indexed ?? ordinaryInner, algorithm: algorithm)]
        }
        var result: [JoinAccessPlan] = []
        for swapped in [false, true] {
            let outer = swapped ? right : left, inner = swapped ? left : right
            let outerOffset = swapped ? left.columns.count : 0, innerOffset = swapped ? 0 : left.columns.count
            let ordinaryOuter = try RelationAccess.inventory(outer, predicate: QueryEstimate.and(QueryEstimate.conjuncts(plan.completePredicate).compactMap {
                RelationAccess.local($0, offset: outerOffset, width: outer.columns.count, earlier: 0..<0)
            }), context: context)
            let ordinaryInner = try RelationAccess.inventory(inner, predicate: QueryEstimate.and(QueryEstimate.conjuncts(plan.completePredicate).compactMap {
                RelationAccess.local($0, offset: innerOffset, width: inner.columns.count, earlier: 0..<0)
            }), context: context)
            let mapped = (QueryEstimate.conjuncts(predicate) + QueryEstimate.conjuncts(plan.completePredicate)).compactMap {
                RelationAccess.local($0, offset: innerOffset, width: inner.columns.count,
                    earlier: outerOffset..<(outerOffset + outer.columns.count))
            }
            let indexed = try RelationAccess.inventory(inner, predicate: QueryEstimate.and(mapped), allowOuter: true, includeScanFilter: false,
                context: context).filter(RelationAccess.correlated)
            let keys = hashKeys(predicate, left: left, right: right, swapped: swapped)
            for access in ordinaryOuter {
                var alternatives: [(ScanCandidate, Algorithm)] = indexed.map { ($0, .indexed) }
                for candidate in ordinaryInner {
                    if let keys { alternatives.append((candidate, .hash(keys))) }
                    alternatives.append((candidate, .nested))
                }
                alternatives.sort { a, b in
                    if a.0.accessRank != b.0.accessRank { return a.0.accessRank < b.0.accessRank }
                    if a.0.indexName != b.0.indexName { return (a.0.indexName ?? SQLText(validatedUTF8: [])) < (b.0.indexName ?? SQLText(validatedUTF8: [])) }
                    func rank(_ value: Algorithm) -> Int { switch value { case .indexed: 0; case .hash: 1; case .nested: 2 } }
                    return rank(a.1) < rank(b.1)
                }
                for (innerAccess, algorithm) in alternatives {
                    result.append(JoinAccessPlan(left: left, right: right, kind: kind, predicate: relation.effectiveOn, swapped: swapped,
                        outerAccess: access, innerAccess: innerAccess, algorithm: algorithm))
                }
            }
        }
        return result
    }

    func providesOrder(_ plan: SelectPlan) -> Bool {
        guard kind != "left", plan.limit != nil, plan.aggregation == nil, plan.window == nil, !plan.distinct,
              plan.distinctOn == nil, !plan.order.isEmpty, [0, 6].contains(outerAccess.accessRank),
              !indexed || innerAccess.accessRank <= 3, let table = outer.baseTable,
              !table.primaryKey.isEmpty, plan.order.count <= table.primaryKey.count else { return false }
        for (ordinal, term) in plan.order.enumerated() {
            guard !term.descending, case .column(let column) = term.expression.node,
                  column == outerOffset + table.primaryKey[ordinal],
                  let descriptor = outer.columns.first(where: { $0.index == table.primaryKey[ordinal] })?.type,
                  (term.type.collation?.description ?? "C") == (descriptor.collation?.description ?? "C") else { return false }
        }
        return true
    }

    func estimate(plan: SelectPlan, parameters: [SQLValue]?, context: ExpressionContext) throws -> Estimate {
        let a = try RelationAccess.estimate(outerAccess, relation: outer, indexed: false, parameters: parameters, context: context)
        let b = try RelationAccess.estimate(innerAccess, relation: inner, indexed: indexed, parameters: parameters, context: context)
        let model = JoinCardinality(left: left, right: right, parameters: parameters, collatedKey: context.collatedKey, planningProbe: context.planningProbe)
        let pairs = QueryEstimate.multiply(a.rows, b.rows)
        let matchedRows = try indexed ? pairs : model.rows(predicate, input: pairs)
        let fullRows = kind == "left" ? max(a.rows, matchedRows) : matchedRows
        let logicalPairs = QueryEstimate.multiply(a.logicalRows, b.logicalRows)
        let matchedLogical = try indexed ? logicalPairs : model.rows(predicate, input: logicalPairs)
        let logical = kind == "left" ? max(a.logicalRows, matchedLogical) : matchedLogical
        var calls = a.rows, rows = fullRows
        if providesOrder(plan), let limit = plan.limit {
            let target = saturatingAdd(limit, plan.offset)
            let filtered = try model.rows(plan.predicate, input: fullRows)
            if target == 0 { calls = 0; rows = 0 }
            else if filtered > target, fullRows > 0 {
                calls = min(a.rows, QueryEstimate.scale(target, numerator: a.rows, denominator: filtered))
                rows = min(fullRows, QueryEstimate.scale(calls, numerator: fullRows, denominator: max(1, a.rows)))
            }
        }
        let inner: PlanEstimate
        let visited: Int64
        if indexed {
            inner = PlanEstimate(rows: QueryEstimate.multiply(b.rows, calls), cost: QueryEstimate.multiply(b.cost, calls),
                logicalRows: QueryEstimate.multiply(b.logicalRows, calls))
            visited = inner.rows
        } else {
            inner = b
            visited = calls < a.rows ? QueryEstimate.scale(calls, numerator: pairs, denominator: max(1, a.rows)) : pairs
        }
        let tree = ExplainTree(options: SQLExplain(), parameters: parameters, context: context)
        var cost = saturatingAdd(a.cost, inner.cost), invocations = visited
        if case .hash(let keys) = algorithm {
            var build: Int64 = 0, probe: Int64 = 0, framed: Int64 = 0
            for key in keys {
                let left = Self.width(key.outer, type: key.type, relation: outer)
                let right = Self.width(key.inner, type: key.type, relation: self.inner)
                build = saturatingAdd(build, right); probe = saturatingAdd(probe, left)
                framed = saturatingAdd(framed, saturatingAdd(4, min(left, right)))
            }
            // Frozen outer-join estimates include the LEFT row floor in hash
            // verification and ON work; runtime still skips ON for NULL extension.
            let matched = rows
            cost = saturatingAdd(cost, saturatingAdd(QueryEstimate.multiply(b.rows, build),
                saturatingAdd(QueryEstimate.multiply(calls, probe), QueryEstimate.multiply(matched, framed))))
            invocations = matched
        }
        cost = saturatingAdd(cost, try tree.expressionCost(predicate.map { [$0] } ?? [], rows: invocations))
        return Estimate(outer: a, inner: inner, total: PlanEstimate(rows: rows, cost: cost, logicalRows: logical), outerCalls: calls)
    }

    private static func width(_ column: Int, type: TypeDescriptor, relation: RelationPlan) -> Int64 {
        if let width = ValueCodec.fixedWidth(type.type) { return Int64(width) }
        if let table = relation.baseTable, let facts = relation.sourceSnapshot?.catalog.statistics.first(where: {
            $0.table == table.name && $0.column == column
        }), facts.analyzed > facts.nulls {
            return max(1, QueryEstimate.scale(facts.widthSum, numerator: 1, denominator: facts.analyzed - facts.nulls))
        }
        return max(1, PlanCardinality.constant("default_variable_key_bytes"))
    }

    private static func hashKeys(_ predicate: ResolvedExpression?, left: RelationPlan, right: RelationPlan,
                                 swapped: Bool) -> [Key]? {
        var keys: [Key] = []
        let boundary = left.columns.count
        let columns = left.columns.sorted { $0.index < $1.index } + right.columns.sorted { $0.index < $1.index }
        func leaf(_ value: ResolvedExpression) -> Bool {
            switch value.node {
            case .column: return true
            case .constant(let value):
                if case .composite = value { return false }
                return true
            default: return false
            }
        }
        for term in QueryEstimate.conjuncts(predicate) {
            guard case .binary(let op, let a, let b, _) = term.node, ["=", "<>", "!="].contains(op),
                  leaf(a), leaf(b) else { return nil }
            guard case .column(let x) = a.node, case .column(let y) = b.node else { continue }
            if op != "=" || (x < boundary) == (y < boundary) { continue }
            let l = min(x, y), r = max(x, y)
            guard columns.indices.contains(l), columns.indices.contains(r), columns[l].type.type == columns[r].type.type,
                  JoinPlan.keyable(columns[l].type.type) else { continue }
            keys.append(Key(outer: swapped ? r - boundary : l, inner: swapped ? l : r - boundary, type: columns[l].type))
        }
        return keys.isEmpty ? nil : keys
    }
}
