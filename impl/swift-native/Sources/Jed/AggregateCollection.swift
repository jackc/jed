/// Retained-input aggregate owner. Reservations follow memory.md §6.3 and are
/// released with the accumulator, before its finalized group row is admitted.
internal final class AggregateCollection {
    static let names: Set<String> = ["array_agg", "string_agg", "json_agg", "jsonb_agg", "json_agg_strict", "jsonb_agg_strict",
                                    "json_object_agg", "jsonb_object_agg", "json_object_agg_unique", "jsonb_object_agg_unique"]
    let plan: AggregatePlan
    let context: ExpressionContext
    private var values: [[SQLValue]] = []
    private var keys: [SQLText] = []
    private var unique: Set<SQLText> = []
    private var reserved: Int64 = 0
    private var seen = false
    private var dimensions: [Int]?, lowerBounds: [Int32] = []
    init(plan: AggregatePlan, context: ExpressionContext) { self.plan = plan; self.context = context }
    private func retain(_ incoming: [SQLValue], measured: [SQLValue]) throws {
        let bytes = measured.reduce(Int64(0)) { saturatingAdd($0, MemorySizes.value($1)) }
        try context.meter.reserveQuery(bytes); reserved = saturatingAdd(reserved, bytes)
        values.append(incoming)
    }
    func fold(_ arguments: [SQLValue]) throws {
        let name = plan.name
        if name == "array_agg" {
            let value = arguments[0]
            if case .array = plan.inputTypes[0].type {
                guard case .array(let array) = value else { throw JedError("22004", "cannot accumulate null arrays") }
                if let dimensions {
                    guard dimensions == array.dimensions && lowerBounds == array.lowerBounds else { throw JedError("2202E", "cannot accumulate arrays of different dimensionality") }
                } else {
                    guard !array.elements.isEmpty else { throw JedError("2202E", "cannot accumulate empty arrays") }
                    guard array.dimensions.count < 6 else { throw JedError("54000", "number of array dimensions exceeds the maximum allowed") }
                    dimensions = array.dimensions; lowerBounds = array.lowerBounds
                }
            }
            seen = true; try retain(arguments, measured: arguments); return
        }
        if name == "string_agg" {
            guard !arguments[0].isNull else { return }
            seen = true; try retain(arguments, measured: arguments); return
        }
        let binary = name.hasPrefix("jsonb"), object = name.contains("object")
        seen = true
        if !object && name.hasSuffix("_strict") && arguments[0].isNull { return }
        let meter = context.meter
        let zoneUnits = arguments.reduce(Int64(0)) { saturatingAdd($0, Self.zoneCount($1)) }
        meter.charge(saturatingAdd(1, zoneUnits)); try meter.step()
        let renderer: JSONFunctions.ScalarRenderer = { value, descriptor, _ in
            switch value {
            case .timestamp, .timestamptz, .date:
                return try Temporal.render(value, timezone: self.context.scalar.timezone, iso: true)
            default:
                var scalar = self.context.scalar; scalar.meter = nil
                return try ContainerText.render(value, type: descriptor, context: scalar, growth: false)
            }
        }
        if object {
            let key = try JSONFunctions.key(arguments[0], type: plan.inputTypes[0], binary: binary, position: 1, meter: meter, scalarRender: renderer)
            if name.hasSuffix("_unique") && unique.contains(key) { throw JedError("22030", "duplicate JSON object key value") }
            let image = try JSONFunctions.image(arguments[1], type: plan.inputTypes[1], binary: binary, meter: meter, scalarRender: renderer)
            try retain([image], measured: arguments)
            keys.append(key)
            if name.hasSuffix("_unique") { unique.insert(key) }
        } else {
            let image = try JSONFunctions.image(arguments[0], type: plan.inputTypes[0], binary: binary, meter: meter, scalarRender: renderer)
            // A json NULL retains its SQL identity for composite/array line breaks.
            try retain([!binary && arguments[0].isNull ? .null : image], measured: arguments)
        }
    }
    func finish() throws -> SQLValue {
        guard seen else { return .null }
        switch plan.name {
        case "array_agg":
            if let dimensions {
                guard values.count <= Int(Int32.max) else { throw JedError("54000", "array size exceeds the maximum allowed") }
                let elements = values.flatMap { row -> [SQLValue] in if case .array(let array) = row[0] { return array.elements }; return [] }
                return .array(try SQLArray(dimensions: [values.count] + dimensions, lowerBounds: [1] + lowerBounds, elements: elements))
            }
            return .array(try SQLArray(elements: values.map { $0[0] }))
        case "string_agg":
            var output: [UInt8] = []
            func bytes(_ value: SQLValue) -> [UInt8] {
                switch value { case .text(let text): text.utf8; case .bytea(let bytes): bytes; default: [] }
            }
            for (index, row) in values.enumerated() { if index > 0 { output += bytes(row[1]) }; output += bytes(row[0]) }
            return plan.resultType.type == .scalar(.bytea) ? .bytea(output) : .text(SQLText(validatedUTF8: output))
        default: break
        }
        let binary = plan.name.hasPrefix("jsonb"), object = plan.name.contains("object")
        if binary {
            let output: SQLJSONValue
            if object { output = .object(SQLJSON.canonicalMembers(try zip(keys, values).map { SQLJSONMember(key: $0.0, value: try JSONFunctions.node($0.1[0])) })) }
            else { output = .array(try values.map { try JSONFunctions.node($0[0]) }) }
            try SQLJSON.validateDepth(output); return .jsonb(output)
        }
        var output: [UInt8] = object ? [123, 32] : [91]
        let breaks: Bool
        switch plan.inputTypes[0].type { case .array, .composite, .record: breaks = true; default: breaks = false }
        for (index, row) in values.enumerated() {
            if index > 0 { output += [44, 32] }
            if object { output += SQLJSON.escaped(keys[index]); output += [32, 58, 32] }
            else if index > 0 && breaks && !row[0].isNull { output += [10, 32] }
            if case .json(let text) = row[0] { output += text.utf8 } else { output += [110, 117, 108, 108] }
        }
        output += object ? [32, 125] : [93]
        let text = SQLText(validatedUTF8: output)
        _ = try JSONDocument(text, decodeStrings: false)
        return .json(text)
    }
    private static func zoneCount(_ value: SQLValue) -> Int64 {
        switch value {
        case .timestamptz(let n): return n == .min || n == .max ? 0 : 1
        case .array(let array): return array.elements.reduce(0) { saturatingAdd($0, zoneCount($1)) }
        case .composite(let fields): return fields.reduce(0) { saturatingAdd($0, zoneCount($1)) }
        case .range(let range): return saturatingAdd(range.lower.map(zoneCount) ?? 0, range.upper.map(zoneCount) ?? 0)
        default: return 0
        }
    }
    func close() { values = []; keys = []; unique = []; context.meter.resources.query.release(reserved); reserved = 0 }
    deinit { close() }
}
