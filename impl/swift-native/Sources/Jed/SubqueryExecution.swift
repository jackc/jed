/// One runtime per statement, shared by every nested block. Uncorrelated
/// results remain live for the statement; correlated results release after use.
internal final class SubqueryExecution {
    private final class Result {
        let rows: [[SQLValue]]
        let account: QueryMemoryAccount
        let bytes: Int64
        init(rows: [[SQLValue]], account: QueryMemoryAccount, bytes: Int64) {
            self.rows = rows; self.account = account; self.bytes = bytes
        }
        deinit { account.release(bytes) }
    }
    private var folded: [ObjectIdentifier: Result] = [:]

    func values(_ query: ResolvedSubquery) -> [[SQLValue]]? { folded[ObjectIdentifier(query)]?.rows }

    func prepare(_ expressions: [ResolvedExpression], context: ExpressionContext) throws {
        var pending = Array(expressions.reversed())
        while let expression = pending.popLast() {
            if case .subquery(let query) = expression.node {
                if let operand = query.operand { try prepare([operand], context: context) }
                try prepare(query.plan.foldExpressions, context: context)
                if !query.correlated, folded[ObjectIdentifier(query)] == nil {
                    let result = try execute(query, context: context)
                    if case .scalar = query.kind, result.rows.count > 1 { throw JedError("21000", "more than one row returned by a subquery used as an expression") }
                    folded[ObjectIdentifier(query)] = result
                }
            } else { pending.append(contentsOf: StoredExpressionDependencies.children(expression).reversed()) }
        }
    }

    private func execute(_ query: ResolvedSubquery, context: ExpressionContext, runtime: Bool = false) throws -> Result {
        var current = context
        // Correlated bodies keep their query-block counter while sharing the
        // lifetime and allocation accounts, then fold cost on successful return.
        if query.correlated || runtime {
            current.meter = CostMeter(limit: context.meter.limit, lifetime: context.meter.lifetime,
                resources: context.meter.resources, cancellation: context.meter.cancellation)
            current.scalar.meter = current.meter
            current.sequences?.install(into: &current)
        }
        current.outerRows = [context.column] + context.outerRows
        current.encloseHostBatchRow(from: context)
        // The owning fold already visited this subplan. Runtime-only bodies do
        // not acquire a new eager fold when entered from an expression.
        current.eagerSubqueries = false
        current.patterns = PatternExecution()
        StatementServices(settings: query.settings, meter: current.meter).install(into: &current)
        var plan = query.plan
        // Materialized query blocks retain their source while collecting the
        // projected result. Specialized aggregate and columnar lanes own theirs.
        if !plan.streamsComputedProjection, !ColumnarExecution.projection(plan, current) { plan.materializeSource = plan.aggregation == nil && plan.window == nil && plan.relation == nil && !plan.orderedByScan && plan.order.isEmpty && plan.limit == nil }
        let execution = SelectExecution(plan: plan, context: current)
        defer { execution.close() }
        var rows: [[SQLValue]] = [], bytes: Int64 = 0
        do {
            while let row = try execution.next() {
                let charge = MemorySizes.row(row)
                try current.meter.reserveQuery(charge)
                bytes = saturatingAdd(bytes, charge); rows.append(row)
            }
        } catch { context.meter.resources.query.release(bytes); throw error }
        if query.correlated || runtime { context.meter.charge(current.meter.accrued) }
        return Result(rows: rows, account: context.meter.resources.query, bytes: bytes)
    }

    func evaluate(_ query: ResolvedSubquery, context: ExpressionContext) throws -> SQLValue {
        let result: Result
        let runtime = query.correlated || ((!context.eagerSubqueries || query.runtimeOrderKey)
            && folded[ObjectIdentifier(query)] == nil)
        if runtime {
            context.meter.charge(1); try context.meter.step()
            result = try execute(query, context: context, runtime: true)
        } else {
            if folded[ObjectIdentifier(query)] == nil {
                // Standalone expression callers also use the same once-only
                // machinery; query drivers eagerly prepare before their scan.
                try prepare([ResolvedExpression(type: query.type, node: .subquery(query), position: 0)], context: context)
            }
            result = folded[ObjectIdentifier(query)]!
        }
        switch query.kind {
        case .scalar:
            guard result.rows.count <= 1 else { throw JedError("21000", "more than one row returned by a subquery used as an expression") }
            guard let row = result.rows.first else { return .null }
            return query.rowArity == nil ? row[0] : .composite(row)
        case .exists: return .boolean(!result.rows.isEmpty)
        case .membership(let operation, let all, let negated, let membership, let operand):
            let rows = result.rows
            if !runtime { context.meter.charge(1); try context.meter.step() }
            let once = try context.evaluate(operand)
            var unknown = false, matched = false
            for row in rows {
                context.meter.charge(1); try context.meter.step()
                var left = once
                var right: SQLValue = query.rowArity == nil ? row[0] : .composite(row)
                let type = query.type ?? TypeDescriptor(.scalar(.text))
                if query.rowArity == nil {
                    left = try context.convert(left, from: operand.type?.type, to: type)
                    right = try context.convert(right, from: query.plan.columns[0].type?.type, to: type)
                }
                try context.comparisonWork(operation, left, right, type: type)
                let comparison = try ValueComparison.sql(operation, left, right, type: type, collatedKey: context.collatedKey)
                if case .boolean(let value) = comparison, value != all {
                    if !membership { return .boolean(negated ? !value : value) }
                    matched = true
                }
                if comparison.isNull { unknown = true }
            }
            // The shared IN-subquery contract visits every result element;
            // ANY/ALL above short-circuit. A match dominates all NULLs.
            if matched { return .boolean(!negated) }
            return unknown ? .null : .boolean(negated ? !all : all)
        }
    }
}
