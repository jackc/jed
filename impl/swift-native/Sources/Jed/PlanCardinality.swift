/// Cardinality uses resident facts and exact ceiling arithmetic. It never
/// opens a storage cursor, evaluates a host callback, or reads session entropy.
internal struct PlanCardinality {
    let table: CatalogTable?
    let snapshot: DomainSnapshot?
    var parameters: [SQLValue]? = nil
    var collatedKey: KeyEncoding.CollatedKey? = nil
    var planningProbe: PlanningProbe? = nil
    // Every statistical fraction belongs to the complete visible table.
    // AND and prefix inversion apply that fraction to their current input;
    // they must not rebuild MCV/NULL/NDV populations after an earlier leaf.
    var statisticsPopulation: Int64?
    init(table: CatalogTable?, snapshot: DomainSnapshot?, parameters: [SQLValue]? = nil,
         collatedKey: KeyEncoding.CollatedKey? = nil, planningProbe: PlanningProbe? = nil) {
        self.table = table; self.snapshot = snapshot; self.parameters = parameters; self.collatedKey = collatedKey
        self.planningProbe = planningProbe
        statisticsPopulation = table?.rowCount
    }

    static func constant(_ key: String) -> Int64 {
        guard case .integer(let value) = GeneratedSpec.estimator[key] else { preconditionFailure("missing estimator fact") }
        return value
    }
    func known(_ expression: ResolvedExpression) -> SQLValue? {
        PlanningProbe.known(expression, parameters: parameters, probe: planningProbe)
    }
    func isNull(_ expression: ResolvedExpression) -> Bool {
        PlanningProbe.isNull(expression, parameters: parameters, probe: planningProbe)
    }
    private func contradicts(_ first: Comparison, _ second: Comparison) -> Bool {
        // Check both source shapes before either read. An outer-column peer
        // cannot prove a contradiction and must not make a parameter sensitive.
        guard PlanningProbe.literal(first.value), PlanningProbe.literal(second.value),
              let a = known(first.value), let b = known(second.value) else { return false }
        return PredicateContradiction.bounds(first.operation, a, second.operation, b)
    }
    private func column(_ expression: ResolvedExpression) -> Int? {
        if case .column(let slot) = expression.node { return slot }
        if case .convert(let value, _) = expression.node { return column(value) }
        return nil
    }
    private struct Comparison { let slot: Int; let operation: String; let value: ResolvedExpression; let type: TypeDescriptor }
    private func comparison(_ expression: ResolvedExpression) -> Comparison? {
        guard case .binary(let op, let a, let b, let type) = expression.node,
              ["=", "<>", "!=", "<", "<=", ">", ">="].contains(op) else { return nil }
        func source(_ expression: ResolvedExpression) -> Bool {
            switch expression.node { case .constant, .parameter, .outerColumn, .subquery: true; case .convert(let value, _): source(value); default: false }
        }
        if let slot = column(a), source(b) { return Comparison(slot: slot, operation: op, value: b, type: type) }
        if let slot = column(b), source(a) {
            let flipped = ["<": ">", "<=": ">=", ">": "<", ">=": "<="][op] ?? op
            return Comparison(slot: slot, operation: flipped, value: a, type: type)
        }
        return nil
    }
    /// A relation-constant array is a deliberately small estimator grammar.
    /// Casts and arbitrary calls stay opaque even when execution can fold them.
    private static func arrayOperand(_ expression: ResolvedExpression) -> Bool {
        switch expression.node {
        case .constant(.array), .constant(.null), .parameter, .outerColumn: return true
        case .array(let elements, false):
            return elements.allSatisfy { element in
                switch element.node {
                case .constant(.composite): false
                case .constant, .parameter, .outerColumn: true
                default: false
                }
            }
        default: return false
        }
    }
    func rows(_ predicate: ResolvedExpression?, input: Int64) throws -> Int64 {
        guard input > 0, let predicate else { return max(0, input) }
        if let rows = try ginRows(predicate, input: input) { return rows }
        if let population = statisticsPopulation, population > 0,
           let count = try statisticalAtom(predicate, population: population) {
            return min(input, QueryEstimate.scale(input, numerator: count, denominator: population))
        }
        if let known = known(predicate) {
            if known.isNull { return 0 }
            if case .boolean(let value) = known { return value ? input : 0 }
        }
        switch predicate.node {
        case .binary("and", _, _, _):
            let terms = QueryEstimate.conjuncts(predicate)
            var result = input, consumed: Set<Int> = [], unique: Set<Int> = []
            for (index, expression) in terms.enumerated() {
                guard !consumed.contains(index) else { continue }
                if let current = comparison(expression) {
                    if isNull(current.value) { return 0 }
                    if current.operation == "=" { unique.insert(current.slot) }
                    let peers = terms.indices.filter { $0 > index && comparison(terms[$0])?.slot == current.slot }
                    for peer in peers {
                        guard let other = comparison(terms[peer]) else { continue }
                        if contradicts(current, other) { return 0 }
                    }
                    let lower = [">", ">="].contains(current.operation), upper = ["<", "<="].contains(current.operation)
                    if lower || upper, let pair = peers.first(where: {
                        guard let candidate = comparison(terms[$0]) else { return false }
                        return lower ? ["<", "<="].contains(candidate.operation) : [">", ">="].contains(candidate.operation)
                    }), let other = comparison(terms[pair]) {
                        let lo = lower ? current : other, hi = lower ? other : current
                        if isNull(lo.value) || isNull(hi.value) || contradicts(lo, hi) { return 0 }
                        if let selected = try statisticalRange(lo, hi, input: result) {
                            result = selected
                        } else { result = QueryEstimate.fraction("paired_range", rows: result) }
                        consumed.insert(pair); continue
                    }
                }
                result = try rows(expression, input: result)
            }
            if let table, !table.primaryKey.isEmpty, table.primaryKey.allSatisfy(unique.contains) { result = min(result, 1) }
            return result
        case .binary("or", _, _, _):
            var pending = [predicate], terms: [ResolvedExpression] = []
            while let expression = pending.popLast() {
                if case .binary("or", let left, let right, _) = expression.node { pending.append(right); pending.append(left) }
                else { terms.append(expression) }
            }
            var result: Int64 = 0, previous: [ResolvedExpression] = []
            for term in terms {
                if let current = comparison(term), current.operation == "=",
                   previous.contains(where: {
                       guard let prior = comparison($0), prior.operation == "=", prior.slot == current.slot else { return false }
                       return PlanningProbe.literal(prior.value) && PlanningProbe.literal(current.value)
                           && ExpressionIdentity.same(prior.value, current.value)
                   }) { continue }
                previous.append(term); result = min(input, saturatingAdd(result, try rows(term, input: input)))
            }
            return result
        case .unary("not", let value): return max(0, input - (try rows(value, input: input)))
        case .binary(let op, let a, let b, _):
            if isNull(a) || isNull(b) { return 0 }
            if let comparison = comparison(predicate) {
                if let count = try statistical(comparison, input: input) { return count }
                if op == "=", let table, table.primaryKey == [comparison.slot] { return min(input, 1) }
            }
            if op == "=" { return QueryEstimate.fraction("equality", rows: input) }
            if op == "<>" || op == "!=" { return input - QueryEstimate.fraction("equality", rows: input) }
            if ["<", "<=", ">", ">="].contains(op) { return QueryEstimate.fraction("inequality", rows: input) }
            return QueryEstimate.fraction("opaque", rows: input)
        case .pattern(let pattern):
            let matched = QueryEstimate.fraction("match", rows: input)
            return pattern.negated ? input - matched : matched
        case .isNull(let value, let negated):
            let nulls = column(value).flatMap { facts($0) }.map { min(input, QueryEstimate.scale(input, numerator: $0.nulls, denominator: $0.analyzed)) }
                ?? QueryEstimate.fraction("null_test", rows: input)
            return negated ? input - nulls : nulls
        case .quantified(let op, let all, let value, let array, _):
            guard (op == "=" && !all || ["<>", "!="].contains(op) && all), Self.arrayOperand(array) else { return QueryEstimate.fraction("opaque", rows: input) }
            let elements: [ResolvedExpression]
            if let literal = known(array) {
                if literal.isNull { return 0 }
                guard case .array(let values) = literal else { return QueryEstimate.fraction("opaque", rows: input) }
                elements = values.elements.map { ResolvedExpression(type: value.type, node: .constant($0), position: 0) }
            } else if case .array(let values, false) = array.node { elements = values }
            else {
                let equality = QueryEstimate.fraction("equality", rows: input)
                let found = min(input, QueryEstimate.multiply(equality, Self.constant("default_array_elements")))
                return all ? input - found : found
            }
            var found: Int64 = 0
            for element in elements {
                if isNull(element) { if all { return 0 }; continue }
                let equality = ResolvedExpression(type: TypeDescriptor(.scalar(.boolean)), node: .binary("=", value, element, value.type ?? TypeDescriptor(.scalar(.text))), position: 0)
                found = min(input, saturatingAdd(found, try rows(equality, input: input)))
            }
            return all ? input - found : found
        case .column:
            if let slot = column(predicate), let count = try statistical(Comparison(slot: slot, operation: "=",
                value: ResolvedExpression(type: predicate.type, node: .constant(.boolean(true)), position: 0),
                type: predicate.type ?? TypeDescriptor(.scalar(.boolean))), input: input) { return count }
            return QueryEstimate.fraction("boolean", rows: input)
        default: return QueryEstimate.fraction("opaque", rows: input)
        }
    }
    /// A logical array equality retains the equality default. Other admitted GIN
    /// leaves use full-population term frequencies independently of chosen access.
    private func ginRows(_ expression: ResolvedExpression, input: Int64) throws -> Int64? {
        guard let table, let snapshot else { return nil }
        let types = try snapshot.tableTypes(table)
        guard let predicate = GinPredicate.match(expression, columns: Array(types.indices), types: types),
              !predicate.isArrayEquality,
              snapshot.catalog.statistics.contains(where: { $0.table == table.name && $0.column == predicate.column && $0.termEligible && $0.analyzed > 0 }),
              let query = try predicate.knownQuery(parameters: parameters, planningProbe: planningProbe),
              let estimate = GinEstimate.resolve(query, column: predicate.column, table: table, snapshot: snapshot) else { return nil }
        guard table.rowCount > 0 else { return 0 }
        return min(input, QueryEstimate.scale(input, numerator: estimate.rows, denominator: table.rowCount))
    }

