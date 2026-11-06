/// Resolved JSON kernels. The evaluator supplies base operator/argument charges
/// and the constant-expression flag; these routines own growth/amplifier extras.
internal enum JSONFunctions {
    typealias ScalarRenderer = (SQLValue, TypeDescriptor, CostMeter) throws -> SQLText

    static func node(_ value: SQLValue) throws -> SQLJSONValue {
        switch value {
        case .jsonb(let node): return node
        case .json(let text), .text(let text): return try JSONDocument(text, validateNumbers: true).materialize()
        default: throw JedError("42804", "JSON value required")
        }
    }

    static func chargeGrowth(_ value: SQLValue, constant: Bool, meter: CostMeter) throws -> SQLValue {
        if !constant { meter.charge(MemorySizes.payload(value)); try meter.guardCost() }
        return value
    }

    static func scalarText(_ value: SQLJSONValue, meter: CostMeter? = nil,
                           constant: Bool = false) throws -> SQLValue {
        switch value {
        case .null: return .null
        case .string(let text): return .text(text)
        case .array, .object: return .text(try SQLJSON.render(value, meter: meter, growth: !constant))
        default: return .text(try SQLJSON.render(value))
        }
    }

    static func field(_ base: SQLValue, key: SQLText, asText: Bool = false,
                      constant: Bool = false, meter: CostMeter) throws -> SQLValue {
        if base.isNull { return .null }
        if case .json(let text) = base {
            let document = try JSONDocument(text)
            guard case .object(let members, _) = document.root,
                  let found = members.last(where: { $0.key == key }) else { return .null }
            return document.extract(found.value, asText: asText)
        }
        guard case .object(let members) = try node(base), let found = members.first(where: { $0.key == key }) else { return .null }
        return asText ? try scalarText(found.value, meter: meter, constant: constant) : .jsonb(found.value)
    }

    static func element(_ base: SQLValue, index: Int32, asText: Bool = false,
                        constant: Bool = false, meter: CostMeter) throws -> SQLValue {
        if base.isNull { return .null }
        if case .json(let text) = base {
            let document = try JSONDocument(text)
            guard case .array(let elements, _) = document.root else { return .null }
            let offset = index < 0 ? Int(index) + elements.count : Int(index)
            guard elements.indices.contains(offset) else { return .null }
            return document.extract(elements[offset], asText: asText)
        }
        let root = try node(base), found: SQLJSONValue
        if case .array(let elements) = root {
            let offset = index < 0 ? Int(index) + elements.count : Int(index)
            guard elements.indices.contains(offset) else { return .null }; found = elements[offset]
        } else {
            guard SQLJSON.kind(root) != "object" && (index == 0 || index == -1) else { return .null }
            found = root
        }
        return asText ? try scalarText(found, meter: meter, constant: constant) : .jsonb(found)
    }

    /// PG's strtoint grammar: leading ASCII whitespace is accepted, trailing
    /// whitespace is not. Array indices must fit i32 before rebasing negatives.
    static func pathIndex(_ text: SQLText) -> Int? {
        let bytes = text.utf8; var cursor = 0
        while cursor < bytes.count && (bytes[cursor] == 32 || (9...13).contains(bytes[cursor])) { cursor += 1 }
        var negative = false
        if cursor < bytes.count && (bytes[cursor] == 43 || bytes[cursor] == 45) { negative = bytes[cursor] == 45; cursor += 1 }
        let start = cursor; var value: Int64 = 0
        while cursor < bytes.count && (48...57).contains(bytes[cursor]) {
            value = value * 10 + Int64(bytes[cursor] - 48)
            if value > (negative ? 2_147_483_648 : 2_147_483_647) { return nil }
            cursor += 1
        }
        guard cursor > start && cursor == bytes.count else { return nil }
        return Int(negative ? -value : value)
    }

