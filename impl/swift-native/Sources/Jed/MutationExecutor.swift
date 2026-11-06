/// DML edits a statement-private snapshot. Validation, RETURNING and resource
/// admission finish before the session can publish that snapshot.
internal enum MutationExecutor {
    static func execute(_ statement: SQLStatement, snapshot: inout DomainSnapshot,
                        settings: SessionSettings, context: ExpressionContext,
                        parameters: [SQLValue]) throws -> MutationResult? {
        switch statement {
        case .insert(let syntax): return try insert(syntax, snapshot: &snapshot, settings: settings, context: context, parameters: parameters)
        case .update(let syntax): return try mutate(table: syntax.table, assignments: syntax.assignments,
            predicate: syntax.predicate, returning: syntax.returning, snapshot: &snapshot,
            settings: settings, context: context, parameters: parameters)
        case .delete(let syntax): return try mutate(table: syntax.table, assignments: nil,
            predicate: syntax.predicate, returning: syntax.returning, snapshot: &snapshot,
            settings: settings, context: context, parameters: parameters)
        default: return nil
        }
    }

    struct Relation {
        let table: CatalogTable
        let types: [TypeDescriptor]
        let store: TableStore
        let view: UpdatableView?
        init(_ name: SQLName, snapshot: DomainSnapshot, tableOverride: CatalogTable? = nil, view: UpdatableView? = nil) throws {
            guard name.database == nil || name.database?.description == "main",
                  let table = tableOverride ?? snapshot.catalog.tables.first(where: { $0.name == name.name }),
                  let store = snapshot.tables[name.name] else { throw JedError("42P01", "relation does not exist: \(name.name)") }
            self.table = table; self.store = store; self.view = view
            types = try table.columns.map { try $0.type.resolve(snapshot.types) }
            guard table.indexes.allSatisfy({ $0.kind == 0 || $0.kind == 2 }) else {
                throw JedError("0A000", "inverted-index mutation is not yet connected")
            }
        }
        var visibleCount: Int { view?.layers.first?.columns.count ?? types.count }
        func columns(qualifier: SQLText? = nil, offset: Int = 0, qualifiedOnly: Bool = false, physical: Bool = false) -> [ExpressionColumn] {
            if !physical, let layer = view?.layers.first {
                return layer.columns.enumerated().map {
                    ExpressionColumn(name: $0.element.name, qualifier: qualifier ?? layer.view.name,
                        type: $0.element.type ?? TypeDescriptor(.scalar(.text)), index: offset + $0.offset,
                        collation: layer.projections[$0.offset].collation?.asColumn, qualifiedOnly: qualifiedOnly)
                }
            }
            return table.columns.enumerated().map {
                ExpressionColumn(name: $0.element.name, qualifier: qualifier ?? table.name, type: types[$0.offset],
                                 index: offset + $0.offset, qualifiedOnly: qualifiedOnly)
            }
        }
        func physicalColumns(_ columns: Set<Int>) -> Set<Int> { view?.physicalColumns(columns) ?? columns }
        func row(_ base: @escaping (Int) throws -> SQLValue, context: ExpressionContext) -> ExpressionContext {
            var row = context
            if let view { row.column = { try view.value($0, base: base, context: context) } }
            else { row.column = base }
            return row
        }
        func requireSelect(_ settings: SessionSettings) throws {
            if let view { try view.requireSelect(settings) }
            else { try settings.privileges.require(.select, on: table.name) }
        }
        func column(_ name: SQLText) throws -> Int {
            guard let index = table.columns.firstIndex(where: { $0.name == name }) else {
                throw JedError("42703", "column does not exist: \(name)")
            }
            return index
        }
        func key(_ values: [SQLValue], store: inout TableStore, collatedKey: KeyEncoding.CollatedKey?) throws -> [UInt8] {
            if table.primaryKey.isEmpty { return try KeyEncoding.encode(.integer(store.allocateRowID()), type: TypeDescriptor(.scalar(.i64))) }
            var key: [UInt8] = []
            for column in table.primaryKey { key += try KeyEncoding.encode(values[column], type: types[column], collatedKey: collatedKey) }
            return key
        }
        func coerce(_ value: SQLValue, expression: ResolvedExpression, column: Int, context: ExpressionContext) throws -> SQLValue {
            try AssignmentValues.store(value, expression: expression, to: types[column], context: context)
        }
        func validateNotNull(_ columns: [Int], value: (Int) throws -> SQLValue) throws {
            for column in columns where table.columns[column].notNull || table.primaryKey.contains(column) {
                if try value(column).isNull { throw JedError("23502", "null value in column \(table.columns[column].name) violates not-null constraint") }
            }
        }
    }
    static func resolver(_ relation: Relation, snapshot: DomainSnapshot, settings: SessionSettings,
                                 context: ExpressionContext, columns: Bool) -> ExpressionResolver {
        let resolver = ExpressionResolver(columns: columns ? relation.columns() : [], composites: snapshot.types, scalar: context.scalar)
        resolver.hostFunctions = settings.environment.signatures; resolver.unicode = settings.environment.unicode
        resolver.queryScope = SubqueryScope(snapshot: snapshot, settings: settings)
        resolver.checkFunction = { try settings.privileges.require(.execute, on: $0, function: true) }
        return resolver
    }
    static func defaultExpression(_ column: Int, relation: Relation, resolver: ExpressionResolver) throws -> ResolvedExpression {
        switch relation.table.columns[column].defaultValue {
        case .none: return ResolvedExpression(type: relation.types[column], node: .constant(.null), position: 0)
        case .constant(let value): return ResolvedExpression(type: relation.types[column], node: .constant(value), position: 0)
        case .expression(let definition):
            guard let syntax = definition.parsed else { throw JedError("XX003", "stored default expression cannot be parsed") }
            let oldColumns = resolver.columns; resolver.columns = []; defer { resolver.columns = oldColumns }
            return try AssignmentValues.resolve(syntax, to: relation.types[column], resolver: resolver)
        }
    }
    struct Check {
        let name: SQLText
        let expression: ResolvedExpression
    }
    private static func checks(_ relation: Relation, resolver: ExpressionResolver, environment: Environment) throws -> [Check] {
        let old = resolver.columns, references = resolver.referencedColumns
        resolver.columns = relation.columns(physical: true); resolver.referencedColumns = []
        defer { resolver.columns = old; resolver.referencedColumns = references }
        return try relation.table.checks.map {
            guard let syntax = $0.expression.parsed else { throw JedError("XX003", "stored check expression cannot be parsed") }
            let expression = try resolver.resolve(syntax, expected: TypeDescriptor(.scalar(.boolean)))
            try StoredExpressionDependencies.validateHosts($0.hosts, expressions: [expression], environment: environment)
            guard expression.type == nil || expression.type?.type == .scalar(.boolean) else { throw JedError("42804", "CHECK requires a boolean expression") }
            return Check(name: $0.name, expression: expression)
        }
    }
    private static func referenced(_ expressions: [ResolvedExpression]) -> Set<Int> {
        var columns: Set<Int> = []
        StoredExpressionDependencies.walk(expressions) { if case .column(let index) = $0.node { columns.insert(index) } }
        return columns
    }
    static func validate(_ checks: [Check], relation: Relation, value: @escaping (Int) throws -> SQLValue, context: ExpressionContext) throws {
        var row = context; row.column = value
        for check in checks {
            if case .boolean(false) = try row.evaluate(check.expression) {
                throw JedError("23514", "new row for relation \(relation.table.name) violates check constraint \(check.name)")
            }
        }
    }
    struct Returning {
        let columns: [ResultColumn]
        let expressions: [ResolvedExpression]
        let oldTouched: Set<Int>
        let newTouched: Set<Int>
        init(_ syntax: SQLReturning?, relation: Relation, resolver: ExpressionResolver, oldDefault: Bool,
             qualifier: SQLText? = nil) throws {
            guard let syntax else { columns = []; expressions = []; oldTouched = []; newTouched = []; return }
            let width = relation.visibleCount
            let oldColumns = resolver.columns, oldReferences = resolver.referencedColumns
            resolver.referencedColumns = []
            let main = relation.columns(qualifier: qualifier, offset: oldDefault ? 0 : width)
            resolver.columns = main
                + relation.columns(qualifier: syntax.oldAlias ?? SQLText(validatedUTF8: Array("old".utf8)), qualifiedOnly: true)
                + relation.columns(qualifier: syntax.newAlias ?? SQLText(validatedUTF8: Array("new".utf8)), offset: width, qualifiedOnly: true)
            defer { resolver.columns = oldColumns; resolver.referencedColumns = oldReferences }
            var columns: [ResultColumn] = [], expressions: [ResolvedExpression] = []
            for item in syntax.items {
                if item.star {
                    let selected = item.qualifier == nil ? main : resolver.columns.filter { $0.qualifier == item.qualifier }
                    guard !selected.isEmpty else { throw JedError("42P01", "RETURNING wildcard has no matching relation") }
                    for column in selected {
                        columns.append(ResultColumn(name: column.name, type: column.type))
                        expressions.append(ResolvedExpression(type: column.type, node: .column(column.index), position: item.span.position))
                        resolver.referencedColumns.insert(column.index)
                    }
                } else if let syntax = item.expression {
                    let expression = try resolver.resolve(syntax)
                    expressions.append(expression)
                    columns.append(ResultColumn(name: item.alias ?? SelectPlan.outputName(syntax), type: expression.type))
                }
            }
            self.columns = columns; self.expressions = expressions
            oldTouched = Set(resolver.referencedColumns.filter { $0 < width })
            newTouched = Set(resolver.referencedColumns.filter { $0 >= width }.map { $0 - width })
        }
        func prepare(relation: Relation, old: Bool, new: Bool, context: ExpressionContext) throws {
            if let view = relation.view {
                try view.prepare(expressions, context: context, column: { index in
                    let width = relation.visibleCount
                    guard index < width ? old : new else { return nil }
                    return index < width ? index : index - width
                })
            } else { try context.subqueries.prepare(expressions, context: context) }
        }
        func emit(old: ((Int) throws -> SQLValue)?, new: ((Int) throws -> SQLValue)?,
                  relation: Relation, context: ExpressionContext, result: MutationResult) throws {
            guard !expressions.isEmpty else { return }
            context.meter.charge(1); try context.meter.guardCost()
            var row = context
            row.column = { index in
                let width = relation.visibleCount
                guard let base = index < width ? old : new else { return .null }
                let column = index < width ? index : index - width
                if let view = relation.view { return try view.value(column, base: base, context: context) }
                return try base(column)
            }
            try result.append(expressions.map { try row.evaluate($0) })
        }
    }
    private static func insert(_ syntax: SQLInsert, snapshot: inout DomainSnapshot, settings inputSettings: SessionSettings,
                               context baseContext: ExpressionContext, parameters: [SQLValue]) throws -> MutationResult {
        try SystemRelationPlan.rejectTarget(syntax.table.name)
        var settings = inputSettings
        let relation: Relation, destinations: [Int]
        var viewDefaults: Set<Int> = []
        if (syntax.table.database == nil || syntax.table.database?.description == "main"),
           let definition = snapshot.catalog.views.first(where: { $0.name == syntax.table.name }) {
            try settings.privileges.require(.insert, on: definition.name)
            let view = try UpdatableView.resolve(definition, operation: "insert into", snapshot: snapshot, settings: settings, context: baseContext)
            let targets: [Int]
            if case .defaultValues = syntax.source { targets = [] }
            else { targets = try syntax.columns.isEmpty ? Array(definition.columns.indices) : syntax.columns.map(view.column) }
            guard Set(targets).count == targets.count else { throw JedError("42701", "column specified more than once") }
            if let conflict = syntax.conflict, !conflict.doNothing {
                let updates = try conflict.assignments.map { try view.column($0.column) }
                guard Set(updates).count == updates.count else { throw JedError("42701", "multiple assignments to same column") }
                _ = try view.destinations(Array(Set(targets + updates)).sorted(), allowDuplicates: true)
                _ = try view.destinations(updates)
            }
            destinations = try view.destinations(targets)
            var table = view.table, defaults: Set<Int> = []
            for (column, expression) in try view.insertDefaults() where defaults.insert(column).inserted {
                if table.columns[column].identity != nil && syntax.overriding == "user" { continue }
                table.columns[column].defaultValue = .expression(expression)
                viewDefaults.insert(column)
            }
            settings = view.layers.last!.settings
            relation = try Relation(SQLName(database: nil, name: table.name, span: syntax.table.span), snapshot: snapshot, tableOverride: table, view: view)
        } else {
            relation = try Relation(syntax.table, snapshot: snapshot)
            destinations = try syntax.columns.isEmpty ? Array(relation.types.indices) : syntax.columns.map(relation.column)
        }
        try settings.privileges.require(.insert, on: relation.table.name)
        try CatalogEnvironment.checkWritable(relation.table, image: snapshot.catalog, environment: settings.environment)
        let resolver = resolver(relation, snapshot: snapshot, settings: inputSettings, context: baseContext, columns: false)
        guard Set(destinations).count == destinations.count else { throw JedError("42701", "column specified more than once") }
        let discarded = Set(relation.types.indices.filter { syntax.overriding == "user" && relation.table.columns[$0].identity != nil })
        let always = Set(destinations.filter { relation.table.columns[$0].identity == true && syntax.overriding != "system" && !discarded.contains($0) })
        let source: [[SQLExpression?]]
        var sourcePlan: SelectPlan?, inputLiterals: Set<Int> = []
        switch syntax.source {
        case .defaultValues: source = [Array(repeating: nil, count: destinations.count)]
        case .values(let rows): source = rows
        case .query(let query):
            guard always.isEmpty else { throw JedError("428C9", "cannot insert a non-DEFAULT value into an identity column") }
            source = []
            sourcePlan = try SelectPlan.resolve(query, snapshot: snapshot, settings: inputSettings, scalar: baseContext.scalar,
                                               using: resolver, finalizeParameters: false)
            guard sourcePlan!.columns.count == destinations.count else { throw JedError("42601", "INSERT source column count does not match target") }
            func literalStrings(_ query: SQLQuery) -> Set<Int> {
                if case .nested(let child) = query.body { return literalStrings(child) }
                guard case .select(let select) = query.body else { return [] }
                return Set(select.items.enumerated().compactMap { index, item in
                    guard let syntax = item.expression else { return nil }
                    if case .string = syntax.kind { return index }; return nil
                })
            }
            let stringLiterals = literalStrings(query)
            for ordinal in destinations.indices {
                if discarded.contains(destinations[ordinal]) { continue }
                let target = relation.types[destinations[ordinal]], projected = sourcePlan!.projections[ordinal]
                if AssignmentValues.admits(projected.type?.type, to: target.type) { continue }
                if case .scalar(let type) = target.type, stringLiterals.contains(ordinal),
                   [.uuid, .bytea, .date, .timestamp, .timestamptz, .interval, .json, .jsonb].contains(type) {
                    inputLiterals.insert(ordinal); continue
                }
                if case .record = projected.type?.type, case .composite = target.type {
                    throw JedError("42846", "cannot cast type record to \(target.type.name)")
                }
                throw JedError("42804", "INSERT source type cannot be assigned to \(target.type.name)")
            }
        }
        var slots: [InsertSlot] = [], defaults: [Int: ResolvedExpression] = [:]
        func defaultSlot(_ index: Int) throws -> ResolvedExpression {
            if let found = defaults[index] { return found }
            if viewDefaults.contains(index), relation.table.columns[index].identity == true, syntax.overriding != "system" {
                throw JedError("428C9", "cannot insert a view default into a GENERATED ALWAYS identity column")
            }
            let expression = try defaultExpression(index, relation: relation, resolver: resolver)
            defaults[index] = expression; return expression
        }
        for row in source {
            guard row.count <= destinations.count else { throw JedError("42601", "INSERT has more expressions than target columns") }
            guard row.count == destinations.count else { throw JedError("42601", "INSERT has more target columns than expressions") }
            var provided: [Int: SQLExpression] = [:]
            for (index, value) in row.enumerated() {
                let column = destinations[index]
                if let value, !discarded.contains(column) {
                    guard !always.contains(column) else { throw JedError("428C9", "cannot insert a non-DEFAULT value into an identity column") }
                    provided[column] = value
                }
            }
            let expressions = try relation.types.indices.map { column in
                try provided[column].map { try AssignmentValues.resolve($0, to: relation.types[column], resolver: resolver) } ?? defaultSlot(column)
            }
            let supplied: Set<Int>
            if case .defaultValues = syntax.source { supplied = [] }
            else { supplied = Set(destinations).subtracting(discarded) }
            slots.append(InsertSlot(expressions: expressions, literalSlots: Set(provided.keys), inputLiterals: [], supplied: supplied))
        }
        if sourcePlan != nil {
            for column in relation.types.indices where !destinations.contains(column) || discarded.contains(column) { _ = try defaultSlot(column) }
        }
        let check = try checks(relation, resolver: resolver, environment: settings.environment)
        let indexes = try relation.table.indexes.sorted { $0.name < $1.name }.map {
            try StoredIndexPlan.resolve(table: relation.table, index: $0, snapshot: snapshot, settings: settings, context: baseContext)
        }
        let returning = try Returning(syntax.returning, relation: relation, resolver: resolver, oldDefault: false, qualifier: syntax.alias)
        if !returning.oldTouched.isEmpty || !returning.newTouched.isEmpty { try relation.requireSelect(inputSettings) }
        let conflict = try syntax.conflict.map { try ConflictPlan($0, alias: syntax.alias, relation: relation,
            indexes: indexes, resolver: resolver, snapshot: snapshot, settings: settings, inputSettings: inputSettings, context: baseContext) }
        var context = baseContext
        context.parameters = try ParameterBinding.bind(parameters, types: ParameterBinding.finalize(resolver), timezone: settings.timeZone)
        var sourceReserved: Int64 = 0
        defer { context.meter.resources.query.release(sourceReserved) }
        if let plan = sourcePlan {
            try context.subqueries.prepare(plan.expressions, context: context)
            try returning.prepare(relation: relation, old: conflict?.doNothing == false, new: true, context: context)
            try relation.view?.prepareChecks(context: context)
            // The source reads the pre-statement roots and remains a Q1 buffer
            // beside all defaults, RETURNING rows and pending writes.
            let execution = SelectExecution(plan: plan, context: context); defer { execution.close() }
            var rows: [[SQLValue]] = []
            while let row = try execution.next() {
                let bytes = MemorySizes.row(row)
                try context.meter.reserveQuery(bytes); sourceReserved = saturatingAdd(sourceReserved, bytes)
                rows.append(row)
            }
            execution.close()
            for row in rows {
                let expressions = try relation.types.indices.map { column in
                    if !discarded.contains(column), let ordinal = destinations.firstIndex(of: column) {
                        return ResolvedExpression(type: plan.projections[ordinal].type, node: .constant(row[ordinal]), position: 0)
                    }
                    return try defaultSlot(column)
                }
                // Parameter finalization has already completed; defaults were
                // resolved above and this only installs each projected value.
                slots.append(InsertSlot(expressions: expressions, literalSlots: Set(destinations).subtracting(discarded),
                    inputLiterals: Set(inputLiterals.map { destinations[$0] }), supplied: Set(destinations).subtracting(discarded)))
            }
        }
        if sourcePlan == nil {
            // Fold in VALUES source-position order, independently of the table
            // declaration order used to materialize supplied values below.
            for slot in slots {
                try context.subqueries.prepare(destinations.filter { slot.supplied.contains($0) }.map { slot.expressions[$0] }, context: context)
            }
        }
        var materialized: [[Int: SQLValue]] = []
        for slot in slots {
            var values: [Int: SQLValue] = [:]
            // ON CONFLICT defaults draw row by row beside SET DEFAULT. Only
            // actual supplied expressions join the eager VALUES materialization.
            for column in relation.types.indices where slot.supplied.contains(column)
                && (conflict == nil || sourcePlan != nil || slot.literalSlots.contains(column)) {
                try context.meter.checkpoint()
                let expression = slot.expressions[column]
                var value = try slot.literalSlots.contains(column) ? AssignmentValues.evaluateSlot(expression, context: context) : context.evaluate(expression)
                if slot.inputLiterals.contains(column), case .text(let text) = value {
                    value = try ScalarCasts.input(text, to: relation.types[column], explicit: false, context: context.scalar)
                }
                values[column] = value
            }
            materialized.append(values)
        }
        if sourcePlan == nil {
            try returning.prepare(relation: relation, old: conflict?.doNothing == false, new: true, context: context)
            try relation.view?.prepareChecks(context: context)
        }
        if let conflict {
            return try upsert(conflict, slots: slots, supplied: materialized, relation: relation, indexes: indexes,
                checks: check, returning: returning, snapshot: &snapshot, settings: settings,
                inputSettings: inputSettings, context: context)
        }
        let result = MutationResult(command: "INSERT", columns: returning.columns, meter: context.meter)
        var store = relation.store
        let compression = CostMeter()
        var prepared: [(record: PackedRecord, values: [SQLValue], prefixes: [OrderedIndexPlan.Prefix?])] = [], batch: Set<[UInt8]> = []
        var indexBatches = Array(repeating: Set<[UInt8]>(), count: indexes.count)
        for (rowIndex, slot) in slots.enumerated() {
            try context.meter.checkpoint()
            let values = try insertedValues(slot, supplied: materialized[rowIndex], relation: relation, context: context)
            try relation.validateNotNull(Array(relation.types.indices), value: { values[$0] })
            try validate(check, relation: relation, value: { values[$0] }, context: context)
            let key = try relation.key(values, store: &store, collatedKey: context.collatedKey)
            guard try store.get(key) == nil, batch.insert(key).inserted else { throw duplicate(relation.table.name) }
            for ordinal in indexes.indices where indexes[ordinal].index.unique {
                let index = indexes[ordinal]
                guard let prefix = try index.prefix({ values[$0] }), !prefix.hasNull else { continue }
                guard let existing = snapshot.indexes[index.index.name] else { throw JedError.corruption("missing index store") }
                guard indexBatches[ordinal].insert(prefix.bytes).inserted, try !index.conflicts(prefix, store: existing) else { throw duplicateIndex(index.index.name) }
            }
            let record = try RecordDisposition.plan(key: key, values: values.map(RecordInput.value), types: relation.types,
                codec: store.tree.codec, meter: compression, pager: store.tree.pool?.pager)
            let prefixes = try indexes.map { try $0.prefix { values[$0] } }
            try relation.view?.validateNew({ values[$0] }, context: context)
            prepared.append((record, values, prefixes))
            result.affected += 1
        }
        let candidates = prepared.map { candidate in
            ForeignKeyValidation.Candidate(key: candidate.record.key, value: { candidate.values[$0] })
        }
        try ExclusionValidation.validate(candidates, table: relation.table, snapshot: snapshot,
            settings: inputSettings, context: context)
        try ForeignKeyValidation.validateChildren(candidates, child: relation.table, snapshot: snapshot, context: context)
        context.meter.charge(compression.accrued); try context.meter.checkpoint()
        for candidate in prepared {
            try returning.emit(old: nil, new: { candidate.values[$0] }, relation: relation, context: context, result: result)
        }
        for candidate in prepared { try store.put(candidate.record, netStaging: settings.options.maxQueryMemoryBytes > 0) }
        if !prepared.isEmpty {
            for ordinal in indexes.indices {
                let name = indexes[ordinal].index.name
                guard var indexStore = snapshot.indexes[name] else { throw JedError.corruption("missing index store") }
                for candidate in prepared {
                    if let prefix = candidate.prefixes[ordinal] {
                        try indexStore.put(PackedRecord(key: prefix.bytes + candidate.record.key, cells: []), netStaging: settings.options.maxQueryMemoryBytes > 0)
                    }
                }
                snapshot.setIndexStore(name, indexStore)
                try indexes[ordinal].rebuild(indexStore, snapshot: &snapshot)
            }
        }
        if result.affected > 0 { try install(store, relation: relation, countDelta: result.affected, snapshot: &snapshot) }
        try context.meter.syncPending(snapshot.pendingBytes)
        return result
    }
    private struct Change {
        let old: PackedRecord
        let new: PackedRecord?
    }
    static func executeAction(_ target: ForeignKeyActions.Target, snapshot: inout DomainSnapshot,
                              settings: SessionSettings, context: ExpressionContext, state: ForeignKeyActions.State) throws {
        var owner = settings
        // Referential actions run as the table's owner (session.md §5): neither
        // table denials nor EXECUTE denials of the originating writer apply.
        owner.privileges = SessionPrivileges(defaults: .allTables)
        var generated = context
        generated.patterns = PatternExecution(); generated.subqueries = SubqueryExecution(); generated.ctes = nil
        // Each generated UPDATE/DELETE is a fresh query-block execution. Its
        // cached clock and UUID counter reset, while injected sources, sequence
        // state and the originating cost/resource account remain shared.
        StatementServices(settings: owner, meter: context.meter).install(into: &generated)
        let name = SQLName(name: target.plan.child.name, span: SQLSourceSpan(byteStart: 0, byteEnd: 0, position: 0))
        _ = try mutate(table: name, assignments: target.deleting ? nil : [], predicate: nil, returning: nil,
            snapshot: &snapshot, settings: owner, context: generated, parameters: [], target: target, actionState: state)
    }
    private static func mutate(table name: SQLName, assignments: [SQLAssignment]?, predicate syntax: SQLExpression?,
                               returning returningSyntax: SQLReturning?, snapshot: inout DomainSnapshot,
                               settings inputSettings: SessionSettings, context baseContext: ExpressionContext,
                               parameters: [SQLValue], target: ForeignKeyActions.Target? = nil,
                               actionState: ForeignKeyActions.State? = nil) throws -> MutationResult {
        try SystemRelationPlan.rejectTarget(name.name)
        let actionState = actionState ?? ForeignKeyActions.State()
        let relation: Relation, deleting = assignments == nil
        var settings = inputSettings, viewTargets: [Int] = [], viewDestinations: [Int] = []
        if (name.database == nil || name.database?.description == "main"),
           let definition = snapshot.catalog.views.first(where: { $0.name == name.name }) {
            try settings.privileges.require(deleting ? .delete : .update, on: definition.name)
            let view = try UpdatableView.resolve(definition, operation: deleting ? "delete from" : "update",
                                                snapshot: snapshot, settings: settings, context: baseContext)
            viewTargets = try (assignments ?? []).map { try view.column($0.column) }
            guard Set(viewTargets).count == viewTargets.count else { throw JedError("42701", "multiple assignments to same column") }
            viewDestinations = try view.destinations(viewTargets)
            settings = view.layers.last!.settings
            relation = try Relation(SQLName(name: view.table.name, span: name.span), snapshot: snapshot, view: view)
        } else { relation = try Relation(name, snapshot: snapshot) }
        try settings.privileges.require(deleting ? .delete : .update, on: relation.table.name)
        try CatalogEnvironment.checkWritable(relation.table, image: snapshot.catalog, environment: settings.environment)
        let resolver = resolver(relation, snapshot: snapshot, settings: inputSettings, context: baseContext, columns: true)
        let predicate = try syntax.map { try resolver.resolve($0, expected: TypeDescriptor(.scalar(.boolean))) }
        guard predicate?.type == nil || predicate?.type?.type == .scalar(.boolean) else { throw JedError("42804", "WHERE requires a boolean expression") }
        var changes: [(Int, ResolvedExpression)] = [], assigned: Set<Int> = []
        if let target, !deleting {
            for column in target.plan.foreign.columns {
                if relation.table.columns[column].identity == true, target.action != 4 {
                    throw JedError("428C9", "identity column can only be updated to DEFAULT")
                }
                let expression: ResolvedExpression
                if target.action == 4 { expression = try defaultExpression(column, relation: relation, resolver: resolver) }
                else if target.replacements != nil {
                    expression = ResolvedExpression(type: relation.types[column], node: .column(column), position: 0)
                } else { expression = ResolvedExpression(type: relation.types[column], node: .constant(.null), position: 0) }
                changes.append((column, expression)); assigned.insert(column)
            }
        }
        for (ordinal, assignment) in (assignments ?? []).enumerated() {
            let column = try relation.view == nil ? relation.column(assignment.column) : viewDestinations[ordinal]
            guard assigned.insert(column).inserted else { throw JedError("42701", "multiple assignments to same column") }
            if relation.table.columns[column].identity == true, assignment.value != nil || relation.view != nil {
                throw JedError("428C9", "identity column can only be updated to DEFAULT")
            }
            let expression: ResolvedExpression
            if let value = assignment.value { expression = try AssignmentValues.resolve(value, to: relation.types[column], resolver: resolver) }
            else if let view = relation.view {
                if let text = view.layers[0].view.columns[viewTargets[ordinal]].defaultText {
                    guard let syntax = CatalogExpression(text).parsed else { throw JedError("XX003", "stored view default cannot be parsed") }
                    let scope = resolver.columns; resolver.columns = []; defer { resolver.columns = scope }
                    expression = try AssignmentValues.resolve(syntax, to: relation.types[column], resolver: resolver)
                } else { expression = ResolvedExpression(type: relation.types[column], node: .constant(.null), position: 0) }
            } else { expression = try defaultExpression(column, relation: relation, resolver: resolver) }
            changes.append((column, expression))
        }
        var touched = relation.physicalColumns(resolver.referencedColumns)
        if let target { touched.formUnion(target.plan.foreign.columns) }
        if !resolver.referencedColumns.isEmpty { try relation.requireSelect(inputSettings) }
        let check = deleting ? [] : try checks(relation, resolver: resolver, environment: settings.environment)
        let indexes = try relation.table.indexes.sorted { $0.name < $1.name }.map {
            try StoredIndexPlan.resolve(table: relation.table, index: $0, snapshot: snapshot, settings: settings, context: baseContext)
        }
        if !deleting { touched.formUnion(referenced(check.map(\.expression)).subtracting(assigned)) }
        let returning = try Returning(returningSyntax, relation: relation, resolver: resolver, oldDefault: deleting)
        if !returning.oldTouched.isEmpty || !returning.newTouched.isEmpty { try relation.requireSelect(inputSettings) }
        touched.formUnion(relation.physicalColumns(returning.oldTouched))
        if !deleting { touched.formUnion(relation.physicalColumns(returning.newTouched).subtracting(assigned)) }
        let filters = relation.view?.filters ?? []
        if let view = relation.view {
            for filter in filters { touched.formUnion(view.physicalColumns(filter.expression, level: filter.level)) }
            if !deleting {
                for check in view.checked { touched.formUnion(view.physicalColumns(check.expression, level: check.level).subtracting(assigned)) }
            }
        }
        var context = baseContext
        context.parameters = try ParameterBinding.bind(parameters, types: ParameterBinding.finalize(resolver), timezone: settings.timeZone)
        if let view = relation.view {
            try view.prepare(changes.map { $0.1 }, context: context)
            for filter in filters { try view.prepare([filter.expression], level: filter.level, context: context) }
            if let predicate { try view.prepare([predicate], context: context) }
        } else { try context.subqueries.prepare(changes.map { $0.1 } + (predicate.map { [$0] } ?? []), context: context) }
        try returning.prepare(relation: relation, old: true, new: !deleting, context: context)
        if !deleting { try relation.view?.prepareChecks(context: context) }
        let result = MutationResult(command: deleting ? "DELETE" : "UPDATE", columns: returning.columns, meter: context.meter)
        var accessPredicate: ResolvedExpression?
        func addFilter(_ expression: ResolvedExpression) {
            if let previous = accessPredicate {
                accessPredicate = ResolvedExpression(type: TypeDescriptor(.scalar(.boolean)),
                    node: .binary("and", previous, expression, TypeDescriptor(.scalar(.boolean))), position: 0)
            } else { accessPredicate = expression }
        }
        if let view = relation.view {
            for filter in filters { addFilter(view.scanExpression(filter.expression, level: filter.level)) }
            if let predicate { addFilter(view.scanExpression(predicate)) }
        } else { accessPredicate = predicate }
        let access = try BaseScanPlan(predicate: accessPredicate, keys: relation.table.primaryKey, types: relation.types).bind(context)
        if access.isEmpty { return result }
        var cursor: ForeignKeyActions.Gather
        if let target { cursor = try target.gather(snapshot: snapshot, settings: settings, context: context, touched: touched.sorted()) }
        else {
            try ScanCost.charge(relation.store, touched: touched.sorted(), meter: context.meter, access: access)
            cursor = ForeignKeyActions.Gather(scan: access.cursor(relation.store))
        }
        defer { cursor.close() }
        var staged: [Change] = [], store = relation.store
        let compression = CostMeter()
        while let old = try cursor.next() {
            context.meter.charge(1); try context.meter.step()
            let row = ScanRecord(old, pager: store.tree.pool?.pager)
            for column in touched.sorted() { _ = try row.value(column) }
            var targetPosition: Int?
            if let target {
                targetPosition = try target.match(row.value, context: context)
                if targetPosition == nil { continue }
            }
            let rowContext = relation.row(row.value, context: context)
            let filterCount = filters.count + (predicate == nil ? 0 : 1)
            if filterCount > 1 {
                // The equivalent base WHERE is left-deep: all inter-level AND
                // nodes are visited before its first innermost conjunct.
                for _ in 1..<filterCount { context.meter.charge(1); try context.meter.step() }
            }
            var visible = true
            if let view = relation.view {
                for filter in filters where visible {
                    visible = try view.matches(filter.expression, level: filter.level, base: row.value, context: context)
                }
            }
            if !visible { continue }
            if let predicate, try !rowContext.matches(predicate) { continue }
            if deleting {
                staged.append(Change(old: old, new: nil))
            } else {
                var replacements: [Int: SQLValue] = [:]
                for (column, expression) in changes {
                    let evaluated = try rowContext.evaluate(expression)
                    let value: SQLValue
                    if let target, let targetPosition, let mapped = try target.mapped(targetPosition, column: column, context: context) { value = mapped }
                    else { value = evaluated }
                    replacements[column] = try relation.coerce(value, expression: expression, column: column, context: context)
                }
                func updated(_ column: Int) throws -> SQLValue { try replacements[column] ?? row.value(column) }
                try relation.validateNotNull(assigned.sorted(), value: updated)
                if !check.isEmpty { try validate(check, relation: relation, value: updated, context: context) }
                let key: [UInt8]
                if assigned.isDisjoint(with: relation.table.primaryKey) { key = old.key }
                else {
                    var encoded: [UInt8] = []
                    for column in relation.table.primaryKey { encoded += try KeyEncoding.encode(updated(column), type: relation.types[column], collatedKey: context.collatedKey) }
                    key = encoded
                }
                let inputs = relation.types.indices.map { column -> RecordInput in
                    if let value = replacements[column] { return .value(value) }
                    return .kept(old.cells[column], pending: old.overflows[column])
                }
                let record = try RecordDisposition.plan(key: key, values: inputs, types: relation.types, codec: store.tree.codec,
                    meter: compression, pager: store.tree.pool?.pager)
                staged.append(Change(old: old, new: record))
            }
            result.affected += 1
        }
        // End-state uniqueness admits key swaps. All constraint errors precede
        // RETURNING, including errors belonging to rows visited later.
        let vacated = Set(staged.map { $0.old.key })
        var replacementKeys: Set<[UInt8]> = []
        for change in staged {
            guard let record = change.new else { continue }
            guard replacementKeys.insert(record.key).inserted else { throw duplicate(relation.table.name) }
            if !vacated.contains(record.key), try store.get(record.key) != nil { throw duplicate(relation.table.name) }
        }
        var indexChanges: [[(old: [UInt8]?, new: [UInt8]?)]] = []
        for index in indexes {
            guard let existing = snapshot.indexes[index.index.name] else { throw JedError.corruption("missing index store") }
            var entries: [(old: [UInt8]?, new: [UInt8]?)] = [], batch: Set<[UInt8]> = []
            for change in staged {
                let old = ScanRecord(change.old, pager: store.tree.pool?.pager)
                let before = try index.prefix(old.value).map { $0.bytes + change.old.key }
                var after: [UInt8]?
                if let record = change.new {
                    let row = ScanRecord(record, pager: store.tree.pool?.pager)
                    if let prefix = try index.prefix(row.value) {
                        if index.index.unique && !prefix.hasNull {
                            guard batch.insert(prefix.bytes).inserted, try !index.conflicts(prefix, store: existing, vacated: vacated) else {
                                throw duplicateIndex(index.index.name)
                            }
                        }
                        after = prefix.bytes + record.key
                    }
                }
                entries.append((before, after))
            }
            indexChanges.append(entries)
        }
        if let view = relation.view, !deleting {
            for change in staged {
                if let record = change.new {
                    let row = ScanRecord(record, pager: store.tree.pool?.pager)
                    try view.validateNew(row.value, context: context)
                }
            }
        }
        if !deleting {
            let candidates = staged.compactMap { change -> ForeignKeyValidation.Candidate? in
                guard let record = change.new else { return nil }
                let row = ScanRecord(record, pager: store.tree.pool?.pager)
                return ForeignKeyValidation.Candidate(key: record.key, value: row.value)
            }
            try ExclusionValidation.validate(candidates, table: relation.table, snapshot: snapshot,
                settings: inputSettings, context: context, vacated: vacated)
            try ForeignKeyValidation.validateChildren(candidates, child: relation.table, snapshot: snapshot,
                assigned: assigned, vacated: vacated, context: context)
        }
        context.meter.charge(compression.accrued); try context.meter.checkpoint()
        for change in staged {
            let old = ScanRecord(change.old, pager: store.tree.pool?.pager)
            let new = change.new.map { ScanRecord($0, pager: store.tree.pool?.pager) }
            try returning.emit(old: old.value, new: new.map { $0.value }, relation: relation, context: context, result: result)
        }
        for change in staged { try store.remove(change.old.key) }
        for change in staged {
            guard let record = change.new else { continue }
            try store.put(record, netStaging: settings.options.maxQueryMemoryBytes > 0)
        }
        if !staged.isEmpty {
            for ordinal in indexes.indices {
                let name = indexes[ordinal].index.name
                guard var indexStore = snapshot.indexes[name] else { throw JedError.corruption("missing index store") }
                for entry in indexChanges[ordinal] where entry.old != entry.new {
                    if let key = entry.old { try indexStore.remove(key) }
                }
                for entry in indexChanges[ordinal] where entry.old != entry.new {
                    if let key = entry.new { try indexStore.put(PackedRecord(key: key, cells: []), netStaging: settings.options.maxQueryMemoryBytes > 0) }
                }
                snapshot.setIndexStore(name, indexStore)
                try indexes[ordinal].rebuild(indexStore, snapshot: &snapshot)
            }
        }
        if !staged.isEmpty {
            try install(store, relation: relation, countDelta: deleting ? -result.affected : 0, snapshot: &snapshot)
            let transitions = staged.map { change -> ForeignKeyActions.Transition in
                let row = ScanRecord(change.old, pager: store.tree.pool?.pager)
                let next = change.new.map { record -> ForeignKeyValidation.Candidate in
                    let values = ScanRecord(record, pager: store.tree.pool?.pager)
                    return ForeignKeyValidation.Candidate(key: record.key, value: values.value)
                }
                return ForeignKeyActions.Transition(old: ForeignKeyValidation.Candidate(key: change.old.key, value: row.value), new: next)
            }
            try ForeignKeyActions.apply(transitions, parent: relation.table, deleting: deleting,
                snapshot: &snapshot, settings: settings, context: context, state: actionState)
        }
        if actionState.depth == 0 { try actionState.settle(snapshot: snapshot, settings: settings, context: context) }
        try context.meter.syncPending(snapshot.pendingBytes)
        return result
    }
    static func duplicate(_ table: SQLText) -> JedError {
        JedError("23505", "duplicate key value violates unique constraint \(table)_pkey")
    }
    static func duplicateIndex(_ name: SQLText) -> JedError {
        JedError("23505", "duplicate key value violates unique constraint \(name)")
    }
    static func install(_ store: TableStore, relation: Relation, countDelta: Int64, snapshot: inout DomainSnapshot) throws {
        snapshot.setTableStore(relation.table.name, store)
        var catalog = snapshot.catalog
        let index = catalog.tables.firstIndex { $0.name == relation.table.name }!
        catalog.tables[index].rowCount += countDelta
        for statistic in catalog.statistics.indices where catalog.statistics[statistic].table == relation.table.name {
            catalog.statistics[statistic].stale = true
        }
        try snapshot.updateCatalog(catalog, schemaChanged: false)
    }
}
