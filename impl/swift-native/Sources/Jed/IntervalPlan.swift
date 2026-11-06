/// A key-set candidate retains resolved endpoint sources. Neither admission nor
/// binding evaluates SQL; the original predicate remains the residual filter.
internal struct IntervalPlan {
    struct Term {
        let operation: String
        let source: ResolvedExpression
        let expression: ResolvedExpression
    }
    enum ArraySource {
        case value(ResolvedExpression), elements([ResolvedExpression]), subquery(ResolvedSubquery)
    }
    struct Disjunct {
        let terms: [Term]
        let array: ArraySource?
        var point: Bool { array != nil || !terms.isEmpty && terms.allSatisfy { $0.operation == "=" } }
    }
    struct Member {
        let column: Int
        let type: TypeDescriptor
        let options: UInt8
        let set: [Disjunct]?
        let clip: [Term]
        var point: Bool { clip.contains { $0.operation == "=" } || set?.allSatisfy(\.point) == true }
    }
    static let maximumProbes = 65_536
    let members: [Member]
    let keyCount: Int
    let index: OrderedIndexPlan?
    let fixedWidth: Bool
    let replacesContiguous: Bool

    var disjunctCount: Int { members.reduce(0) { $0 + ($1.set?.count ?? 0) } }
    var arrayCount: Int { members.reduce(0) { $0 + ($1.set?.filter { $0.array != nil }.count ?? 0) } }
    var usesOuter: Bool {
        for member in members {
            for term in member.clip + (member.set ?? []).flatMap(\.terms) {
                if case .outerColumn = term.source.node { return true }
            }
            for part in member.set ?? [] {
                switch part.array {
                case .value(let source): if case .outerColumn = source.node { return true }
                case .elements(let elements):
                    if elements.contains(where: { if case .outerColumn = $0.node { return true }; return false }) { return true }
                case .subquery, nil: break
                }
            }
        }
        return false
    }

