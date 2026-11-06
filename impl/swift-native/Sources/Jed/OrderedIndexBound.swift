/// A leading equality prefix and optional trailing range over an admitted
/// ordered index definition. Stored predicates and key expressions remain data.
internal struct OrderedIndexBound {
    struct Term {
        let operation: String
        let source: ResolvedExpression
        let predicate: ResolvedExpression
    }
    let definition: OrderedIndexPlan
    let terms: [[Term]]
    let accessPredicate: ResolvedExpression?
    let prefixCount: Int
    let equalCount: Int
    var bounded: Bool { prefixCount > 0 }

    init(definition: OrderedIndexPlan, terms: [[Term]], accessPredicate: ResolvedExpression?,
         prefixCount: Int, equalCount: Int) {
        self.definition = definition; self.terms = terms; self.accessPredicate = accessPredicate
        self.prefixCount = prefixCount; self.equalCount = equalCount
    }

    init(definition: OrderedIndexPlan, predicate: ResolvedExpression?, indexedJoin: Bool = false) {
        self.definition = definition
        // The frozen ordinary secondary INL path probes one leading scalar
        // column by sibling equality. Expression/range/multi-key probes remain
        // ordinary scan candidates; interval sets have their separate path.
        if indexedJoin && (!definition.index.keys.allSatisfy({ if case .column = $0 { true } else { false } }) ||
            !definition.types.allSatisfy({ if case .scalar = $0.type { true } else { false } })) {
            terms = []; accessPredicate = nil; prefixCount = 0; equalCount = 0
            return
        }
        let conjuncts = QueryEstimate.conjuncts(predicate)
        var selected: [[Term]] = [], admitted: [ResolvedExpression] = [], equalCount = 0
        func source(_ expression: ResolvedExpression) -> ResolvedExpression? {
            switch expression.node {
            case .constant, .parameter: return expression
            default: return BaseScanPlan.outerSource(expression)
            }
        }
        for ordinal in definition.keys.indices {
            if indexedJoin && ordinal > 0 { break }
            let key = definition.keys[ordinal], type = definition.types[ordinal]
            var found: [Term] = []
            for predicate in conjuncts {
                guard case .binary(let operation, let lhs, let rhs, let comparison) = predicate.node,
                      ["=", "<", "<=", ">", ">="].contains(operation) else { continue }
                let value: ResolvedExpression, op: String
                if Self.matches(key, lhs), let input = source(rhs) { value = input; op = operation }
                else if Self.matches(key, rhs), let input = source(lhs) {
                    value = input; op = Self.reversed(operation)
                } else { continue }
                let keyCollation = type.collation?.description ?? "C"
                let expressionEquality: Bool
                if case .expression = definition.index.keys[ordinal] {
                    expressionEquality = op == "=" && keyCollation == "C"
                } else { expressionEquality = false }
                // Persisted text expressions keep the earlier C-keyed rule.
                // Equality remains byte identity under every query collation;
                // an ordering predicate must agree with the stored key order.
                guard (comparison.collation?.description ?? "C") == keyCollation || expressionEquality else { continue }
                let compatible = value.type == nil || value.type?.type == type.type || {
                    guard case .scalar(let a) = value.type?.type, case .scalar(let b) = type.type else { return false }
                    return a.integerWidth != nil && b.integerWidth != nil || BaseScanPlan.promotedOuter(value, key: type)
                }()
                if compatible { found.append(Term(operation: op, source: value, predicate: predicate)) }
            }
            guard !found.isEmpty else { break }
            if indexedJoin {
                guard found.contains(where: {
                    if case .outerColumn = $0.source.node { return $0.operation == "=" }
                    return false
                }) else { break }
            }
            if found.contains(where: { $0.operation == "=" }) {
                let equalities = found.filter { $0.operation == "=" }
                selected.append(equalities); admitted += equalities.map(\.predicate); equalCount += 1
            } else {
                selected.append(found); admitted += found.map(\.predicate); break
            }
        }
        terms = selected; prefixCount = selected.count; self.equalCount = equalCount
        accessPredicate = QueryEstimate.and(admitted)
    }
    private static func matches(_ key: ResolvedExpression, _ operand: ResolvedExpression) -> Bool {
        if case .column(let ordinal) = key.node {
            var expression = operand
            while case .convert(let child, _) = expression.node { expression = child }
            if case .column(let column) = expression.node { return ordinal == column }
            return false
        }
        return ExpressionIdentity.sameIndexKey(key, operand)
    }
    private static func reversed(_ op: String) -> String {
        ["<": ">", "<=": ">=", ">": "<", ">=": "<="][op] ?? op
    }
    func bind(_ context: ExpressionContext) throws -> ScanAccess {
        guard bounded else { return .full }
        var prefix: [UInt8] = []
        func value(_ source: ResolvedExpression) throws -> SQLValue {
            switch source.node {
            case .constant(let value): return value
            case .parameter(let index):
                guard context.parameters.indices.contains(index - 1) else { throw JedError("42P02", "there is no parameter $\(index)") }
                return context.parameters[index - 1]
            case .outerColumn: return try context.evaluate(source)
            default: throw JedError("XX000", "invalid index bound operand")
            }
        }
        for ordinal in terms.indices {
            let type = definition.types[ordinal]
            let option = definition.index.keyOptions.isEmpty ? 0 : definition.index.keyOptions[ordinal]
            let descending = option & 1 != 0, nullsFirst = option & 2 != 0
            let present: UInt8 = descending ? (nullsFirst ? 255 : 254) : (nullsFirst ? 1 : 0)
            var lower = TreeBound(prefix + [present])
            var upper = OrderedIndexPlan.successor(prefix + [present]).map { TreeBound($0, inclusive: false) }
            var equality: [UInt8]?
            for term in terms[ordinal] {
                let input = try value(term.source)
                if input.isNull { return .empty }
                guard let input = IntervalPlan.pointValue(input, type: type) else {
                    if term.operation == "=" { return .empty }
                    continue
                }
                let encoded: [UInt8]
                do {
                    encoded = try KeyEncoding.encodeSlot(input, type: type, descending: descending,
                        nullsFirst: nullsFirst, collatedKey: context.collatedKey)
                } catch let error as JedError where error.code == "0A000" &&
                    type.collation != nil && type.collation?.description != "C" {
                    // An unmapped probe cannot match a stored collated key.
                    // A range remains a sound superset; its residual owns any
                    // comparison failure on a row actually visited.
                    if term.operation == "=" { return .empty }
                    continue
                }
                let endpoint = prefix + encoded
                let operation = descending ? Self.reversed(term.operation) : term.operation
                switch operation {
                case "=":
                    if let equality, equality != encoded { return .empty }
                    equality = encoded
                case ">", ">=":
                    let key: [UInt8]
                    if operation == ">" {
                        guard let successor = OrderedIndexPlan.successor(endpoint) else { return .empty }
                        key = successor
                    } else { key = endpoint }
                    if lower.key.lexicographicallyPrecedes(key) { lower = TreeBound(key) }
                case "<", "<=":
                    let key = operation == "<" ? endpoint : OrderedIndexPlan.successor(endpoint)
                    if let key, upper == nil || key.lexicographicallyPrecedes(upper!.key) { upper = TreeBound(key, inclusive: false) }
                default: break
                }
            }
            if let equality {
                let key = prefix + equality
                if key.lexicographicallyPrecedes(lower.key) || upper.map({ !key.lexicographicallyPrecedes($0.key) }) == true { return .empty }
                prefix += equality
                continue
            }
            if let upper, !lower.key.lexicographicallyPrecedes(upper.key) { return .empty }
            return ScanAccess(lower: lower, upper: upper, isEmpty: false)
        }
        return ScanAccess(lower: TreeBound(prefix), upper: OrderedIndexPlan.successor(prefix).map { TreeBound($0, inclusive: false) }, isEmpty: false)
    }
}