    static func extractPath(_ base: SQLValue, path: [SQLText?], asText: Bool = false,
                            constant: Bool = false, meter: CostMeter) throws -> SQLValue {
        if base.isNull || path.contains(where: { $0 == nil }) { return .null }
        let keys = path.compactMap { $0 }
        if case .json(let text) = base {
            let document = try JSONDocument(text)
            if keys.isEmpty { return document.extract(document.root, asText: asText, whole: true) }
            var rebased = [Int?](repeating: nil, count: keys.count)
            var found: JSONSourceNode?
            func walk(_ node: JSONSourceNode, depth: Int, matched: Bool) {
                if depth == keys.count { if matched { found = node }; return }
                switch node {
                case .object(let members, _):
                    for member in members { walk(member.value, depth: depth + 1, matched: matched && member.key == keys[depth]) }
                case .array(let elements, _):
                    var index = pathIndex(keys[depth])
                    if let parsed = index, parsed < 0 {
                        if rebased[depth] == nil { rebased[depth] = parsed + elements.count }
                        index = rebased[depth]
                    }
                    for (offset, element) in elements.enumerated() { walk(element, depth: depth + 1, matched: matched && index == offset) }
                default: break
                }
            }
            walk(document.root, depth: 0, matched: true)
            return found.map { document.extract($0, asText: asText) } ?? .null
        }
        var selected = try node(base)
        for key in keys {
            switch selected {
            case .object(let members):
                guard let member = members.first(where: { $0.key == key }) else { return .null }; selected = member.value
            case .array(let elements):
                guard let index = pathIndex(key) else { return .null }
                let offset = index < 0 ? index + elements.count : index
                guard elements.indices.contains(offset) else { return .null }; selected = elements[offset]
            default: return .null
            }
        }
        return asText ? try scalarText(selected, meter: meter, constant: constant) : .jsonb(selected)
    }

    static func contains(_ left: SQLJSONValue, _ right: SQLJSONValue, root: Bool = true) -> Bool {
        switch (left, right) {
        case (.object(let a), .object(let b)):
            return b.allSatisfy { item in a.contains { $0.key == item.key && contains($0.value, item.value, root: false) } }
        case (.array(let a), .array(let b)):
            return b.allSatisfy { wanted in a.contains { contains($0, wanted, root: false) } }
        case (.array(let a), _) where root && SQLJSON.kind(right) != "object":
            return a.contains { SQLJSON.compare($0, right, root: false) == 0 }
        default:
            return SQLJSON.kind(left) == SQLJSON.kind(right) && SQLJSON.compare(left, right, root: false) == 0
        }
    }

    static func exists(_ value: SQLJSONValue, key: SQLText) -> Bool {
        switch value {
        case .object(let members): return members.contains { $0.key == key }
        case .array(let elements): return elements.contains { if case .string(let text) = $0 { return text == key }; return false }
        case .string(let text): return text == key
        default: return false
        }
    }

    static func concatenate(_ left: SQLJSONValue, _ right: SQLJSONValue,
                            constant: Bool, meter: CostMeter) throws -> SQLValue {
        let result: SQLJSONValue
        switch (left, right) {
        case (.object(let a), .object(let b)): result = .object(SQLJSON.canonicalMembers(a + b))
        case (.array(let a), .array(let b)): result = .array(a + b)
        case (.array(let a), _): result = .array(a + [right])
        case (_, .array(let b)): result = .array([left] + b)
        default: result = .array([left, right])
        }
        try SQLJSON.validateDepth(result)
        return try chargeGrowth(.jsonb(result), constant: constant, meter: meter)
    }

    static func delete(_ value: SQLJSONValue, keys: [SQLText?], dimensions: Int = 1) throws -> SQLJSONValue {
        guard dimensions <= 1 else { throw JedError("2202E", "wrong number of array subscripts") }
        let keys = Set(keys.compactMap { $0 })
        switch value {
        case .object(let members): return .object(members.filter { !keys.contains($0.key) })
        case .array(let elements): return .array(elements.filter { if case .string(let text) = $0 { return !keys.contains(text) }; return true })
        default: throw JedError("22023", "cannot delete from scalar")
        }
    }

    static func delete(_ value: SQLJSONValue, index: Int32) throws -> SQLJSONValue {
        guard case .array(var elements) = value else {
            throw JedError("22023", SQLJSON.kind(value) == "object" ? "cannot delete from object using integer index" : "cannot delete from scalar")
        }
        let index = index < 0 ? Int(index) + elements.count : Int(index)
        if elements.indices.contains(index) { elements.remove(at: index) }
        return .array(elements)
    }

    enum Mutation { case set(create: Bool), insert(after: Bool), delete }

