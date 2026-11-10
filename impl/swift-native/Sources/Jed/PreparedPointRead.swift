/// A cached plan for the commonest statement there is: read some bare columns
/// of the one row whose primary key equals the bound parameters.
///
/// The general prepared path rebinds a SelectPlan, re-derives the key bound
/// and builds a lane for that one row. This object records, once, what those
/// steps would decide, and replays their charges, guards and reservations in
/// the order of whichever lane the general path would take: the packed
/// columnar lane without a cost ceiling, the materializing scalar lane with
/// one. A ceiling's abort names the accrued cost, so the order is contract.
///
/// Key columns are integers and UUIDs: fixed-width types whose equality
/// costs one unit in every lane and whose parameter is its own key value.
internal final class PreparedPointRead {
    struct Part { let parameter: Int; let type: TypeDescriptor }
    let columns: [ResultColumn]
    private let domain: SQLText
    private let table: SQLText
    private let parts: [Part]
    private let variable: [Int]
    private let touched: [Int]
    private let width: Int
    /// Each projection's table column.
    private let projected: [Int]
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
        func keyed(_ column: TypeDescriptor, by parameter: TypeDescriptor) -> Bool {
            guard case .scalar(let stored) = column.type, case .scalar(let supplied) = parameter.type else { return false }
            if stored == .uuid { return supplied == .uuid }
            return stored.integerWidth != nil && supplied.integerWidth != nil
        }
        // The key: one `=` against a parameter for every primary-key column.
        var parts: [Part] = []
        for column in scan.keys {
            guard let terms = scan.terms[column], terms.count == 1, terms[0].operation == "=",
                  case .parameter(let index) = terms[0].source.node, index > 0, index <= parameters.count,
                  types.indices.contains(column), keyed(types[column], by: parameters[index - 1]) else { return nil }
            parts.append(Part(parameter: index - 1, type: types[column]))
        }
        // The WHERE: exactly those comparisons, joined by AND, so the encoded
        // predicate is one-unit comparisons throughout and each one holds.
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
        self.touched = plan.touched; width = types.count; self.projected = projected
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
            guard bound.indices.contains(part.parameter) else { return nil }
            switch bound[part.parameter] {
            case .integer, .uuid: break
            default: return nil
            }
            guard let value = IntervalPlan.pointValue(bound[part.parameter], type: part.type) else { return nil }
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
            let pager = store.tree.pool?.pager
            if meter.limit != 0 || meter.lifetimeLimit != 0 {
                // The scalar lane (`SelectInput`, materialized source): the
                // row is decoded whole and reserved before WHERE runs over
                // it, and a checkpoint precedes the production unit.
                try meter.step(); meter.charge(1)
                var row = Array(repeating: SQLValue.null, count: plan.width)
                for column in plan.touched { row[column] = try position.materialize(column, pager: pager) }
                let bytes = MemorySizes.row(row)
                try meter.reserveQuery(bytes); reserved = bytes
                for _ in 0..<plan.filterUnits { meter.charge(1); try meter.step() }
                try meter.checkpoint()
                if plan.productionCharge { meter.charge(1); try meter.guardCost() }
                return plan.projected.map { row[$0] }
            }
            // The packed columnar lane: WHERE reads the page in place, then
            // only the projected values are gathered and reserved.
            try meter.step(); meter.charge(1)
            for _ in 0..<plan.filterUnits { meter.charge(1); try meter.step() }
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
