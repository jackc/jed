/// The primary-key candidate consumes a leading equality tuple and optional
/// range on its next member. The complete WHERE remains a residual predicate.
internal struct BaseScanPlan {
    struct Term { let operation: String; let source: ResolvedExpression }
    let keys: [Int]
    let types: [TypeDescriptor]
    let terms: [Int: [Term]]
    let contradiction: Bool
    let pointSets: [Int: ResolvedSubquery]

    init(predicate: ResolvedExpression?, keys: [Int], types: [TypeDescriptor]) {
        self.keys = keys; self.types = types
        var terms: [Int: [Term]] = [:], pointSets: [Int: ResolvedSubquery] = [:], contradiction = false
        func constant(_ expression: ResolvedExpression) -> Bool {
            switch expression.node {
            case .constant, .parameter, .outerColumn: return true
            default: return false
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
               let index = column(operand), types.indices.contains(index), pointSets[index] == nil {
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
            if operation == "and" { visit(left); visit(right); return }
            guard ["=", "<", "<=", ">", ">="].contains(operation) else { return }
            let index: Int, source: ResolvedExpression, op: String
            if let found = column(left), constant(right) { index = found; source = right; op = operation }
            else if let found = column(right), constant(left) {
                index = found; source = left; op = ["<": ">", "<=": ">=", ">": "<", ">=": "<="][operation] ?? operation
            } else { return }
            guard types.indices.contains(index) else { return }
            if ExpressionCollation.collatable(types[index].type),
               (comparison.collation?.description ?? "C") != (types[index].collation?.description ?? "C") { return }
            let compatible = source.type == nil || source.type?.type == types[index].type || {
                if case .scalar(let a) = source.type?.type, case .scalar(let b) = types[index].type {
                    return a.integerWidth != nil && b.integerWidth != nil
                }
                return false
            }()
            guard compatible else { return }
            terms[index, default: []].append(Term(operation: op, source: source))
        }
        if let predicate { visit(predicate) }
        self.terms = terms; self.contradiction = contradiction; self.pointSets = pointSets
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

    private func contiguous(_ context: ExpressionContext) throws -> ScanAccess {
        if contradiction { return .empty }
        if try contradicts(context) { return .empty }
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
            let value = try value(term.source)
            if value.isNull { return nil }
            if case .integer(let n) = value, case .scalar(let type) = types[column].type,
               type.integerWidth != nil, !type.admits(n) { return nil }
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

    private func contradicts(_ context: ExpressionContext) throws -> Bool {
        for column in terms.keys.sorted() {
            var equal: [UInt8]?, lower: TreeBound?, upper: TreeBound?
            for term in terms[column]! {
                let value: SQLValue
                switch term.source.node {
                case .constant(let constant): value = constant
                case .parameter(let index): value = context.parameters[index - 1]
                case .outerColumn: continue
                default: continue
                }
                if value.isNull { return true }
                // An endpoint that cannot encode in the column's own domain
                // cannot establish this contradiction; ordinary evaluation or
                // the access candidate still handles it.
                if case .integer(let number) = value, case .scalar(let type) = types[column].type,
                   type.integerWidth != nil, !type.admits(number) { continue }
                guard let key = try? KeyEncoding.encode(value, type: types[column], collatedKey: context.collatedKey) else { continue }
                switch term.operation {
                case "=":
                    if let equal, equal != key { return true }; equal = key
                case ">", ">=":
                    let bound = TreeBound(key, inclusive: term.operation == ">=")
                    if lower == nil || lower!.key.lexicographicallyPrecedes(key) || lower!.key == key && !bound.inclusive { lower = bound }
                case "<", "<=":
                    let bound = TreeBound(key, inclusive: term.operation == "<=")
                    if upper == nil || key.lexicographicallyPrecedes(upper!.key) || upper!.key == key && !bound.inclusive { upper = bound }
                default: break
                }
            }
            if let lower, let upper, upper.key.lexicographicallyPrecedes(lower.key) ||
                upper.key == lower.key && (!lower.inclusive || !upper.inclusive) { return true }
            if let equal {
                if let lower, equal.lexicographicallyPrecedes(lower.key) || equal == lower.key && !lower.inclusive { return true }
                if let upper, upper.key.lexicographicallyPrecedes(equal) || equal == upper.key && !upper.inclusive { return true }
            }
        }
        return false
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
        guard !isEmpty, let root = tree.root else { return 0 }
        if let intervals {
            return intervals.reduce(0) { saturatingAdd($0, ScanAccess(lower: $1.lower, upper: $1.upper, isEmpty: false).pages(tree)) }
        }
        if lower == nil && upper == nil { return Int64(tree.nodeCount) }
        func overlaps(_ low: [UInt8]?, _ high: [UInt8]?) -> Bool {
            if let lower, let high, !lower.key.lexicographicallyPrecedes(high) { return false }
            if let upper, let low, upper.key.lexicographicallyPrecedes(low) || (upper.key == low && !upper.inclusive) { return false }
            return true
        }
        func visit(_ node: TreeNode, low: [UInt8]?, high: [UInt8]?) -> Int64 {
            if !overlaps(low, high) { return 0 }
            var count: Int64 = 1
            for ordinal in node.children.indices {
                let a = ordinal == 0 ? low : node.keys[ordinal - 1]
                let b = ordinal < node.keys.count ? node.keys[ordinal] : high
                guard overlaps(a, b) else { continue }
                switch node.children[ordinal] {
                case .resident(let child): count += visit(child, low: a, high: b)
                case .onDisk: count += 1
                }
            }
            return count
        }
        switch root {
        case .resident(let node): return visit(node, low: nil, high: nil)
        case .onDisk: return 1
        }
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
        while true {
            if current == nil {
                guard position < intervals.count else { return nil }
                let bounds = intervals[position]; position += 1
                current = store.cursor(lower: bounds.lower, upper: bounds.upper, descending: descending)
            }
            if let row = try current?.next() { return row }
            current?.close(); current = nil
        }
    }
    mutating func close() { current?.close(); current = nil; position = intervals.count }
}
