extension MutationExecutor {
    struct ConflictPlan {
        let arbiter: ConflictArbiter
        let doNothing: Bool
        let changes: [(Int, ResolvedExpression)]
        let predicate: ResolvedExpression?
        let exclusions: [ExclusionValidation]
        var assigned: Set<Int> { Set(changes.map { $0.0 }) }

        init(_ syntax: SQLConflict, alias: SQLText?, relation: Relation, indexes: [StoredIndexPlan],
             resolver: ExpressionResolver, snapshot: DomainSnapshot, settings: SessionSettings,
             inputSettings: SessionSettings, context: ExpressionContext) throws {
            doNothing = syntax.doNothing
            exclusions = try relation.table.exclusions.sorted { $0.name < $1.name }.map {
                try ExclusionValidation(table: relation.table, definition: $0, snapshot: snapshot, settings: inputSettings, context: context)
            }
            let previousColumns = resolver.columns, previousReferences = resolver.referencedColumns
            defer { resolver.columns = previousColumns; resolver.referencedColumns = previousReferences }
            resolver.columns = relation.columns(qualifier: alias); resolver.referencedColumns = []
            if syntax.constraint != nil || !syntax.columns.isEmpty { try relation.requireSelect(inputSettings) }
            arbiter = try ConflictArbiter.resolve(syntax, relation: relation, indexes: indexes, exclusions: exclusions, resolver: resolver)
            if syntax.doNothing { changes = []; predicate = nil; return }
            if let view = relation.view {
                var privileges = inputSettings.privileges
                try privileges.require(.update, on: view.layers[0].view.name)
                for layer in view.layers {
                    if layer.view.securityInvoker { try privileges.require(.update, on: layer.base.name) }
                    else { privileges = privileges.withTableAccess() }
                }
            } else { try settings.privileges.require(.update, on: relation.table.name) }
            let width = relation.visibleCount, excluded = SQLText(validatedUTF8: Array("excluded".utf8))
            resolver.columns += relation.columns(qualifier: excluded, offset: width, qualifiedOnly: true)
            if (alias ?? relation.view?.layers.first?.view.name ?? relation.table.name) == excluded {
                for ordinal in resolver.columns.indices { resolver.columns[ordinal].ambiguousRelation = true }
            }
            var assigned: Set<Int> = [], changes: [(Int, ResolvedExpression)] = []
            let targets = try syntax.assignments.map { try relation.view?.column($0.column) ?? relation.column($0.column) }
            let destinations = try relation.view?.destinations(targets) ?? targets
            for (ordinal, assignment) in syntax.assignments.enumerated() {
                let column = destinations[ordinal]
                guard assigned.insert(column).inserted else { throw JedError("42701", "multiple assignments to same column") }
                if relation.table.columns[column].identity == true, assignment.value != nil || relation.view != nil {
                    throw JedError("428C9", "identity column can only be updated to DEFAULT")
                }
                let expression: ResolvedExpression
                if let value = assignment.value { expression = try AssignmentValues.resolve(value, to: relation.types[column], resolver: resolver) }
                else if let view = relation.view {
                    if let text = view.layers[0].view.columns[targets[ordinal]].defaultText {
                        guard let syntax = CatalogExpression(text).parsed else { throw JedError("XX003", "stored view default cannot be parsed") }
                        let old = resolver.columns; resolver.columns = []; defer { resolver.columns = old }
                        expression = try AssignmentValues.resolve(syntax, to: relation.types[column], resolver: resolver)
                    } else { expression = ResolvedExpression(type: relation.types[column], node: .constant(.null), position: 0) }
                } else { expression = try defaultExpression(column, relation: relation, resolver: resolver) }
                changes.append((column, expression))
            }
            predicate = try syntax.updatePredicate.map { try resolver.resolve($0, expected: TypeDescriptor(.scalar(.boolean))) }
            guard predicate?.type == nil || predicate?.type?.type == .scalar(.boolean) else { throw JedError("42804", "WHERE requires a boolean expression") }
            if !resolver.referencedColumns.isEmpty { try relation.requireSelect(inputSettings) }
            self.changes = changes
        }
        func prepare(relation: Relation, context: ExpressionContext) throws {
            let expressions = changes.map { $0.1 } + (predicate.map { [$0] } ?? [])
            if let view = relation.view {
                try view.prepare(expressions, context: context, column: { $0 % relation.visibleCount })
            } else { try context.subqueries.prepare(expressions, context: context) }
        }
    }
    struct InsertSlot {
        let expressions: [ResolvedExpression]
        let literalSlots: Set<Int>
        let inputLiterals: Set<Int>
        let supplied: Set<Int>
    }
    static func insertedValues(_ slot: InsertSlot, supplied: [Int: SQLValue], relation: Relation,
                               context: ExpressionContext) throws -> [SQLValue] {
        var values: [SQLValue] = []
        for column in relation.types.indices {
            var expression = slot.expressions[column]
            let value: SQLValue
            if let provided = supplied[column] { value = provided }
            else { try context.meter.checkpoint(); value = try context.evaluate(expression) }
            if slot.inputLiterals.contains(column) { expression.type = relation.types[column] }
            values.append(try relation.coerce(value, expression: expression, column: column, context: context))
        }
        return values
    }
    private struct ConflictRow {
        let old: PackedRecord?
        let record: PackedRecord
        let values: [SQLValue]
        let prefixes: [OrderedIndexPlan.Prefix?]
        var candidate: ForeignKeyValidation.Candidate { .init(key: record.key, value: { values[$0] }) }
    }
    static func upsert(_ plan: ConflictPlan, slots: [InsertSlot], supplied: [[Int: SQLValue]],
                       relation: Relation, indexes: [StoredIndexPlan], checks: [Check], returning: Returning,
                       snapshot: inout DomainSnapshot, settings: SessionSettings,
                       inputSettings: SessionSettings, context: ExpressionContext) throws -> MutationResult {
        try plan.prepare(relation: relation, context: context)
        let result = MutationResult(command: "INSERT", columns: returning.columns, meter: context.meter)
        var store = relation.store, inserted: [ConflictRow] = [], updated: [ConflictRow] = []
        var affected: Set<[UInt8]> = [], vacated: Set<[UInt8]> = []
        let compression = CostMeter(resources: context.meter.resources, cancellation: context.meter.cancellation)
        func skipped(_ values: [SQLValue]) throws -> Bool {
            switch plan.arbiter {
            case .all:
                if !relation.table.primaryKey.isEmpty {
                    let primary = ConflictArbiter.primary
                    if let key = try primary.key(values, relation: relation, indexes: indexes, context: context) {
                        if try store.get(key) != nil || inserted.contains(where: { $0.record.key == key }) { return true }
                    }
                }
                for ordinal in indexes.indices where indexes[ordinal].index.unique {
                    let arbiter = ConflictArbiter.index(ordinal)
                    if let key = try arbiter.key(values, relation: relation, indexes: indexes, context: context) {
                        if try arbiter.existing(key, relation: relation, indexes: indexes, snapshot: snapshot, vacated: []) != nil { return true }
                        if inserted.contains(where: { $0.prefixes[ordinal]?.bytes == key && $0.prefixes[ordinal]?.hasNull == false }) { return true }
                    }
                }
                for exclusion in plan.exclusions {
                    guard let probe = try exclusion.member({ values[$0] })?.probe else { continue }
                    if try exclusion.existing(probe, snapshot: snapshot, vacated: []) != nil { return true }
                    for previous in inserted {
                        if let other = try exclusion.member({ previous.values[$0] })?.probe,
                           try exclusion.conflicts(probe, other) { return true }
                    }
                }
                return false
            case .exclusion(let ordinal):
                let exclusion = plan.exclusions[ordinal]
                guard let probe = try exclusion.member({ values[$0] })?.probe else { return false }
                if try exclusion.existing(probe, snapshot: snapshot, vacated: []) != nil { return true }
                for previous in inserted {
                    if let other = try exclusion.member({ previous.values[$0] })?.probe,
                       try exclusion.conflicts(probe, other) { return true }
                }
                return false
            default: return false
            }
        }
        for (ordinal, slot) in slots.enumerated() {
            try context.meter.checkpoint()
            let values = try insertedValues(slot, supplied: supplied[ordinal], relation: relation, context: context)
            try relation.validateNotNull(Array(relation.types.indices), value: { values[$0] })
            try validate(checks, relation: relation, value: { values[$0] }, context: context)
            if try skipped(values) { continue }
            let key = try plan.arbiter.key(values, relation: relation, indexes: indexes, context: context)
            let existing = try key.flatMap { try plan.arbiter.existing($0, relation: relation, indexes: indexes, snapshot: snapshot, vacated: vacated) }
            if let old = existing {
                if plan.doNothing { continue }
                let oldRow = ScanRecord(old, pager: store.tree.pool?.pager)
                var row = context
                row.column = { index in
                    let proposed = index >= relation.visibleCount
                    let column = proposed ? index - relation.visibleCount : index
                    if let view = relation.view {
                        return try view.value(column, base: proposed ? { values[$0] } : oldRow.value, context: context)
                    }
                    return try proposed ? values[column] : oldRow.value(column)
                }
                if let predicate = plan.predicate, try !row.matches(predicate) { continue }
                var replacements: [Int: SQLValue] = [:]
                for (column, expression) in plan.changes {
                    replacements[column] = try relation.coerce(row.evaluate(expression), expression: expression, column: column, context: context)
                }
                let changed = try relation.types.indices.map { try replacements[$0] ?? oldRow.value($0) }
                try relation.validateNotNull(plan.assigned.sorted(), value: { changed[$0] })
                try validate(checks, relation: relation, value: { changed[$0] }, context: context)
                try relation.view?.validateNew({ changed[$0] }, context: context)
                let storageKey: [UInt8]
                if plan.assigned.isDisjoint(with: relation.table.primaryKey) { storageKey = old.key }
                else { storageKey = try relation.key(changed, store: &store, collatedKey: context.collatedKey) }
                let inputs = relation.types.indices.map { column -> RecordInput in
                    if let value = replacements[column] { return .value(value) }
                    return .kept(old.cells[column], pending: old.overflows[column])
                }
                let record = try RecordDisposition.plan(key: storageKey, values: inputs, types: relation.types,
                    codec: store.tree.codec, meter: compression, pager: store.tree.pool?.pager)
                let prefixes = try indexes.map { try $0.prefix { changed[$0] } }
                updated.append(ConflictRow(old: old, record: record, values: changed, prefixes: prefixes))
                vacated.insert(old.key)
                if let key = try plan.arbiter.key(changed, relation: relation, indexes: indexes, context: context) { affected.insert(key) }
            } else {
                if let key, affected.contains(key) {
                    if plan.doNothing { continue }
                    throw JedError("21000", "ON CONFLICT DO UPDATE command cannot affect row a second time")
                }
                try relation.view?.validateNew({ values[$0] }, context: context)
                let storageKey = try relation.key(values, store: &store, collatedKey: context.collatedKey)
                let record = try RecordDisposition.plan(key: storageKey, values: values.map(RecordInput.value), types: relation.types,
                    codec: store.tree.codec, meter: compression, pager: store.tree.pool?.pager)
                let prefixes = try indexes.map { try $0.prefix { values[$0] } }
                inserted.append(ConflictRow(old: nil, record: record, values: values, prefixes: prefixes))
                if let key { affected.insert(key) }
            }
        }
        let changes = inserted + updated
        var primaryKeys: Set<[UInt8]> = []
        for change in changes {
            guard primaryKeys.insert(change.record.key).inserted else { throw duplicate(relation.table.name) }
            if !vacated.contains(change.record.key), try store.get(change.record.key) != nil { throw duplicate(relation.table.name) }
        }
        for ordinal in indexes.indices where indexes[ordinal].index.unique {
            guard let existing = snapshot.indexes[indexes[ordinal].index.name] else { throw JedError.corruption("missing unique index") }
            var keys: Set<[UInt8]> = []
            for change in changes {
                guard let prefix = change.prefixes[ordinal], !prefix.hasNull else { continue }
                guard keys.insert(prefix.bytes).inserted, try !indexes[ordinal].conflicts(prefix, store: existing, vacated: vacated) else {
                    throw duplicateIndex(indexes[ordinal].index.name)
                }
            }
        }
        // Child checks see the complete end state, including inserts and
        // updates of self-referenced parent keys. UPDATE checks only assigned
        // local FK columns; INSERT checks every FK.
        let candidates = changes.map(\.candidate)
        let foreignKeys = try relation.table.foreignKeys.sorted { $0.name < $1.name }.map {
            try ForeignKeyValidation.Plan(child: relation.table, foreign: $0, snapshot: snapshot)
        }
        for change in changes {
            for foreign in foreignKeys where change.old == nil || !Set(foreign.foreign.columns).isDisjoint(with: plan.assigned) {
                switch try foreign.childProbe(change.candidate.value, context: context) {
                case .exempt: break
                case .missing: throw foreign.violation()
                case .key(let key):
                    let same = foreign.parent.name == relation.table.name
                    guard try foreign.parentExists(key, snapshot: snapshot, candidates: same ? candidates : [],
                        vacated: same ? vacated : [], context: context) else { throw foreign.violation() }
                }
            }
        }
        try ExclusionValidation.validate(candidates, table: relation.table, snapshot: snapshot,
            settings: inputSettings, context: context, vacated: vacated)
        context.meter.charge(compression.accrued); try context.meter.checkpoint()
        for change in changes {
            let old = change.old.map { ScanRecord($0, pager: store.tree.pool?.pager) }
            try returning.emit(old: old.map { $0.value }, new: { change.values[$0] }, relation: relation, context: context, result: result)
        }
        for change in updated { try store.remove(change.old!.key) }
        for change in changes { try store.put(change.record, netStaging: settings.options.maxQueryMemoryBytes > 0) }
        if !changes.isEmpty {
            for ordinal in indexes.indices {
                let name = indexes[ordinal].index.name
                guard var indexStore = snapshot.indexes[name] else { throw JedError.corruption("missing index store") }
                for change in updated {
                    let old = ScanRecord(change.old!, pager: store.tree.pool?.pager)
                    if let prefix = try indexes[ordinal].prefix(old.value) {
                        let before = prefix.bytes + change.old!.key
                        let after = change.prefixes[ordinal].map { $0.bytes + change.record.key }
                        if before != after { try indexStore.remove(before) }
                    }
                }
                for change in changes {
                    if let prefix = change.prefixes[ordinal] {
                        let after = prefix.bytes + change.record.key
                        if let old = change.old {
                            let row = ScanRecord(old, pager: store.tree.pool?.pager)
                            if try indexes[ordinal].prefix(row.value).map({ $0.bytes + old.key }) == after { continue }
                        }
                        try indexStore.put(PackedRecord(key: after, cells: []), netStaging: settings.options.maxQueryMemoryBytes > 0)
                    }
                }
                snapshot.setIndexStore(name, indexStore); try indexes[ordinal].rebuild(indexStore, snapshot: &snapshot)
            }
            result.affected = Int64(changes.count)
            try install(store, relation: relation, countDelta: Int64(inserted.count), snapshot: &snapshot)
            if !updated.isEmpty {
                let transitions = updated.map { change -> ForeignKeyActions.Transition in
                    let old = ScanRecord(change.old!, pager: store.tree.pool?.pager)
                    return .init(old: .init(key: change.old!.key, value: old.value), new: change.candidate)
                }
                let state = ForeignKeyActions.State()
                try ForeignKeyActions.apply(transitions, parent: relation.table, deleting: false,
                    snapshot: &snapshot, settings: settings, context: context, state: state)
                try state.settle(snapshot: snapshot, settings: settings, context: context)
            }
        }
        try context.meter.syncPending(snapshot.pendingBytes)
        return result
    }
}
