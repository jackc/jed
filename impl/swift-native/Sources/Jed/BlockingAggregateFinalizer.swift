/// Finalize retained aggregate inputs by replaying one owner's rows. The final
/// scalar may grow; no second input-sized row collection is held beside it.
internal enum BlockingAggregateFinalizer {
    static func fold(_ state: AggregateState, arguments: [SQLValue], unique: SpillStateMap,
                     prefix: [SQLValue], collections: HashRowTable, owner: UInt64) throws {
        try state.fold(arguments)
        try state.drainRetained { row in
            if state.plan.name.contains("object"), state.plan.name.hasSuffix("_unique") {
                guard try unique.insert(prefix + [row[0]]) else { throw JedError("22030", "duplicate JSON object key value") }
            }
            try collections.append(hash: owner, row: row)
        }
    }

    static func finish(_ state: AggregateState, call: ResolvedAggregateCall, direct: [SQLValue],
                       collections: HashRowTable, owner: UInt64, context: ExpressionContext,
                       ordered: Bool = true) throws -> SQLValue {
        if ordered && !call.order.isEmpty {
            let config = try BlockingRows.configuration(context)
            let sorter = ExternalRowSorter(workMemoryBytes: config.workMemoryBytes, host: config.host,
                meter: context.meter) { left, right in
                    for index in call.order.indices {
                        let term = call.order[index]
                        var type = term.type
                        if ExpressionCollation.collatable(type.type), let name = type.collation, name.description != "C" {
                            type = TypeDescriptor(.scalar(.bytea))
                        }
                        let value = try compare(left[index], right[index], type: type,
                            descending: term.descending, nullsFirst: term.nullsFirst, context: context)
                        if value != 0 { return value }
                    }
                    return 0
                }
            defer { sorter.close() }
            let cursor = try collections.cursor(hash: owner)
            defer { cursor.close() }
            while let row = try cursor.next() { try sorter.append(row) }
            try sorter.finish()
            let inner = AggregateState(plan: call.plan, context: context, bounded: true)
            let unique = SpillStateMap(config: config), folded = HashRowTable(config: config)
            defer { inner.close(); unique.close(); folded.close() }
            while let row = try sorter.next() {
                try context.meter.checkpoint()
                let operand = row[call.order.count]
                let arguments: [SQLValue]
                if call.arguments.count == 1 { arguments = [operand] }
                else if case .composite(let tuple) = operand { arguments = tuple }
                else { throw JedError("58030", "invalid ordered aggregate operand") }
                try fold(inner, arguments: arguments, unique: unique, prefix: [.integer(0), .integer(0)], collections: folded, owner: 0)
            }
            return try finish(inner, call: call, direct: direct, collections: folded, owner: 0, context: context, ordered: false)
        }
        if !call.plan.withinGroup.isEmpty {
            if AggregateOrderedSet.hypothetical.contains(call.plan.name) {
                return try hypothetical(call.plan, direct: direct, collections: collections, owner: owner, context: context)
            }
            return try orderedSet(call.plan, direct: direct, collections: collections, owner: owner, context: context)
        }
        if AggregateCollection.names.contains(call.plan.name) {
            return try collection(state, collections: collections, owner: owner, context: context)
        }
        return try state.finish()
    }

    private static func compare(_ a: SQLValue, _ b: SQLValue, type: TypeDescriptor,
                                descending: Bool, nullsFirst: Bool, context: ExpressionContext) throws -> Int {
        if a.isNull != b.isNull { return a.isNull == nullsFirst ? -1 : 1 }
        let result = try ValueComparison.total(a, b, type: type, collatedKey: context.collatedKey)
        return descending ? -result : result
    }

    private static func hypothetical(_ plan: AggregatePlan, direct: [SQLValue], collections: HashRowTable,
                                     owner: UInt64, context: ExpressionContext) throws -> SQLValue {
        guard direct.count == plan.withinGroup.count else { throw JedError("XX000", "hypothetical argument width mismatch") }
        let distinct = SpillStateMap(config: try BlockingRows.configuration(context))
        defer { distinct.close() }
        let cursor = try collections.cursor(hash: owner); defer { cursor.close() }
        var count: Int64 = 0, before: Int64 = 0, through: Int64 = 0, unique: Int64 = 0
        while let row = try cursor.next() {
            count += 1
            var result = 0
            for index in plan.withinGroup.indices {
                let term = plan.withinGroup[index]
                result = try compare(row[index], direct[index], type: term.type,
                    descending: term.descending, nullsFirst: term.nullsFirst, context: context)
                if result != 0 { break }
            }
            if result < 0 {
                before += 1; through += 1
                if plan.name == "dense_rank", try distinct.insert(row) { unique += 1 }
            } else if result == 0 { through += 1 }
        }
        switch plan.name {
        case "rank": return .integer(before + 1)
        case "dense_rank": return .integer(unique + 1)
        case "percent_rank": return .f64((count == 0 ? 0 : Double(before) / Double(count)).bitPattern)
        case "cume_dist": return .f64((Double(through + 1) / Double(count + 1)).bitPattern)
        default: throw JedError("XX000", "invalid hypothetical aggregate")
        }
    }

