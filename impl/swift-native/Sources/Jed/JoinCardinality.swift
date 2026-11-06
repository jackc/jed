/// Equality-join facts belong to each owning base relation's full visible
/// population. Pushed-filter and physical-access rows scale that same fraction;
/// they do not rescale the NDV once again at the join edge.
internal struct JoinCardinality {
    private struct Input {
        let offset: Int
        let width: Int
        let table: CatalogTable
        let snapshot: DomainSnapshot
    }
    private struct Column {
        let input: Int
        let population: Int64
        let nonnull: Int64
        let distinct: Int64
    }
    private let inputs: [Input]
    let parameters: [SQLValue]?
    let collatedKey: KeyEncoding.CollatedKey?
    init(left: RelationPlan, right: RelationPlan, parameters: [SQLValue]? = nil,
         collatedKey: KeyEncoding.CollatedKey? = nil) {
        var inputs: [Input] = []
        func visit(_ plan: RelationPlan, offset: Int) {
            if case .join(let left, let right, _, _) = plan.body {
                visit(left, offset: offset); visit(right, offset: offset + left.columns.count)
            } else if case .base = plan.body, let table = plan.baseTable, let snapshot = plan.sourceSnapshot {
                inputs.append(Input(offset: offset, width: plan.columns.count, table: table, snapshot: snapshot))
            }
        }
        visit(left, offset: 0); visit(right, offset: left.columns.count)
        self.inputs = inputs; self.parameters = parameters; self.collatedKey = collatedKey
    }
    private func column(_ global: Int) -> Column? {
        for (ordinal, input) in inputs.enumerated() where global >= input.offset && global < input.offset + input.width {
            guard let facts = input.snapshot.catalog.statistics.first(where: {
                $0.table == input.table.name && $0.column == global - input.offset && $0.distributionEligible
            }), facts.analyzed > 0 || input.table.rowCount == 0 else { return nil }
            let population = input.table.rowCount
            let nulls = facts.analyzed == 0 ? 0 : min(population, QueryEstimate.scale(population, numerator: facts.nulls, denominator: facts.analyzed))
            let nonnull = population - nulls, analyzed = facts.analyzed - facts.nulls
            let distinct = analyzed == 0 ? 0 : min(nonnull, facts.distinct > analyzed / 10
                ? QueryEstimate.scale(nonnull, numerator: facts.distinct, denominator: analyzed) : facts.distinct)
            return Column(input: ordinal, population: population, nonnull: nonnull, distinct: distinct)
        }
        return nil
    }
    func rows(_ predicate: ResolvedExpression?, input: Int64) throws -> Int64 {
        guard input > 0, let predicate else { return max(0, input) }
        switch predicate.node {
        case .binary("=", let a, let b, _):
            if case .column(let x) = a.node, case .column(let y) = b.node,
               let left = column(x), let right = column(y), left.input != right.input {
                let population = QueryEstimate.multiply(left.population, right.population)
                guard population > 0 else { return 0 }
                let matching = min(population, Self.productCeil(left.nonnull, right.nonnull,
                                                               denominator: max(1, max(left.distinct, right.distinct))))
                return min(input, QueryEstimate.scale(input, numerator: matching, denominator: population))
            }
        case .binary("and", let a, let b, _): return try rows(b, input: rows(a, input: input))
        case .binary("or", let a, let b, _): return min(input, try saturatingAdd(rows(a, input: input), rows(b, input: input)))
        case .unary("not", let child): return input - (try rows(child, input: input))
        default: break
        }
        return try PlanCardinality(table: nil, snapshot: nil, parameters: parameters, collatedKey: collatedKey).rows(predicate, input: input)
    }
    private static func productCeil(_ a: Int64, _ b: Int64, denominator: Int64) -> Int64 {
        guard a > 0, b > 0, denominator > 0 else { return 0 }
        let product = UInt64(a).multipliedFullWidth(by: UInt64(b)), divisor = UInt64(denominator)
        guard product.high < divisor else { return Int64.max }
        let result = divisor.dividingFullWidth(product)
        guard result.quotient < UInt64(Int64.max) else { return Int64.max }
        return Int64(result.quotient) + (result.remainder == 0 ? 0 : 1)
    }
}
