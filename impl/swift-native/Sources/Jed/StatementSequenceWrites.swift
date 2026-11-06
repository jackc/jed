/// A private relation target does not by itself prove a private statement:
/// defaults, CHECKs, source views, and caller expressions may advance main.
/// Admission inspects syntax and resident catalog facts, never evaluated names.
internal enum StatementSequenceWrites {
    private struct Action: Hashable {
        let table: SQLText
        let deleting: Bool
        let assigned: Set<Int>
    }
    static func requiresSharedWriter(_ statement: SQLStatement, domains: QueryDomains,
                                     settings: SessionSettings) -> Bool {
        if case .explain(let options, _) = statement, !options.analyze { return false }
        var shared = false
        let temporary = Set(domains.snapshots[DomainNames.temporary]?.catalog.sequences.map(\.name) ?? [])
        let requirements = StatementPrivileges(domains: domains)
        var actions: [Action] = []
        requirements.observeWrite = { table, statement, owner in
            guard owner == DomainNames.temporary else { return }
            func update(_ items: [SQLAssignment]) {
                let assigned = Set(items.compactMap { item in table.columns.firstIndex { $0.name == item.column } })
                actions.append(Action(table: table.name, deleting: false, assigned: assigned))
            }
            switch statement {
            case .insert(let value):
                if let conflict = value.conflict, !conflict.doNothing { update(conflict.assignments) }
            case .update(let value): update(value.assignments)
            case .delete: actions.append(Action(table: table.name, deleting: true, assigned: []))
            case .merge(let value):
                for clause in value.clauses {
                    switch clause.action {
                    case .update(let items): update(items)
                    case .delete: actions.append(Action(table: table.name, deleting: true, assigned: []))
                    default: break
                    }
                }
            default: break
            }
        }
        requirements.observeCall = { call in
            guard call.name.description == "nextval" || call.name.description == "setval" else { return }
            // An authored name is a proof only when it names a current private
            // sequence. A parameter, computed string, malformed or absent name
            // retains ordinary shared admission and its later SQL diagnostic.
            guard let first = call.arguments.first, first.name == nil,
                  let text = literal(first.value), let name = try? SequenceName.parse(text),
                  temporary.contains(name) else { shared = true; return }
        }
        requirements.collect(statement)
        requirements.collectDependencies(settings: settings)
        // Referential actions may run a child's CHECK or SET DEFAULT without
        // naming that child in the SQL. Follow the possible action graph using
        // changed-column facts, retaining no rows and evaluating no predicates.
        var visited: Set<Action> = []
        let tables = domains.snapshots[DomainNames.temporary]?.catalog.tables ?? []
        while !shared, let action = actions.popLast() {
            guard visited.insert(action).inserted else { continue }
            for table in tables {
                for foreign in table.foreignKeys where foreign.referencedTable == action.table {
                    if !action.deleting && Set(foreign.referencedColumns).isDisjoint(with: action.assigned) { continue }
                    let kind = action.deleting ? foreign.onDelete : foreign.onUpdate
                    guard kind > 1 else { continue }
                    if action.deleting && kind == 2 {
                        actions.append(Action(table: table.name, deleting: true, assigned: []))
                        continue
                    }
                    for check in table.checks { requirements.expression(check.expression.parsed) }
                    if kind == 4 {
                        for column in foreign.columns {
                            if case .expression(let value) = table.columns[column].defaultValue {
                                requirements.expression(value.parsed)
                            }
                        }
                    }
                    actions.append(Action(table: table.name, deleting: false, assigned: Set(foreign.columns)))
                }
            }
        }
        return shared
    }

    private static func literal(_ expression: SQLExpression) -> SQLText? {
        var current = expression
        while case .parenthesized(let child) = current.kind { current = child }
        if case .string(let value) = current.kind { return value }
        return nil
    }
}