    private static func orderedSet(_ plan: AggregatePlan, direct: [SQLValue], collections: HashRowTable,
                                   owner: UInt64, context: ExpressionContext) throws -> SQLValue {
        let config = try BlockingRows.configuration(context), order = plan.withinGroup[0]
        var type = order.type
        if plan.name == "percentile_cont", plan.inputTypes[0].type != .scalar(.interval) { type = TypeDescriptor(.scalar(.f64)) }
        let collated = ExpressionCollation.collatable(type.type) && type.collation != nil && type.collation?.description != "C"
        let sortType = collated ? TypeDescriptor(.scalar(.bytea)) : type
        let sorter = ExternalRowSorter(workMemoryBytes: config.workMemoryBytes, host: config.host, meter: context.meter) { a, b in
            try compare(a[collated ? 1 : 0], b[collated ? 1 : 0], type: sortType,
                descending: order.descending, nullsFirst: order.nullsFirst, context: context)
        }
        defer { sorter.close() }
        let cursor = try collections.cursor(hash: owner); defer { cursor.close() }
        while var row = try cursor.next() {
            if collated { row.append(.bytea(try KeyEncoding.encode(row[0], type: type, collatedKey: context.collatedKey))) }
            try sorter.append(row)
        }
        try sorter.finish()
        if plan.name == "mode" {
            var current = SQLValue.null, best = SQLValue.null, run: Int64 = 0, bestCount: Int64 = 0
            while let row = try sorter.next() {
                if run > 0, try ValueComparison.total(current, row[0], type: type, equality: true) == 0 { run += 1 }
                else { current = row[0]; run = 1 }
                if run > bestCount { best = current; bestCount = run }
            }
            return best
        }
        let values = RowSpool(config: config); defer { values.close() }
        while let row = try sorter.next() { try values.append(row) }
        guard direct.count == 1 else { throw JedError("XX000", "percentile argument width mismatch") }
        func fraction(_ value: SQLValue) throws -> Double? {
            if value.isNull { return nil }
            let result = try AggregateStatistics.number(value)
            guard !result.isNaN, result >= 0, result <= 1 else { throw JedError("22003", "percentile value is not between 0 and 1") }
            return result
        }
        func at(_ p: Double) throws -> SQLValue {
            let position = plan.name == "percentile_disc" ? 0 : p * Double(values.count - 1)
            let lower = plan.name == "percentile_disc" ? min(values.count - 1, max(0, Int64((p * Double(values.count)).rounded(.up)) - 1)) : Int64(position.rounded(.down))
            let upper = plan.name == "percentile_disc" ? lower : Int64(position.rounded(.up))
            let scan = try values.cursor(); defer { scan.close() }
            var index: Int64 = 0, low = SQLValue.null, high = SQLValue.null
            while let row = try scan.next() {
                if index == lower { low = row[0] }
                if index == upper { high = row[0]; break }
                index += 1
            }
            if lower == upper { return low }
            let proportion = position - Double(lower)
            if case .interval(let lo) = low, case .interval(let hi) = high {
                let difference = try TemporalFunctions.add(hi, lo, subtract: true)
                return .interval(try TemporalFunctions.add(lo, AggregateOrderedSet.scaleInterval(difference, by: proportion)))
            }
            let lo = try FloatFunctions.number(low), hi = try FloatFunctions.number(high)
            return .f64((lo + proportion * (hi - lo)).bitPattern)
        }
        if case .array(let array) = direct[0] {
            let fractions = try array.elements.map(fraction)
            guard values.count > 0 else { return .null }
            return .array(try SQLArray(dimensions: array.dimensions, lowerBounds: array.lowerBounds,
                elements: fractions.map { try $0.map(at) ?? .null }))
        }
        guard let value = try fraction(direct[0]), values.count > 0 else { return .null }
        return try at(value)
    }