    static func mutate(_ value: SQLJSONValue, path: [SQLText?], dimensions: Int = 1,
                       replacement: SQLJSONValue = .null, operation: Mutation,
                       constant: Bool, meter: CostMeter) throws -> SQLValue {
        guard dimensions <= 1 else { throw JedError("2202E", "wrong number of array subscripts") }
        let deleting: Bool; if case .delete = operation { deleting = true } else { deleting = false }
        guard SQLJSON.kind(value) == "array" || SQLJSON.kind(value) == "object" else {
            throw JedError("22023", deleting ? "cannot delete path in scalar" : "cannot set path in scalar")
        }
        let empty: Bool
        switch value { case .array(let a): empty = a.isEmpty; case .object(let a): empty = a.isEmpty; default: empty = false }
        if empty {
            if case .set(create: false) = operation { return .jsonb(value) }
            if deleting { return .jsonb(value) }
        }
        func walk(_ value: SQLJSONValue, at depth: Int) throws -> SQLJSONValue {
            if depth == path.count { return value }
            guard let step = path[depth] else { throw JedError("22004", "path element at position \(depth + 1) is null") }
            let last = depth + 1 == path.count
            switch value {
            case .object(var members):
                if let index = members.firstIndex(where: { $0.key == step }) {
                    if last {
                        switch operation {
                        case .delete: members.remove(at: index)
                        case .set: members[index] = SQLJSONMember(key: step, value: replacement)
                        case .insert:
                            var error = JedError("22023", "cannot replace existing key")
                            error.hint = "Try using the function jsonb_set to replace key value."
                            throw error
                        }
                    } else { members[index] = SQLJSONMember(key: step, value: try walk(members[index].value, at: depth + 1)) }
                } else if last {
                    switch operation {
                    case .set(create: true), .insert: members.append(SQLJSONMember(key: step, value: replacement))
                    default: break
                    }
                }
                return .object(SQLJSON.canonicalMembers(members))
            case .array(var elements):
                guard let parsed = pathIndex(step) else { throw JedError("22P02", "path element at position \(depth + 1) is not an integer: \"\(step)\"") }
                let index = parsed < 0 ? parsed + elements.count : parsed
                if elements.indices.contains(index) {
                    if last {
                        switch operation {
                        case .delete: elements.remove(at: index)
                        case .set: elements[index] = replacement
                        case .insert(let after): elements.insert(replacement, at: index + (after ? 1 : 0))
                        }
                    } else { elements[index] = try walk(elements[index], at: depth + 1) }
                } else if last {
                    switch operation {
                    case .set(create: true), .insert: elements.insert(replacement, at: index < 0 ? 0 : elements.count)
                    default: break
                    }
                }
                return .array(elements)
            default: return value
            }
        }
        let result = try walk(value, at: 0)
        if deleting { return .jsonb(result) }
        try SQLJSON.validateDepth(result)
        return try chargeGrowth(.jsonb(result), constant: constant, meter: meter)
    }

    static func setLax(_ target: SQLValue, path: SQLArray?, replacement: SQLValue,
                       create: Bool?, treatment: SQLText?, constant: Bool, meter: CostMeter) throws -> SQLValue {
        guard !target.isNull, let path, let create else { return .null }
        func invalidTreatment() -> JedError { JedError("22023", "null_value_treatment must be \"delete_key\", \"return_target\", \"use_json_null\", or \"raise_exception\"") }
        guard let treatment else { throw invalidTreatment() }
        let keys = try path.elements.map { item -> SQLText? in
            if item.isNull { return nil }; guard case .text(let text) = item else { throw JedError("42804", "text[] path required") }; return text
        }
        let root = try node(target)
        if !replacement.isNull {
            return try mutate(root, path: keys, dimensions: path.dimensions.count, replacement: node(replacement), operation: .set(create: create), constant: constant, meter: meter)
        }
        switch treatment.description {
        case "return_target": return target
        case "delete_key": return try mutate(root, path: keys, dimensions: path.dimensions.count, operation: .delete, constant: constant, meter: meter)
        case "use_json_null": return try mutate(root, path: keys, dimensions: path.dimensions.count, operation: .set(create: create), constant: constant, meter: meter)
        case "raise_exception":
            var error = JedError("22004", "JSON value must not be null")
            error.detail = "Exception was raised because null_value_treatment is \"raise_exception\"."
            error.hint = "To avoid, either change the null_value_treatment argument or ensure that an SQL NULL is not passed."
            throw error
        default: throw invalidTreatment()
        }
    }

