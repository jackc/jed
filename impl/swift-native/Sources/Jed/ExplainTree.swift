internal final class ExplainNode {
    let name: String
    var attributes: [String]
    var rows: Int64
    var logicalRows: Int64
    var cost: Int64
    var children: [ExplainNode]
    var actual: (() -> Int64)?
    init(_ name: String, _ attributes: [String] = [], rows: Int64, cost: Int64,
         logicalRows: Int64? = nil, children: [ExplainNode] = []) {
        self.name = name; self.attributes = attributes; self.rows = rows
        self.logicalRows = logicalRows ?? rows; self.cost = cost; self.children = children
    }
    var detail: String { attributes.isEmpty ? "-" : attributes.joined(separator: "; ") }
}

/// Rendering and estimates walk retained physical plans. No displayed node is
/// used as an alternate executor, and estimation never faults a storage leaf.
internal final class ExplainTree {
    let options: SQLExplain
    let parameters: [SQLValue]?
    let context: ExpressionContext
    private var bindings: [ObjectIdentifier: ExplainNode] = [:]
    init(options: SQLExplain, parameters: [SQLValue]?, context: ExpressionContext) {
        self.options = options; self.parameters = parameters; self.context = context
    }
    func query(_ plan: SelectPlan) throws -> ExplainNode {
        if let withQuery = plan.withQuery {
            var definitions: [ExplainNode] = [], cost: Int64 = 0
            for binding in withQuery.bindings {
                guard let body = binding.plan else { throw JedError("XX000", "read-only WITH contains mutation metadata") }
                let child = try query(body)
                let key = ObjectIdentifier(binding)
                bindings[key] = child
                let inline = !binding.buffers && binding.references > 0
                let contribution = binding.buffers && binding.references > 0 ? child.cost : 0
                cost = saturatingAdd(cost, contribution)
                let definition = ExplainNode("CTE " + binding.name.description, [binding.buffers ? "materialized" : "inlined"],
                    rows: child.rows, cost: contribution, children: inline ? [] : [child])
                let profile = context.executionProfile
                definition.actual = { profile?.value(binding, .total) ?? 0 }
                definitions.append(definition)
            }
            let body = try query(withQuery.body)
            let result = ExplainNode("WITH", ["ctes=\(withQuery.bindings.count)"], rows: body.rows,
                cost: saturatingAdd(cost, body.cost), children: definitions + [body])
            markRoot(result, plan); return result
        }
        if let compound = plan.compound { return try compoundQuery(plan, compound) }
        let model = PlanCardinality(table: plan.sourceTable, snapshot: plan.sourceSnapshot,
                                   parameters: parameters, collatedKey: context.collatedKey)
        var node: ExplainNode
        if let relation = plan.relation { node = try source(relation, touched: Set(plan.touched), offset: 0) }
        else if let store = plan.store, let table = plan.sourceTable {
            node = try scan(table: table, store: store, bound: plan.scan, touched: plan.touched,
                            snapshot: plan.sourceSnapshot, filter: nil)
        } else { node = ExplainNode("Result", rows: 1, cost: 0) }
        mark(node, plan, .source)
        if plan.scan.contradiction { zero(node) }
        if let limit = plan.limit, plan.relation == nil, plan.aggregation == nil,
           !plan.distinct, plan.distinctOn == nil, plan.order.isEmpty || plan.orderedByScan {
            let target = saturatingAdd(limit, plan.offset)
            let matching = try model.rows(plan.predicate, input: node.logicalRows)
            let pulled = target == 0 ? 0 : matching > target
                ? min(node.rows, QueryEstimate.scale(node.rows, numerator: target, denominator: matching)) : node.rows
            if pulled < node.rows {
                let pages = max(0, node.cost - node.rows)
                let height = Int64(plan.store?.tree.height ?? 0)
                let expectedPages = pulled == 0 ? 0 : min(pages, max(height, QueryEstimate.scale(pages, numerator: pulled, denominator: max(1, node.rows))))
                node.rows = pulled; node.cost = saturatingAdd(pulled, expectedPages)
            }
        }
        if plan.orderedByScan, !plan.order.isEmpty {
            node.attributes.append(plan.relation == nil ? "ordered: pk ordered" + (plan.descending ? " (reverse)" : "") : "ordered: join pk ordered")
        }
        if let predicate = plan.predicate {
            let logical = try model.rows(predicate, input: node.logicalRows)
            let cost = saturatingAdd(node.cost, try expressionCost([predicate], rows: node.rows))
            var attributes = [try condition(predicate, label: "filter", compact: "conjuncts=")]
            if plan.scan.contradiction { attributes.append("contradiction") }
            node = ExplainNode("Filter", attributes, rows: min(node.rows, logical), cost: cost, logicalRows: logical, children: [node])
            mark(node, plan, .filter)
        }
        if let aggregate = plan.aggregation {
            var count: Int64 = 0
            for set in aggregate.sets {
                count = saturatingAdd(count, set.isEmpty ? 1 : model.distinct(set.map { aggregate.keys[$0] }, input: node.rows))
            }
            var cost = saturatingAdd(node.cost, QueryEstimate.multiply(node.rows, Int64(aggregate.calls.count)))
            cost = saturatingAdd(cost, try expressionCost(aggregate.keys, rows: node.rows))
            for call in aggregate.calls {
                cost = saturatingAdd(cost, try expressionCost(call.arguments + call.order.map(\.expression) + (call.filter.map { [$0] } ?? []), rows: node.rows))
            }
            var attributes = ["groups=\(aggregate.keys.count) aggs=\(aggregate.calls.count)"]
            if let having = aggregate.having {
                cost = saturatingAdd(cost, try expressionCost([having], rows: count))
                count = try model.rows(having, input: count)
                attributes.append(try condition(having, label: "having", compact: "having:conjuncts="))
            }
            node = ExplainNode("Aggregate", attributes, rows: count, cost: cost, children: [node])
            mark(node, plan, .aggregate)
        }
        if let window = plan.window {
            let expressions = window.keys + window.calls.flatMap { $0.arguments + ($0.filter.map { [$0] } ?? []) }
            let cost = saturatingAdd(node.cost, saturatingAdd(QueryEstimate.multiply(node.rows, Int64(window.calls.count)),
                try expressionCost(expressions, rows: node.rows)))
            node = ExplainNode("Window", ["funcs=\(window.calls.count)"], rows: node.rows, cost: cost, children: [node])
            mark(node, plan, .window)
        }
        var projected = false
        if plan.distinct {
            let count = model.distinct(plan.projections, input: node.rows)
            let cost = saturatingAdd(node.cost, try expressionCost(plan.projections, rows: node.rows)); projected = true
            node = ExplainNode("Distinct", rows: count, cost: cost, children: [node])
            mark(node, plan, .distinct)
        }
        if !plan.order.isEmpty && !plan.orderedByScan {
            let keys = plan.order.map(\.expression)
            let cost = saturatingAdd(node.cost, try expressionCost(keys, rows: node.rows))
            var detail = "keys=\(plan.order.count)"
            if let limit = plan.limit, !plan.distinct, plan.distinctOn == nil { detail += ", top-k=\(saturatingAdd(limit, plan.offset))" }
            node = ExplainNode("Sort", [detail], rows: node.rows, cost: cost, children: [node])
            mark(node, plan, .sort)
        }
        if let distinct = plan.distinctOn {
            let count = model.distinct(Array(plan.order.prefix(distinct.count)).map(\.expression), input: node.rows)
            node = ExplainNode("Distinct", ["on=\(distinct.count)"], rows: count, cost: node.cost, children: [node])
            mark(node, plan, .distinct)
        }
        if plan.limit != nil || plan.offset > 0 {
            var attributes: [String] = []
            if let limit = plan.limit { attributes.append("limit=\(limit)") }
            if plan.offset > 0 { attributes.append("offset=\(plan.offset)") }
            let count = min(max(0, node.rows - plan.offset), plan.limit ?? Int64.max)
            node = ExplainNode("Limit", [attributes.joined(separator: " ")], rows: count, cost: node.cost, children: [node])
        }
        if !projected { node.cost = saturatingAdd(node.cost, try expressionCost(plan.projections, rows: node.rows)) }
        node.cost = saturatingAdd(node.cost, node.rows)
        if options.verbose { node.attributes.append("output=[" + (try plan.projections.map(ExplainExpression.render)).joined(separator: ", ") + "]") }
        markRoot(node, plan)
        return node
    }
    private func compoundQuery(_ plan: SelectPlan, _ compound: CompoundQueryPlan) throws -> ExplainNode {
        var node: ExplainNode
        switch compound.body {
        case .nested(let child): node = try query(child)
        case .values(let values, let wrapper):
            let count = Int64(values.count), cost = saturatingAdd(count, try expressionCost(values.flatMap { $0 }, rows: 1))
            node = ExplainNode("Values", ["rows=\(count)"], rows: count, cost: cost)
            if wrapper { node = ExplainNode("Subquery *VALUES*", rows: count, cost: cost, children: [node]) }
        case .set(let operation, let all, let left, let right):
            let a = try query(left), b = try query(right), total = saturatingAdd(a.rows, b.rows)
            let model = PlanCardinality(table: nil, snapshot: nil)
            let count: Int64
            switch operation {
            case "union": count = all ? total : model.distinct(plan.projections, input: total)
            case "intersect": count = QueryEstimate.fraction("opaque", rows: min(a.rows, b.rows))
            default: count = QueryEstimate.fraction("opaque", rows: a.rows)
            }
            node = ExplainNode(operation == "union" ? "Union" : operation == "intersect" ? "Intersect" : "Except",
                [all ? "all" : "distinct"], rows: count, cost: saturatingAdd(a.cost, b.cost), children: [a, b])
        }
        if !plan.order.isEmpty { node = ExplainNode("Sort", ["keys=\(plan.order.count)"], rows: node.rows, cost: node.cost, children: [node]) }
        if plan.limit != nil || plan.offset > 0 {
            var details: [String] = []
            if let limit = plan.limit { details.append("limit=\(limit)") }
            if plan.offset > 0 { details.append("offset=\(plan.offset)") }
            node = ExplainNode("Limit", [details.joined(separator: " ")], rows: min(max(0, node.rows - plan.offset), plan.limit ?? Int64.max), cost: node.cost, children: [node])
        }
        if case .values(_, true) = compound.body { node.cost = saturatingAdd(node.cost, node.rows) }
        markRoot(node, plan); return node
    }
    private func source(_ plan: RelationPlan, touched: Set<Int>, offset: Int) throws -> ExplainNode {
        var node: ExplainNode
        var touched = touched
        if case .join(_, _, _, let predicate) = plan.body, let predicate {
            StoredExpressionDependencies.walk([predicate]) { if case .column(let column) = $0.node { touched.insert(offset + column) } }
        }
        switch plan.body {
        case .base(let store):
            guard let table = plan.baseTable else { throw JedError("XX000", "base relation lacks catalog identity") }
            let local = touched.filter { $0 >= offset && $0 < offset + plan.columns.count }.map { $0 - offset }.sorted()
            let bound = BaseScanPlan(predicate: plan.scanFilter, keys: DomainNames.isAttachment(plan.sourceDomain ?? DomainNames.main) ? [] : table.primaryKey, types: store.tree.types)
            node = try scan(table: table, store: store, bound: bound, touched: local, snapshot: plan.sourceSnapshot, filter: nil)
        case .derived(let body, _), .lateralDerived(let body, _):
            let child = try query(body)
            let label = plan.columns.first?.qualifier?.description
            node = ExplainNode("Subquery" + (label.map { " " + $0 } ?? ""), rows: child.rows, cost: child.cost, children: [child])
        case .cte(let reference):
            let binding = reference.binding
            if reference.selfReference { node = ExplainNode("CTE Scan " + binding.name.description, rows: 0, cost: 0) }
            else {
                let child: ExplainNode
                if let existing = bindings[ObjectIdentifier(binding)] { child = existing }
                else if let body = binding.plan { child = try query(body) }
                else { throw JedError("XX000", "CTE estimate has no definition") }
                node = ExplainNode("CTE Scan " + binding.name.description, rows: child.rows,
                    cost: binding.buffers ? child.rows : child.cost, children: binding.buffers ? [] : [child])
            }
        case .function(let function):
            let count = series(function) ?? PlanCardinality.constant("default_srf_rows")
            let call = function.function
            let overhead: Int64 = ["generate_series", "unnest"].contains(call.name) ? 0 : 1
            node = ExplainNode("SRF " + call.name, rows: count, cost: saturatingAdd(count,
                saturatingAdd(overhead, try expressionCost(function.expressions, rows: 1))))
        case .jsonTable:
            let count = PlanCardinality.constant("default_srf_rows")
            node = ExplainNode("SRF json_table", rows: count, cost: count)
        case .system(let system):
            let count = catalogRows(system)
            node = ExplainNode("Catalog Scan " + system.kind.rawValue, ["db=" + system.sourceDomain.description], rows: count, cost: count)
        case .join(let left, let right, let kind, let predicate):
            let physical = JoinPlan.choose(left: left, right: right, kind: kind, predicate: predicate)
            let a = try source(left, touched: touched, offset: offset), b = try source(right, touched: touched, offset: offset + left.columns.count)
            let outer = physical.swapped ? b : a, inner = physical.swapped ? a : b
            let model = PlanCardinality(table: nil, snapshot: nil)
            let product = QueryEstimate.multiply(a.rows, b.rows)
            var count = try model.rows(predicate, input: product)
            if kind == "left" { count = max(count, a.rows) }
            if kind == "right" { count = max(count, b.rows) }
            if kind == "full" { count = max(count, max(a.rows, b.rows)) }
            var detail = [kind], label = "Nested Loop", cost = saturatingAdd(a.cost, b.cost)
            switch physical.algorithm {
            case .nested: cost = saturatingAdd(cost, try expressionCost(predicate.map { [$0] } ?? [], rows: product))
            case .hash(let keys):
                label = "Hash Join"; detail.append("keys=\(keys.count)")
                let width = keys.reduce(Int64(0)) { saturatingAdd($0, Int64(max(1, ValueCodec.fixedWidth($1.type.type) ?? 1))) }
                let hash = QueryEstimate.multiply(saturatingAdd(a.rows, b.rows), width)
                let check = QueryEstimate.multiply(count, saturatingAdd(width, Int64(keys.count * 4)))
                cost = saturatingAdd(cost, saturatingAdd(hash, saturatingAdd(check, try expressionCost(predicate.map { [$0] } ?? [], rows: count))))
            case .indexed(let bound):
                let input = physical.swapped ? left : right
                if case .base(let store) = input.body, let table = input.baseTable {
                    let localOffset = physical.swapped ? offset : offset + left.columns.count
                    let local = touched.filter { $0 >= localOffset && $0 < localOffset + input.columns.count }.map { $0 - localOffset }.sorted()
                    let candidate = try scan(table: table, store: store, bound: bound, touched: local, snapshot: input.sourceSnapshot, filter: nil, joining: true)
                    inner.attributes = candidate.attributes; inner.rows = candidate.rows; inner.cost = candidate.cost
                    cost = saturatingAdd(outer.cost, QueryEstimate.multiply(outer.rows,
                        saturatingAdd(inner.cost, try expressionCost(predicate.map { [$0] } ?? [], rows: inner.rows))))
                    count = try model.rows(predicate, input: QueryEstimate.multiply(outer.rows, inner.rows))
                    if kind == "left" { count = max(count, outer.rows) }
                }
            }
            if let predicate { detail.append(try condition(predicate, label: "on", compact: "on:conjuncts=")) }
            node = ExplainNode(label, detail, rows: count, cost: cost, children: [outer, inner])
        }
        if let filter = plan.scanFilter {
            let model = PlanCardinality(table: plan.baseTable, snapshot: plan.sourceSnapshot, parameters: parameters, collatedKey: context.collatedKey)
            node.cost = saturatingAdd(node.cost, try expressionCost([filter], rows: node.rows))
            node.rows = try model.rows(filter, input: node.logicalRows); node.logicalRows = node.rows
            node.attributes.append(try condition(filter, label: "filter", compact: "filter:conjuncts="))
        }
        let profile = context.executionProfile
        node.actual = { profile?.value(plan, .total) ?? 0 }
        return node
    }
    func scan(table: CatalogTable, store: TableStore, bound: BaseScanPlan, touched: [Int],
                      snapshot: DomainSnapshot?, filter: ResolvedExpression?, joining: Bool = false) throws -> ExplainNode {
        var terms: [(Int, BaseScanPlan.Term)] = [], unique = !bound.keys.isEmpty
        for key in bound.keys {
            guard let found = bound.terms[key], !found.isEmpty else { unique = false; break }
            if let equality = found.first(where: { $0.operation == "=" }) { terms.append((key, equality)) }
            else { terms += found.map { (key, $0) }; unique = false; break }
        }
        var selected = table.rowCount, pages = Int64(store.tree.nodeCount), details: [String]
        if terms.isEmpty { details = ["Full scan"] }
        else {
            let spelled = try terms.map { key, term in
                let source: String
                if case .outerColumn = term.source.node { source = joining ? "join" : "outer" }
                else { source = try ExplainExpression.render(term.source) }
                return table.columns[key].name.description + " " + term.operation + " " + source
            }.joined(separator: " and ")
            details = [(joining ? "Index-nested-loop " : "") + "PK bound: " + spelled]
            if unique { selected = min(selected, 1); pages = selected == 0 ? 0 : Int64(store.tree.height) }
            else {
                for key in bound.keys {
                    let found = terms.filter { $0.0 == key }.map(\.1)
                    if found.isEmpty { break }
                    let equality = found.contains { $0.operation == "=" }
                    let pair = found.contains { [">", ">="].contains($0.operation) } && found.contains { ["<", "<="].contains($0.operation) }
                    let fraction = equality ? "equality" : pair ? "paired_range" : "inequality"
                    selected = QueryEstimate.fraction(fraction, rows: selected); pages = QueryEstimate.fraction(fraction, rows: pages)
                    if !equality { break }
                }
                pages = selected == 0 ? 0 : min(Int64(store.tree.nodeCount), max(Int64(store.tree.height), pages))
            }
        }
        if bound.contradiction || terms.contains(where: { PlanCardinality(table: table, snapshot: snapshot, parameters: parameters).known($0.1.source)?.isNull == true }) {
            selected = 0; pages = 0
        }
        if !touched.isEmpty { details.append("touched=\(touched.count)") }
        return ExplainNode("Scan " + table.name.description, details, rows: selected, cost: saturatingAdd(pages, selected), logicalRows: table.rowCount)
    }
    func expressionCost(_ expressions: [ResolvedExpression], rows: Int64) throws -> Int64 {
        var cost: Int64 = 0, pending = expressions, subqueries: Set<ObjectIdentifier> = []
        while let expression = pending.popLast() {
            if case .subquery(let subquery) = expression.node {
                cost = saturatingAdd(cost, rows)
                if !subquery.correlated, !subqueries.insert(ObjectIdentifier(subquery)).inserted { continue }
                let child = try query(subquery.plan)
                cost = saturatingAdd(cost, subquery.correlated ? QueryEstimate.multiply(rows, child.cost) : child.cost)
                continue
            }
            let own: Int64
            switch expression.node {
            case .constant, .column, .outerColumn, .parameter, .convert: own = 0
            case .call(let call): own = saturatingAdd(call.cost, ["nextval", "setval", "timezone"].contains(call.name) ? 1 : 0)
            case .quantified(_, _, _, let array, _):
                let model = PlanCardinality(table: nil, snapshot: nil, parameters: parameters)
                if case .array(let value) = model.known(array) { own = saturatingAdd(1, Int64(value.elements.count)) }
                else if case .array(let values, _) = array.node { own = saturatingAdd(1, Int64(values.count)) }
                else { own = 1 }
            default: own = 1
            }
            cost = saturatingAdd(cost, QueryEstimate.multiply(rows, own))
            pending.append(contentsOf: StoredExpressionDependencies.children(expression))
        }
        return cost
    }
    func condition(_ expression: ResolvedExpression, label: String, compact: String) throws -> String {
        options.verbose ? label + "=" + (try ExplainExpression.render(expression)) : compact + String(QueryEstimate.conjuncts(expression).count)
    }
    private func mark(_ node: ExplainNode, _ plan: SelectPlan, _ stage: ExecutionProfile.Stage) {
        let profile = context.executionProfile, identity = plan.profileIdentity
        node.actual = { profile?.cumulative(identity, through: stage) ?? 0 }
    }
    private func markRoot(_ node: ExplainNode, _ plan: SelectPlan) {
        let profile = context.executionProfile, identity = plan.profileIdentity
        node.actual = { profile?.value(identity, .total) ?? 0 }
    }
    private func zero(_ node: ExplainNode) { node.rows = 0; node.logicalRows = 0; node.cost = 0; for child in node.children { zero(child) } }
    private func series(_ function: TableFunctionPlan) -> Int64? {
        guard function.function.name == "generate_series" else { return nil }
        let model = PlanCardinality(table: nil, snapshot: nil, parameters: parameters)
        let args = function.expressions.map(model.known)
        if args.contains(where: { $0?.isNull == true }) { return 0 }
        guard args.count >= 2, case .integer(let start) = args[0], case .integer(let end) = args[1] else { return nil }
        let step: Int64
        if args.count > 2 { guard case .integer(let value) = args[2] else { return nil }; step = value } else { step = 1 }
        guard step != 0 else { return 0 }
        if step > 0 && start > end || step < 0 && start < end { return 0 }
        let count = (Int128(end) - Int128(start)) / Int128(step) + 1
        return Int64(min(Int128(Int64.max), count))
    }
    private func catalogRows(_ plan: SystemRelationPlan) -> Int64 {
        let image = plan.snapshot.catalog
        switch plan.kind {
        case .tables: return Int64(image.tables.count)
        case .columns: return Int64(image.tables.reduce(0) { $0 + $1.columns.count } + image.views.reduce(0) { $0 + $1.columns.count })
        case .indexes: return Int64(image.tables.reduce(0) { $0 + $1.indexes.count })
        case .constraints: return Int64(image.tables.reduce(0) { $0 + $1.checks.count + $1.foreignKeys.count + $1.exclusions.count + $1.indexes.filter { $0.unique && $0.isConstraint }.count })
        case .statistics: return Int64(image.statistics.count)
        case .sequences: return Int64(image.sequences.count)
        case .types: return Int64(image.composites.count)
        case .fields: return Int64(image.composites.reduce(0) { $0 + $1.fields.count })
        case .views: return Int64(image.views.count)
        }
    }
}
