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
    let useUnregisteredCTEPlans: Bool
    private var bindings: [ObjectIdentifier: ExplainNode] = [:]
    private var inlineReferences: [ObjectIdentifier: Bool] = [:]
    init(options: SQLExplain, parameters: [SQLValue]?, context: ExpressionContext,
         useUnregisteredCTEPlans: Bool = true) {
        self.options = options; self.parameters = parameters; self.context = context
        self.useUnregisteredCTEPlans = useUnregisteredCTEPlans
    }
    func register(_ binding: CTEBinding, body: ExplainNode, inlineAtReference: Bool? = nil) {
        let identity = ObjectIdentifier(binding)
        bindings[identity] = body
        if let inlineAtReference { inlineReferences[identity] = inlineAtReference }
    }
    func query(_ input: SelectPlan, source override: PlanEstimate? = nil) throws -> ExplainNode {
        let plan = try override == nil ? input.selectAccess(parameters: parameters, context: context) : input
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
        // Execution addresses grouping keys in sparse source slots. The plan
        // text exposes the compact grouped row, followed by window slots.
        func outputColumn(_ slot: Int) -> Int {
            let physical = plan.window?.slot(slot) ?? slot
            guard let aggregate = plan.aggregation else { return physical }
            if physical < 0 { return aggregate.keys.count + aggregate.calls.count - physical - 1 }
            if let key = aggregate.keyColumns.firstIndex(of: physical) { return key }
            if physical >= aggregate.layoutWidth { return aggregate.keys.count + physical - aggregate.layoutWidth }
            return physical
        }
        let model = PlanCardinality(table: plan.sourceTable, snapshot: plan.sourceSnapshot,
                                   parameters: parameters, collatedKey: context.collatedKey, planningProbe: context.planningProbe)
        var node: ExplainNode
        if let override {
            node = ExplainNode("Scan " + (plan.sourceTable?.name.description ?? "source"), rows: override.rows,
                cost: override.cost, logicalRows: override.logicalRows)
        } else if let selected = plan.chosenNWay {
            node = try nway(selected, plan: plan)
        } else if let selected = plan.chosenJoin {
            node = try join(selected, plan: plan)
        } else if let candidate = plan.chosenAccess {
            node = try scan(candidate, touched: plan.touched)
        } else if let relation = plan.relation { node = try source(relation, touched: Set(plan.touched), offset: 0) }
        else if let store = plan.store, let table = plan.sourceTable {
            node = try scan(table: table, store: store, bound: plan.scan, touched: plan.touched,
                            snapshot: plan.sourceSnapshot, filter: nil)
        } else { node = ExplainNode("Result", rows: 1, cost: 0) }
        mark(node, plan, .source)
        let contradiction = try plan.scan.hasKnownContradiction(parameters: parameters, collatedKey: context.collatedKey, planningProbe: context.planningProbe)
        if contradiction { zero(node) }
        if let limit = plan.limit, plan.relation == nil, plan.aggregation == nil,
           !plan.distinct, plan.distinctOn == nil, plan.order.isEmpty || plan.orderedByScan {
            let target = saturatingAdd(limit, plan.offset)
            let pulled = try SelectEstimate.requiredInput(model: model, predicate: plan.predicate,
                target: target, maximum: node.rows)
            if pulled < node.rows {
                let prefix = try override?.prefix ?? plan.chosenAccess?.estimate(parameters: parameters, context: context).prefix
                    ?? PlanEstimate.ScanPrefix(scanPages: max(0, node.cost - node.rows),
                        minimumScanPages: Int64(plan.store?.tree.height ?? 0), pointFetchPagesPerRow: 0)
                let perRow = saturatingAdd(1, prefix.pointFetchPagesPerRow)
                node.rows = pulled
                node.cost = saturatingAdd(QueryEstimate.multiply(pulled, perRow), prefix.pages(pulling: pulled))
            }
        }
        if plan.orderedByScan, !plan.order.isEmpty {
            if let index = plan.chosenAccess?.orderedDefinition?.index.name {
                node.attributes.append("ordered: index order: " + index.description)
            } else {
                node.attributes.append(plan.relation == nil ? "ordered: pk ordered" + (plan.descending ? " (reverse)" : "") : "ordered: join pk ordered")
            }
        }
        if let predicate = plan.predicate {
            let logical: Int64
            if let join = plan.chosenNWay {
                logical = try join.cardinality(parameters: parameters, context: context).rows(predicate, input: node.logicalRows)
            } else if let join = plan.chosenJoin {
                logical = try JoinCardinality(left: join.left, right: join.right, parameters: parameters,
                    collatedKey: context.collatedKey, planningProbe: context.planningProbe).rows(predicate, input: node.logicalRows)
            } else { logical = try model.rows(predicate, input: node.logicalRows) }
            let cost = saturatingAdd(node.cost, try expressionCost([predicate], rows: node.rows))
            var attributes = [try condition(predicate, label: "filter", compact: "conjuncts=")]
            if contradiction { attributes.append("contradiction") }
            node = ExplainNode("Filter", attributes, rows: min(node.rows, logical), cost: cost, logicalRows: logical, children: [node])
            mark(node, plan, .filter)
        }
        if let aggregate = plan.aggregation {
            let groups = aggregate.keys.isEmpty ? 1 : model.distinct(aggregate.keys, input: node.rows)
            var count = QueryEstimate.multiply(groups, Int64(max(1, aggregate.sets.count)))
            var cost = saturatingAdd(node.cost, QueryEstimate.multiply(node.rows, Int64(aggregate.calls.count)))
            cost = saturatingAdd(cost, try expressionCost(aggregate.keys, rows: node.rows))
            for call in aggregate.calls {
                cost = saturatingAdd(cost, try expressionCost(call.arguments + call.order.map(\.expression) + (call.filter.map { [$0] } ?? []), rows: node.rows))
            }
            var attributes = ["groups=\(aggregate.keys.count) aggs=\(aggregate.calls.count)"]
            if aggregate.sets.count > 1 { attributes.append("sets=\(aggregate.sets.count)") }
            if let having = aggregate.having {
                cost = saturatingAdd(cost, try expressionCost([having], rows: count))
                count = try model.rows(having, input: count)
                attributes.append(options.verbose ? "having=" + (try ExplainExpression.render(having, column: outputColumn))
                    : "having:conjuncts=" + String(QueryEstimate.conjuncts(having).count))
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
            if let limit = plan.limit, !plan.distinct, plan.distinctOn == nil,
               plan.aggregation == nil, plan.window == nil {
                let (count, overflow) = plan.offset.addingReportingOverflow(limit)
                if limit == 0 || !overflow { detail += ", top-k=\(limit == 0 ? 0 : count)" }
            }
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
        if options.verbose { node.attributes.append("output=[" + (try plan.projections.map { try ExplainExpression.render($0, column: outputColumn) }).joined(separator: ", ") + "]") }
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
    func source(_ plan: RelationPlan, touched: Set<Int>, offset: Int) throws -> ExplainNode {
        var node: ExplainNode
        var touched = touched
        if case .join(_, let right, _, let predicate) = plan.body {
            if let predicate {
                StoredExpressionDependencies.walk([predicate]) { if case .column(let column) = $0.node { touched.insert(offset + column) } }
            }
            touched.formUnion(right.lateralColumns.filter { $0 >= plan.precedingWidth }.map { offset + $0 - plan.precedingWidth })
        }
        switch plan.body {
        case .base(let store):
            guard let table = plan.baseTable else { throw JedError("XX000", "base relation lacks catalog identity") }
            let local = touched.filter { $0 >= offset && $0 < offset + plan.columns.count }.map { $0 - offset }.sorted()
            let bound = BaseScanPlan(predicate: plan.scanFilter, keys: DomainNames.isAttachment(plan.sourceDomain ?? DomainNames.main) ? [] : table.primaryKey, types: store.tree.types)
            node = try scan(table: table, store: store, bound: bound, touched: local, snapshot: plan.sourceSnapshot, filter: nil)
        case .derived(let body, _), .lateralDerived(let body, _):
            let child = try query(body)
            let label = plan.subqueryAlias?.description
            node = ExplainNode("Subquery" + (label.map { " " + $0 } ?? ""), rows: child.rows, cost: child.cost, children: [child])
        case .cte(let reference):
            let binding = reference.binding
            if reference.selfReference || !useUnregisteredCTEPlans && bindings[ObjectIdentifier(binding)] == nil {
                node = ExplainNode("CTE Scan " + binding.name.description, rows: 0, cost: 0)
            }
            else {
                let child: ExplainNode
                if let inline = reference.inlinePlan { child = try query(inline) }
                else if let existing = bindings[ObjectIdentifier(binding)] { child = existing }
                else if let body = binding.plan { child = try query(body) }
                else { throw JedError("XX000", "CTE estimate has no definition") }
                node = ExplainNode("CTE Scan " + binding.name.description, rows: child.rows,
                    cost: binding.buffers ? child.rows : child.cost,
                    children: binding.buffers || inlineReferences[ObjectIdentifier(binding)] == false ? [] : [child])
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
            let residual = plan.effectiveOn
            let a = try source(left, touched: touched, offset: offset)
            let rightNode = try source(right, touched: touched, offset: offset + left.columns.count)
            let b = plan.repeatsRight ? repeatedSource(rightNode, times: a.rows) : rightNode
            let outer = physical.swapped ? b : a, inner = physical.swapped ? a : b
            let model = JoinCardinality(left: left, right: right, parameters: parameters, collatedKey: context.collatedKey, planningProbe: context.planningProbe)
            let product = QueryEstimate.multiply(a.rows, rightNode.rows)
            var count = try model.rows(residual, input: product)
            if kind == "left" { count = max(count, a.rows) }
            if kind == "right" { count = max(count, rightNode.rows) }
            if kind == "full" { count = max(count, max(a.rows, rightNode.rows)) }
            var detail = [kind], label = "Nested Loop", cost = saturatingAdd(a.cost, b.cost)
            switch physical.algorithm {
            case .nested: cost = saturatingAdd(cost, try expressionCost(residual.map { [$0] } ?? [], rows: product))
            case .hash(let keys):
                label = "Hash Join"; detail.append("keys=\(keys.count)")
                let width = keys.reduce(Int64(0)) { saturatingAdd($0, Int64(max(1, ValueCodec.fixedWidth($1.type.type) ?? 1))) }
                let hash = QueryEstimate.multiply(saturatingAdd(a.rows, b.rows), width)
                let check = QueryEstimate.multiply(count, saturatingAdd(width, Int64(keys.count * 4)))
                cost = saturatingAdd(cost, saturatingAdd(hash, saturatingAdd(check, try expressionCost(residual.map { [$0] } ?? [], rows: count))))
            case .indexed(let bound):
                let input = physical.swapped ? left : right
                if case .base(let store) = input.body, let table = input.baseTable {
                    let localOffset = physical.swapped ? offset : offset + left.columns.count
                    let local = touched.filter { $0 >= localOffset && $0 < localOffset + input.columns.count }.map { $0 - localOffset }.sorted()
                    let candidate = try scan(table: table, store: store, bound: bound, touched: local, snapshot: input.sourceSnapshot, filter: nil, joining: true)
                    // The correlated bound already applies the join equality.
                    // Its delivered pairs are not reduced a second time by ON.
                    count = QueryEstimate.multiply(outer.rows, candidate.rows)
                    cost = saturatingAdd(outer.cost, QueryEstimate.multiply(outer.rows,
                        saturatingAdd(candidate.cost, try expressionCost(residual.map { [$0] } ?? [], rows: candidate.rows))))
                    inner.attributes = candidate.attributes
                    inner.rows = count; inner.logicalRows = count
                    inner.cost = QueryEstimate.multiply(outer.rows, candidate.cost)
                    if kind == "left" { count = max(count, outer.rows) }
                }
            }
            if let residual { detail.append(try condition(residual, label: "on", compact: "on:conjuncts=")) }
            node = ExplainNode(label, detail, rows: count, cost: cost, children: [outer, inner])
        }
        if let filter = plan.scanFilter {
            let model = PlanCardinality(table: plan.baseTable, snapshot: plan.sourceSnapshot, parameters: parameters, collatedKey: context.collatedKey, planningProbe: context.planningProbe)
            node.cost = saturatingAdd(node.cost, try expressionCost([filter], rows: node.rows))
            node.rows = try model.rows(filter, input: node.logicalRows); node.logicalRows = node.rows
            node.attributes.append(try condition(plan.scanDisplayFilter ?? filter, label: "filter", compact: "filter:conjuncts="))
        }
        let profile = context.executionProfile
        node.actual = { profile?.value(plan, .total) ?? 0 }
        return node
    }
    private func repeatedSource(_ node: ExplainNode, times: Int64) -> ExplainNode {
        let copy = ExplainNode(node.name, node.attributes, rows: QueryEstimate.multiply(node.rows, times),
            cost: QueryEstimate.multiply(node.cost, times), logicalRows: QueryEstimate.multiply(node.logicalRows, times),
            children: node.children.map { repeatedSource($0, times: times) })
        copy.actual = node.actual
        return copy
    }
    func scan(_ candidate: ScanCandidate, touched: [Int]) throws -> ExplainNode {
        let estimate = try candidate.estimate(parameters: parameters, context: context)
        let node: ExplainNode
        if let intervals = candidate.intervals {
            var attributes = intervals.explain(table: candidate.table)
            if !touched.isEmpty { attributes.append("touched=\(touched.count)") }
            node = ExplainNode("Scan " + candidate.table.name.description, attributes, rows: estimate.rows,
                cost: estimate.cost, logicalRows: estimate.logicalRows)
        } else if let bound = candidate.ordered {
            let access = bound.bounded ? "Index bound: using " + bound.definition.index.name.description
                : bound.definition.predicate != nil ? "Index scan: using " + bound.definition.index.name.description
                : "Full scan"
            var attributes = [access]
            if !touched.isEmpty { attributes.append("touched=\(touched.count)") }
            node = ExplainNode("Scan " + candidate.table.name.description, attributes, rows: estimate.rows,
                cost: estimate.cost, logicalRows: estimate.logicalRows)
        } else if let gist = candidate.gist {
            var attributes = ["GiST bound: using " + gist.index.name.description]
            if !touched.isEmpty { attributes.append("touched=\(touched.count)") }
            node = ExplainNode("Scan " + candidate.table.name.description, attributes, rows: estimate.rows,
                cost: estimate.cost, logicalRows: estimate.logicalRows)
        } else if let gin = candidate.gin {
            var attributes = ["GIN bound: using " + gin.index.name.description]
            if !touched.isEmpty { attributes.append("touched=\(touched.count)") }
            node = ExplainNode("Scan " + candidate.table.name.description, attributes, rows: estimate.rows,
                cost: estimate.cost, logicalRows: estimate.logicalRows)
        } else {
            node = try scan(table: candidate.table, store: candidate.store, bound: candidate.primary!, touched: touched,
                snapshot: candidate.snapshot, filter: nil)
            node.rows = estimate.rows; node.cost = estimate.cost; node.logicalRows = estimate.logicalRows
        }
        return node
    }
    func scan(table: CatalogTable, store: TableStore, bound: BaseScanPlan, touched: [Int],
                      snapshot: DomainSnapshot?, filter: ResolvedExpression?, joining: Bool = false) throws -> ExplainNode {
        var terms: [(Int, BaseScanPlan.Term)] = [], unique = !bound.keys.isEmpty
        for key in bound.keys {
            guard let found = bound.terms[key], !found.isEmpty else { unique = false; break }
            terms += found.map { (key, $0) }
            if !found.contains(where: { $0.operation == "=" }) { unique = false; break }
        }
        var selected = table.rowCount, pages = Int64(store.tree.nodeCount), details: [String]
        if terms.isEmpty { details = ["Full scan"] }
        else {
            let spelled = try terms.map { key, term in
                let source: String
                if case .outerColumn = term.source.node { source = joining ? "join" : "outer" }
                else { source = try ExplainExpression.bound(term.source) }
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
        if try bound.hasKnownContradiction(parameters: parameters, collatedKey: context.collatedKey, planningProbe: context.planningProbe) {
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
                let model = PlanCardinality(table: nil, snapshot: nil, parameters: parameters, planningProbe: context.planningProbe)
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
        let model = PlanCardinality(table: nil, snapshot: nil, parameters: parameters, planningProbe: context.planningProbe)
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
