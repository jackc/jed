/// Identity belongs to a resolved call, while outcomes belong to one execution
/// site. Prepared metadata may retain the identity but never an outcome cache.
internal final class HostCallIdentity {}

/// Speculative host work only. Scalar replay owns every SQL charge, guard,
/// argument failure, strict NULL and result validation (extensibility.md §4.2.1).
internal final class HostBatch {
    private struct Slot {
        let identity: ObjectIdentifier
        let function: ResolvedFunction
        let host: HostFunction
        var start = 0
        var end = 0
        var outcomes: [Result<SQLValue, Error>?] = []
    }
    private var slots: [Slot] = []
    private var indices: [ObjectIdentifier: Int] = [:]
    private var row = 0

    init?(expressions: [ResolvedExpression], context: ExpressionContext) {
        var pending = Array(expressions.reversed())
        while let expression = pending.popLast() {
            switch expression.node {
            case .call(let function):
                guard let id = function.hostID, id < context.functions.hostFunctions.count,
                      let identity = function.hostCallIdentity, !function.arguments.isEmpty,
                      function.arguments.allSatisfy({ Self.trivial($0) }) else { continue }
                let host = context.functions.hostFunctions[Int(id)]
                guard host.batchKernel != nil, host.volatility != .volatile else { continue }
                let key = ObjectIdentifier(identity)
                if indices[key] == nil {
                    indices[key] = slots.count
                    slots.append(Slot(identity: key, function: function, host: host))
                }
            case .cast(let child, _, _), .convert(let child, _), .isNull(let child, _):
                pending.append(child)
            case .unary(let operation, let child):
                if ["-", "+", "not"].contains(operation) { pending.append(child) }
            case .binary(let operation, let left, let right, _):
                if ["+", "-", "*", "/", "%", "^", "=", "<>", "<", "<=", ">", ">="].contains(operation) {
                    pending.append(right); pending.append(left)
                }
            default: break
            }
        }
        if slots.isEmpty { return nil }
    }

    private static func trivial(_ expression: ResolvedExpression) -> Bool {
        switch expression.node { case .column, .outerColumn, .constant, .parameter: true; default: false }
    }

    func reset() {
        row = 0
        for index in slots.indices {
            slots[index].start = 0; slots[index].end = 0
            slots[index].outcomes.removeAll(keepingCapacity: true)
        }
    }

    func setRow(_ index: Int, count: Int, context: ExpressionContext,
                column: (Int, Int) throws -> SQLValue?) throws {
        precondition(index >= 0 && index < count)
        row = index
        for slotIndex in slots.indices {
            guard index < slots[slotIndex].start || index >= slots[slotIndex].end else { continue }
            let function = slots[slotIndex].function, host = slots[slotIndex].host
            let length = min(count - index, HostReadAhead<SQLValue>.boundedCount(meter: context.meter,
                unit: function.cost, maximum: 1024))
            var columns = Array(repeating: [SQLValue](), count: function.arguments.count)
            var members: [Int] = []
            for sourceRow in index..<(index + length) {
                var arguments: [SQLValue] = []
                for argument in function.arguments {
                    let value: SQLValue?
                    switch argument.node {
                    case .column(let offset):
                        // A corrupt inline field, like an unfetched one, belongs
                        // to replay. Prefetch never changes its failure point.
                        value = try? column(sourceRow, offset)
                    case .outerColumn(let reference):
                        value = reference.depth > 0 && reference.depth <= context.hostBatchOuterRows.count
                            ? context.hostBatchOuterRows[reference.depth - 1]?(reference.index) : nil
                    case .constant(let constant): value = constant
                    case .parameter(let parameter):
                        value = parameter > 0 && parameter <= context.parameters.count ? context.parameters[parameter - 1] : nil
                    default: value = nil
                    }
                    guard let value, !value.isNull else { break }
                    arguments.append(value)
                }
                guard arguments.count == function.arguments.count else { continue }
                for argument in arguments.indices { columns[argument].append(arguments[argument]) }
                members.append(sourceRow)
            }
            var outcomes = Array<Result<SQLValue, Error>?>(repeating: nil, count: length)
            var end = index + length
            if !members.isEmpty {
                try context.meter.poll()
                var output: [SQLValue] = [], failure: Error?
                do { try host.batchKernel!(columns, &output) } catch { failure = error }
                if output.count > members.count || failure != nil && output.count >= members.count {
                    outcomes[members[0] - index] = .failure(JedError("22000", "host batch returned an invalid result count"))
                    end = members[0] + 1
                } else {
                    for outputIndex in output.indices { outcomes[members[outputIndex] - index] = .success(output[outputIndex]) }
                    if output.count < members.count {
                        let failingRow = members[output.count]
                        outcomes[failingRow - index] = .failure(failure ?? JedError("22000", "host batch returned too few results"))
                        end = failingRow + 1
                    }
                }
            }
            slots[slotIndex].start = index; slots[slotIndex].end = end
            slots[slotIndex].outcomes = outcomes
        }
    }

    /// Called only after the original call node's charge and guard. A missing
    /// outcome follows normal argument evaluation and a batch-of-one callback.
    func take(_ function: ResolvedFunction) throws -> SQLValue? {
        guard let identity = function.hostCallIdentity, let index = indices[ObjectIdentifier(identity)],
              row >= slots[index].start, row < slots[index].end else { return nil }
        let offset = row - slots[index].start
        guard let outcome = slots[index].outcomes[offset] else { return nil }
        slots[index].outcomes[offset] = nil
        return try slots[index].host.validateResult(outcome.get())
    }
}

/// A bounded window over an unmetered source. Pull errors follow the successfully
/// read prefix; closing or satisfying LIMIT drops errors replay never reaches.
internal final class HostReadAhead<Element> {
    private(set) var rows: [Element] = []
    private var position = 0
    private var deferred: Error?
    private var exhausted = false

    func reset() {
        rows.removeAll(keepingCapacity: false); position = 0
        deferred = nil; exhausted = false
    }

    func next(cap: Int, pull: () throws -> Element?) throws -> Int? {
        if position == rows.count {
            rows.removeAll(keepingCapacity: true); position = 0
            if let error = deferred { deferred = nil; exhausted = true; throw error }
            if exhausted { return nil }
            for _ in 0..<max(1, min(64, cap)) {
                do {
                    guard let value = try pull() else { exhausted = true; break }
                    rows.append(value)
                } catch { deferred = error; break }
            }
            if rows.isEmpty {
                if let error = deferred { deferred = nil; exhausted = true; throw error }
                return nil
            }
        }
        let index = position; position += 1; return index
    }

    static func cap(meter: CostMeter, unit: Int64, remaining: Int64?) -> Int {
        let bound = boundedCount(meter: meter, unit: unit, maximum: 64)
        return remaining.map { Int(max(1, min(Int64(bound), $0))) } ?? bound
    }

    static func boundedCount(meter: CostMeter, unit: Int64, maximum: Int) -> Int {
        guard unit > 0 else { return maximum }
        var headroom = Int64.max
        if meter.limit > 0 { headroom = max(0, meter.limit - meter.accrued) }
        if let lifetime = meter.lifetime, meter.lifetimeLimit > 0 {
            headroom = min(headroom, max(0, meter.lifetimeLimit - lifetime.accrued))
        }
        // Compare before adding one to avoid overflowing an unlimited budget.
        return Int(min(Int64(maximum - 1), headroom / unit)) + 1
    }
}
