/// Blocking aggregation owns its source rows, group table, and final rows. The
/// compact logical group row is [keys..., results...]; sparse base ordinals are
/// solely the evaluator's addressing layout and do not add logical row fields.
internal final class GroupedSelectExecution {
    private final class Group {
        let keys: [SQLValue]
        let states: [AggregateState]
        let ordering: [AggregateOrdering?]
        var seen: [Set<[UInt8]>]
        var reserved: Int64
        init(keys: [SQLValue], states: [AggregateState], ordering: [AggregateOrdering?], reserved: Int64) {
            self.keys = keys; self.states = states; self.reserved = reserved
            self.ordering = ordering
            seen = Array(repeating: Set<[UInt8]>(), count: states.count)
        }
    }
    private enum PackedOperand {
        case star, nullness(Int), direct(Int), cached(Int)
    }
    private struct Output {
        let row: [SQLValue]
        var bytes: Int64
        let ordinal: Int
        var order: [SQLValue] = []
    }
    private let plan: SelectPlan
    private let aggregate: AggregateQueryPlan
    private let context: ExpressionContext
    private let hostBatch: HostBatch?
    private let rawOutput: Bool
    private var sources: [[SQLValue]] = []
    private var groups: [Group] = []
    private var lookup: [[UInt8]: Int] = [:]
    private var outputs: [Output] = []
    private var reserved: Int64 = 0
    private var sourceBytes: Int64 = 0
    private var bounded: BlockingAggregateExecution?
    private var started = false
    private var position = 0
    private var emitted: Int64 = 0

