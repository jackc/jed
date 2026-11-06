/// The primary-key candidate consumes a leading equality tuple and optional
/// range on its next member. The complete WHERE remains a residual predicate.
internal struct BaseScanPlan {
    struct Term { let operation: String; let source: ResolvedExpression }
    let keys: [Int]
    let types: [TypeDescriptor]
    let terms: [Int: [Term]]
    let contradiction: Bool
    let predicate: ResolvedExpression?
    let pointSets: [Int: ResolvedSubquery]

    init(predicate: ResolvedExpression?, keys: [Int], types: [TypeDescriptor]) {
        self.keys = keys; self.types = types; self.predicate = predicate
        var terms: [Int: [Term]] = [:], pointSets: [Int: ResolvedSubquery] = [:], contradiction = false
        func constantSource(_ expression: ResolvedExpression) -> ResolvedExpression? {
            switch expression.node {
            case .constant, .parameter: return expression
            default: return Self.outerSource(expression)
            }
        }
        func column(_ expression: ResolvedExpression) -> Int? {
            if case .column(let index) = expression.node { return index }
            if case .convert(let inner, _) = expression.node { return column(inner) }
            return nil
        }
        func visit(_ expression: ResolvedExpression) {
            if case .subquery(let query) = expression.node, !query.correlated, query.rowArity == nil,
               case .membership("=", false, false, let membership, let operand) = query.kind,
               let index = column(operand), keys.count == 1, keys.first == index,
               types.indices.contains(index), pointSets[index] == nil {
                let type = types[index]
                let input = query.plan.columns[0].type?.type
                let numeric: Bool
                if case .scalar(let a) = type.type, case .scalar(let b) = input {
                    numeric = (a.integerWidth != nil || a == .decimal) && (b.integerWidth != nil || b == .decimal)
                } else { numeric = false }
                if (input == nil || input == type.type || numeric),
                   (query.type?.collation?.description ?? "C") == (type.collation?.description ?? "C"),
                   !membership || type.collation == nil || type.collation?.description == "C" {
                    pointSets[index] = query
                }
            }
            if case .constant(let value) = expression.node {
                if value.isNull { contradiction = true; return }
                if case .boolean(false) = value { contradiction = true; return }
            }
            guard case .binary(let operation, let left, let right, let comparison) = expression.node else { return }
            guard ["=", "<", "<=", ">", ">="].contains(operation) else { return }
            let index: Int, source: ResolvedExpression, op: String
            if let found = column(left), let value = constantSource(right) { index = found; source = value; op = operation }
            else if let found = column(right), let value = constantSource(left) {
                index = found; source = value; op = ["<": ">", "<=": ">=", ">": "<", ">=": "<="][operation] ?? operation
            } else { return }
            guard types.indices.contains(index) else { return }
            if ExpressionCollation.collatable(types[index].type),
               (comparison.collation?.description ?? "C") != (types[index].collation?.description ?? "C") { return }
            let compatible = source.type == nil || source.type?.type == types[index].type || {
                if case .scalar(let a) = source.type?.type, case .scalar(let b) = types[index].type {
                    return a.integerWidth != nil && b.integerWidth != nil || Self.promotedOuter(source, key: types[index])
                }
                return false
            }()
            guard compatible else { return }
            terms[index, default: []].append(Term(operation: op, source: source))
        }
        // View/body pushdown can synthesize an AND chain much deeper than
        // authored expression nesting. Visit its leaves without host recursion.
        for term in QueryEstimate.conjuncts(predicate) { visit(term) }
        self.terms = terms; self.contradiction = contradiction || PredicateContradiction.proves(predicate); self.pointSets = pointSets
    }
    /// Implicit integer promotion preserves a sibling/outer value exactly. Keep
    /// explicit casts and converted literals outside the constant-source grammar.
    static func outerSource(_ expression: ResolvedExpression) -> ResolvedExpression? {
        switch expression.node {
        case .outerColumn: return expression
        case .convert(let child, let target):
            guard case .scalar(let source) = child.type?.type, source.integerWidth != nil,
                  case .scalar(let destination) = target.type,
                  destination.integerWidth != nil || destination == .decimal else { return nil }
            return outerSource(child)
        default: return nil
        }
    }
    static func promotedOuter(_ expression: ResolvedExpression, key: TypeDescriptor) -> Bool {
        guard case .outerColumn = expression.node, case .scalar(let source) = expression.type?.type,
              case .scalar(let target) = key.type else { return false }
        return source.integerWidth != nil && target == .decimal || source == .decimal && target.integerWidth != nil
    }