    static func primary(predicate: ResolvedExpression?, keys: [Int], types: [TypeDescriptor]) -> IntervalPlan? {
        build(predicate: predicate, columns: keys, types: keys.map { types[$0] }, index: nil)
    }
    static func ordered(predicate: ResolvedExpression?, definition: OrderedIndexPlan) -> IntervalPlan? {
        var columns: [Int] = []
        for key in definition.keys {
            guard case .column(let column) = key.node else { return nil }
            columns.append(column)
        }
        return build(predicate: predicate, columns: columns, types: definition.types, index: definition)
    }
    private static func build(predicate: ResolvedExpression?, columns: [Int], types: [TypeDescriptor],
                              index: OrderedIndexPlan?) -> IntervalPlan? {
        guard !columns.isEmpty, let predicate else { return nil }
        let conjuncts = QueryEstimate.conjuncts(predicate)
        var members: [Member] = [], sawSet = false
        for ordinal in columns.indices {
            let column = columns[ordinal], type = types[ordinal]
            let scalar = scalarKey(type.type)
            if index == nil && !scalar { break }
            let clip = conjuncts.compactMap { comparison($0, column: column, type: type) }
            let set = scalar ? conjuncts.lazy.compactMap { disjunction($0, column: column, type: type) }.first : nil
            guard set != nil || !clip.isEmpty else { break }
            let option = index?.index.keyOptions.isEmpty == false ? index!.index.keyOptions[ordinal] : 0
            let member = Member(column: column, type: type, options: option, set: set, clip: clip)
            if !scalar && !member.point { break }
            members.append(member); sawSet = sawSet || set != nil
            if !member.point { break }
        }
        guard sawSet else { return nil }
        if index != nil {
            let suffix = members.indices.dropFirst().first { members[$0].set != nil || !members[$0].point } ?? members.count
            guard types.dropFirst(suffix).allSatisfy({ scalarKey($0.type) }) else { return nil }
            if members[0].clip.contains(where: { $0.operation != "=" }) ||
                members[0].set?.contains(where: { $0.terms.contains { $0.operation != "=" } }) == true {
                guard scalarKey(types[0].type) else { return nil }
            }
        }
        return IntervalPlan(members: members, keyCount: columns.count, index: index,
            fixedWidth: types.allSatisfy { ValueCodec.fixedWidth($0.type) != nil },
            replacesContiguous: members.count > 1 || members.contains { $0.set != nil && !$0.clip.isEmpty })
    }
    static func scalarKey(_ type: SQLType) -> Bool {
        guard case .scalar(let scalar) = type else { return false }
        return ![.json, .jsonb, .jsonpath].contains(scalar)
    }
    static func source(_ expression: ResolvedExpression) -> Bool {
        switch expression.node { case .constant, .parameter, .outerColumn: true; default: false }
    }
    static func column(_ expression: ResolvedExpression) -> Int? {
        var value = expression
        while case .convert(let child, _) = value.node { value = child }
        if case .column(let slot) = value.node { return slot }
        return nil
    }
    static func reversed(_ operation: String) -> String {
        ["<": ">", "<=": ">=", ">": "<", ">=": "<="][operation] ?? operation
    }
    static func comparison(_ expression: ResolvedExpression, column: Int, type: TypeDescriptor) -> Term? {
        guard case .binary(let operation, let lhs, let rhs, let comparison) = expression.node,
              ["=", "<", "<=", ">", ">="].contains(operation),
              (comparison.collation?.description ?? "C") == (type.collation?.description ?? "C") else { return nil }
        let value: ResolvedExpression, op: String
        if Self.column(lhs) == column, source(rhs) { value = rhs; op = operation }
        else if Self.column(rhs) == column, source(lhs) { value = lhs; op = reversed(operation) }
        else { return nil }
        let compatible: Bool
        if value.type == nil || value.type?.type == type.type { compatible = true }
        else if case .scalar(let a) = value.type?.type, case .scalar(let b) = type.type {
            compatible = a.integerWidth != nil && b.integerWidth != nil
        } else { compatible = false }
        guard compatible else { return nil }
        return Term(operation: op, source: value, expression: expression)
    }
    private static func array(_ expression: ResolvedExpression, column: Int, type: TypeDescriptor) -> ArraySource? {
        func promoted(_ input: SQLType?) -> Bool {
            guard case .scalar(let target) = type.type, case .scalar(let source) = input else { return false }
            return (target.integerWidth != nil || target == .decimal) && (source.integerWidth != nil || source == .decimal)
        }
        if case .subquery(let query) = expression.node, !query.correlated, query.rowArity == nil,
           case .membership("=", false, false, let membership, let operand) = query.kind,
           Self.column(operand) == column {
            let input = query.plan.columns.first?.type?.type
            guard input == nil || input == type.type || promoted(input),
                  (query.type?.collation?.description ?? "C") == (type.collation?.description ?? "C"),
                  !membership || type.collation == nil || type.collation?.description == "C" else { return nil }
            return .subquery(query)
        }
        guard case .quantified("=", false, let operand, let value, let comparison) = expression.node,
              Self.column(operand) == column,
              (comparison.collation?.description ?? "C") == (type.collation?.description ?? "C"),
              case .array(let input) = value.type?.type else { return nil }
        guard input == type.type || promoted(input) else { return nil }
        if source(value) { return .value(value) }
        if input == type.type, case .array(let elements, false) = value.node, elements.allSatisfy(source) { return .elements(elements) }
        return nil
    }
    private static func disjunction(_ expression: ResolvedExpression, column: Int, type: TypeDescriptor) -> [Disjunct]? {
        if let array = array(expression, column: column, type: type) { return [Disjunct(terms: [], array: array)] }
        guard case .binary("or", _, _, _) = expression.node else { return nil }
        var pending = [expression], result: [Disjunct] = []
        while let item = pending.popLast() {
            if case .binary("or", let left, let right, _) = item.node { pending += [right, left]; continue }
            var terms: [Term] = [], selectedArray: ArraySource?
            for leaf in QueryEstimate.conjuncts(item) {
                if let term = comparison(leaf, column: column, type: type) { terms.append(term) }
                else if selectedArray == nil, let found = array(leaf, column: column, type: type) { selectedArray = found }
                else { return nil }
            }
            result.append(Disjunct(terms: terms, array: selectedArray))
        }
        return result
    }
}

extension IntervalPlan {
    func explain(table: CatalogTable) -> [String] {
        let columns = members.map { table.columns[$0.column].name.description }.joined(separator: ", ")
        let label: String
        if let index {
            label = "Index interval set: using " + index.index.name.description + (members.count > 1 ? " (" + columns + ")" : "")
        } else { label = "PK interval set: " + columns }
        var result = [label, "intervals=\(disjunctCount)"]
        if arrayCount > 0 { result.append("arrays=\(arrayCount)") }
        return result
    }
}
