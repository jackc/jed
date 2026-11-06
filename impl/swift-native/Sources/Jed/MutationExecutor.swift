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
        case .merge(let syntax):
            let plan = try MergeExecutor.prepare(syntax, snapshot: snapshot, settings: settings, context: context)
            return try MergeExecutor.execute(plan, snapshot: &snapshot, context: context, parameters: parameters)
        default: return nil
        }
    }

    struct Relation {
        let table: CatalogTable
        let types: [TypeDescriptor]
        let store: TableStore
        let view: UpdatableView?
        init(_ name: SQLName, snapshot: DomainSnapshot, tableOverride: CatalogTable? = nil, view: UpdatableView? = nil, domain: SQLText = DomainNames.main) throws {
            guard name.database == nil || name.database == domain,
                  let table = tableOverride ?? snapshot.catalog.tables.first(where: { $0.name == name.name }),
                  let store = snapshot.tables[name.name] else { throw JedError("42P01", "relation does not exist: \(name.name)").at(name.span.position) }
            self.table = table; self.store = store; self.view = view
            types = try snapshot.tableTypes(table)
        }
        var visibleCount: Int { view?.layers.first?.columns.count ?? types.count }
        func columns(qualifier: SQLText? = nil, offset: Int = 0, qualifiedOnly: Bool = false, physical: Bool = false) -> [ExpressionColumn] {
            if !physical, let layer = view?.layers.first {
                return layer.columns.enumerated().map {
                    ExpressionColumn(name: $0.element.name, qualifier: qualifier ?? layer.view.name,
                        type: $0.element.type ?? TypeDescriptor(.scalar(.text)), index: offset + $0.offset,
                        collation: layer.projections[$0.offset].collation?.asColumn, qualifiedOnly: qualifiedOnly, catalogName: layer.view.name)
                }
            }
            return table.columns.enumerated().map {
                ExpressionColumn(name: $0.element.name, qualifier: qualifier ?? table.name, type: types[$0.offset],
                                 index: offset + $0.offset, qualifiedOnly: qualifiedOnly, catalogName: table.name)
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
            else if settings.targetDomain != DomainNames.temporary { try settings.privileges.require(.select, on: table.name) }
        }
        func column(_ name: SQLText, position: Int = 0) throws -> Int {
            guard let index = table.columns.firstIndex(where: { $0.name == name }) else {
                throw JedError("42703", "column does not exist: \(name)").at(position)
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
            do { return try AssignmentValues.store(value, expression: expression, to: types[column], context: context) }
            catch let error as JedError { throw error.storing(table: table.name, column: table.columns[column], type: types[column]) }
        }
        func validateNotNull(_ columns: [Int], value: (Int) throws -> SQLValue, context: ExpressionContext) throws {
            for column in columns where table.columns[column].notNull || table.primaryKey.contains(column) {
                if try value(column).isNull { throw JedError("23502", "null value in column \(table.columns[column].name) violates not-null constraint")
                    .object(table: table.name, column: table.columns[column].name, detail: context.errorDetail?.row(table, types: types, value: value)) }
            }
        }
    }
    static func resolver(_ relation: Relation, snapshot: DomainSnapshot, settings: SessionSettings,
                                 context: ExpressionContext, columns: Bool) -> ExpressionResolver {
        let read = context.writeScope?.readSnapshot ?? snapshot
        let resolver = ExpressionResolver(columns: columns ? relation.columns() : [], composites: read.types, scalar: context.scalar)
        resolver.cteBindings = context.writeScope?.cteBindings ?? [:]
        resolver.resolveAggregate = AggregateQueryPlan.forbidden
        resolver.hostFunctions = settings.environment.signatures; resolver.unicode = settings.environment.unicode
        resolver.queryScope = SubqueryScope(snapshot: read, settings: settings)
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
    static func checks(_ relation: Relation, resolver: ExpressionResolver, environment: Environment) throws -> [Check] {
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
                    .object(table: relation.table.name, constraint: check.name, detail: context.errorDetail?.row(relation.table, types: relation.types, value: value))
            }
        }
    }
    struct Returning {
        let present: Bool
        let columns: [ResultColumn]
        let expressions: [ResolvedExpression]
        let oldTouched: Set<Int>
        let newTouched: Set<Int>
        init(_ syntax: SQLReturning?, relation: Relation, resolver: ExpressionResolver, oldDefault: Bool,
             qualifier: SQLText? = nil) throws {
            present = syntax != nil
            guard let syntax else { columns = []; expressions = []; oldTouched = []; newTouched = []; return }
            let width = relation.visibleCount
            let oldColumns = resolver.columns, oldReferences = resolver.referencedColumns
            defer { resolver.columns = oldColumns; resolver.referencedColumns = oldReferences }
            resolver.referencedColumns = []
            let main = relation.columns(qualifier: qualifier, offset: oldDefault ? 0 : width)
            var scope = main
            var labels: Set<SQLText> = [qualifier ?? relation.view?.layers.first?.view.name ?? relation.table.name]
            // Authored aliases occupy their labels before either implicit row
            // version. The target keeps its ordinary label even if it is old/new.
            for (alias, offset) in [(syntax.oldAlias, 0), (syntax.newAlias, width)] {
                guard let alias else { continue }
                guard labels.insert(alias).inserted else { throw JedError("42712", "table name specified more than once: \(alias)") }
                scope += relation.columns(qualifier: alias, offset: offset, qualifiedOnly: true).map {
                    var column = $0; column.catalogName = nil; return column
                }
            }
            for (name, offset, aliased) in [("old", 0, syntax.oldAlias != nil), ("new", width, syntax.newAlias != nil)] {
                let label = SQLText(validatedUTF8: Array(name.utf8))
                if !aliased && labels.insert(label).inserted {
                    scope += relation.columns(qualifier: label, offset: offset, qualifiedOnly: true).map {
                        var column = $0; column.catalogName = nil; return column
                    }
                }
            }
            var hintOrders: [SQLText: Int] = [:]
            for index in scope.indices {
                guard let label = scope[index].qualifier else { continue }
                if hintOrders[label] == nil { hintOrders[label] = hintOrders.count }
                scope[index].hintRelationOrder = hintOrders[label]!
            }
            resolver.columns = scope
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
        let plan = try prepareInsert(syntax, snapshot: snapshot, settings: inputSettings, context: baseContext)
        return try executeInsert(plan, snapshot: &snapshot, context: baseContext, parameters: parameters)
    }
    static func prepareInsert(_ syntax: SQLInsert, snapshot: DomainSnapshot, settings inputSettings: SessionSettings,
                              context baseContext: ExpressionContext) throws -> InsertPreparation {
        do {
        try SystemRelationPlan.rejectTarget(syntax.table.name)
        let read = baseContext.writeScope?.readSnapshot ?? snapshot
        if DomainNames.isAttachment(inputSettings.targetDomain), syntax.conflict != nil { throw JedError("0A000", "ON CONFLICT on attached tables is not yet supported") }
        let targetPlan = try MutationTarget.resolve(syntax.table, operation: "insert into", snapshot: read,
            settings: inputSettings, context: baseContext)
        let settings = targetPlan.settings
        let relation: Relation, destinations: [Int]
        var viewDefaults: Set<Int> = []
        if let view = targetPlan.relation.view {
            let definition = view.layers[0].view
            let targets: [Int]
            if case .defaultValues = syntax.source { targets = [] }
            else { targets = try syntax.columns.isEmpty ? Array(definition.columns.indices) : syntax.columns.enumerated().map { try view.column($0.element, position: syntax.columnPosition($0.offset)) } }
            guard Set(targets).count == targets.count else { throw JedError("42701", "column specified more than once") }
            if let conflict = syntax.conflict, !conflict.doNothing {
                let updates = try conflict.assignments.map { try view.column($0.column, position: $0.position) }
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
            relation = try Relation(SQLName(database: nil, name: table.name, span: syntax.table.span), snapshot: read, tableOverride: table, view: view, domain: settings.targetDomain)
        } else {
            relation = targetPlan.relation
            destinations = try syntax.columns.isEmpty ? Array(relation.types.indices) : syntax.columns.enumerated().map { try relation.column($0.element, position: syntax.columnPosition($0.offset)) }
        }
        try CatalogEnvironment.checkWritable(relation.table, image: read.catalog, environment: settings.environment,
            borrowedTypes: read.borrowedTypes, rowTypes: relation.types)
        let resolver = resolver(relation, snapshot: read, settings: inputSettings, context: baseContext, columns: false)
        guard Set(destinations).count == destinations.count else { throw JedError("42701", "column specified more than once") }
        let discarded = Set(relation.types.indices.filter { syntax.overriding == "user" && relation.table.columns[$0].identity != nil })
        let always = Set(destinations.filter { relation.table.columns[$0].identity == true && syntax.overriding != "system" && !discarded.contains($0) })
        let source: [[SQLExpression?]]
        var sourcePlan: SelectPlan?, inputLiterals: Set<Int> = [], sourceValidationError: JedError?
        switch syntax.source {
        case .defaultValues: source = [Array(repeating: nil, count: destinations.count)]
        case .values(let rows): source = rows
        case .query(let query):
            if let column = always.min() { throw JedError("428C9", "cannot insert a non-DEFAULT value into an identity column").identity(relation.table.columns[column].name, insert: true) }
            source = []
            sourcePlan = try SelectPlan.resolve(query, snapshot: read, settings: inputSettings, scalar: baseContext.scalar,
                                               using: resolver, finalizeParameters: false)
            // INSERT owns an eager source result. Its scalar scan/filter input
            // overlaps projection output under the same Q1 schedule as other
            // internally materialized queries; a public cursor may stream it.
            if let plan = sourcePlan, !ColumnarExecution.projection(plan, baseContext) {
                sourcePlan!.materializeSource = plan.aggregation == nil && plan.window == nil && plan.relation == nil
                    && !plan.orderedByScan && plan.order.isEmpty && plan.limit == nil
            }
            // The source is an eager query result. Its admission/evaluation
            // precedes the assignment gate even when no target row can be
            // written. Retain the static error for replay after source drain;
            // EXPLAIN validates it without running the source.
            do {
                guard sourcePlan!.columns.count == destinations.count else { throw JedError("42601", "INSERT source column count does not match target") }
                let stringLiterals = InsertLiteralInput.columns(query)
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
                    throw JedError("42804", "INSERT source type cannot be assigned to \(target.type.name)").at(projected.location).hinted(ErrorHints.assignment)
                }
            } catch let error as JedError { sourceValidationError = error }
        }
        var slots: [InsertSlot] = [], defaults: [Int: ResolvedExpression] = [:]
        func defaultSlot(_ index: Int) throws -> ResolvedExpression {
            if let found = defaults[index] { return found }
            if viewDefaults.contains(index), relation.table.columns[index].identity == true, syntax.overriding != "system" {
                throw JedError("428C9", "cannot insert a view default into a GENERATED ALWAYS identity column").identity(relation.table.columns[index].name, insert: true)
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
                    guard !always.contains(column) else { throw JedError("428C9", "cannot insert a non-DEFAULT value into an identity column").identity(relation.table.columns[column].name, insert: true) }
                    provided[column] = value
                }
            }
            let expressions = try relation.types.indices.map { column in
                do { return try provided[column].map { try AssignmentValues.resolve($0, to: relation.types[column], resolver: resolver) } ?? defaultSlot(column) }
                catch let error as JedError {
                    guard let syntax = provided[column], ErrorDetail.literalSlot(syntax),
                          case .scalar = relation.types[column].type,
                          error.code == "42804" || error.code.hasPrefix("22") else { throw error }
                    throw error.storing(table: relation.table.name, column: relation.table.columns[column], type: relation.types[column])
                }
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
            try StoredIndexPlan.resolve(table: relation.table, index: $0, snapshot: read, settings: settings, context: baseContext)
        }
        let returning = try Returning(syntax.returning, relation: relation, resolver: resolver, oldDefault: false, qualifier: syntax.alias)
        if !returning.oldTouched.isEmpty || !returning.newTouched.isEmpty { try relation.requireSelect(inputSettings) }
        let conflict = try syntax.conflict.map { try ConflictPlan($0, alias: syntax.alias, relation: relation,
            indexes: indexes, resolver: resolver, snapshot: read, settings: settings, inputSettings: inputSettings, context: baseContext) }
        return InsertPreparation(read: read, settings: settings, inputSettings: inputSettings, relation: relation,
            destinations: destinations, discarded: discarded, sourcePlan: sourcePlan, inputLiterals: inputLiterals,
            slots: slots, defaults: defaults, checks: check, indexes: indexes, returning: returning,
            conflict: conflict, parameterTypes: try ParameterBinding.finalize(resolver), sourceValidationError: sourceValidationError)
        } catch let error as JedError { throw error.inDatabase(inputSettings.targetDomain) }
    }
    static func executeInsert(_ plan: InsertPreparation, snapshot: inout DomainSnapshot,
                              context baseContext: ExpressionContext, parameters: [SQLValue]) throws -> MutationResult {
        do {
        let read = plan.read, settings = plan.settings, inputSettings = plan.inputSettings, relation = plan.relation
        let destinations = plan.destinations, discarded = plan.discarded, sourcePlan = plan.sourcePlan
        let inputLiterals = plan.inputLiterals, check = plan.checks, indexes = plan.indexes, returning = plan.returning
        let conflict = plan.conflict
        var slots = plan.slots
        func defaultSlot(_ column: Int) throws -> ResolvedExpression {
            guard let value = plan.defaults[column] else { throw JedError("XX000", "prepared INSERT default is missing") }
            return value
        }
        var context = baseContext
        context.parameters = try ParameterBinding.bind(parameters, types: plan.parameterTypes, timezone: settings.timeZone)
        let profileStart = context.meter.accrued
        defer { context.executionProfile?.add(plan.profileIdentity, .total, context.meter.accrued - profileStart) }
        var sourceReserved: Int64 = 0
        defer { context.meter.resources.query.release(sourceReserved) }
        if let source = sourcePlan {
            try context.subqueries.prepare(source.foldExpressions, context: context)
            try returning.prepare(relation: relation, old: conflict?.doNothing == false, new: true, context: context)
            try relation.view?.prepareChecks(context: context)
            // The source reads the pre-statement roots and remains a Q1 buffer
            // beside all defaults, RETURNING rows and pending writes.
            let execution = SelectExecution(plan: source, context: context); defer { execution.close() }
            var rows: [[SQLValue]] = []
            while let row = try execution.next() {
                let bytes = MemorySizes.row(row)
                try context.meter.reserveQuery(bytes); sourceReserved = saturatingAdd(sourceReserved, bytes)
                rows.append(row)
            }
            execution.close()
            context.meter.resources.retainStatementRows(rows, bytes: sourceReserved)
            sourceReserved = 0
            if let error = plan.sourceValidationError { throw error }
            for row in rows {
                let expressions = try relation.types.indices.map { column in
                    if !discarded.contains(column), let ordinal = destinations.firstIndex(of: column) {
                        return ResolvedExpression(type: source.projections[ordinal].type, node: .constant(row[ordinal]), position: 0)
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
                    // SELECT projections are row data even when their string
                    // spelling is eligible for the target's input function.
                    var input = context.scalar
                    input.statementMicros = nil; input.statementClock = nil
                    value = try ScalarCasts.input(text, to: relation.types[column], explicit: false, context: input)
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
        let result = MutationResult(command: "INSERT", columns: returning.columns, hasAffectedRows: !returning.present, meter: context.meter)
        var store = snapshot.tables[relation.table.name] ?? relation.store
        let compression = CostMeter()
        var prepared: [(record: PackedRecord, values: [SQLValue], entries: [[StoredIndexPlan.Entry]])] = [], batch: Set<[UInt8]> = []
        var indexBatches = Array(repeating: Set<[UInt8]>(), count: indexes.count)
        for (rowIndex, slot) in slots.enumerated() {
            try context.meter.checkpoint()
            let values = try insertedValues(slot, supplied: materialized[rowIndex], relation: relation, context: context)
            try relation.validateNotNull(Array(relation.types.indices), value: { values[$0] }, context: context)
            try validate(check, relation: relation, value: { values[$0] }, context: context)
            let key = try relation.key(values, store: &store, collatedKey: context.collatedKey)
            guard try relation.store.get(key) == nil, batch.insert(key).inserted else { throw duplicate(relation, value: { values[$0] }, context: context) }
            for ordinal in indexes.indices where indexes[ordinal].index.unique {
                let index = indexes[ordinal]
                guard let prefix = try index.prefix({ values[$0] }), !prefix.hasNull else { continue }
                guard let existing = read.indexes[index.index.name] else { throw JedError.corruption("missing index store") }
                guard indexBatches[ordinal].insert(prefix.bytes).inserted, try !index.conflicts(prefix, store: existing) else { throw duplicateIndex(index, relation: relation, value: { values[$0] }, context: context) }
            }
            let record = try RecordDisposition.plan(key: key, values: values.map(RecordInput.value), types: relation.types,
                codec: store.tree.codec, meter: compression, pager: store.tree.pool?.pager)
            let entries = try indexes.map { try $0.entries({ values[$0] }, storageKey: key) }
            try relation.view?.validateNew({ values[$0] }, context: context)
            prepared.append((record, values, entries))
            result.affected += 1
        }
        let candidates = prepared.map { candidate in
            ForeignKeyValidation.Candidate(key: candidate.record.key, value: { candidate.values[$0] })
        }
        try ExclusionValidation.validate(candidates, table: relation.table, snapshot: read,
            settings: inputSettings, context: context)
        let foreignKeys = context.writeScope?.foreignKeys ?? ForeignKeyActions.State()
        try foreignKeys.validateChildren(candidates, child: relation.table, snapshot: snapshot, context: context)
        context.meter.charge(compression.accrued); try context.meter.checkpoint()
        for candidate in prepared {
            try returning.emit(old: nil, new: { candidate.values[$0] }, relation: relation, context: context, result: result)
        }
        if context.writeScope != nil {
            try mergeWrites(prepared.map { WriteChange(old: nil, new: $0.record) }, relation: relation,
                indexes: indexes, allocated: store, snapshot: &snapshot, settings: settings)
        } else {
            for candidate in prepared { try store.put(candidate.record, netStaging: settings.options.maxQueryMemoryBytes > 0) }
            if !prepared.isEmpty {
                for ordinal in indexes.indices {
                    let name = indexes[ordinal].index.name
                    guard var indexStore = snapshot.indexes[name] else { throw JedError.corruption("missing index store") }
                    for candidate in prepared {
                        for entry in candidate.entries[ordinal] {
                            try indexes[ordinal].insert(entry, into: &indexStore, netStaging: settings.options.maxQueryMemoryBytes > 0)
                        }
                    }
                    snapshot.setIndexStore(name, indexStore)
                    try indexes[ordinal].rebuild(indexStore, snapshot: &snapshot)
                }
            }
            if result.affected > 0 { try install(store, relation: relation, countDelta: result.affected, snapshot: &snapshot) }
        }
        try context.meter.syncPending(snapshot.pendingBytes)
        return result
        } catch let error as JedError { throw error.inDatabase(plan.settings.targetDomain) }
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
        if context.writeScope != nil {
            generated.writeScope = StatementWriteScope(readSnapshot: snapshot, cteBindings: [:], foreignKeys: state)
        }
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
        let state = actionState ?? baseContext.writeScope?.foreignKeys ?? ForeignKeyActions.State()
        let plan = try prepareMutation(table: name, assignments: assignments, predicate: syntax, returning: returningSyntax,
            snapshot: snapshot, settings: inputSettings, context: baseContext, target: target)
        return try executeMutation(plan, snapshot: &snapshot, context: baseContext, parameters: parameters, actionState: state)
    }
    static func prepareMutation(table name: SQLName, assignments: [SQLAssignment]?, predicate syntax: SQLExpression?,
                                returning returningSyntax: SQLReturning?, snapshot: DomainSnapshot,
                                settings inputSettings: SessionSettings, context baseContext: ExpressionContext,
                                target: ForeignKeyActions.Target? = nil) throws -> MutationPreparation {
        do {
        try SystemRelationPlan.rejectTarget(name.name)
        let read = baseContext.writeScope?.readSnapshot ?? snapshot
        let relation: Relation, deleting = assignments == nil
        let prepared = try MutationTarget.resolve(name, operation: deleting ? "delete from" : "update", snapshot: read,
            settings: inputSettings, context: baseContext)
        let settings = prepared.settings
        var viewTargets: [Int] = [], viewDestinations: [Int] = []
        if let view = prepared.relation.view {
            viewTargets = try (assignments ?? []).map { try view.column($0.column, position: $0.position) }
            guard Set(viewTargets).count == viewTargets.count else { throw JedError("42701", "multiple assignments to same column") }
            viewDestinations = try view.destinations(viewTargets)
            relation = try Relation(SQLName(name: view.table.name, span: name.span), snapshot: read, view: view, domain: settings.targetDomain)
        } else { relation = prepared.relation }
        try CatalogEnvironment.checkWritable(relation.table, image: read.catalog, environment: settings.environment,
            borrowedTypes: read.borrowedTypes, rowTypes: relation.types)
        let resolver = resolver(relation, snapshot: read, settings: inputSettings, context: baseContext, columns: true)
        let predicate = try syntax.map { PredicateOrdering.order(try resolver.resolve($0, expected: TypeDescriptor(.scalar(.boolean)))) }
        guard predicate?.type == nil || predicate?.type?.type == .scalar(.boolean) else { throw JedError("42804", "WHERE requires a boolean expression").at(syntax?.expressionLocation ?? 0) }
        var changes: [(Int, ResolvedExpression)] = [], assigned: Set<Int> = []
        if let target, !deleting {
            for column in target.plan.foreign.columns {
                if relation.table.columns[column].identity == true, target.action != 4 {
                    throw JedError("428C9", "identity column can only be updated to DEFAULT").identity(relation.table.columns[column].name, insert: false)
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
            let column = try relation.view == nil ? relation.column(assignment.column, position: assignment.position) : viewDestinations[ordinal]
            guard assigned.insert(column).inserted else { throw JedError("42701", "multiple assignments to same column") }
            if relation.table.columns[column].identity == true, assignment.value != nil || relation.view != nil {
                throw JedError("428C9", "identity column can only be updated to DEFAULT").identity(relation.table.columns[column].name, insert: false)
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
            try StoredIndexPlan.resolve(table: relation.table, index: $0, snapshot: read, settings: settings, context: baseContext)
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
        return MutationPreparation(read: read, settings: settings, inputSettings: inputSettings, relation: relation,
            deleting: deleting, predicate: predicate, changes: changes, assigned: assigned, touched: touched,
            checks: check, indexes: indexes, returning: returning, filters: filters, target: target,
            parameterTypes: try ParameterBinding.finalize(resolver))
        } catch let error as JedError { throw error.inDatabase(inputSettings.targetDomain) }
    }
    static func executeMutation(_ plan: MutationPreparation, snapshot: inout DomainSnapshot,
                                context baseContext: ExpressionContext, parameters: [SQLValue],
                                actionState suppliedState: ForeignKeyActions.State? = nil) throws -> MutationResult {
        do {
        let read = plan.read, settings = plan.settings, inputSettings = plan.inputSettings, relation = plan.relation
        let deleting = plan.deleting, predicate = plan.predicate, changes = plan.changes, assigned = plan.assigned
        let touched = plan.touched, check = plan.checks, indexes = plan.indexes, returning = plan.returning
        let filters = plan.filters, target = plan.target
        let actionState = suppliedState ?? baseContext.writeScope?.foreignKeys ?? ForeignKeyActions.State()
        var context = baseContext
        context.parameters = try ParameterBinding.bind(parameters, types: plan.parameterTypes, timezone: settings.timeZone)
        let profileStart = context.meter.accrued
        defer { context.executionProfile?.add(plan.profileIdentity, .total, context.meter.accrued - profileStart) }
        if let view = relation.view {
            try view.prepare(changes.map { $0.1 }, context: context)
            for filter in filters { try view.prepare([filter.expression], level: filter.level, context: context) }
            if let predicate { try view.prepare([predicate], context: context) }
        } else { try context.subqueries.prepare(changes.map { $0.1 } + (predicate.map { [$0] } ?? []), context: context) }
        try returning.prepare(relation: relation, old: true, new: !deleting, context: context)
        if !deleting { try relation.view?.prepareChecks(context: context) }
        let result = MutationResult(command: deleting ? "DELETE" : "UPDATE", columns: returning.columns, hasAffectedRows: !returning.present, meter: context.meter)
        let candidate: ScanCandidate?
        if target == nil { candidate = try plan.selectAccess(parameters: context.parameters, context: context) }
        else { candidate = nil }
        let access = try candidate?.bind(context) ?? plan.scan.bind(context)
        if access.isEmpty { return result }
        var cursor: MutationScanCursor
        if let target {
            cursor = .foreign(try target.gather(snapshot: read, settings: settings, context: context, touched: touched.sorted()))
        } else {
            guard let candidate else { throw JedError("XX000", "mutation lost its selected scan") }
            cursor = try context.profile(plan.profileIdentity, .source) {
                .access(try candidate.makeCursor(context: context, touched: touched.sorted(), access: access))
            }
        }
        defer { cursor.close() }
        var staged: [Change] = [], store = snapshot.tables[relation.table.name] ?? relation.store
        let compression = CostMeter()
        // Canonical intervals are disjoint today; retaining the fetched key
        // also preserves first-probe mutation order if a later access method
        // admits overlapping candidates. This is preparation scratch.
        var intervalKeys: Set<[UInt8]> = []
        while let old = try context.profile(plan.profileIdentity, .source, { try cursor.next() }) {
            if candidate?.intervals != nil, !intervalKeys.insert(old.key).inserted { continue }
            let row = try context.profile(plan.profileIdentity, .source) {
                context.meter.charge(1); try context.meter.step()
                let value = ScanRecord(old, pager: store.tree.pool?.pager)
                for column in touched.sorted() { _ = try value.value(column) }
                return value
            }
            var targetPosition: Int?
            if let target {
                targetPosition = try target.match(row.value, context: context)
                if targetPosition == nil { continue }
            }
            let rowContext = relation.row(row.value, context: context)
            let matches = try context.profile(plan.profileIdentity, .filter) {
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
                if !visible { return false }
                if let predicate, try !rowContext.matches(predicate) { return false }
                return true
            }
            if !matches { continue }
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
                try relation.validateNotNull(assigned.sorted(), value: updated, context: context)
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
            let row = ScanRecord(record, pager: store.tree.pool?.pager)
            guard replacementKeys.insert(record.key).inserted else { throw duplicate(relation, value: row.value, context: context) }
            if !vacated.contains(record.key), try relation.store.get(record.key) != nil { throw duplicate(relation, value: row.value, context: context) }
        }
        var indexChanges: [[StoredIndexPlan.Change]] = []
        for index in indexes {
            guard let existing = read.indexes[index.index.name] else { throw JedError.corruption("missing index store") }
            var entries: [StoredIndexPlan.Change] = [], batch: Set<[UInt8]> = []
            for change in staged {
                let old = ScanRecord(change.old, pager: store.tree.pool?.pager)
                let before = try index.entries(old.value, storageKey: change.old.key)
                var after: [StoredIndexPlan.Entry] = []
                if let record = change.new {
                    let row = ScanRecord(record, pager: store.tree.pool?.pager)
                    let indexed = try index.row(row.value, storageKey: record.key)
                    if let prefix = indexed.prefix {
                        if index.index.unique && !prefix.hasNull {
                            guard batch.insert(prefix.bytes).inserted, try !index.conflicts(prefix, store: existing, vacated: vacated) else {
                                throw duplicateIndex(index, relation: relation, value: row.value, context: context)
                            }
                        }
                    }
                    after = indexed.entries
                }
                entries.append(StoredIndexPlan.Change(before: before, after: after))
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
            try ExclusionValidation.validate(candidates, table: relation.table, snapshot: read,
                settings: inputSettings, context: context, vacated: vacated)
            try actionState.validateChildren(candidates, child: relation.table, snapshot: snapshot,
                assigned: assigned, vacated: vacated, context: context)
        }
        context.meter.charge(compression.accrued); try context.meter.checkpoint()
        for change in staged {
            let old = ScanRecord(change.old, pager: store.tree.pool?.pager)
            let new = change.new.map { ScanRecord($0, pager: store.tree.pool?.pager) }
            try returning.emit(old: old.value, new: new.map { $0.value }, relation: relation, context: context, result: result)
        }
        if context.writeScope != nil {
            try mergeWrites(staged.map { WriteChange(old: $0.old, new: $0.new) }, relation: relation,
                indexes: indexes, allocated: store, snapshot: &snapshot, settings: settings,
                twoPass: !assigned.isDisjoint(with: relation.table.primaryKey))
        } else {
            let twoPass = !assigned.isDisjoint(with: relation.table.primaryKey)
            if deleting || twoPass { for change in staged { try store.remove(change.old.key) } }
            for change in staged {
                guard let record = change.new else { continue }
                try store.put(record, netStaging: settings.options.maxQueryMemoryBytes > 0)
            }
            if !staged.isEmpty {
                for ordinal in indexes.indices {
                    let name = indexes[ordinal].index.name
                    guard var indexStore = snapshot.indexes[name] else { throw JedError.corruption("missing index store") }
                    try indexes[ordinal].apply(indexChanges[ordinal], to: &indexStore, twoPass: twoPass,
                        netStaging: settings.options.maxQueryMemoryBytes > 0)
                    snapshot.setIndexStore(name, indexStore)
                    try indexes[ordinal].rebuild(indexStore, snapshot: &snapshot)
                }
            }
            if !staged.isEmpty {
                try install(store, relation: relation, countDelta: deleting ? -result.affected : 0, snapshot: &snapshot)
            }
        }
        try context.meter.syncPending(snapshot.pendingBytes)
        if !staged.isEmpty {
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
        if actionState.depth == 0 && !actionState.deferUntilStatementEnd { try actionState.settle(snapshot: snapshot, settings: settings, context: context) }
        try context.meter.syncPending(snapshot.pendingBytes)
        return result
        } catch let error as JedError { throw error.inDatabase(plan.settings.targetDomain) }
    }
    static func duplicate(_ table: SQLText) -> JedError {
        let name = CatalogSQL.objectName(table, label: "pkey")
        return JedError("23505", "duplicate key value violates unique constraint \(name)")
            .object(table: table, constraint: name)
    }
    static func duplicateIndex(_ name: SQLText, table: SQLText? = nil) -> JedError {
        var error = JedError("23505", "duplicate key value violates unique constraint \(name)")
        error.constraintName = name.description; error.tableName = table?.description
        return error
    }
    static func duplicate(_ relation: Relation, value: (Int) throws -> SQLValue, context: ExpressionContext) -> JedError {
        var error = duplicate(relation.table.name)
        error.detail = context.errorDetail?.primary(relation.table, types: relation.types, value: value)
        return error
    }
    static func duplicateIndex(_ index: StoredIndexPlan, relation: Relation,
                               value: @escaping (Int) throws -> SQLValue, context: ExpressionContext) -> JedError {
        var error = duplicateIndex(index.index.name, table: relation.table.name)
        error.detail = context.errorDetail?.index(relation.table, plan: index, value: value)
        return error
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
