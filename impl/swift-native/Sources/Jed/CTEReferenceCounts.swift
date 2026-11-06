/// A single syntax walk counts all relation names used by an unplanned DML
/// part. Targets are catalog names; only FROM references contribute. Inner
/// WITH declarations shadow outer bindings in their lexical visibility region.
internal enum CTEReferenceCounts {
    /// Count each later part once. A DML reference disqualifies expansion even
    /// when its syntax contains a SELECT, because that part plans at execution.
    static func expansionCandidates(_ clause: SQLWith, primary: SQLStatement?) -> Set<SQLText> {
        var queries: [SQLText: Int] = [:], mutations: [SQLText: Int] = [:]
        func add(_ statement: SQLStatement) {
            let counts = count(statement)
            if statement.queryBody != nil {
                for (name, count) in counts { queries[name, default: 0] += count }
            } else {
                for (name, count) in counts { mutations[name, default: 0] += count }
            }
        }
        if let primary { add(primary) }
        var result: Set<SQLText> = []
        for binding in clause.bindings.reversed() {
            if binding.body.queryBody != nil, binding.materialized != true,
               queries[binding.name] == 1, mutations[binding.name, default: 0] == 0 {
                result.insert(binding.name)
            }
            add(binding.body)
        }
        return result
    }

    static func count(_ statement: SQLStatement) -> [SQLText: Int] {
        var counts: [SQLText: Int] = [:], shadowed: Set<SQLText> = []
        func expression(_ value: SQLExpression) {
            var pending = [value]
            while let value = pending.popLast() {
                switch value.kind {
                case .scalarQuery(let value), .exists(let value): query(value)
                case .memberQuery(let left, let value, _), .quantifiedQuery(_, let left, _, let value):
                    expression(left); query(value)
                case .call(let call): if let window = call.over { visitWindow(window) }
                default: break
                }
                pending.append(contentsOf: CatalogExpressionPolicy.children(value).reversed())
            }
        }
        func visitWindow(_ value: SQLWindow) {
            for item in RecursiveCTEPlan.windowExpressions(value) { expression(item) }
        }
        func group(_ value: SQLGroup) {
            switch value {
            case .set(let values): for value in values { expression(value) }
            case .rollup(let values), .cube(let values), .sets(let values): for value in values { group(value) }
            }
        }
        func tableColumns(_ columns: [SQLJSONTableColumn]) {
            for column in columns {
                switch column {
                case .ordinality: break
                case .regular(_, _, _, _, _, _, _, let empty, let error):
                    for value in [empty?.value, error?.value].compactMap({ $0 }) { expression(value) }
                case .nested(_, _, let columns): tableColumns(columns)
                }
            }
        }
        func from(_ value: SQLFrom) {
            switch value.source {
            case .table(let name):
                if name.database == nil && !shadowed.contains(name.name) { counts[name.name, default: 0] += 1 }
            case .query(let value): query(value)
            case .function(let value): expression(value)
            case .joined(let value): from(value)
            case .jsonTable(let value):
                expression(value.context); expression(value.path)
                for item in value.passing { expression(item.value) }
                tableColumns(value.columns)
                if let value = value.onError?.value { expression(value) }
            }
            for join in value.joins { from(join.right); if let value = join.condition { expression(value) } }
        }
        func withClause(_ clause: SQLWith, body: () -> Void) {
            let prior = shadowed
            defer { shadowed = prior }
            for binding in clause.bindings {
                if clause.recursive { shadowed.insert(binding.name) }
                visit(binding.body)
                if let cycle = binding.cycle {
                    for value in [cycle.markValue, cycle.defaultValue].compactMap({ $0 }) { expression(value) }
                }
                shadowed.insert(binding.name)
            }
            body()
        }
        func query(_ value: SQLQuery) {
            switch value.body {
            case .select(let select):
                for item in select.items { if let value = item.expression { expression(value) } }
                for value in select.distinctOn { expression(value) }
                for value in select.from { from(value) }
                for value in [select.predicate, select.having].compactMap({ $0 }) { expression(value) }
                for value in select.grouping { group(value) }
                for window in select.windows { visitWindow(window.definition) }
            case .values(let rows): for row in rows { for value in row { expression(value) } }
            case .set(_, _, let a, let b): query(a); query(b)
            case .nested(let value): query(value)
            case .with(let clause, let body):
                withClause(clause) {
                    query(body)
                    for item in value.orderBy { expression(item.expression) }
                }
                return
            }
            for item in value.orderBy { expression(item.expression) }
        }
        func returning(_ syntax: SQLReturning?) {
            for item in syntax?.items ?? [] { if let value = item.expression { expression(value) } }
        }
        func visit(_ statement: SQLStatement) {
            switch statement {
            case .query(let value): query(value)
            case .with(let clause, let body): withClause(clause) { visit(body) }
            case .insert(let syntax):
                switch syntax.source {
                case .defaultValues: break
                case .values(let rows): for row in rows { for value in row.compactMap({ $0 }) { expression(value) } }
                case .query(let value): query(value)
                }
                if let conflict = syntax.conflict {
                    for value in [conflict.predicate, conflict.updatePredicate].compactMap({ $0 }) { expression(value) }
                    for item in conflict.assignments { if let value = item.value { expression(value) } }
                }
                returning(syntax.returning)
            case .update(let syntax):
                for item in syntax.assignments { if let value = item.value { expression(value) } }
                if let value = syntax.predicate { expression(value) }; returning(syntax.returning)
            case .delete(let syntax):
                if let value = syntax.predicate { expression(value) }; returning(syntax.returning)
            case .merge(let syntax):
                from(syntax.source); expression(syntax.condition)
                for clause in syntax.clauses {
                    if let value = clause.condition { expression(value) }
                    switch clause.action {
                    case .update(let items): for item in items { if let value = item.value { expression(value) } }
                    case .insert(_, _, let values): for value in (values ?? []).compactMap({ $0 }) { expression(value) }
                    default: break
                    }
                }
                returning(syntax.returning)
            default: break
            }
        }
        visit(statement)
        return counts
    }
}
