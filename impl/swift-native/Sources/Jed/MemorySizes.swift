/// Deterministic logical sizes, never Swift object sizes (memory.md §3).
internal enum MemorySizes {
    static func payload(_ value: SQLValue) -> Int64 {
        switch value {
        case .text(let text), .json(let text), .jsonpath(let text): Int64(text.utf8.count)
        case .bytea(let bytes): Int64(bytes.count)
        case .decimal(let decimal): Int64(decimal.canonicalDigitGroupCount) * GeneratedSpec.memoryDecimalGroup
        case .array(let array):
            array.elements.reduce(Int64(array.dimensions.count) * GeneratedSpec.memoryArrayDimension) {
                saturatingAdd($0, self.value($1))
            }
        case .composite(let fields): fields.reduce(0) { saturatingAdd($0, self.value($1)) }
        case .range(let range):
            saturatingAdd(range.lower.map(self.value) ?? 0, range.upper.map(self.value) ?? 0)
        case .jsonb(let root): json(root) - GeneratedSpec.memoryValue
        default: 0
        }
    }

    static func value(_ value: SQLValue) -> Int64 {
        saturatingAdd(GeneratedSpec.memoryValue, payload(value))
    }

    static func row(_ row: [SQLValue], touched: [Bool]? = nil) -> Int64 {
        var bytes = GeneratedSpec.memoryRow
        for (index, value) in row.enumerated() {
            let measure = touched == nil || index >= touched!.count || touched![index]
            bytes = saturatingAdd(bytes, measure ? self.value(value) : GeneratedSpec.memoryValue)
        }
        return bytes
    }

    static func json(_ value: SQLJSONValue) -> Int64 {
        let payload: Int64
        switch value {
        case .null, .boolean: payload = 0
        case .string(let string): payload = Int64(string.utf8.count)
        case .number(let decimal): payload = Int64(decimal.canonicalDigitGroupCount) * GeneratedSpec.memoryDecimalGroup
        case .array(let elements): payload = elements.reduce(0) { saturatingAdd($0, json($1)) }
        case .object(let members):
            payload = members.reduce(0) { saturatingAdd($0, saturatingAdd(Int64($1.key.utf8.count), json($1.value))) }
        }
        return saturatingAdd(GeneratedSpec.memoryValue, payload)
    }
}