    /// Statistics-backed leaves are fractions of one fixed population. Boolean
    /// composition is deliberately left to rows(_:input:) so structural AND/OR
    /// keeps its prescribed sequence of ceiling operations.
    private func statisticalAtom(_ expression: ResolvedExpression, population: Int64) throws -> Int64? {
        var original = self; original.statisticsPopulation = nil
        func nonnull(_ slot: Int) -> Int64? {
            guard let fact = facts(slot), fact.distributionEligible else { return nil }
            return population - min(population, QueryEstimate.scale(population, numerator: fact.nulls, denominator: fact.analyzed))
        }
        if let comparison = comparison(expression) { return try original.statistical(comparison, input: population) }
        switch expression.node {
        case .isNull(let value, let negated):
            guard let slot = column(value), let fact = facts(slot) else { return nil }
            let count = population - min(population, QueryEstimate.scale(population, numerator: fact.nulls, denominator: fact.analyzed))
            return negated ? count : population - count
        case .column(let slot):
            guard nonnull(slot) != nil else { return nil }
            return try original.rows(expression, input: population)
        case .binary("or", _, _, _):
            var pending = [expression], common: Int?
            while let item = pending.popLast() {
                if case .binary("or", let left, let right, _) = item.node { pending += [right, left]; continue }
                guard let part = comparison(item), part.operation == "=", nonnull(part.slot) != nil else { return nil }
                if let common, common != part.slot { return nil }
                common = part.slot
            }
            return try original.rows(expression, input: population)
        case .quantified(let op, let all, let value, let array, _):
            guard op == "=" && !all || ["<>", "!="].contains(op) && all,
                  Self.arrayOperand(array), let slot = column(value), nonnull(slot) != nil else { return nil }
            return try original.rows(expression, input: population)
        case .unary("not", let value):
            if case .isNull = value.node {
                return try statisticalAtom(value, population: population).map { population - $0 }
            }
            if let part = comparison(value), let count = nonnull(part.slot),
               let selected = try original.statistical(part, input: population) {
                return isNull(part.value) ? 0 : max(0, count - selected)
            }
            return nil
        default: return nil
        }
    }
    private func facts(_ slot: Int) -> CatalogStatistics? {
        guard let table else { return nil }
        return snapshot?.catalog.statistics.first { $0.table == table.name && $0.column == slot && $0.analyzed > 0 }
    }
    private static func matches(_ order: Int, _ operation: String) -> Bool {
        switch operation { case "=": order == 0; case "<>", "!=": order != 0; case "<": order < 0; case "<=": order <= 0; case ">": order > 0; case ">=": order >= 0; default: false }
    }
    private func statisticalRange(_ lower: Comparison, _ upper: Comparison, input: Int64) throws -> Int64? {
        if let population = statisticsPopulation, population > 0 {
            var original = self; original.statisticsPopulation = nil
            return try original.statisticalRange(lower, upper, input: population).map {
                min(input, QueryEstimate.scale(input, numerator: $0, denominator: population))
            }
        }
        guard let facts = facts(lower.slot), facts.distributionEligible,
              let lo = known(lower.value), let hi = known(upper.value), !lo.isNull, !hi.isNull,
              let table, let snapshot else { return nil }
        let type = try snapshot.tableTypes(table)[lower.slot]
        let nonnull = input - min(input, QueryEstimate.scale(input, numerator: facts.nulls, denominator: facts.analyzed))
        var mass: Int64 = 0, matched: Int64 = 0
        for entry in facts.mcv {
            let count = min(nonnull - mass, QueryEstimate.scale(input, numerator: Int64(entry.frequency), denominator: max(1, Int64(facts.sampleRows))))
            mass += count
            let value = try ValueCodec.decode(entry.bytes, type: type)
            if try Self.matches(ValueComparison.total(value, lo, type: type, collatedKey: collatedKey), lower.operation)
                && Self.matches(ValueComparison.total(value, hi, type: type, collatedKey: collatedKey), upper.operation) {
                matched = saturatingAdd(matched, count)
            }
        }
        guard facts.histogram.count > 1 else { return min(nonnull, matched) }
        var low: Int64 = 0, high: Int64 = 0
        for bytes in facts.histogram {
            let value = try ValueCodec.decode(bytes, type: type)
            let below = try ValueComparison.total(value, lo, type: type, collatedKey: collatedKey)
            let above = try ValueComparison.total(value, hi, type: type, collatedKey: collatedKey)
            if below < 0 || below == 0 && lower.operation == ">" { low += 1 }
            if above < 0 || above == 0 && upper.operation == "<=" { high += 1 }
        }
        let buckets = Int64(facts.histogram.count - 1)
        let fraction = max(0, min(buckets, high) - min(buckets, low))
        return min(nonnull, saturatingAdd(matched, QueryEstimate.scale(nonnull - mass, numerator: fraction, denominator: buckets)))
    }
    private func statistical(_ comparison: Comparison, input: Int64) throws -> Int64? {
        if let population = statisticsPopulation, population > 0 {
            var original = self; original.statisticsPopulation = nil
            return try original.statistical(comparison, input: population).map {
                min(input, QueryEstimate.scale(input, numerator: $0, denominator: population))
            }
        }
        guard let facts = facts(comparison.slot), facts.distributionEligible else { return nil }
        let storedType: TypeDescriptor
        if let table, let snapshot { storedType = try snapshot.tableTypes(table)[comparison.slot] }
        else { storedType = comparison.type }
        let nulls = min(input, QueryEstimate.scale(input, numerator: facts.nulls, denominator: facts.analyzed)), nonnull = input - nulls
        let analyzedNonnull = facts.analyzed - facts.nulls
        guard analyzedNonnull > 0 else { return 0 }
        let ndv = min(nonnull, QueryEstimate.multiply(facts.distinct, 10) > analyzedNonnull
            ? QueryEstimate.scale(nonnull, numerator: facts.distinct, denominator: analyzedNonnull) : facts.distinct)
        if isNull(comparison.value) { return 0 }
        // With no MCV and no proof of absence, every non-NULL parameter takes
        // the same NDV average. Avoid reading its payload merely to discover it.
        if case .parameter = comparison.value.node,
           ["=", "<>", "!="].contains(comparison.operation), facts.mcv.isEmpty,
           !(!facts.stale && Int64(facts.sampleRows) == facts.analyzed && Int64(facts.mcv.count) == facts.distinct),
           storedType.collation == nil {
            let equality = QueryEstimate.scale(nonnull, numerator: 1, denominator: max(1, ndv))
            return comparison.operation == "=" ? equality : nonnull - equality
        }
        let value = known(comparison.value)
        guard let value else {
            if comparison.operation == "=" { return QueryEstimate.scale(nonnull, numerator: 1, denominator: max(1, ndv)) }
            if ["<>", "!="].contains(comparison.operation) { return nonnull - QueryEstimate.scale(nonnull, numerator: 1, denominator: max(1, ndv)) }
            return nil
        }
        var mass: Int64 = 0, match: Int64 = 0, equality: Int64?
        for entry in facts.mcv {
            let count = min(nonnull - mass, QueryEstimate.scale(input, numerator: Int64(entry.frequency), denominator: max(1, Int64(facts.sampleRows))))
            mass += count
            let item = try ValueCodec.decode(entry.bytes, type: storedType)
            let order = try ValueComparison.total(item, value, type: storedType, collatedKey: collatedKey)
            if order == 0 { equality = count }
            if Self.matches(order, comparison.operation) { match = saturatingAdd(match, count) }
        }
        let residual = nonnull - mass
        if ["=", "<>", "!="].contains(comparison.operation) {
            let complete = !facts.stale && Int64(facts.sampleRows) == facts.analyzed && Int64(facts.mcv.count) == facts.distinct
            let count = equality ?? (complete ? 0 : max(1, QueryEstimate.scale(residual, numerator: 1, denominator: max(1, ndv - Int64(facts.mcv.count)))))
            let bounded = min(nonnull, count)
            return comparison.operation == "=" ? bounded : nonnull - bounded
        }
        guard facts.histogram.count > 1 else { return min(nonnull, match) }
        let inclusive = comparison.operation == "<=" || comparison.operation == ">"
        var ordinal: Int64 = 0
        for bytes in facts.histogram {
            let item = try ValueCodec.decode(bytes, type: storedType)
            let order = try ValueComparison.total(item, value, type: storedType, collatedKey: collatedKey)
            if order < 0 || inclusive && order == 0 { ordinal += 1 } else { break }
        }
        let cdf = min(residual, QueryEstimate.scale(residual, numerator: ordinal, denominator: Int64(facts.histogram.count - 1)))
        return min(nonnull, saturatingAdd(match, comparison.operation == ">" || comparison.operation == ">=" ? residual - cdf : cdf))
    }
    func distinct(_ keys: [ResolvedExpression], input: Int64) -> Int64 {
        guard input > 0 else { return 0 }
        var count: Int64 = 1
        for key in keys {
            var values = Self.constant("default_distinct_values")
            if let slot = column(key), let facts = facts(slot), facts.distributionEligible {
                let nulls = min(input, QueryEstimate.scale(input, numerator: facts.nulls, denominator: facts.analyzed))
                let nonnull = input - nulls, analyzed = facts.analyzed - facts.nulls
                if analyzed > 0 {
                    values = min(nonnull, QueryEstimate.multiply(facts.distinct, 10) > analyzed ? QueryEstimate.scale(nonnull, numerator: facts.distinct, denominator: analyzed) : facts.distinct)
                } else { values = 0 }
                if nulls > 0 { values = saturatingAdd(values, 1) }
            }
            count = min(input, QueryEstimate.multiply(count, values))
        }
        return min(input, count)
    }
}