    func bind(_ context: ExpressionContext) throws -> ScanAccess {
        let clip = try contiguous(context)
        guard !clip.isEmpty, keys.count == 1, let column = keys.first,
              let source = pointSets[column], let rows = context.subqueries.values(source) else { return clip }
        var points: Set<[UInt8]> = []
        for row in rows {
            let input = row[0]
            if input.isNull { continue }
            let value: SQLValue
            if case .scalar(let target) = types[column].type, target.integerWidth != nil {
                let number: Int64
                switch input {
                case .integer(let integer): number = integer
                case .decimal(let decimal):
                    guard let integer = try? decimal.integerValue(), decimal.compare(to: SQLDecimal(integer)) == 0 else { continue }
                    number = integer
                default: continue
                }
                guard target.admits(number) else { continue }; value = .integer(number)
            } else if types[column].type == .scalar(.decimal), case .integer(let number) = input { value = .decimal(SQLDecimal(number)) }
            else { value = input }
            let key = try KeyEncoding.encode(value, type: types[column], collatedKey: context.collatedKey)
            if let lower = clip.lower, key.lexicographicallyPrecedes(lower.key) || key == lower.key && !lower.inclusive { continue }
            if let upper = clip.upper, upper.key.lexicographicallyPrecedes(key) || key == upper.key && !upper.inclusive { continue }
            points.insert(key)
        }
        let ordered = points.sorted { $0.lexicographicallyPrecedes($1) }
        var intervals: [ScanInterval] = []
        for key in ordered {
            if ValueCodec.fixedWidth(types[column].type) != nil, let last = intervals.last,
               let upper = last.upper, OrderedIndexPlan.successor(upper.key) == key {
                intervals[intervals.count - 1] = ScanInterval(lower: last.lower, upper: TreeBound(key))
            } else { intervals.append(ScanInterval(lower: TreeBound(key), upper: TreeBound(key))) }
        }
        return ScanAccess(lower: nil, upper: nil, isEmpty: intervals.isEmpty, intervals: intervals)
    }

    func contiguousBound(_ context: ExpressionContext) throws -> ScanAccess { try contiguous(context) }

    private func contiguous(_ context: ExpressionContext) throws -> ScanAccess {
        if contradiction { return .empty }
        if try hasKnownContradiction(parameters: context.parameters, collatedKey: context.collatedKey) { return .empty }
        guard !keys.isEmpty else { return .full }
        var prefix: [UInt8] = [], lower: TreeBound?, upper: TreeBound?, bounded = false
        func value(_ expression: ResolvedExpression) throws -> SQLValue {
            switch expression.node {
            case .constant(let value): return value
            case .parameter(let index): return context.parameters[index - 1]
            case .outerColumn: return try context.evaluate(expression)
            default: throw JedError("XX000", "invalid constant scan operand")
            }
        }
        func encoded(_ term: Term, column: Int) throws -> [UInt8]? {
            let input = try value(term.source)
            guard let value = IntervalPlan.pointValue(input, type: types[column]) else { return nil }
            return try KeyEncoding.encode(value, type: types[column], collatedKey: context.collatedKey)
        }
        func tightenLower(_ bound: TreeBound) {
            if lower == nil || lower!.key.lexicographicallyPrecedes(bound.key) || lower!.key == bound.key && !bound.inclusive { lower = bound }
        }
        func tightenUpper(_ bound: TreeBound) {
            if upper == nil || bound.key.lexicographicallyPrecedes(upper!.key) || upper!.key == bound.key && !bound.inclusive { upper = bound }
        }
        for (ordinal, column) in keys.enumerated() {
            let candidates = terms[column] ?? [], equalities = candidates.filter { $0.operation == "=" }
            if !equalities.isEmpty {
                var agreed: [UInt8]?
                for term in equalities {
                    guard let key = try encoded(term, column: column) else { return .empty }
                    if let agreed, agreed != key { return .empty }
                    agreed = key
                }
                // Every same-member range constrains an equality prefix too.
                // Unencodable range endpoints retain the ordinary widening,
                // while NULL or a contradictory endpoint makes this probe empty.
                for term in candidates where term.operation != "=" {
                    if try value(term.source).isNull { return .empty }
                    guard let endpoint = try encoded(term, column: column) else { continue }
                    let less = agreed!.lexicographicallyPrecedes(endpoint), equal = agreed! == endpoint
                    if term.operation == ">" && (less || equal) || term.operation == ">=" && less ||
                        term.operation == "<" && !less || term.operation == "<=" && !less && !equal { return .empty }
                }
                prefix += agreed!; bounded = true
                if ordinal + 1 == keys.count { return ScanAccess(lower: TreeBound(prefix), upper: TreeBound(prefix), isEmpty: false) }
                continue
            }
            if !prefix.isEmpty {
                lower = TreeBound(prefix)
                upper = OrderedIndexPlan.successor(prefix).map { TreeBound($0, inclusive: false) }
            }
            for term in candidates {
                if try value(term.source).isNull { return .empty }
                guard let encoded = try encoded(term, column: column) else { continue }
                let endpoint = prefix + encoded; bounded = true
                switch term.operation {
                case ">=": tightenLower(TreeBound(endpoint))
                case ">":
                    guard let successor = OrderedIndexPlan.successor(endpoint) else { return .empty }
                    tightenLower(TreeBound(successor))
                case "<": tightenUpper(TreeBound(endpoint, inclusive: false))
                case "<=": if let successor = OrderedIndexPlan.successor(endpoint) { tightenUpper(TreeBound(successor, inclusive: false)) }
                default: break
                }
            }
            break
        }
        if !bounded { return .full }
        if let lower, let upper, upper.key.lexicographicallyPrecedes(lower.key) ||
            (upper.key == lower.key && (!lower.inclusive || !upper.inclusive)) { return .empty }
        return ScanAccess(lower: lower, upper: upper, isEmpty: false)
    }