    private static func collection(_ state: AggregateState, collections: HashRowTable,
                                   owner: UInt64, context: ExpressionContext) throws -> SQLValue {
        let packed = state.packed(), plan = state.plan
        guard case .boolean(true) = packed[0] else { return .null }
        let cursor = try collections.cursor(hash: owner); defer { cursor.close() }
        if plan.name == "array_agg" {
            var elements: [SQLValue] = [], count = 0
            let nested: Bool
            if case .array = plan.inputTypes[0].type { nested = true } else { nested = false }
            while let row = try cursor.next() {
                count += 1
                if nested {
                    guard case .array(let array) = row[0] else { throw JedError("58030", "invalid aggregate array input") }
                    elements.append(contentsOf: array.elements)
                } else { elements.append(row[0]) }
            }
            if nested {
                guard case .integer(let n) = packed[1], n > 0 else { throw JedError("58030", "invalid aggregate array shape") }
                var dimensions: [Int] = [], lower: [Int32] = []
                for index in 0..<Int(n) {
                    guard case .integer(let length) = packed[2 + 2 * index], case .integer(let bound) = packed[3 + 2 * index] else { throw JedError("58030", "invalid aggregate array shape") }
                    dimensions.append(Int(length)); lower.append(Int32(bound))
                }
                return .array(try SQLArray(dimensions: [count] + dimensions, lowerBounds: [1] + lower, elements: elements))
            }
            return .array(try SQLArray(elements: elements))
        }
        if plan.name == "string_agg" {
            var bytes: [UInt8] = [], first = true
            func image(_ value: SQLValue) -> [UInt8] { switch value { case .text(let t): t.utf8; case .bytea(let b): b; default: [] } }
            while let row = try cursor.next() {
                if !first { bytes += image(row[1]) }; first = false; bytes += image(row[0])
            }
            return plan.resultType.type == .scalar(.bytea) ? .bytea(bytes) : .text(SQLText(validatedUTF8: bytes))
        }
        let binary = plan.name.hasPrefix("jsonb"), object = plan.name.contains("object")
        if binary && object {
            let config = try BlockingRows.configuration(context)
            let sorter = ExternalRowSorter(workMemoryBytes: config.workMemoryBytes, host: config.host, meter: context.meter) { a, b in
                guard case .integer(let al) = a[0], case .integer(let bl) = b[0], case .text(let at) = a[1], case .text(let bt) = b[1] else { throw JedError("58030", "invalid JSON key scratch row") }
                if al != bl { return al < bl ? -1 : 1 }
                return at == bt ? 0 : at.utf8.lexicographicallyPrecedes(bt.utf8) ? -1 : 1
            }
            defer { sorter.close() }
            while let row = try cursor.next() {
                guard case .text(let key) = row[0] else { throw JedError("58030", "invalid JSON object key") }
                try sorter.append([.integer(Int64(key.utf8.count)), row[0], row[1]])
            }
            try sorter.finish()
            var members: [SQLJSONMember] = []
            while let row = try sorter.next() {
                guard case .text(let key) = row[1] else { throw JedError("58030", "invalid sorted JSON key") }
                let member = SQLJSONMember(key: key, value: try JSONFunctions.node(row[2]))
                if members.last?.key == key { members[members.count - 1] = member } else { members.append(member) }
            }
            let value = SQLJSONValue.object(members); try SQLJSON.validateDepth(value); return .jsonb(value)
        }
        if binary {
            var nodes: [SQLJSONValue] = []
            while let row = try cursor.next() { nodes.append(try JSONFunctions.node(row[0])) }
            let value = SQLJSONValue.array(nodes); try SQLJSON.validateDepth(value); return .jsonb(value)
        }
        var bytes: [UInt8] = object ? [123, 32] : [91], count = 0
        let breaks: Bool
        switch plan.inputTypes[0].type { case .array, .composite, .record: breaks = true; default: breaks = false }
        while let row = try cursor.next() {
            if count > 0 { bytes += [44, 32] }
            let image: SQLValue
            if object {
                guard case .text(let key) = row[0] else { throw JedError("58030", "invalid JSON object key") }
                bytes += SQLJSON.escaped(key); bytes += [32, 58, 32]; image = row[1]
            } else {
                image = row[0]
                if count > 0 && breaks && !image.isNull { bytes += [10, 32] }
            }
            if case .json(let text) = image { bytes += text.utf8 } else { bytes += [110, 117, 108, 108] }
            count += 1
        }
        bytes += object ? [32, 125] : [93]
        let text = SQLText(validatedUTF8: bytes); _ = try JSONDocument(text, decodeStrings: false)
        return .json(text)
    }
}