    init(plan: SelectPlan, aggregate: AggregateQueryPlan, context: ExpressionContext, rawOutput: Bool = false) {
        self.plan = plan; self.aggregate = aggregate; self.context = context; self.rawOutput = rawOutput
        hostBatch = rawOutput ? nil : HostBatch(expressions: plan.projections, context: context)
    }
    func nextRaw() throws -> (row: [SQLValue], bytes: Int64)? {
        guard rawOutput else { throw JedError("XX000", "grouped raw output was not requested") }
        if !started { try prepare(); started = true }
        guard position < outputs.count else { return nil }
        let output = outputs[position]; position += 1
        outputs[position - 1] = Output(row: [], bytes: 0, ordinal: output.ordinal)
        release(output.bytes)
        return (output.row, output.bytes)
    }
    func next() throws -> [SQLValue]? {
        let wholeTableFold = context.executionProfile == nil && aggregate.keys.isEmpty && ColumnarExecution.aggregation(plan, aggregate, context)
        if !rawOutput, context.spill?.canSpill == true, !wholeTableFold,
           try !plan.scan.hasKnownContradiction(parameters: context.parameters, collatedKey: context.collatedKey) {
            if bounded == nil { bounded = BlockingAggregateExecution(plan: plan, aggregate: aggregate, context: context) }
            return try bounded?.next()
        }
        if !started { try prepare(); started = true }
        guard position < outputs.count, plan.limit == nil || emitted < plan.limit! else { return nil }
        let at = position
        let end = position + Int(min(Int64(outputs.count - position), plan.limit.map { $0 - emitted } ?? Int64.max))
        let row = outputs[position].row; position += 1; emitted += 1
        if plan.productionCharge { context.meter.charge(1); try context.meter.guardCost() }
        var current = groupedContext(row); current.hostBatch = hostBatch
        let groupingBase = aggregate.layoutWidth + aggregate.calls.count
        try hostBatch?.setRow(at, count: end, context: current) { source, column in
            let values = self.outputs[source].row
            let slot = column >= 0 ? column : groupingBase - column - 1
            return values.indices.contains(slot) ? values[slot] : nil
        }
        return try plan.projections.map { try current.evaluate($0) }
    }
    private func reserve(_ bytes: Int64) throws {
        try context.meter.reserveQuery(bytes); reserved = saturatingAdd(reserved, bytes)
    }
    private func release(_ bytes: Int64) {
        context.meter.resources.query.release(bytes); reserved -= bytes
    }
    private func source(_ row: [SQLValue]) throws {
        let bytes = MemorySizes.row(row)
        try reserve(bytes); sourceBytes = saturatingAdd(sourceBytes, bytes); sources.append(row)
    }
    private func prepare() throws {
        try context.profile(plan.profileIdentity, .aggregate, excluding: [.source, .filter, .sort]) { try prepareBody() }
    }
    private func prepareBody() throws {
        let meter = context.meter
        let columnar = ColumnarExecution.aggregation(plan, aggregate, context)
        let input = SelectInput(plan: plan, context: context)
        defer { input.close() }
        if !columnar { while let row = try input.next() { try source(row) } }
        // WHERE consumes the complete source materialization, preserving error
        // and memory visitation before any aggregate argument is evaluated.
        if !input.predicateApplied, let predicate = plan.predicate {
            var kept: [[SQLValue]] = []
            for row in sources {
                var current = context; current.column = { row[$0] }
                if try context.profile(plan.profileIdentity, .filter, { try current.matches(predicate) }) { kept.append(row) }
                else { let bytes = MemorySizes.row(row); release(bytes); sourceBytes -= bytes }
            }
            sources = kept
        }
        // Materialized key expressions run once, before expansion into sets.
        for sourceIndex in sources.indices {
            let row = sources[sourceIndex]
            try meter.checkpoint()
            var current = context; current.column = { row[$0] }
            for (column, expression) in zip(aggregate.keyColumns, aggregate.keys) where column >= aggregate.sourceWidth {
                let value = try current.evaluate(expression)
                let bytes = MemorySizes.value(value)
                try reserve(bytes); sourceBytes = saturatingAdd(sourceBytes, bytes)
                sources[sourceIndex].append(value)
            }
        }
        for members in aggregate.sets {
            let membership = Set(members)
            if members.isEmpty { _ = try create(keys: Array(repeating: .null, count: aggregate.keys.count), selected: [], key: []) }
            func fold(_ row: [SQLValue]) throws {
            try meter.checkpoint()
            var current = context; current.column = { row[$0] }
            let allKeys = aggregate.keyColumns.map { row[$0] }
            let keys = members.map { allKeys[$0] }
            for value in keys {
                let payload = MemorySizes.payload(value)
                if payload > 0 { meter.charge(payload); try meter.guardCost() }
            }
            let key = keys.isEmpty ? [] : try ValueEqualityKey.encode(keys)
            let index: Int
            if let existing = lookup[key] { index = existing }
            else { index = try create(keys: allKeys.indices.map { membership.contains($0) ? allKeys[$0] : .null }, selected: keys, key: key) }
            let group = groups[index]
            for (ordinal, call) in aggregate.calls.enumerated() {
                if let filter = call.filter {
                    guard case .boolean(true) = try current.evaluate(filter) else { continue }
                }
                meter.charge(1)
                if AggregateOrderedSet.hypothetical.contains(call.plan.name), !call.plan.withinGroup.isEmpty {
                    var arguments: [SQLValue] = []
                    for (index, expression) in call.arguments.enumerated() {
                        let value = try current.evaluate(expression)
                        try AggregateOrderedSet.chargeKey(value, type: call.plan.withinGroup[index].type, context: current)
                        arguments.append(value)
                    }
                    try group.states[ordinal].fold(arguments)
                    continue
                }
                // Multi-argument aggregates use one logical ROW operand.
                if call.arguments.count > 1 && call.plan.withinGroup.isEmpty { meter.charge(1); try meter.step() }
                let arguments = try call.arguments.map { try current.evaluate($0) }
                let ordered = try group.ordering[ordinal]?.tuple(arguments: arguments, current: current)
                if call.distinct {
                    let operand = arguments.count == 1 ? arguments[0] : .composite(arguments)
                    let payload = MemorySizes.payload(operand)
                    if payload > 0 { meter.charge(payload); try meter.guardCost() }
                    let keepsNull = ["array_agg", "json_agg", "jsonb_agg", "json_agg_strict", "jsonb_agg_strict"].contains(call.plan.name)
                    if operand.isNull && !keepsNull { continue }
                    let key = try ValueEqualityKey.encode([operand])
                    if group.seen[ordinal].contains(key) { continue }
                    let bytes = saturatingAdd(GeneratedSpec.memoryEntry, MemorySizes.value(operand))
                    try reserve(bytes); group.reserved = saturatingAdd(group.reserved, bytes)
                    group.seen[ordinal].insert(key)
                }
                if let ordered { try group.ordering[ordinal]?.append(ordered) }
                else { try group.states[ordinal].fold(arguments) }
            }
        }
            if ColumnarExecution.packedAggregation(plan, aggregate, context) {
                try foldPacked(members: members, membership: membership)
            } else if columnar {
                while let row = try input.next() {
                    var current = context; current.column = { row[$0] }
                    if !input.predicateApplied, let predicate = plan.predicate, try !context.profile(plan.profileIdentity, .filter, { try current.matches(predicate) }) { continue }
                    try fold(row)
                }
            } else { for row in sources { try fold(row) } }
            var finalized: [Output] = []
        for group in groups {
            for index in group.states.indices { try group.ordering[index]?.fold(into: group.states[index]) }
            var row = Array(repeating: SQLValue.null, count: aggregate.layoutWidth)
            for (index, value) in zip(aggregate.keyColumns, group.keys) { row[index] = value }
            let directContext = unmeteredGroup(row)
            let results = try group.states.indices.map { index in
                let direct = try aggregate.calls[index].direct.map { try directContext.evaluate($0) }
                return try group.states[index].finish(direct: direct)
            }
            row.append(contentsOf: results)
            let masks = aggregate.grouping.map { positions -> SQLValue in
                .integer(positions.reduce(Int64(0)) { ($0 << 1) | (membership.contains($1) ? 0 : 1) })
            }
            row.append(contentsOf: masks)
            let logical = group.keys + results + masks, bytes = MemorySizes.row(logical)
            for state in group.states { state.close() }
            for ordering in group.ordering { ordering?.close() }
            group.seen = []; release(group.reserved); group.reserved = 0
            let output = Output(row: row, bytes: bytes, ordinal: outputs.count + finalized.count)
            if columnar { finalized.append(output) }
            else { try reserve(bytes); outputs.append(output) }
        }
            for output in finalized { try reserve(output.bytes); outputs.append(output) }
            groups = []; lookup = [:]
        }
        sources = []; release(sourceBytes); sourceBytes = 0
        if let having = aggregate.having {
            var kept: [Output] = []
            for output in outputs {
                let current = groupedContext(output.row)
                if try current.matches(having) { kept.append(output) } else { release(output.bytes) }
            }
            outputs = kept
        }
        if !rawOutput {
            try sort()
            position = Int(min(Int64(outputs.count), plan.offset))
        }
    }
    private func foldPacked(members: [Int], membership: Set<Int>) throws {
        let meter = context.meter
        // As on SelectInput's scalar drive, the group exists before the access
        // block is charged. No rows are gathered or admitted to the Q1 account.
        let input = try PackedLaneInput(plan: plan, context: context)
        defer { input.close() }
        let row = input.row
        var current = context; current.column = { try row.value($0) }
        let operands = aggregate.calls.map { call -> (column: Int?, nullness: Bool) in
            guard let argument = call.arguments.first, case .column(let column) = argument.node else { return (nil, false) }
            return (column, call.plan.name == "count")
        }
        let filter = PackedLaneFilter(predicate: plan.predicate, types: plan.store!.tree.types,
            touched: plan.touched, context: current)
        // COUNT needs only stored nullness (packed-leaf.md §11 A4). Generic
        // filters retain the original eager touched-column decode; encoded
        // filters defer aggregate/key decoding until their row survives.
        var nullnessOnly = Set(operands.filter { $0.nullness }.compactMap { $0.column })
        for operand in operands where !operand.nullness {
            if let column = operand.column { nullnessOnly.remove(column) }
        }
        for column in aggregate.keyColumns { nullnessOnly.remove(column) }
        let required = Set(operands.filter { !$0.nullness }.compactMap { $0.column })
            .union(aggregate.keyColumns)
        let decoded: [Int]
        if filter.hasEncodedPredicates { decoded = plan.touched.filter { required.contains($0) } }
        else if plan.predicate == nil { decoded = plan.touched.filter { !nullnessOnly.contains($0) } }
        else { decoded = [] } // The plain filter prepares all touched columns.
        // With no predicate there is no decoded value to reuse. Fill one
        // compact vector directly, retaining touched order before any fold.
        // Filtered consumers keep the frame's once-per-record value reuse.
        let direct = plan.predicate == nil, pager = plan.store!.tree.pool?.pager
        var values = Array(repeating: SQLValue.null, count: direct ? decoded.count : 0)
        let keySlots = direct ? aggregate.keyColumns.map { decoded.firstIndex(of: $0)! } : []
        let reads: [PackedOperand] = operands.map { operand in
            guard let column = operand.column else { return .star }
            if operand.nullness { return .nullness(column) }
            return direct ? .direct(decoded.firstIndex(of: column)!) : .cached(column)
        }
        // Without grouping keys the single group and its states never change.
        let fixedStates: [AggregateState]? = aggregate.keys.isEmpty ? groups[0].states : nil
        while let run = try input.nextRun() {
            let node = run.node
            // A clean page decodes common scalars in place; pending overflow
            // values and writer leaves take the general position path.
            let packed: PackedLeaf? = node.pending.isEmpty ? node.leaf : nil
            for ordinal in 0..<run.count {
                let index = run.row(ordinal)
                try input.admitRow()
                if direct {
                    for slot in 0..<decoded.count {
                        let column = decoded[slot]
                        if let packed, let value = try packed.fastValue(row: index, column: column) { values[slot] = value }
                        else { values[slot] = try TreePosition(node: node, row: index).materialize(column, pager: pager) }
                    }
                } else {
                    row.begin(TreePosition(node: node, row: index))
                    guard try filter.keep(row, context: current) else { continue }
                    try row.prepare(decoded)
                }
                try meter.checkpoint()
                let states: [AggregateState]
                if let fixedStates { states = fixedStates }
                else {
                    let allKeys: [SQLValue]
                    if direct { allKeys = keySlots.map { values[$0] } }
                    else { allKeys = try aggregate.keyColumns.map { try row.value($0) } }
                    let keys = members.map { allKeys[$0] }
                    for value in keys {
                        let payload = MemorySizes.payload(value)
                        if payload > 0 { meter.charge(payload); try meter.guardCost() }
                    }
                    let key = try ValueEqualityKey.encode(keys)
                    let found: Int
                    if let existing = lookup[key] { found = existing }
                    else { found = try create(keys: allKeys.indices.map { membership.contains($0) ? allKeys[$0] : .null }, selected: keys, key: key) }
                    states = groups[found].states
                }
                for ordinal in states.indices {
                    // Bare columns add no expression cost; every aggregate,
                    // including COUNT(*) and NULL, pays its accumulation unit.
                    meter.charge(1)
                    switch reads[ordinal] {
                    case .star: try states[ordinal].foldStar()
                    case .nullness(let column):
                        try states[ordinal].foldCount(isNull: packed?.isNull(row: index, column: column) ?? node.isNull(row: index, column: column))
                    case .direct(let slot): try states[ordinal].foldScalar(values[slot])
                    case .cached(let column): try states[ordinal].foldScalar(row.value(column))
                    }
                }
            }
        }
    }
    private func create(keys: [SQLValue], selected: [SQLValue], key: [UInt8]) throws -> Int {
        let bytes = selected.reduce(GeneratedSpec.memoryEntry) { saturatingAdd($0, MemorySizes.value($1)) }
        try reserve(bytes)
        let index = groups.count
        groups.append(Group(keys: keys, states: aggregate.calls.map { $0.plan.makeState(context: context) },
                            ordering: aggregate.calls.map { $0.order.isEmpty ? nil : AggregateOrdering(order: $0.order, context: context) }, reserved: bytes))
        lookup[key] = index; return index
    }
    private func groupedContext(_ row: [SQLValue]) -> ExpressionContext {
        var current = context
        let groupingBase = aggregate.layoutWidth + aggregate.calls.count
        current.column = { index in row[index >= 0 ? index : groupingBase - index - 1] }
        current.hostBatchColumn = { index in
            let slot = index >= 0 ? index : groupingBase - index - 1
            return row.indices.contains(slot) ? row[slot] : nil
        }
        return current
    }
    private func unmeteredGroup(_ row: [SQLValue]) -> ExpressionContext {
        let meter = CostMeter(resources: context.meter.resources, cancellation: context.meter.cancellation)
        var current = ExpressionContext(meter: meter, parameters: context.parameters, column: { row[$0] })
        current.scalar = context.scalar; current.scalar.meter = meter
        current.collatedKey = context.collatedKey; current.functions = context.functions; current.patterns = context.patterns
        current.outerRows = context.outerRows; current.subqueries = context.subqueries
        current.hostBatchOuterRows = context.hostBatchOuterRows
        current.eagerSubqueries = context.eagerSubqueries
        current.ctes = context.ctes
        return current
    }
    private func sort() throws {
        try context.profile(plan.profileIdentity, .sort) { try sortBody() }
    }
    private func sortBody() throws {
        guard !plan.order.isEmpty else { return }
        let meter = context.meter
        for index in outputs.indices {
            let row = outputs[index].row
            let current = groupedContext(row)
            outputs[index].order = Array(repeating: .null, count: plan.order.count)
            for ordinal in plan.keyEvaluationOrder {
                let term = plan.order[ordinal]
                let value = try current.evaluate(term.expression)
                if plan.materializesOrderKey(term) {
                    let bytes = MemorySizes.value(value); try reserve(bytes); outputs[index].bytes = saturatingAdd(outputs[index].bytes, bytes)
                }
                outputs[index].order[ordinal] = value
            }
        }
        var decorations: Int64 = 0
        for index in outputs.indices {
            for ordinal in plan.keyEvaluationOrder {
                let value = outputs[index].order[ordinal], type = plan.order[ordinal].type
                if let name = type.collation, name.description != "C", !value.isNull, ExpressionCollation.collatable(type.type) {
                    let units: Int64
                    switch value {
                    case .text(let text): units = Int64(text.scalarCount)
                    case .array(let array): units = array.elements.reduce(0) { n, value in
                        if case .text(let text) = value { return saturatingAdd(n, Int64(text.scalarCount)) }; return n
                    }
                    default: units = 0
                    }
                    if units > 0 { meter.charge(units); try meter.guardCost() }
                    let key = try KeyEncoding.encode(value, type: type, collatedKey: context.collatedKey)
                    let bytes = saturatingAdd(GeneratedSpec.memoryValue, Int64(key.count))
                    try reserve(bytes); decorations = saturatingAdd(decorations, bytes)
                    outputs[index].order[ordinal] = .bytea(key)
                } else {
                    let units = MemorySizes.payload(value)
                    if units > 0 { meter.charge(units); try meter.guardCost() }
                }
            }
        }
        try outputs.sort { a, b in
            for ordinal in plan.order.indices {
                let term = plan.order[ordinal], left = a.order[ordinal], right = b.order[ordinal]
                if left.isNull != right.isNull { return left.isNull == term.nullsFirst }
                var type = term.type
                if type.collation?.description != nil && type.collation?.description != "C" && ExpressionCollation.collatable(type.type) {
                    type = TypeDescriptor(.scalar(.bytea))
                }
                let result = try ValueComparison.total(left, right, type: type)
                if result != 0 { return term.descending ? result > 0 : result < 0 }
            }
            return a.ordinal < b.ordinal
        }
        if let distinct = plan.distinctOn {
            var kept: [Output] = []
            for output in outputs {
                if let previous = kept.last, try distinct.same(previous.order, output.order, order: plan.order) { release(output.bytes) }
                else { kept.append(output) }
            }
            outputs = kept
        }
        for index in outputs.indices { outputs[index].order = [] }
        release(decorations)
    }
    func close() {
        hostBatch?.reset()
        bounded?.close(); bounded = nil
        for group in groups { for state in group.states { state.close() }; for ordering in group.ordering { ordering?.close() } }
        sources = []; groups = []; lookup = [:]; outputs = []
        context.meter.resources.query.release(reserved); reserved = 0; sourceBytes = 0
    }
    deinit { close() }
}

