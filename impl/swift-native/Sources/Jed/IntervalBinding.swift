extension IntervalPlan {
    private enum Encoded { case key([UInt8]), absent, below, above, unmapped }
    private static func read(_ expression: ResolvedExpression, context: ExpressionContext) throws -> SQLValue {
        switch expression.node {
        case .constant(let value): return value
        case .parameter(let index):
            guard context.parameters.indices.contains(index - 1) else { throw JedError("42P02", "there is no parameter $\(index)") }
            return context.parameters[index - 1]
        case .outerColumn: return try context.evaluate(expression)
        default: throw JedError("XX000", "interval endpoint is not a constant source")
        }
    }
    /// Promoted array elements are equality probes, never rounded assignments.
    static func pointValue(_ value: SQLValue, type: TypeDescriptor) -> SQLValue? {
        if value.isNull { return nil }
        if case .scalar(let scalar) = type.type, scalar.integerWidth != nil {
            let number: Int64
            switch value {
            case .integer(let input): number = input
            case .decimal(let input):
                guard let integer = try? input.integerValue(), input.compare(to: SQLDecimal(integer)) == 0 else { return nil }
                number = integer
            default: return nil
            }
            return scalar.admits(number) ? .integer(number) : nil
        }
        if type.type == .scalar(.decimal), case .integer(let number) = value { return .decimal(SQLDecimal(number)) }
        return value
    }
    private func encode(_ value: SQLValue, member: Member, context: ExpressionContext) throws -> Encoded {
        if value.isNull { return .absent }
        if case .integer(let number) = value, case .scalar(let scalar) = member.type.type,
           scalar.integerWidth != nil, !scalar.admits(number) { return number < 0 ? .below : .above }
        do {
            if index != nil {
                return .key(try KeyEncoding.encodeSlot(value, type: member.type,
                    descending: member.options & 1 != 0, nullsFirst: member.options & 2 != 0,
                    collatedKey: context.collatedKey))
            }
            return .key(try KeyEncoding.encode(value, type: member.type, collatedKey: context.collatedKey))
        } catch let error as JedError where error.code == "0A000" && member.type.collation != nil && member.type.collation?.description != "C" {
            return .unmapped
        }
    }
    private func all(_ member: Member) -> ScanInterval {
        guard index != nil else { return ScanInterval(lower: nil, upper: nil) }
        let descending = member.options & 1 != 0, nullsFirst = member.options & 2 != 0
        let tag: UInt8 = descending ? (nullsFirst ? 255 : 254) : (nullsFirst ? 1 : 0)
        return ScanInterval(lower: TreeBound([tag]), upper: OrderedIndexPlan.successor([tag]).map { TreeBound($0, inclusive: false) })
    }
    private func clip(_ interval: ScanInterval, terms: [Term], member: Member,
                      context: ExpressionContext) throws -> ScanInterval? {
        var result: ScanInterval? = interval
        for term in terms {
            guard let current = result else { return nil }
            let value = try Self.read(term.source, context: context)
            let encoded = try encode(value, member: member, context: context)
            let bound: ScanInterval
            switch encoded {
            case .absent: return nil
            case .below:
                if ["=", "<", "<="].contains(term.operation) { return nil }; continue
            case .above:
                if ["=", ">", ">="].contains(term.operation) { return nil }; continue
            case .unmapped:
                if term.operation == "=" { return nil }; continue
            case .key(let key):
                let operation = index != nil && member.options & 1 != 0 ? Self.reversed(term.operation) : term.operation
                switch operation {
                case "=": bound = ScanInterval(lower: TreeBound(key), upper: TreeBound(key))
                case ">", ">=": bound = ScanInterval(lower: TreeBound(key, inclusive: operation == ">="), upper: nil)
                case "<", "<=": bound = ScanInterval(lower: nil, upper: TreeBound(key, inclusive: operation == "<="))
                default: throw JedError("XX000", "invalid interval comparison")
                }
            }
            result = Self.intersection(current, bound)
        }
        return result
    }
    private func values(_ source: ArraySource, context: ExpressionContext) throws -> [SQLValue]? {
        switch source {
        case .subquery(let query): return context.subqueries.values(query)?.map { $0[0] }
        case .elements(let elements): return try elements.map { try Self.read($0, context: context) }
        case .value(let expression):
            let value = try Self.read(expression, context: context)
            if value.isNull { return [] }
            guard case .array(let array) = value else { throw JedError("XX000", "interval array source has a non-array value") }
            return array.elements
        }
    }
    private func bind(_ member: Member, context: ExpressionContext) throws -> [ScanInterval] {
        var intervals: [ScanInterval] = []
        for part in member.set ?? [Disjunct(terms: [], array: nil)] {
            guard let base = try clip(all(member), terms: part.terms + member.clip, member: member, context: context) else { continue }
            if let source = part.array, let values = try values(source, context: context) {
                for value in values {
                    guard let value = Self.pointValue(value, type: member.type),
                          case .key(let key) = try encode(value, member: member, context: context),
                          let point = Self.intersection(base, ScanInterval(lower: TreeBound(key), upper: TreeBound(key))) else { continue }
                    intervals.append(point)
                }
            } else { intervals.append(base) }
        }
        return Self.canonical(intervals, adjacent: !member.point && ValueCodec.fixedWidth(member.type.type) != nil)
    }
    func bind(context: ExpressionContext) throws -> ScanAccess {
        var prefixes: [[UInt8]] = [[]], consumed = 0, final: [ScanInterval]?
        for member in members {
            let intervals = try bind(member, context: context)
            if intervals.isEmpty { return .empty }
            if consumed > 0, intervals.count > 1, prefixes.count > Self.maximumProbes / intervals.count { break }
            let points = intervals.allSatisfy(Self.isPoint)
            if member.point && points {
                var expanded: [[UInt8]] = []
                for prefix in prefixes { for point in intervals { expanded.append(prefix + point.lower!.key) } }
                prefixes = expanded; consumed += 1
            } else { final = intervals; consumed += 1; break }
        }
        var result: [ScanInterval] = []
        for prefix in prefixes {
            var group: [ScanInterval] = []
            if let final {
                for part in final {
                    let lower: TreeBound?
                    if let endpoint = part.lower {
                        let bytes = prefix + endpoint.key
                        if endpoint.inclusive { lower = TreeBound(bytes) }
                        else {
                            guard let next = OrderedIndexPlan.successor(bytes) else { continue }
                            lower = TreeBound(next)
                        }
                    } else { lower = prefix.isEmpty ? nil : TreeBound(prefix) }
                    let upper: TreeBound?
                    if let endpoint = part.upper {
                        let bytes = prefix + endpoint.key
                        upper = endpoint.inclusive ? OrderedIndexPlan.successor(bytes).map { TreeBound($0, inclusive: false) }
                            : TreeBound(bytes, inclusive: false)
                    } else { upper = prefix.isEmpty ? nil : OrderedIndexPlan.successor(prefix).map { TreeBound($0, inclusive: false) } }
                    let interval = ScanInterval(lower: lower, upper: upper)
                    if !Self.empty(interval) { group.append(interval) }
                }
            } else if index == nil && consumed == keyCount {
                group = [ScanInterval(lower: TreeBound(prefix), upper: TreeBound(prefix))]
            } else {
                group = [ScanInterval(lower: prefix.isEmpty ? nil : TreeBound(prefix),
                    upper: prefix.isEmpty ? nil : OrderedIndexPlan.successor(prefix).map { TreeBound($0, inclusive: false) })]
            }
            result += Self.canonical(group, adjacent: fixedWidth)
        }
        // Variable-length index prefixes are self-delimiting and disjoint. Do
        // not merge across their independently canonical final-member groups.
        if index == nil || fixedWidth { result = Self.canonical(result, adjacent: fixedWidth) }
        return ScanAccess(lower: nil, upper: nil, isEmpty: result.isEmpty, intervals: result)
    }
    static func isPoint(_ interval: ScanInterval) -> Bool {
        guard let a = interval.lower, let b = interval.upper else { return false }
        return a.inclusive && b.inclusive && a.key == b.key
    }
    static func empty(_ interval: ScanInterval) -> Bool {
        guard let a = interval.lower, let b = interval.upper else { return false }
        return b.key.lexicographicallyPrecedes(a.key) || a.key == b.key && (!a.inclusive || !b.inclusive)
    }
    static func intersection(_ a: ScanInterval, _ b: ScanInterval) -> ScanInterval? {
        func lower(_ a: TreeBound?, _ b: TreeBound?) -> TreeBound? {
            guard let a else { return b }; guard let b else { return a }
            if a.key == b.key { return TreeBound(a.key, inclusive: a.inclusive && b.inclusive) }
            return a.key.lexicographicallyPrecedes(b.key) ? b : a
        }
        func upper(_ a: TreeBound?, _ b: TreeBound?) -> TreeBound? {
            guard let a else { return b }; guard let b else { return a }
            if a.key == b.key { return TreeBound(a.key, inclusive: a.inclusive && b.inclusive) }
            return a.key.lexicographicallyPrecedes(b.key) ? a : b
        }
        let result = ScanInterval(lower: lower(a.lower, b.lower), upper: upper(a.upper, b.upper))
        return empty(result) ? nil : result
    }
    static func canonical(_ intervals: [ScanInterval], adjacent: Bool) -> [ScanInterval] {
        let sorted = intervals.filter { !empty($0) }.sorted { a, b in
            guard let x = a.lower else { return b.lower != nil }; guard let y = b.lower else { return false }
            if x.key != y.key { return x.key.lexicographicallyPrecedes(y.key) }
            if x.inclusive != y.inclusive { return x.inclusive }
            guard let x = a.upper else { return false }; guard let y = b.upper else { return true }
            if x.key != y.key { return x.key.lexicographicallyPrecedes(y.key) }
            return !x.inclusive && y.inclusive
        }
        var output: [ScanInterval] = []
        for interval in sorted {
            guard let prior = output.last else { output.append(interval); continue }
            let overlaps: Bool
            if let hi = prior.upper, let lo = interval.lower {
                overlaps = lo.key.lexicographicallyPrecedes(hi.key) || lo.key == hi.key && (lo.inclusive || hi.inclusive)
                    || adjacent && hi.inclusive && lo.inclusive && OrderedIndexPlan.successor(hi.key) == lo.key
            } else { overlaps = true }
            guard overlaps else { output.append(interval); continue }
            let upper: TreeBound?
            if let a = prior.upper, let b = interval.upper {
                if a.key == b.key { upper = TreeBound(a.key, inclusive: a.inclusive || b.inclusive) }
                else { upper = a.key.lexicographicallyPrecedes(b.key) ? b : a }
            } else { upper = nil }
            output[output.count - 1] = ScanInterval(lower: prior.lower, upper: upper)
        }
        return output
    }
}
