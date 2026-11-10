/// Authorization is a separate, unmetered admission phase. It visits syntax
/// without planning it, so a later missing name/type cannot conceal a denial on
/// an existing object (session.md §5.3; create-table-as.md §3).
internal final class StatementPrivileges {
    struct TableRequirement { let name: SQLName; var privileges: PrivilegeSet }
    private struct ViewKey: Hashable { let domain: SQLText; let name: SQLText }
    let domains: QueryDomains
    var tables: [TableRequirement] = []
    var functions: [SQLText] = []
    var reads: [SQLName] = []
    var catalogWrites: [SQLStatement] = []
    var ddl = false
    var temporary = false
    var observeCall: ((SQLCall) -> Void)?
    var observeWrite: ((CatalogTable, SQLStatement, SQLText) -> Void)?
    init(domains: QueryDomains) { self.domains = domains }

    /// A session that may do anything: `check` has nothing to visit.
    static func unrestricted(_ settings: SessionSettings) -> Bool {
        settings.options.allowDDL && (settings.options.allowTemporaryDDL ?? settings.options.allowDDL)
            && settings.privileges.isUnrestricted
    }
    static func check(_ statement: SQLStatement, snapshot: DomainSnapshot, settings: SessionSettings) throws {
        if unrestricted(settings) { return }
        let domains = QueryDomains.statement(snapshot: snapshot, settings: settings)
        let requirements = StatementPrivileges(domains: domains)
        requirements.collect(statement)
        requirements.collectDependencies(settings: settings)
        if case .reindex(let value) = statement, value.object == "index", let name = value.name,
           let owner = try? domains.relation(name),
           let table = owner.snapshot.catalog.tables.first(where: { $0.indexes.contains { $0.name == name.name } }) {
            requirements.need(SQLName(database: owner.name, name: table.name, span: name.span), .select)
        }
        if requirements.ddl {
            let temporary = requirements.temporary || requirements.targetsTemporaryDDL(statement)
            let allowed = temporary ? settings.options.allowTemporaryDDL ?? settings.options.allowDDL : settings.options.allowDDL
            guard allowed else { throw JedError("42501", "DDL is not permitted in this session") }
        }
        for table in requirements.tables {
            guard let owner = try? domains.relation(table.name) else { continue }
            if owner.name == DomainNames.temporary && !SystemRelationPlan.contains(table.name.name) { continue }
            if SystemRelationPlan.contains(table.name.name) || owner.snapshot.catalog.tables.contains(where: { $0.name == table.name.name })
                || owner.snapshot.catalog.views.contains(where: { $0.name == table.name.name }) {
                try settings.privileges.require(table.privileges, on: table.name.name)
            }
        }
        for function in requirements.functions { try settings.privileges.require(.execute, on: function, function: true) }
    }
    private func qualified(_ name: SQLName) -> SQLName {
        guard name.database == nil, let owner = try? domains.relation(name) else { return name }
        return SQLName(database: owner.name, name: name.name, span: name.span)
    }
    func need(_ name: SQLName, _ privileges: PrivilegeSet) {
        tables.append(TableRequirement(name: qualified(name), privileges: privileges))
    }
    private func targetsTemporaryDDL(_ statement: SQLStatement) -> Bool {
        func temporary(_ name: SQLName) -> Bool {
            guard let owner = try? domains.relation(name) else { return false }
            return owner.name == DomainNames.temporary && QueryDomains.hasRelation(name.name, in: owner.snapshot)
        }
        switch statement {
        case .createTable(let value): return value.temporary || value.name.database == DomainNames.temporary
        case .createView(let value): return value.temporary || value.name.database == DomainNames.temporary
        case .createIndex(let value): return temporary(value.table)
        case .alterTable(let value): return temporary(value.name)
        case .alterView(let value): return temporary(value.name)
        case .alterSequence(let value): return temporary(value.name)
        case .drop(let value) where value.object != "type": return value.names.contains(where: temporary)
        case .explain(_, let body): return targetsTemporaryDDL(body)
        default: return false
        }
    }
    func function(_ name: SQLText) { if !functions.contains(name) { functions.append(name) } }
    func collect(_ statement: SQLStatement, locals: Set<SQLText> = []) {
        switch statement {
        case .query(let value): query(value, locals: locals)
        case .with(let clause, let body): collect(body, locals: with(clause, locals: locals))
        case .explain(_, let body): collect(body, locals: locals)
        case .createTable(let value):
            ddl = true; temporary = value.temporary
            if value.withData, let body = value.query { query(body, locals: locals) }
        case .createView(let value): ddl = true; temporary = value.temporary || value.name.database?.description == "temp"
        case .createType, .createSequence, .alterSequence, .createIndex, .alterTable, .alterView, .drop: ddl = true
        case .analyze(let name, _): ddl = true; need(name, .select)
        case .reindex(let value):
            ddl = true
            if value.object == "table", let name = value.name { need(name, .select) }
        case .insert(let value):
            catalogWrites.append(statement); need(value.table, .insert)
            if let conflict = value.conflict {
                if !conflict.columns.isEmpty || conflict.constraint != nil { need(value.table, .select) }
                if !conflict.doNothing { need(value.table, .update) }
            }
            if returnsColumns(value.returning) { need(value.table, .select) }
            switch value.source {
            case .query(let body): query(body, locals: locals)
            case .values(let rows): for row in rows { for item in row { expression(item, locals: locals) } }
            case .defaultValues: break
            }
            if let conflict = value.conflict, !conflict.doNothing {
                for item in conflict.assignments { expression(item.value, locals: locals) }
                expression(conflict.updatePredicate, locals: locals)
            }
            returning(value.returning, locals: locals)
        case .update(let value):
            catalogWrites.append(statement); need(value.table, .update)
            if readsColumns(value.predicate) || returnsColumns(value.returning) || value.assignments.contains(where: { readsColumns($0.value) }) {
                need(value.table, .select)
            }
            for item in value.assignments { expression(item.value, locals: locals) }
            expression(value.predicate, locals: locals); returning(value.returning, locals: locals)
        case .delete(let value):
            catalogWrites.append(statement); need(value.table, .delete)
            if readsColumns(value.predicate) || returnsColumns(value.returning) { need(value.table, .select) }
            expression(value.predicate, locals: locals); returning(value.returning, locals: locals)
        case .merge(let value):
            catalogWrites.append(statement); need(value.table, .select)
            for clause in value.clauses {
                switch clause.action {
                case .insert: need(value.table, .insert)
                case .update: need(value.table, .update)
                case .delete: need(value.table, .delete)
                case .nothing: break
                }
            }
            from(value.source, locals: locals); expression(value.condition, locals: locals)
            for clause in value.clauses {
                expression(clause.condition, locals: locals)
                switch clause.action {
                case .update(let items): for item in items { expression(item.value, locals: locals) }
                case .insert(_, _, let values): for value in values ?? [] { expression(value, locals: locals) }
                default: break
                }
            }
            returning(value.returning, locals: locals)
        default: break
        }
    }
    private func with(_ clause: SQLWith, locals: Set<SQLText>) -> Set<SQLText> {
        var scope = locals
        for binding in clause.bindings {
            if clause.recursive { scope.insert(binding.name) }
            collect(binding.body, locals: scope); scope.insert(binding.name)
            expression(binding.cycle?.markValue, locals: scope); expression(binding.cycle?.defaultValue, locals: scope)
        }
        return scope
    }
    func query(_ value: SQLQuery, locals: Set<SQLText> = []) {
        switch value.body {
        case .select(let value):
            for source in value.from { from(source, locals: locals) }
            for item in value.items { expression(item.expression, locals: locals) }
            for item in value.distinctOn { expression(item, locals: locals) }
            expression(value.predicate, locals: locals)
            for item in value.grouping { group(item, locals: locals) }
            expression(value.having, locals: locals)
            for item in value.windows { window(item.definition, locals: locals) }
        case .values(let rows): for row in rows { for item in row { expression(item, locals: locals) } }
        case .set(_, _, let left, let right): query(left, locals: locals); query(right, locals: locals)
        case .nested(let child): query(child, locals: locals)
        case .with(let clause, let body): query(body, locals: with(clause, locals: locals))
        }
        for item in value.orderBy { expression(item.expression, locals: locals) }
    }
    private func from(_ value: SQLFrom, locals: Set<SQLText>) {
        switch value.source {
        case .table(let name):
            if name.database != nil || !locals.contains(name.name) { need(name, .select); reads.append(qualified(name)) }
        case .query(let value): query(value, locals: locals)
        case .function(let value): expression(value, locals: locals)
        case .joined(let value): from(value, locals: locals)
        case .jsonTable(let value):
            expression(value.context, locals: locals); expression(value.path, locals: locals)
            for item in value.passing { expression(item.value, locals: locals) }
            func columns(_ items: [SQLJSONTableColumn]) {
                for item in items {
                    switch item {
                    case .regular(_, _, _, _, _, _, _, let empty, let error):
                        expression(empty?.value, locals: locals); expression(error?.value, locals: locals)
                    case .nested(_, _, let children): columns(children)
                    case .ordinality: break
                    }
                }
            }
            columns(value.columns)
        }
        for join in value.joins { from(join.right, locals: locals); expression(join.condition, locals: locals) }
    }
    func expression(_ value: SQLExpression?, locals: Set<SQLText> = []) {
        guard let value else { return }
        var pending = [value]
        while let value = pending.popLast() {
            switch value.kind {
            case .call(let call):
                observeCall?(call)
                function(call.name)
                for argument in call.arguments { expression(argument.value, locals: locals) }
                expression(call.filter, locals: locals)
                for key in call.withinGroup { expression(key.expression, locals: locals) }
                for key in call.orderBy { expression(key.expression, locals: locals) }
                if let value = call.over { window(value, locals: locals) }
                continue
            case .scalarQuery(let value), .exists(let value): query(value, locals: locals)
            case .memberQuery(let left, let value, _), .quantifiedQuery(_, let left, _, let value):
                expression(left, locals: locals); query(value, locals: locals)
            default: break
            }
            pending.append(contentsOf: CatalogExpressionPolicy.children(value).reversed())
        }
    }
    private func group(_ value: SQLGroup, locals: Set<SQLText>) {
        switch value {
        case .set(let items): for item in items { expression(item, locals: locals) }
        case .rollup(let items), .cube(let items), .sets(let items): for item in items { group(item, locals: locals) }
        }
    }
    private func window(_ value: SQLWindow, locals: Set<SQLText>) {
        for item in value.partitionBy { expression(item, locals: locals) }
        for item in value.orderBy { expression(item.expression, locals: locals) }
        if let frame = value.frame {
            for bound in [frame.start, frame.end].compactMap({ $0 }) {
                if case .offset(let value, _) = bound { expression(value, locals: locals) }
            }
        }
    }
    private func returning(_ value: SQLReturning?, locals: Set<SQLText>) {
        for item in value?.items ?? [] { expression(item.expression, locals: locals) }
    }
    private func returnsColumns(_ value: SQLReturning?) -> Bool {
        value?.items.contains { $0.star || readsColumns($0.expression) } ?? false
    }
    private func readsColumns(_ value: SQLExpression?) -> Bool {
        guard let value else { return false }
        var pending = [value]
        while let value = pending.popLast() {
            switch value.kind { case .column, .scalarQuery, .exists, .memberQuery, .quantifiedQuery: return true; default: break }
            pending += CatalogExpressionPolicy.children(value)
        }
        return false
    }
    /// Shared admission traversal: selected defaults, CHECKs, and view bodies
    /// are walked without evaluating any SQL expression or host function.
    func collectDependencies(settings: SessionSettings) {
        collectViewFunctions()
        collectStoredExpressions(settings: settings)
    }
    private func collectViewFunctions() {
        let originalTables = tables
        let writeTargets = originalTables.filter { !$0.privileges.intersection([.insert, .update, .delete]).isEmpty }.map(\.name)
        var pending = reads + writeTargets
        var visited: Set<ViewKey> = []
        while let read = pending.popLast() {
            guard let owner = try? domains.relation(read), !DomainNames.isAttachment(owner.name),
                  visited.insert(ViewKey(domain: owner.name, name: read.name)).inserted,
                  let view = owner.snapshot.catalog.views.first(where: { $0.name == read.name }),
                  let body = try? ViewDefinition.parsed(view), let scope = try? domains.viewScope(owner: owner.name) else { continue }
            let nested = StatementPrivileges(domains: scope); nested.observeCall = observeCall; nested.query(body.query)
            for name in nested.functions { function(name) }
            pending += nested.reads
        }
        pending = reads; visited = []
        while let read = pending.popLast() {
            guard let owner = try? domains.relation(read), !DomainNames.isAttachment(owner.name),
                  visited.insert(ViewKey(domain: owner.name, name: read.name)).inserted,
                  let view = owner.snapshot.catalog.views.first(where: { $0.name == read.name }), view.securityInvoker,
                  let body = try? ViewDefinition.parsed(view), let scope = try? domains.viewScope(owner: owner.name) else { continue }
            let nested = StatementPrivileges(domains: scope); nested.observeCall = observeCall; nested.query(body.query)
            tables += nested.tables; pending += nested.reads
        }
        for target in writeTargets {
            var name = target, seen: Set<ViewKey> = []
            var privileges = originalTables.filter { $0.name.name == target.name && $0.name.database == target.database }.map(\.privileges)
            while let owner = try? domains.relation(name), !DomainNames.isAttachment(owner.name),
                  seen.insert(ViewKey(domain: owner.name, name: name.name)).inserted,
                  let view = owner.snapshot.catalog.views.first(where: { $0.name == name.name }), view.securityInvoker,
                  let body = try? ViewDefinition.parsed(view), case .select(let select) = body.query.body,
                  select.from.count == 1, let source = select.from.first, source.joins.isEmpty, case .table(let base) = source.source {
                guard !source.lateral, let scope = try? domains.viewScope(owner: owner.name),
                      (try? UpdatableView.checkShape(body.query, name: name.name, operation: "delete from")) != nil else { break }
                let nested = StatementPrivileges(domains: scope); nested.observeCall = observeCall
                if let predicate = select.predicate {
                    if privileges.contains(where: { !$0.intersection([.update, .delete]).isEmpty }), !privileges.contains(.select) { privileges.append(.select) }
                    nested.expression(predicate)
                }
                for privilege in privileges { nested.need(base, privilege) }
                tables += nested.tables
                name = nested.qualified(base)
            }
        }
    }
    private func collectStoredExpressions(settings: SessionSettings) {
        for original in catalogWrites {
            var statement = original
            let name: SQLName
            switch statement { case .insert(let v): name = v.table; case .update(let v): name = v.table; case .delete(let v): name = v.table; case .merge(let v): name = v.table; default: continue }
            guard let owner = try? domains.relation(name) else { continue }
            let snapshot = owner.snapshot
            var targetDomain = owner.name
            var target = snapshot.catalog.tables.first(where: { $0.name == name.name })
            if let view = snapshot.catalog.views.first(where: { $0.name == name.name }) {
                guard !DomainNames.isAttachment(owner.name) else { continue }
                var unrestricted = settings; unrestricted.privileges = SessionPrivileges(defaults: .allTables)
                unrestricted.domains = try? domains.viewScope(owner: owner.name); unrestricted.targetDomain = owner.name
                let context = StatementServices(settings: unrestricted, meter: CostMeter()).context()
                let operation: String
                switch statement { case .insert: operation = "insert into"; case .update: operation = "update"; case .delete: operation = "delete from"; default: continue }
                guard let resolved = try? UpdatableView.resolve(view, operation: operation, snapshot: snapshot, settings: unrestricted, context: context) else { continue }
                target = resolved.table; targetDomain = resolved.tableDomain
                var viewDefaults: [SQLExpression] = []
                do {
                    func assignment(_ value: SQLAssignment) throws -> SQLAssignment {
                        let ordinal = try resolved.column(value.column)
                        let destination = try resolved.destinations([ordinal])[0]
                        var result = value; result.column = resolved.table.columns[destination].name
                        if value.value == nil {
                            result.value = view.columns[ordinal].defaultText.flatMap { CatalogExpression($0).parsed }
                                ?? SQLExpression(kind: .null, span: .init(byteStart: 0, byteEnd: 0, position: 0), position: 0)
                            if let value = result.value { viewDefaults.append(value) }
                        }
                        return result
                    }
                    switch statement {
                    case .insert(var value):
                        let ordinals = try value.columns.isEmpty ? Array(view.columns.indices) : value.columns.map { try resolved.column($0) }
                        value.columns = try resolved.destinations(ordinals).map { resolved.table.columns[$0].name }
                        var seen: Set<Int> = []
                        for (column, valueDefault) in try resolved.insertDefaults() where seen.insert(column).inserted {
                            if resolved.table.columns[column].identity != nil && value.overriding == "user" { continue }
                            target!.columns[column].defaultValue = .expression(valueDefault)
                        }
                        if var conflict = value.conflict { conflict.assignments = try conflict.assignments.map(assignment); value.conflict = conflict }
                        statement = .insert(value)
                    case .update(var value): value.assignments = try value.assignments.map(assignment); statement = .update(value)
                    default: break
                    }
                } catch { continue } // Execution owns invalid target/column diagnostics.
                for value in viewDefaults { expression(value) }
            }
            guard let table = target else { continue }
            observeWrite?(table, statement, targetDomain)
            var defaulted: Set<Int> = [], checks = true
            func assignments(_ items: [SQLAssignment]) {
                for item in items where item.value == nil {
                    if let index = table.columns.firstIndex(where: { $0.name == item.column }) { defaulted.insert(index) }
                }
            }
            func insertion(_ names: [SQLText], _ overriding: String?, _ rows: [[SQLExpression?]]?) {
                let targets = names.isEmpty ? Array(table.columns.indices) : names.map { name in table.columns.firstIndex { $0.name == name } ?? -1 }
                defaulted.formUnion(table.columns.indices.filter { !targets.contains($0) })
                for (index, column) in table.columns.enumerated() where column.identity != nil && overriding == "user" { defaulted.insert(index) }
                for target in table.columns.indices {
                    guard let position = targets.firstIndex(of: target) else { continue }
                    for row in rows ?? [] where position < row.count && row[position] == nil { defaulted.insert(target) }
                }
            }
            switch statement {
            case .insert(let value):
                switch value.source {
                case .defaultValues: defaulted.formUnion(table.columns.indices)
                case .values(let rows): insertion(value.columns, value.overriding, rows)
                case .query: insertion(value.columns, value.overriding, nil)
                }
                if let conflict = value.conflict, !conflict.doNothing { assignments(conflict.assignments) }
            case .update(let value): assignments(value.assignments)
            case .delete: checks = false
            case .merge(let value):
                checks = false
                for clause in value.clauses {
                    switch clause.action {
                    case .update(let items): checks = true; assignments(items)
                    case .insert(let names, let overriding, let values):
                        checks = true
                        if let values { insertion(names, overriding, [values]) } else { defaulted.formUnion(table.columns.indices) }
                    default: break
                    }
                }
            default: break
            }
            for index in defaulted.sorted() {
                if case .expression(let value) = table.columns[index].defaultValue { expression(value.parsed) }
            }
            if checks { for check in table.checks { expression(check.expression.parsed) } }
        }
    }
}