/// Equality keys are ephemeral and byte-canonical, independent of collation and
/// the persisted ordered-key type restrictions. Dictionary order is never read.
internal enum ValueEqualityKey {
    static func encode(_ values: [SQLValue]) throws -> [UInt8] {
        var writer = ByteWriter()
        for value in values { try append(value, to: &writer) }
        return writer.bytes
    }
    private static func bytes(_ bytes: [UInt8], to writer: inout ByteWriter) {
        writer.u64(UInt64(bytes.count)); writer.append(bytes)
    }
    private static func append(_ value: SQLValue, to writer: inout ByteWriter) throws {
        switch value {
        case .null: writer.u8(0)
        case .boolean(let value): writer.u8(1); writer.u8(value ? 1 : 0)
        case .integer(let value): writer.u8(2); writer.u64(UInt64(bitPattern: value))
        case .decimal(let value): writer.u8(3); bytes(try value.keyBytes(), to: &writer)
        case .f32(let raw):
            writer.u8(4); let value = Float(bitPattern: raw)
            writer.u32(value == 0 ? 0 : ValueCodec.canonicalFloat32(raw))
        case .f64(let raw):
            writer.u8(5); let value = Double(bitPattern: raw)
            writer.u64(value == 0 ? 0 : ValueCodec.canonicalFloat64(raw))
        case .text(let value): writer.u8(6); bytes(value.utf8, to: &writer)
        case .bytea(let value): writer.u8(7); bytes(value, to: &writer)
        case .uuid(let value): writer.u8(8); writer.append(value.bytes)
        case .date(let value): writer.u8(9); writer.u32(UInt32(bitPattern: value))
        case .timestamp(let value): writer.u8(10); writer.u64(UInt64(bitPattern: value))
        case .timestamptz(let value): writer.u8(11); writer.u64(UInt64(bitPattern: value))
        case .interval(let value):
            writer.u8(12); let span = UInt128(bitPattern: value.comparisonSpan)
            writer.u64(UInt64(truncatingIfNeeded: span >> 64)); writer.u64(UInt64(truncatingIfNeeded: span))
        case .array(let value):
            writer.u8(13); writer.u64(UInt64(value.dimensions.count))
            for (length, lower) in zip(value.dimensions, value.lowerBounds) { writer.u32(UInt32(length)); writer.u32(UInt32(bitPattern: lower)) }
            writer.u64(UInt64(value.elements.count))
            for element in value.elements { try append(element, to: &writer) }
        case .composite(let fields):
            writer.u8(14); writer.u64(UInt64(fields.count))
            for field in fields { try append(field, to: &writer) }
        case .range(let range):
            writer.u8(15); writer.u8(range.isEmpty ? 1 : 0)
            if !range.isEmpty {
                writer.u8(range.lowerInclusive ? 1 : 0); writer.u8(range.upperInclusive ? 1 : 0)
                try append(range.lower ?? .null, to: &writer); try append(range.upper ?? .null, to: &writer)
            }
        case .jsonb(let value): writer.u8(16); try json(value, to: &writer)
        case .json, .jsonpath: throw JedError("42883", "type has no equality operator")
        }
    }
    private static func json(_ value: SQLJSONValue, to writer: inout ByteWriter) throws {
        switch value {
        case .null: writer.u8(0)
        case .boolean(let value): writer.u8(1); writer.u8(value ? 1 : 0)
        case .number(let value): writer.u8(2); bytes(try value.keyBytes(), to: &writer)
        case .string(let value): writer.u8(3); bytes(value.utf8, to: &writer)
        case .array(let values):
            writer.u8(4); writer.u64(UInt64(values.count)); for value in values { try json(value, to: &writer) }
        case .object(let members):
            writer.u8(5); writer.u64(UInt64(members.count))
            for member in members { bytes(member.key.utf8, to: &writer); try json(member.value, to: &writer) }
        }
    }
}
