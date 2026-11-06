/// Window rows remain a charged Q1 buffer through lazy projection. Partition
/// entries, decorations and operand caches have separate deterministic owners.
internal final class WindowSelectExecution {
    private struct Shared {
        var partitions: [[Int]]
        let order: [[SQLValue]]
        let terms: [WindowOrder]
    }
    private let plan: SelectPlan
    private let window: WindowQueryPlan
    private let context: ExpressionContext
    private let hostBatch: HostBatch?
    private var rows: [[SQLValue]] = []
    private var rowBytes: [Int64] = []
    private var reserved: Int64 = 0
    private var position = 0
    private var produced: Int64 = 0
    private var bounded: BlockingWindowExecution?
    private var started = false

    init(plan: SelectPlan, window: WindowQueryPlan, context: ExpressionContext) {
        self.plan = plan; self.window = window; self.context = context
        hostBatch = HostBatch(expressions: plan.projections, context: context)
    }
    func next() throws -> [SQLValue]? {
        if (plan.aggregation != nil || plan.distinctOn != nil || BlockingJoinRows.eligible(plan)), context.spill?.canSpill == true,
           try !plan.scan.hasKnownContradiction(parameters: context.parameters, collatedKey: context.collatedKey) {
            if bounded == nil { bounded = BlockingWindowExecution(plan: plan, context: context) }
            return try bounded?.next()
        }
        if !started { try prepare(); started = true }
        guard position < rows.count, plan.limit == nil || produced < plan.limit! else { return nil }
        let at = position
        let end = position + Int(min(Int64(rows.count - position), plan.limit.map { $0 - produced } ?? Int64.max))
        let row = rows[position]; position += 1; produced += 1
        if plan.productionCharge { context.meter.charge(1); try context.meter.guardCost() }
        var current = window.rowContext(row, parent: context); current.hostBatch = hostBatch
        try hostBatch?.setRow(at, count: end, context: current) { source, column in
            let values = self.rows[source], slot = self.window.slot(column)
            return values.indices.contains(slot) ? values[slot] : nil
        }
        return try plan.projections.map { try current.evaluate($0) }
    }
    private func reserve(_ bytes: Int64) throws {
        try context.meter.reserveQuery(bytes); reserved = saturatingAdd(reserved, bytes)
    }
    private func release(_ bytes: Int64) { context.meter.resources.query.release(bytes); reserved -= bytes }
    private func append(_ row: [SQLValue], bytes: Int64) throws {
        try reserve(bytes); rows.append(row); rowBytes.append(bytes)
    }
    private func append(_ value: SQLValue, row: Int) throws {
        let bytes = MemorySizes.value(value)
        try reserve(bytes); rowBytes[row] = saturatingAdd(rowBytes[row], bytes); rows[row].append(value)
    }
    private func prepare() throws {
        try context.profile(plan.profileIdentity, .window, excluding: [.source, .filter, .aggregate, .sort]) { try prepareBody() }
    }
    private func prepareBody() throws {
        if let aggregate = plan.aggregation {
            let input = GroupedSelectExecution(plan: plan, aggregate: aggregate, context: context, rawOutput: true)
            defer { input.close() }
            while let item = try input.nextRaw() { try append(item.row, bytes: item.bytes) }
        } else {
            let input = SelectInput(plan: plan, context: context); defer { input.close() }
            if let prefix = prefixLimit() {
                try input.prime()
                while Int64(rows.count) < prefix, let row = try input.next() {
                    var current = context; current.column = { row[$0] }
                    if !input.predicateApplied, let predicate = plan.predicate, try !context.profile(plan.profileIdentity, .filter, { try current.matches(predicate) }) { continue }
                    try append(row, bytes: MemorySizes.row(row))
                }
            } else {
                while let row = try input.next() { try append(row, bytes: MemorySizes.row(row)) }
                if !input.predicateApplied, let predicate = plan.predicate {
                    var kept: [[SQLValue]] = [], sizes: [Int64] = []
                    for (row, bytes) in zip(rows, rowBytes) {
                        var current = context; current.column = { row[$0] }
                        if try context.profile(plan.profileIdentity, .filter, { try current.matches(predicate) }) { kept.append(row); sizes.append(bytes) } else { release(bytes) }
                    }
                    rows = kept; rowBytes = sizes
                }
            }
        }
        try apply()
        if !plan.orderedByScan { try sortOutput() }
        position = Int(min(Int64(rows.count), plan.offset))
    }
    private func prefixLimit() -> Int64? {
        guard plan.distinctOn == nil, plan.store != nil, plan.relation == nil, plan.aggregation == nil, !plan.distinct,
              plan.orderedByScan, let limit = plan.limit, window.keys.isEmpty else { return nil }
        let keys = plan.scan.keys
        guard !keys.isEmpty else { return nil }
        for call in window.calls {
            guard call.partition.isEmpty, !call.order.isEmpty,
                  !plan.descending || call.order.count >= keys.count else { return nil }
            for index in 0..<min(keys.count, call.order.count) {
                guard call.order[index].slot == keys[index], call.order[index].descending == plan.descending,
                      (call.order[index].type.collation?.description ?? "C") == (plan.scan.types[keys[index]].collation?.description ?? "C") else { return nil }
            }
            switch call.name {
            case "row_number", "rank", "dense_rank", "lag": break
            case "percent_rank", "cume_dist", "ntile", "lead": return nil
            default:
                let end = call.frame?.end ?? .current
                switch end {
                case .unbounded(false), .offset(_, false): return nil
                case .current:
                    if call.frame?.mode != "rows", call.order.count < keys.count { return nil }
                default: break
                }
            }
        }
        return saturatingAdd(plan.offset, limit)
    }
    private func charge(_ units: Int64) throws {
        if units > 0 { context.meter.charge(units); try context.meter.guardCost() }
    }
    private func decorated(_ value: SQLValue, type: TypeDescriptor) throws -> (SQLValue, Int64) {
        guard let name = type.collation, name.description != "C", !value.isNull, ExpressionCollation.collatable(type.type) else {
            try charge(MemorySizes.payload(value)); return (value, 0)
        }
        let units: Int64
        switch value {
        case .text(let text): units = Int64(text.scalarCount)
        case .array(let array): units = array.elements.reduce(0) { total, item in
            if case .text(let text) = item { return saturatingAdd(total, Int64(text.scalarCount)) }; return total
        }
        default: units = 0
        }
        try charge(units)
        let key = SQLValue.bytea(try KeyEncoding.encode(value, type: type, collatedKey: context.collatedKey))
        let bytes = MemorySizes.value(key); try reserve(bytes); return (key, bytes)
    }
    private func compare(_ a: [SQLValue], _ b: [SQLValue], terms: [WindowOrder]) throws -> Int {
        for index in terms.indices {
            let term = terms[index], left = a[index], right = b[index]
            if left.isNull != right.isNull { return left.isNull == term.nullsFirst ? -1 : 1 }
            var type = term.type
            if let name = type.collation, name.description != "C", ExpressionCollation.collatable(type.type) { type = TypeDescriptor(.scalar(.bytea)) }
            let result = try ValueComparison.total(left, right, type: type)
            if result != 0 { return term.descending ? -result : result }
        }
        return 0
    }
    static func boundedStage(_ input: RowSpool, plan: SelectPlan, context: ExpressionContext) throws -> RowSpool {
        guard let window = plan.window else { return input }
        defer { input.close() }
        let execution = WindowSelectExecution(plan: plan, window: window, context: context)
        defer { execution.close() }
        return try context.profile(plan.profileIdentity, .window) {
            let cursor = try input.cursor(); defer { cursor.close() }
            while let row = try cursor.next() {
                try execution.append(BlockingOutputLayout.expanded(row, aggregate: plan.aggregation), bytes: MemorySizes.row(row))
            }
            try execution.apply()
            execution.release(execution.reserved)
            let output = RowSpool(config: try BlockingRows.configuration(context))
            do {
                for row in execution.rows { try output.append(BlockingOutputLayout.compact(row, aggregate: plan.aggregation)) }
                return output
            } catch { output.close(); throw error }
        }
    }
    private func apply() throws {
        guard !rows.isEmpty else { return }
        for index in rows.indices {
            let current = window.rowContext(rows[index], parent: context)
            for key in window.keys { try append(current.evaluate(key), row: index) }
        }
        func same(_ a: ResolvedWindow, _ b: ResolvedWindow) -> Bool {
            a.partition == b.partition && a.order.count == b.order.count && zip(a.order, b.order).allSatisfy {
                $0.slot == $1.slot && $0.descending == $1.descending && $0.nullsFirst == $1.nullsFirst &&
                    ($0.type.collation?.description ?? "C") == ($1.type.collation?.description ?? "C")
            }
        }
        var representatives: [Int] = [], assigned: [Int] = []
        for index in window.calls.indices {
            if let group = representatives.firstIndex(where: { same(window.calls[$0], window.calls[index]) }) { assigned.append(group) }
            else { assigned.append(representatives.count); representatives.append(index) }
        }
        var groups: [Shared] = [], transient: Int64 = 0
        defer { release(transient) }
        for representative in representatives {
            let call = window.calls[representative]
            var partitions: [[Int]] = [], lookup: [[UInt8]: Int] = [:]
            for index in rows.indices {
                let values = call.partition.map { rows[index][window.slot($0)] }
                for value in values { try charge(MemorySizes.payload(value)) }
                let key = try ValueEqualityKey.encode(values)
                let partition: Int
                if let existing = lookup[key] { partition = existing }
                else {
                    let bytes = values.reduce(GeneratedSpec.memoryEntry) { saturatingAdd($0, MemorySizes.value($1)) }
                    try reserve(bytes); transient = saturatingAdd(transient, bytes)
                    partition = partitions.count; lookup[key] = partition; partitions.append([])
                }
                partitions[partition].append(index)
            }
            var ordering: [[SQLValue]] = []
            for row in rows {
                var keys: [SQLValue] = []
                for term in call.order {
                    let (key, bytes) = try decorated(row[window.slot(term.slot)], type: term.type)
                    transient = saturatingAdd(transient, bytes); keys.append(key)
                }
                ordering.append(keys)
            }
            for index in partitions.indices {
                try partitions[index].sort {
                    let result = try compare(ordering[$0], ordering[$1], terms: call.order)
                    return result == 0 ? $0 < $1 : result < 0
                }
            }
            groups.append(Shared(partitions: partitions, order: ordering, terms: call.order))
        }
        for index in window.calls.indices {
            let call = window.calls[index], shared = groups[assigned[index]]
            var results = Array(repeating: SQLValue.null, count: rows.count)
            for ordered in shared.partitions {
                let peers = try WindowFrameIndex(size: ordered.count) {
                    try compare(shared.order[ordered[$0]], shared.order[ordered[$1]], terms: shared.terms) == 0
                }
                try evaluate(call, ordered: ordered, peers: peers, results: &results)
            }
            for row in rows.indices { try append(results[row], row: row) }
        }
    }
    private func evaluate(_ call: ResolvedWindow, ordered: [Int], peers: WindowFrameIndex, results: inout [SQLValue]) throws {
        let meter = context.meter, size = ordered.count
        guard size > 0 else { return }
        func current(_ position: Int) -> ExpressionContext { window.rowContext(rows[ordered[position]], parent: context) }
        func operand(_ position: Int) throws -> [SQLValue] {
            if call.arguments.count > 1, call.aggregate != nil { meter.charge(1); try meter.step() }
            let context = current(position)
            return try call.arguments.map { try context.evaluate($0) }
        }
        func resultStep() throws { try meter.step(); meter.charge(1) }
        func bounds(_ position: Int) throws -> Range<Int> {
            try peers.bounds(position, frame: call.frame, order: call.order) { at, term in rows[ordered[at]][window.slot(term.slot)] }
        }
        func filter(_ position: Int) throws -> Bool {
            guard let filter = call.filter else { return true }
            if case .boolean(true) = try current(position).evaluate(filter) { return true }; return false
        }
        switch call.name {
        case "row_number", "rank", "dense_rank", "percent_rank", "cume_dist":
            for position in 0..<size {
                try resultStep()
                let value: SQLValue
                switch call.name {
                case "row_number": value = .integer(Int64(position + 1))
                case "rank": value = .integer(Int64(peers.starts[position] + 1))
                case "dense_rank": value = .integer(Int64(peers.group[position] + 1))
                case "percent_rank": value = .f64((size == 1 ? 0 : Double(peers.starts[position]) / Double(size - 1)).bitPattern)
                default: value = .f64((Double(peers.ends[position]) / Double(size)).bitPattern)
                }
                results[ordered[position]] = value
            }
        case "ntile":
            let argument = try current(0).evaluate(call.arguments[0])
            var buckets: Int64?
            if case .integer(let value) = argument {
                guard value > 0 else { throw JedError("22014", "argument of ntile must be greater than zero") }; buckets = value
            }
            for position in 0..<size {
                try resultStep()
                if let buckets {
                    let base = Int64(size) / buckets, remainder = Int64(size) % buckets, big = remainder * (base + 1)
                    let at = Int64(position)
                    results[ordered[position]] = .integer(at < big ? at / (base + 1) + 1 : remainder + (at - big) / base + 1)
                }
            }
        case "lag", "lead", "first_value", "last_value", "nth_value":
            var cache: [SQLValue] = [], bytes: Int64 = 0
            defer { release(bytes) }
            for position in 0..<size {
                let value = try current(position).evaluate(call.arguments[0]), amount = MemorySizes.value(value)
                try reserve(amount); bytes = saturatingAdd(bytes, amount); cache.append(value)
            }
            var count: Int64? = 1
            if call.arguments.count > 1 {
                let value = try current(0).evaluate(call.arguments[1])
                if case .integer(let integer) = value { count = integer } else { count = nil }
            }
            if call.name == "nth_value", let count, count < 1 { throw JedError("22016", "argument of nth_value must be greater than zero") }
            for position in 0..<size {
                try resultStep()
                if call.name == "lag" || call.name == "lead" {
                    guard let count else { continue }
                    let target = Int128(position) + (call.name == "lag" ? -Int128(count) : Int128(count))
                    if target >= 0 && target < Int128(size) { results[ordered[position]] = cache[Int(target)] }
                    else if call.arguments.count == 3 { results[ordered[position]] = try current(position).evaluate(call.arguments[2]) }
                } else {
                    let range = try bounds(position)
                    var target: Int?
                    if call.name == "last_value" {
                        for at in range.reversed() where !peers.excludes(position, candidate: at, frame: call.frame) { target = at; break }
                    } else if let count {
                        var remaining = call.name == "first_value" ? 1 : count
                        for at in range where !peers.excludes(position, candidate: at, frame: call.frame) {
                            remaining -= 1; if remaining == 0 { target = at; break }
                        }
                    }
                    if let target { results[ordered[position]] = cache[target] }
                }
            }
        default:
            guard let aggregate = call.aggregate else { throw JedError("XX000", "missing window aggregate plan") }
            if call.frame == nil {
                let state = aggregate.makeState(context: context); defer { state.close() }
                for peer in peers.groups {
                    for position in peer {
                        meter.charge(1)
                        if try filter(position) { try state.fold(operand(position)) }
                    }
                    let value = try state.finish()
                    for position in peer { try resultStep(); results[ordered[position]] = value }
                }
                return
            }
            var cache = Array<[SQLValue]?>(repeating: nil, count: size), bytes: Int64 = 0
            defer { release(bytes) }
            func cached(_ position: Int) throws -> [SQLValue] {
                if call.arguments.isEmpty { return [] }
                if let values = cache[position] { return values }
                let values = try operand(position)
                let amount = MemorySizes.value(values.count == 1 ? values[0] : .composite(values))
                try reserve(amount); bytes = saturatingAdd(bytes, amount); cache[position] = values; return values
            }
            let refold = call.filter != nil || call.frame!.exclusion != "no others"
            var state = aggregate.makeState(context: context)
            defer { state.close() }
            var previous = 0..<0
            let removable = aggregate.name == "count" || aggregate.name == "count_star"
            for position in 0..<size {
                let range = try bounds(position)
                if refold {
                    state.close(); state = aggregate.makeState(context: context)
                    for at in range where !peers.excludes(position, candidate: at, frame: call.frame) {
                        meter.charge(1)
                        if try filter(at) { try state.fold(cached(at)) }
                    }
                } else if (!removable && range.lowerBound > previous.lowerBound) || range.lowerBound < previous.lowerBound || range.upperBound < previous.upperBound {
                    state.close(); state = aggregate.makeState(context: context)
                    for at in range { meter.charge(1); try state.fold(cached(at)) }
                } else {
                    let removeEnd = min(range.lowerBound, previous.upperBound)
                    if previous.lowerBound < removeEnd {
                        for at in previous.lowerBound..<removeEnd { meter.charge(1); try state.unfold(cached(at)) }
                    }
                    let addStart = max(previous.upperBound, range.lowerBound)
                    if addStart < range.upperBound {
                        for at in addStart..<range.upperBound { meter.charge(1); try state.fold(cached(at)) }
                    }
                }
                previous = range
                try resultStep(); results[ordered[position]] = try state.finish()
                if refold { state.close() }
            }
        }
    }
    private func sortOutput() throws {
        try context.profile(plan.profileIdentity, .sort) { try sortOutputBody() }
    }
    private func sortOutputBody() throws {
        guard !plan.order.isEmpty else { return }
        var ordering: [[SQLValue]] = [], decorations: Int64 = 0
        defer { release(decorations) }
        for index in rows.indices {
            let current = window.rowContext(rows[index], parent: context)
            var values = Array(repeating: SQLValue.null, count: plan.order.count)
            for ordinal in plan.keyEvaluationOrder {
                let term = plan.order[ordinal]
                let value = try current.evaluate(term.expression)
                if plan.materializesOrderKey(term) {
                    let bytes = MemorySizes.value(value); try reserve(bytes); rowBytes[index] = saturatingAdd(rowBytes[index], bytes)
                }
                values[ordinal] = value
            }
            ordering.append(values)
        }
        for index in rows.indices {
            for key in plan.keyEvaluationOrder {
                let (value, bytes) = try decorated(ordering[index][key], type: plan.order[key].type)
                decorations = saturatingAdd(decorations, bytes); ordering[index][key] = value
            }
        }
        let terms = plan.order.map { WindowOrder(slot: 0, type: $0.type, descending: $0.descending, nullsFirst: $0.nullsFirst) }
        var indices = Array(rows.indices)
        try indices.sort {
            let result = try compare(ordering[$0], ordering[$1], terms: terms)
            return result == 0 ? $0 < $1 : result < 0
        }
        if let distinct = plan.distinctOn {
            var kept: [Int] = []
            for index in indices {
                if let previous = kept.last, try distinct.same(ordering[previous], ordering[index], order: plan.order) { release(rowBytes[index]) }
                else { kept.append(index) }
            }
            indices = kept
        }
        rows = indices.map { rows[$0] }; rowBytes = indices.map { rowBytes[$0] }
    }
    func close() {
        hostBatch?.reset(); bounded?.close(); bounded = nil; rows = []; rowBytes = []; context.meter.resources.query.release(reserved); reserved = 0 }
    deinit { close() }
}