    static func stripNulls(_ node: SQLJSONValue, arrays: Bool = false) -> SQLJSONValue {
        switch node {
        case .object(let members):
            return .object(members.compactMap { member in
                if case .null = member.value { return nil }
                return SQLJSONMember(key: member.key, value: stripNulls(member.value, arrays: arrays))
            })
        case .array(let elements):
            return .array(elements.compactMap { element in
                if arrays, case .null = element { return nil }; return stripNulls(element, arrays: arrays)
            })
        default: return node
        }
    }

    static func stripNulls(_ value: SQLValue, arrays: Bool = false) throws -> SQLValue {
        if value.isNull { return .null }
        guard case .json(let text) = value else { return .jsonb(stripNulls(try node(value), arrays: arrays)) }
        let document = try JSONDocument(text)
        func compact(_ node: JSONSourceNode) -> [UInt8] {
            switch node {
            case .array(let elements, _):
                let kept = elements.filter { if arrays, case .null = $0 { return false }; return true }
                var output: [UInt8] = [91]
                for (index, child) in kept.enumerated() { if index > 0 { output.append(44) }; output += compact(child) }
                return output + [93]
            case .object(let members, _):
                let kept = members.filter { if case .null = $0.value { return false }; return true }
                var output: [UInt8] = [123]
                for (index, member) in kept.enumerated() {
                    if index > 0 { output.append(44) }
                    output += SQLJSON.escaped(member.key!); output.append(58); output += compact(member.value)
                }
                return output + [125]
            default: return Array(text.utf8[node.span])
            }
        }
        return .json(SQLText(validatedUTF8: compact(document.root)))
    }

    static func arrayLength(_ value: SQLValue) throws -> Int64 {
        if case .json(let text) = value {
            let error = JedError("22023", "cannot get array length of a non-array")
            let document = try JSONDocument(text, decodeStrings: false, expectedRoot: "array", rootError: error)
            if case .array(let elements, _) = document.root { return Int64(elements.count) }
        }
        if case .array(let elements) = try node(value) { return Int64(elements.count) }
        throw JedError("22023", "cannot get array length of a non-array")
    }

    static func typeOf(_ value: SQLValue) throws -> SQLText {
        let kind: String
        if case .json(let text) = value { kind = try JSONDocument(text, decodeStrings: false).root.kind }
        else { kind = SQLJSON.kind(try node(value)) }
        return SQLText(validatedUTF8: Array(kind.utf8))
    }

    /// One visitor supports the JSON SRFs; yielding rows and generated_row cost
    /// belong to the evaluator. A caller can stop without copying later values.
    static func each(_ value: SQLValue, array: Bool, asText: Bool = false, keysOnly: Bool = false,
                     meter: CostMeter, yield: (SQLText?, SQLValue) throws -> Bool) throws {
        let kind = array ? "array" : "object"
        if case .json(let text) = value {
            let error = JedError("22023", array ? "cannot call json_array_elements on a non-array" : "cannot deconstruct a scalar")
            let document = try JSONDocument(text, decodeStrings: !array || asText || keysOnly, expectedRoot: kind, rootError: error)
            if case .array(let elements, _) = document.root {
                for element in elements { if try !yield(nil, document.extract(element, asText: asText)) { break } }
            } else if case .object(let members, _) = document.root {
                for member in members { if try !yield(member.key, keysOnly ? .null : document.extract(member.value, asText: asText)) { break } }
            }
            return
        }
        let root = try node(value)
        if array, case .array(let elements) = root {
            for element in elements { if try !yield(nil, asText ? scalarText(element, meter: meter) : .jsonb(element)) { break } }
        } else if !array, case .object(let members) = root {
            for member in members { if try !yield(member.key, keysOnly ? .null : asText ? scalarText(member.value, meter: meter) : .jsonb(member.value)) { break } }
        } else { throw JedError("22023", array ? "cannot extract elements from a non-array" : "cannot call jsonb_each on a non-object") }
    }