    /// Resolve only already-known endpoint values. Generic EXPLAIN parameters
    /// and correlated operands remain unknown, without evaluating expressions.
    func hasKnownContradiction(parameters: [SQLValue]?, collatedKey: KeyEncoding.CollatedKey?, planningProbe: PlanningProbe? = nil) throws -> Bool {
        PredicateContradiction.proves(predicate, parameters: parameters, planningProbe: planningProbe)
    }
}

internal struct ScanAccess {
    let lower: TreeBound?
    let upper: TreeBound?
    let isEmpty: Bool
    var intervals: [ScanInterval]? = nil
    static let full = ScanAccess(lower: nil, upper: nil, isEmpty: false)
    static let empty = ScanAccess(lower: nil, upper: nil, isEmpty: true)
    func cursor(_ store: TableStore, descending: Bool = false) -> ScanCursor {
        ScanCursor(store: store, intervals: isEmpty ? [] : intervals ?? [ScanInterval(lower: lower, upper: upper)], descending: descending)
    }
    func pages(_ tree: BPlusTree) -> Int64 {
        guard !isEmpty, tree.root != nil else { return 0 }
        if let intervals {
            return intervals.reduce(0) { saturatingAdd($0, ScanAccess(lower: $1.lower, upper: $1.upper, isEmpty: false).pages(tree)) }
        }
        return Int64(tree.overlapNodeCount(lower: lower, upper: upper))
    }
}

internal struct ScanInterval { let lower: TreeBound?; let upper: TreeBound? }

/// Interval scans open one bounded packed-tree cursor at a time. The intervals
/// are disjoint and canonical, so no row-dedup buffer is needed here.
internal struct ScanCursor {
    private let store: TableStore
    private let intervals: [ScanInterval]
    private let descending: Bool
    private var position = 0
    private var current: TreeCursor?
    init(store: TableStore, intervals: [ScanInterval], descending: Bool) {
        self.store = store; self.intervals = descending ? Array(intervals.reversed()) : intervals; self.descending = descending
    }
    mutating func next() throws -> PackedRecord? {
        try nextPosition()?.record()
    }
    mutating func nextPosition() throws -> TreePosition? {
        while true {
            if current == nil {
                guard position < intervals.count else { return nil }
                let bounds = intervals[position]; position += 1
                current = store.cursor(lower: bounds.lower, upper: bounds.upper, descending: descending)
            }
            if let row = try current?.nextPosition() { return row }
            current?.close(); current = nil
        }
    }
    mutating func close() { current?.close(); current = nil; position = intervals.count }
}
