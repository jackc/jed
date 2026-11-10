extension MergeExecutor {
    /// A planned action carries unchanged cells by reference. New values are
    /// coerced before end-state validation; compression waits until it passes.
    private enum Work { case probe, unmatched, unmatchedFilter, condition, hash, source }
    private final class Action {
        let name: String
        let old: ScanRecord?
        let replacements: [Int: SQLValue]
        let source: Int?
        let clause: Int
        var key: [UInt8]
        var record: PackedRecord?
        init(name: String, old: ScanRecord?, replacements: [Int: SQLValue], source: Int?, clause: Int, key: [UInt8]) {
            self.name = name; self.old = old; self.replacements = replacements
            self.source = source; self.clause = clause; self.key = key
        }
        func value(_ column: Int) throws -> SQLValue {
            if let value = replacements[column] { return value }
            guard let old else { throw JedError("XX000", "MERGE row has an absent column") }
            return try old.value(column)
        }
        var candidate: ForeignKeyValidation.Candidate { .init(key: key, value: value) }
    }
    static func execute(_ plan: MergePreparation, snapshot: inout DomainSnapshot,
                        context initial: ExpressionContext, parameters: [SQLValue]) throws -> MutationResult {
        do {
        var context = initial
        context.parameters = try ParameterBinding.bind(parameters, types: plan.parameterTypes, timezone: plan.settings.timeZone)
        let start = context.meter.accrued
        var sourceCost: Int64 = 0, probeCost: Int64 = 0, conditionCost: Int64 = 0, hashCost: Int64 = 0, unmatchedCost: Int64 = 0, unmatchedScanCost: Int64 = 0
        defer {
            context.executionProfile?.add(plan.profileIdentity, .total, context.meter.accrued - start)
            context.executionProfile?.add(plan.probeProfileIdentity, .total, probeCost)
            context.executionProfile?.add(plan.loopProfileIdentity, .total,
                saturatingAdd(saturatingAdd(sourceCost, probeCost), saturatingAdd(conditionCost, hashCost)))
            context.executionProfile?.add(plan.unmatchedProfileIdentity, .total, unmatchedCost)
            context.executionProfile?.add(plan.unmatchedProfileIdentity, .source, unmatchedScanCost)
        }
        func measure<T>(_ lane: Work, _ body: () throws -> T) rethrows -> T {
            let before = context.meter.accrued
            defer {
                let units = context.meter.accrued - before
                switch lane {
                case .probe: probeCost = saturatingAdd(probeCost, units)
                case .unmatched:
                    unmatchedCost = saturatingAdd(unmatchedCost, units)
                    unmatchedScanCost = saturatingAdd(unmatchedScanCost, units)
                case .unmatchedFilter: unmatchedCost = saturatingAdd(unmatchedCost, units)
                case .condition: conditionCost = saturatingAdd(conditionCost, units)
                case .hash: hashCost = saturatingAdd(hashCost, units)
                case .source: sourceCost = saturatingAdd(sourceCost, units)
                }
            }
            return try body()
        }
        let relation = plan.relation, state = context.writeScope?.foreignKeys ?? ForeignKeyActions.State()
        let filters = relation.view?.filters ?? []
        // Initplans fold in authored source/ON/clause/RETURNING order. Stored
        // defaults retain ordinary per-action evaluation rather than replay.
        try context.subqueries.prepare(plan.source.foldExpressions, context: context)
        func prepare(_ expressions: [ResolvedExpression], sourceOnly: Bool = false) throws {
            if let view = relation.view, !sourceOnly {
                try view.prepare(expressions, context: context, column: { $0 < relation.visibleCount ? $0 : nil })
            } else { try context.subqueries.prepare(expressions, context: context) }
        }
        try prepare([plan.condition])
        if let view = relation.view {
            for filter in filters { try view.prepare([filter.expression], level: filter.level, context: context) }
        }
        for clause in plan.clauses { try prepare(clause.expressions, sourceOnly: clause.match == "not matched by target") }
        try plan.returning.prepare(relation: relation, context: context)
        try relation.view?.prepareChecks(context: context)
        let result = MutationResult(command: "MERGE", columns: plan.returning.columns, hasAffectedRows: !plan.returning.present, meter: context.meter)
        let sourceExecution = SelectExecution(plan: plan.source, context: context)
        defer { sourceExecution.close() }
        var sources: [[SQLValue]] = [], sourceBytes: Int64 = 0
        defer { context.meter.resources.query.release(sourceBytes) }
        while let row = try measure(.source, { try sourceExecution.next() }) {
            let bytes = MemorySizes.row(row)
            try context.meter.reserveQuery(bytes); sourceBytes = saturatingAdd(sourceBytes, bytes)
            sources.append(row)
        }
        sourceExecution.close()
        context.meter.resources.retainStatementRows(sources, bytes: sourceBytes)
        sourceBytes = 0
        let access = try plan.selectAccess(parameters: context.parameters, context: context)
        var actions: [Action] = [], matched: Set<[UInt8]> = [], affected: Set<[UInt8]> = []
        func sourceRow(_ index: Int?) -> [SQLValue] { index.map { sources[$0] } ?? Array(repeating: .null, count: plan.sourceWidth) }
        func scoped(_ old: ScanRecord?, source: Int?, kind: String) -> ExpressionContext {
            let values = sourceRow(source)
            var row = context
            if kind == "not matched by target" { row.column = { values[$0] }; return row }
            let base = old.map { relation.row($0.value, context: context).column }
            row.column = { column in
                if column < relation.visibleCount { return try base?(column) ?? .null }
                return values[column - relation.visibleCount]
            }
            return row
        }
        func choose(_ kind: String, old: ScanRecord?, source: Int?) throws {
            let row = scoped(old, source: source, kind: kind)
            for (ordinal, clause) in plan.clauses.enumerated() where clause.match == kind {
                if let condition = clause.condition, try !row.matches(condition) { continue }
                if case .nothing = clause.action { return }
                if let old {
                    guard affected.insert(old.record.key).inserted else { throw JedError("21000", "MERGE command cannot affect row a second time") }
                }
                let action: Action
                switch clause.action {
                case .nothing: return
                case .delete:
                    guard let old else { throw JedError("XX000", "MERGE DELETE has no target") }
                    action = Action(name: "DELETE", old: old, replacements: [:], source: source, clause: ordinal, key: old.record.key)
                case .update(let changes):
                    guard let old else { throw JedError("XX000", "MERGE UPDATE has no target") }
                    var values: [Int: SQLValue] = [:]
                    for (column, expression) in changes {
                        values[column] = try relation.coerce(row.evaluate(expression), expression: expression, column: column, context: context)
                    }
                    func value(_ column: Int) throws -> SQLValue { try values[column] ?? old.value(column) }
                    try relation.validateNotNull(Array(relation.types.indices), value: value, context: context)
                    try MutationExecutor.validate(plan.checks, relation: relation, value: value, context: context)
                    var key = old.record.key
                    if !Set(changes.map { $0.0 }).isDisjoint(with: relation.table.primaryKey) {
                        key = []
                        for column in relation.table.primaryKey { key += try KeyEncoding.encode(value(column), type: relation.types[column], collatedKey: context.collatedKey) }
                    }
                    action = Action(name: "UPDATE", old: old, replacements: values, source: source, clause: ordinal, key: key)
                case .insert(let rowRelation, let expressions, let supplied):
                    var values: [Int: SQLValue] = [:], evaluated: [Int: SQLValue] = [:]
                    for column in supplied { evaluated[column] = try AssignmentValues.evaluateSlot(expressions[column], context: row) }
                    for column in relation.types.indices {
                        let expression = expressions[column]
                        let value = try evaluated[column] ?? row.evaluate(expression)
                        values[column] = try rowRelation.coerce(value, expression: expression, column: column, context: context)
                    }
                    try relation.validateNotNull(Array(relation.types.indices), value: { values[$0]! }, context: context)
                    try MutationExecutor.validate(plan.checks, relation: relation, value: { values[$0]! }, context: context)
                    var key: [UInt8] = []
                    for column in relation.table.primaryKey { key += try KeyEncoding.encode(values[column]!, type: relation.types[column], collatedKey: context.collatedKey) }
                    // No-PK insertion reserves no rowid before phase two. Its
                    // eventual fixed-width key still participates in disposition.
                    if relation.table.primaryKey.isEmpty { key = Array(repeating: 0, count: 8) }
                    action = Action(name: "INSERT", old: nil, replacements: values, source: source, clause: ordinal, key: key)
                }
                if action.name != "DELETE" { try relation.view?.validateNew(action.value, context: context) }
                actions.append(action); return
            }
        }
        func visible(_ row: ScanRecord) throws -> Bool {
            guard let view = relation.view else { return true }
            if filters.count > 1 { for _ in 1..<filters.count { context.meter.charge(1); try context.meter.step() } }
            for filter in filters {
                if try !view.matches(filter.expression, level: filter.level, base: row.value, context: context) { return false }
            }
            return true
        }
        func compare(_ row: ScanRecord, source: Int) throws -> Bool {
            try measure(.condition) {
                if !filters.isEmpty { context.meter.charge(1); try context.meter.step() }
                guard try visible(row) else { return false }
                return try scoped(row, source: source, kind: "matched").matches(plan.condition)
            }
        }
        func read(_ record: PackedRecord, materialize: Bool = true) throws -> ScanRecord {
            context.meter.charge(1); try context.meter.step()
            let row = ScanRecord(record, types: relation.store.tree.types, pager: relation.store.tree.pool?.pager)
            if materialize { for column in plan.touched { _ = try row.value(column) } }
            return row
        }
        let hashKeys = try plan.hashKeys(access: access, parameters: context.parameters, context: context)
        // The matching table remains live through RETURNING and pending-write
        // admission; its ownership is the whole MERGE, not only the probe loop.
        var retainedHash: HashJoinTable?
        defer { retainedHash?.close() }
        if let keys = hashKeys {
            let cursor = try measure(.probe) { try access.makeCursor(context: context, touched: plan.touched) }
            defer { cursor.close() }
            // Packed target candidates use the ordinary prepared-mutation
            // retention lane. HashJoinTable owns the scheduled Q2 key state.
            var rows: [ScanRecord] = [], values: [[SQLValue]] = []
            while let record = try measure(.probe, { try cursor.next() }) {
                let row = try measure(.probe) { try read(record) }
                var value = Array(repeating: SQLValue.null, count: relation.types.count)
                for column in keys.map(\.left) { value[column] = try row.value(column) }
                rows.append(row); values.append(value)
            }
            cursor.close()
            let hash = try measure(.hash) { try HashJoinTable(rows: values, keys: keys.map { (column: $0.left, type: $0.type) }, context: context) }
            retainedHash = hash
            for (source, values) in sources.enumerated() {
                try context.meter.checkpoint()
                var found = false
                do {
                    let before = context.meter.accrued
                    var callbackCost: Int64 = 0
                    defer { hashCost = saturatingAdd(hashCost, context.meter.accrued - before - callbackCost) }
                    try hash.probe(values, keys: keys.map { (column: $0.right, type: $0.type) }) { index in
                        let callbackStart = context.meter.accrued
                        defer { callbackCost = saturatingAdd(callbackCost, context.meter.accrued - callbackStart) }
                        let row = rows[index]
                        if try compare(row, source: source) {
                            found = true
                            if plan.hasBySource { matched.insert(row.record.key) }
                            try choose("matched", old: row, source: source)
                        }
                    }
                }
                if !found { try choose("not matched by target", old: nil, source: source) }
            }
        } else {
            for source in sources.indices {
                try context.meter.checkpoint()
                var probeContext = context
                let values = sources[source]
                probeContext.outerRows.insert({ values[$0] }, at: 0)
                let cursor = try measure(.probe) { try access.makeCursor(context: probeContext, touched: plan.touched) }
                defer { cursor.close() }
                var found = false
                while let record = try measure(.probe, { try cursor.next() }) {
                    let row = try measure(.probe) { try read(record, materialize: false) }
                    for column in plan.touched { _ = try row.value(column) }
                    if try compare(row, source: source) {
                        found = true
                        if plan.hasBySource { matched.insert(record.key) }
                        try choose("matched", old: row, source: source)
                    }
                }
                cursor.close()
                if !found { try choose("not matched by target", old: nil, source: source) }
            }
        }
        if plan.hasBySource {
            let unmatched = try plan.selectUnmatchedAccess(parameters: context.parameters, context: context)
            let cursor = try measure(.unmatched) { try unmatched.makeCursor(context: context, touched: plan.touched) }
            defer { cursor.close() }
            while let record = try measure(.unmatched, { try cursor.next() }) {
                let row = try measure(.unmatched) { try read(record, materialize: false) }
                if matched.contains(record.key) { continue }
                for column in plan.touched { _ = try row.value(column) }
                if try measure(.unmatchedFilter, { try visible(row) }) { try choose("not matched by source", old: row, source: nil) }
            }
        }
        let vacated = Set(actions.compactMap { $0.old?.record.key }), placed = actions.filter { $0.name != "DELETE" }
        var keys: Set<[UInt8]> = []
        if !relation.table.primaryKey.isEmpty {
            for action in placed {
                guard keys.insert(action.key).inserted,
                      try vacated.contains(action.key) || relation.store.get(action.key) == nil else { throw MutationExecutor.duplicate(relation, value: action.value, context: context) }
            }
        }
        for index in plan.indexes where index.index.unique {
            guard let existing = plan.read.indexes[index.index.name] else { throw JedError.corruption("missing MERGE unique index") }
            var prefixes: Set<[UInt8]> = []
            for action in placed {
                guard let prefix = try index.prefix(action.value), !prefix.hasNull else { continue }
                guard prefixes.insert(prefix.bytes).inserted,
                      try !index.conflicts(prefix, store: existing, vacated: vacated) else { throw MutationExecutor.duplicateIndex(index, relation: relation, value: action.value, context: context) }
            }
        }
        let candidates = placed.map(\.candidate)
        try ExclusionValidation.validate(candidates, table: relation.table, snapshot: plan.read,
            settings: plan.inputSettings, context: context, vacated: vacated)
        for action in placed {
            try state.validateChildren([action.candidate], child: relation.table, snapshot: snapshot,
                assigned: plan.clauses[action.clause].assigned, vacated: vacated, context: context, selfCandidates: candidates)
        }
        let compression = CostMeter()
        for action in placed {
            let inputs = relation.types.indices.map { column -> RecordInput in
                if let value = action.replacements[column] { return .value(value) }
                let old = action.old!.record
                return .kept(old.cells[column], pending: old.overflows[column])
            }
            action.record = try RecordDisposition.plan(key: action.key, values: inputs, types: relation.types,
                codec: relation.store.tree.codec, meter: compression, pager: relation.store.tree.pool?.pager)
        }
        context.meter.charge(compression.accrued); try context.meter.checkpoint()
        for action in actions {
            try plan.returning.emit(source: action.source.map { sources[$0] }, old: action.old.map { $0.value },
                new: action.name == "DELETE" ? nil : action.value, action: action.name,
                relation: relation, context: context, result: result)
        }
        var store = snapshot.tables[relation.table.name] ?? relation.store
        if relation.table.primaryKey.isEmpty {
            for action in placed where action.name == "INSERT" {
                action.key = try KeyEncoding.encode(.integer(store.allocateRowID()), type: TypeDescriptor(.scalar(.i64)))
                let record = action.record!
                action.record = PackedRecord(key: action.key, cells: record.cells, overflows: record.overflows)
            }
        }
        // All index removals precede additions, while unchanged table keys are
        // replaced in place. Writable WITH removes the current working entry.
        try MutationExecutor.mergeWrites(actions.map { .init(old: $0.old?.record, new: $0.record) }, relation: relation,
            indexes: plan.indexes, allocated: store, snapshot: &snapshot, settings: plan.settings, indexTwoPass: true)
        try context.meter.syncPending(snapshot.pendingBytes)
        for deleting in [true, false] {
            let transitions = actions.filter { deleting ? $0.name == "DELETE" : $0.name == "UPDATE" }.map { action in
                ForeignKeyActions.Transition(old: .init(key: action.old!.record.key, value: action.old!.value),
                    new: deleting ? nil : action.candidate)
            }
            try ForeignKeyActions.apply(transitions, parent: relation.table, deleting: deleting, snapshot: &snapshot,
                settings: plan.settings, context: context, state: state)
        }
        if state.depth == 0 && !state.deferUntilStatementEnd { try state.settle(snapshot: snapshot, settings: plan.settings, context: context) }
        result.affected = Int64(actions.count)
        try context.meter.syncPending(snapshot.pendingBytes)
        return result
        } catch let error as JedError { throw error.inDatabase(plan.settings.targetDomain) }
    }
}