    static func isJSON(_ value: SQLValue, kind: String = "value", unique: Bool = false) -> SQLValue {
        if value.isNull { return .null }
        do {
            let actual: String
            switch value {
            case .text(let text), .json(let text): actual = try JSONDocument(text, decodeStrings: unique, uniqueKeys: unique).root.kind
            case .jsonb(let value): actual = SQLJSON.kind(value)
            default: return .boolean(false)
            }
            return .boolean(kind == "value" || kind == actual || (kind == "scalar" && actual != "array" && actual != "object"))
        } catch { return .boolean(false) }
    }

    static func image(_ value: SQLValue, type: TypeDescriptor, binary: Bool, meter: CostMeter,
                      scalarRender: ScalarRenderer, pretty: Bool = false, depth: Int = 0) throws -> SQLValue {
        if value.isNull { return binary ? .jsonb(.null) : .json(SQLText(validatedUTF8: Array("null".utf8))) }
        func result(_ node: SQLJSONValue) throws -> SQLValue {
            binary ? .jsonb(node) : .json(try SQLJSON.render(node))
        }
        switch value {
        case .jsonb(let node): return try result(node)
        case .json(let text): return binary ? .jsonb(try JSONDocument(text, validateNumbers: true).materialize()) : .json(text)
        case .boolean(let value): return try result(.boolean(value))
        case .integer(let value): return try result(.number(SQLDecimal(value)))
        case .decimal(let value): return try result(.number(value))
        case .text(let value): return try result(.string(value))
        case .f32, .f64:
            let rendered = try scalarRender(value, type, meter)
            if ["NaN", "Infinity", "-Infinity"].contains(rendered.description) { return try result(.string(rendered)) }
            return binary ? .jsonb(.number(try SQLDecimal.parse(rendered.description))) : .json(rendered)
        case .array(let array):
            guard case .array(let element) = type.type else { throw JedError("42804", "array descriptor required") }
            var cursor = 0
            func dimension(_ index: Int) throws -> SQLValue {
                try SQLJSON.checkDepth(depth + index + 1)
                let count = array.dimensions.isEmpty ? 0 : array.dimensions[index]
                var output: [SQLValue] = []
                for _ in 0..<count {
                    if index + 1 < array.dimensions.count { output.append(try dimension(index + 1)) }
                    else {
                        output.append(try image(array.elements[cursor], type: TypeDescriptor(element), binary: binary,
                                                meter: meter, scalarRender: scalarRender, depth: depth + index + 1))
                        cursor += 1
                    }
                }
                return try joinArray(output, binary: binary, separator: index == 0 && pretty ? [44, 10, 32] : [44])
            }
            return try dimension(0)
        case .composite(let values):
            let fields: [TypeField]
            switch type.type { case .composite(let descriptor): fields = descriptor.fields; case .record(let members): fields = members; default: throw JedError("42804", "composite descriptor required") }
            guard fields.count == values.count else { throw JedError("42804", "composite field count mismatch") }
            try SQLJSON.checkDepth(depth + 1)
            var members: [(SQLText, SQLValue)] = []
            for (index, field) in fields.enumerated() {
                members.append((field.name, try image(values[index], type: field.descriptor, binary: binary,
                                                     meter: meter, scalarRender: scalarRender, depth: depth + 1)))
            }
            return try joinObject(members, binary: binary, separator: pretty ? [44, 10, 32] : [44], colon: [58])
        default: return try result(.string(scalarRender(value, type, meter)))
        }
    }

    static func joinArray(_ values: [SQLValue], binary: Bool, separator: [UInt8] = [44, 32]) throws -> SQLValue {
        if binary {
            let result = SQLJSONValue.array(try values.map(node))
            try SQLJSON.validateDepth(result); return .jsonb(result)
        }
        var output: [UInt8] = [91]
        for (index, value) in values.enumerated() {
            if index > 0 { output += separator }
            guard case .json(let text) = value else { throw JedError("XX000", "JSON image expected") }; output += text.utf8
        }
        output.append(93)
        let text = SQLText(validatedUTF8: output)
        _ = try JSONDocument(text, decodeStrings: false)
        return .json(text)
    }

