/// A cached plan for the commonest statement there is: read some bare columns
/// of the one row whose integer primary key equals the bound parameters.
///
/// The general prepared path rebinds a SelectPlan, re-derives the key bound,
/// compiles the WHERE into an encoded predicate and builds a columnar lane for
/// that one row. This object records, once, what those steps would decide,
/// and replays their charges in the same order: the point preflight, one unit
/// for the scanned row, one unit per AND and per integer comparison, the
/// result reservation and the production unit. It is admitted only where the
/// general path would take the packed columnar lane (no cost ceiling), so no
/// abort schedule can observe a difference.
internal final class PreparedPointRead {
    struct Part { let parameter: Int; let type: TypeDescriptor }
    let columns: [ResultColumn]
    private let domain: SQLText
    private let table: SQLText
    private let parts: [Part]
    private let variable: [Int]
    /// Distinct projected table columns, and each projection's index in them.
    private let lanes: [Int]
    private let output: [Int]
    private let filterUnits: Int
    private let productionCharge: Bool

    init?(_ plan: SelectPlan, parameters: [TypeDescriptor]) {
        guard let store = plan.store, let domain = plan.sourceDomain, domain != DomainNames.temporary,
              let table = plan.sourceTable, plan.relation == nil, plan.compound == nil, plan.withQuery == nil,
              plan.aggregation == nil, plan.window == nil, !plan.distinct, plan.distinctOn == nil,
              plan.order.isEmpty, plan.limit == nil, plan.offset == 0, !plan.materializeSource,
              plan.chosenJoin == nil, plan.chosenNWay == nil,
              let predicate = plan.predicate, !plan.projections.isEmpty else { return nil }
        let scan: BaseScanPlan
        switch plan.chosenAccess?.path {
        case .none: scan = plan.scan
        case .primary(let bound): scan = bound
        default: return nil
        }
        let types = store.tree.types
        guard !scan.keys.isEmpty, !scan.contradiction, scan.pointSets.isEmpty,
              plan.scan.pointSets.isEmpty, !plan.scan.contradiction else { return nil }
        func integer(_ type: TypeDescriptor) -> Bool {
            if case .scalar(let scalar) = type.type { return scalar.integerWidth != nil }
            return false
        }
        // The key: one `=` against a parameter for every primary-key column.
        var parts: [Part] = []
        for column in scan.keys {
            guard let terms = scan.terms[column], terms.count == 1, terms[0].operation == "=",
                  case .parameter(let index) = terms[0].source.node, index > 0, index <= parameters.count,
                  types.indices.contains(column), integer(types[column]), integer(parameters[index - 1]) else { return nil }
            parts.append(Part(parameter: index - 1, type: types[column]))
        }
        // The WHERE: exactly those comparisons, joined by AND, so the encoded
        // predicate is integer atoms throughout and every atom holds.
        var compared: Set<Int> = [], atoms = 0, conjunctions = 0, admitted = true
        let touched = Set(plan.touched)
        func visit(_ expression: ResolvedExpression) {
            switch expression.node {
            case .binary("and", let left, let right, _): conjunctions += 1; visit(left); visit(right)
            case .binary("=", let left, let right, _):
                let column: Int
                switch (left.node, right.node) {
                case (.column(let index), .parameter), (.parameter, .column(let index)): column = index
                default: admitted = false; return
                }
                guard scan.keys.contains(column), touched.contains(column) else { admitted = false; return }
                compared.insert(column); atoms += 1
            default: admitted = false
            }
        }
        visit(predicate)
        guard admitted, atoms == scan.keys.count, compared.count == atoms else { return nil }
        var projected: [Int] = []
        for expression in plan.projections {
            guard case .column(let index) = expression.node, types.indices.contains(index), touched.contains(index) else { return nil }
            projected.append(index)
        }
        let distinct = Array(Set(projected)).sorted()
        lanes = distinct
        output = projected.map { distinct.firstIndex(of: $0)! }
        variable = ScanCost.variableColumns(plan.touched, types: types)
        filterUnits = atoms + conjunctions
        productionCharge = plan.productionCharge
        columns = plan.columns
        self.domain = domain; self.table = table.name; self.parts = parts
    }

    /// The storage key the general bound would build from these parameters,
    /// or nil when it would not name a single key (the caller then takes the
    /// general path, which decides what an out-of-range value means).
    func key(_ bound: [SQLValue]) throws -> [UInt8]? {
        var key: [UInt8] = []
        key.reserveCapacity(parts.count * 8)
        for part in parts {
            guard bound.indices.contains(part.parameter), case .integer = bound[part.parameter],
                  let value = IntervalPlan.pointValue(bound[part.parameter], type: part.type) else { return nil }
            key += try KeyEncoding.encode(value, type: part.type)
        }
        return key
    }

    func open(domains: QueryDomains, bound: [SQLValue], meter: CostMeter) throws -> Cursor? {
        guard let store = try domains.snapshot(domain).tables[table], let key = try key(bound) else { return nil }
        return Cursor(plan: self, store: store, key: key, meter: meter)
    }

    final class Cursor {
        private let plan: PreparedPointRead
        private let store: TableStore
        private let key: [UInt8]
        private let meter: CostMeter
        private var finished = false
        private var reserved: Int64 = 0

        fileprivate init(plan: PreparedPointRead, store: TableStore, key: [UInt8], meter: CostMeter) {
            self.plan = plan; self.store = store; self.key = key; self.meter = meter
        }

        func next() throws -> [SQLValue]? {
            guard !finished else { return nil }
            finished = true
            guard let position = try ScanCost.chargePoint(store, key: key, variable: plan.variable, meter: meter) else { return nil }
            try meter.step(); meter.charge(1)
            for _ in 0..<plan.filterUnits { meter.charge(1); try meter.step() }
            let pager = store.tree.pool?.pager
            var values: [SQLValue] = [], bytes: Int64 = 0
            values.reserveCapacity(plan.lanes.count)
            for column in plan.lanes {
                let value = try position.materialize(column, pager: pager)
                bytes = saturatingAdd(bytes, MemorySizes.value(value))
                values.append(value)
            }
            try meter.reserveQuery(bytes); reserved = bytes
            if plan.productionCharge { meter.charge(1); try meter.guardCost() }
            return plan.output.map { values[$0] }
        }

        func close() { meter.resources.query.release(reserved); reserved = 0 }
        deinit { close() }
    }
}