    static func joinObject(_ members: [(SQLText, SQLValue)], binary: Bool,
                           separator: [UInt8] = [44, 32], colon: [UInt8] = [32, 58, 32]) throws -> SQLValue {
        if binary {
            let result = SQLJSONValue.object(SQLJSON.canonicalMembers(try members.map { SQLJSONMember(key: $0.0, value: try node($0.1)) }))
            try SQLJSON.validateDepth(result); return .jsonb(result)
        }
        var output: [UInt8] = [123]
        for (index, member) in members.enumerated() {
            if index > 0 { output += separator }
            output += SQLJSON.escaped(member.0); output += colon
            guard case .json(let text) = member.1 else { throw JedError("XX000", "JSON image expected") }; output += text.utf8
        }
        output.append(125)
        let text = SQLText(validatedUTF8: output)
        _ = try JSONDocument(text, decodeStrings: false)
        return .json(text)
    }

    static func key(_ value: SQLValue, type: TypeDescriptor, binary: Bool, position: Int,
                    meter: CostMeter, scalarRender: ScalarRenderer) throws -> SQLText {
        if value.isNull { throw JedError(binary ? "22023" : "22004", binary ? "argument \(position): key must not be null" : "null value not allowed for object key") }
        switch value {
        case .array, .composite, .json, .jsonb: throw JedError("22023", "key value must be scalar, not array, composite, or json")
        case .text(let text): return text
        case .boolean(let boolean): return SQLText(validatedUTF8: Array((boolean ? "true" : "false").utf8))
        case .integer(let integer): return SQLText(validatedUTF8: Array(String(integer).utf8))
        case .decimal(let decimal): return SQLText(validatedUTF8: Array(decimal.description.utf8))
        default: return try scalarRender(value, type, meter)
        }
    }

    static func build(_ values: [SQLValue], types: [TypeDescriptor], object: Bool, binary: Bool,
                      constant: Bool, meter: CostMeter, scalarRender: ScalarRenderer) throws -> SQLValue {
        guard values.count == types.count else { throw JedError("XX000", "builder descriptor count mismatch") }
        if object && values.count % 2 != 0 { throw JedError("22023", "argument list must have even number of elements") }
        let result: SQLValue
        if object {
            var members: [(SQLText, SQLValue)] = []
            for index in stride(from: 0, to: values.count, by: 2) {
                members.append((try key(values[index], type: types[index], binary: binary, position: index + 1,
                                        meter: meter, scalarRender: scalarRender),
                                try image(values[index + 1], type: types[index + 1], binary: binary,
                                          meter: meter, scalarRender: scalarRender)))
            }
            result = try joinObject(members, binary: binary)
        } else {
            result = try joinArray(zip(values, types).map { try image($0.0, type: $0.1, binary: binary, meter: meter, scalarRender: scalarRender) }, binary: binary)
        }
        return try chargeGrowth(result, constant: constant, meter: meter)
    }

    static func object(_ keys: SQLArray, values: SQLArray? = nil, binary: Bool,
                       constant: Bool, meter: CostMeter) throws -> SQLValue {
        var pairs: [(SQLValue, SQLValue)] = []
        if let values {
            guard keys.dimensions.count <= 1 && values.dimensions.count <= 1 else { throw JedError("2202E", "wrong number of array subscripts") }
            guard keys.elements.count == values.elements.count else { throw JedError("2202E", "mismatched array dimensions") }
            pairs = Array(zip(keys.elements, values.elements))
        } else {
            guard keys.dimensions.count <= 2 else { throw JedError("2202E", "wrong number of array subscripts") }
            if keys.dimensions.count == 2 && keys.dimensions[1] != 2 { throw JedError("2202E", "array must have two columns") }
            guard keys.elements.count % 2 == 0 else { throw JedError("2202E", "array must have even number of elements") }
            for index in stride(from: 0, to: keys.elements.count, by: 2) { pairs.append((keys.elements[index], keys.elements[index + 1])) }
        }
        var members: [(SQLText, SQLValue)] = []
        meter.charge(Int64(pairs.count)); try meter.guardCost()
        for (key, value) in pairs {
            guard case .text(let key) = key else { throw JedError("22004", "null value not allowed for object key") }
            let node: SQLJSONValue
            if case .text(let text) = value { node = .string(text) }
            else if value.isNull { node = .null }
            else { throw JedError("42804", "text[] values required") }
            members.append((key, binary ? .jsonb(node) : .json(try SQLJSON.render(node))))
        }
        return try chargeGrowth(joinObject(members, binary: binary, colon: [32, 58, 32]), constant: constant, meter: meter)
    }
}
