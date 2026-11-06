/// Array kernels receive already resolved, common-element operands. They own
/// only the documented size/inner-comparison charges; callers own operator_eval.
internal enum ArrayFunctions {
    private static let maximumElements = 134_217_727
    private static func array(_ value: SQLValue) throws -> SQLArray? {
        if value.isNull { return nil }
        guard case .array(let array) = value else { throw JedError("42804", "expected array value") }
        return array
    }
    private static func empty() throws -> SQLValue { .array(try SQLArray(elements: [])) }
    private static func growth(_ value: SQLValue, constant: Bool, meter: CostMeter?) throws -> SQLValue {
        if !constant && !value.isNull { meter?.charge(MemorySizes.payload(value)); try meter?.guardCost() }
        return value
    }
    private static func checkSize(_ count: Int) throws {
        guard count <= maximumElements else { throw JedError("54000", "array size exceeds the maximum allowed (134217727)") }
    }
    private static func checkNextBound(_ lower: Int32, length: Int) throws {
        guard Int64(lower) + Int64(length) <= Int32.max else { throw JedError("54000", "array lower bound is too large: \(lower)") }
    }

    static func construct(_ values: [SQLValue], nested: Bool, constant: Bool = false,
                          meter: CostMeter? = nil) throws -> SQLValue {
        if !nested { return try growth(.array(SQLArray(elements: values)), constant: constant, meter: meter) }
        if values.isEmpty { return try empty() }
        guard case .array(let first) = values[0] else {
            throw JedError("2202E", "multidimensional arrays must have array expressions with matching dimensions")
        }
        guard first.elements.isEmpty || values.count <= maximumElements / first.elements.count else { throw JedError("54000", "array size exceeds the maximum allowed (134217727)") }
        var elements: [SQLValue] = []
        for value in values {
            guard case .array(let next) = value, next.dimensions == first.dimensions, next.lowerBounds == first.lowerBounds else {
                throw JedError("2202E", "multidimensional arrays must have array expressions with matching dimensions")
            }
            elements += next.elements
        }
        if first.elements.isEmpty { return try empty() }
        let result = try SQLArray(dimensions: [values.count] + first.dimensions, lowerBounds: [1] + first.lowerBounds, elements: elements)
        return try growth(.array(result), constant: constant, meter: meter)
    }

    private static func append(_ source: SQLValue, value: SQLValue, prepend: Bool) throws -> SQLValue {
        guard let source = try array(source), !source.elements.isEmpty else { return .array(try SQLArray(elements: [value])) }
        guard source.dimensions.count == 1 else { throw JedError("22000", "argument must be empty or one-dimensional array") }
        let lower = source.lowerBounds[0], count = source.elements.count
        if prepend {
            guard lower > Int32.min else { throw JedError("22003", "value out of range for type i32") }
        } else {
            guard Int64(lower) + Int64(count) <= Int32.max else { throw JedError("22003", "value out of range for type i32") }
        }
        try checkSize(count + 1)
        if !prepend { try checkNextBound(lower, length: count + 1) }
        return .array(try SQLArray(dimensions: [count + 1], lowerBounds: [lower],
                                   elements: prepend ? [value] + source.elements : source.elements + [value]))
    }

    private static func concatenate(_ left: SQLValue, _ right: SQLValue) throws -> SQLValue {
        guard let a = try array(left) else { return right }
        guard let b = try array(right) else { return left }
        if a.elements.isEmpty { return right }; if b.elements.isEmpty { return left }
        var dimensions: [Int], lowerBounds: [Int32]
        func incompatible() -> JedError { JedError("2202E", "cannot concatenate incompatible arrays") }
        if a.dimensions.count == b.dimensions.count {
            guard a.dimensions.dropFirst().elementsEqual(b.dimensions.dropFirst()),
                  a.lowerBounds.dropFirst().elementsEqual(b.lowerBounds.dropFirst()) else { throw incompatible() }
            dimensions = a.dimensions; lowerBounds = a.lowerBounds; dimensions[0] += b.dimensions[0]
        } else if a.dimensions.count == b.dimensions.count + 1 {
            guard a.dimensions.dropFirst().elementsEqual(b.dimensions), a.lowerBounds.dropFirst().elementsEqual(b.lowerBounds) else { throw incompatible() }
            dimensions = a.dimensions; lowerBounds = a.lowerBounds; dimensions[0] += 1
        } else if b.dimensions.count == a.dimensions.count + 1 {
            guard b.dimensions.dropFirst().elementsEqual(a.dimensions), b.lowerBounds.dropFirst().elementsEqual(a.lowerBounds) else { throw incompatible() }
            dimensions = b.dimensions; lowerBounds = b.lowerBounds; dimensions[0] += 1
        } else { throw incompatible() }
        try checkSize(a.elements.count + b.elements.count); try checkNextBound(lowerBounds[0], length: dimensions[0])
        return .array(try SQLArray(dimensions: dimensions, lowerBounds: lowerBounds, elements: a.elements + b.elements))
    }

    static func call(name: String, args: [SQLValue], element: TypeDescriptor,
                     context: ScalarCasts.Context = .init(), constant: Bool = false,
                     collatedKey: KeyEncoding.CollatedKey? = nil) throws -> SQLValue {
        func require(_ minimum: Int, _ maximum: Int? = nil) throws {
            guard args.count >= minimum && args.count <= (maximum ?? minimum) else { throw JedError("42883", "invalid array function arguments") }
        }
        switch name {
        case "array_append", "array_prepend", "array_cat":
            try require(2)
            let result: SQLValue
            if name == "array_cat" { result = try concatenate(args[0], args[1]) }
            else { result = try append(args[name == "array_prepend" ? 1 : 0], value: args[name == "array_prepend" ? 0 : 1], prepend: name == "array_prepend") }
            return try growth(result, constant: constant, meter: context.meter)
        case "array_to_string":
            try require(2, 3); return try toString(args, element: element, context: context)
        case "string_to_array":
            try require(2, 3); return try fromString(args)
        case "@>", "<@", "&&", "contains", "contained_by", "overlaps":
            try require(2)
            guard var a = try array(args[0]), var b = try array(args[1]) else { return .null }
            if name == "<@" || name == "contained_by" { swap(&a, &b) }
            let overlap = name == "&&" || name == "overlaps"
            for value in b.elements {
                var found = false
                if !value.isNull {
                    for candidate in a.elements where !candidate.isNull {
                        if try ValueComparison.total(value, candidate, type: element, collatedKey: collatedKey, equality: true) == 0 { found = true; break }
                    }
                }
                if overlap && found { return .boolean(true) }
                if !overlap && !found { return .boolean(false) }
            }
            return .boolean(!overlap)
        case "array_remove", "array_replace", "array_position", "array_positions":
            try require(name == "array_replace" ? 3 : 2, name == "array_position" || name == "array_replace" ? 3 : 2)
            guard let a = try array(args[0]) else { return .null }
            if name != "array_replace" && a.dimensions.count > 1 {
                throw JedError("0A000", name == "array_remove" ? "removing elements from multidimensional arrays is not supported" : "searching for elements in multidimensional arrays is not supported")
            }
            var start = Int64(a.lowerBounds.first ?? 1)
            if name == "array_position" && args.count == 3 {
                guard !args[2].isNull else { throw JedError("22004", "initial position must not be null") }
                guard case .integer(let position) = args[2] else { throw JedError("42804", "expected integer position") }
                start = max(start, position)
            }
            if name == "array_replace" { context.meter?.charge(MemorySizes.payload(args[0])); try context.meter?.guardCost() }
            var matched: [Bool] = [], positions: [SQLValue] = []
            var size = Int64(a.dimensions.count) * GeneratedSpec.memoryArrayDimension
            for (index, value) in a.elements.enumerated() {
                let equal = try ValueComparison.total(value, args[1], type: element, collatedKey: collatedKey, equality: true) == 0
                if name == "array_replace" {
                    matched.append(equal)
                    size = saturatingAdd(size, MemorySizes.value(equal ? args[2] : value))
                } else if name == "array_remove" { matched.append(equal) }
                else if equal {
                    let subscriptValue = Int64(a.lowerBounds[0]) + Int64(index)
                    if name == "array_position", subscriptValue >= start { return .integer(subscriptValue) }
                    if name == "array_positions" { positions.append(.integer(subscriptValue)) }
                }
            }
            if name == "array_position" { return .null }
            if name == "array_positions" { return .array(try SQLArray(elements: positions)) }
            if name == "array_replace" {
                context.meter?.charge(size); try context.meter?.guardCost(); try context.meter?.reserveScalar(size)
                let values = a.elements.indices.map { matched[$0] ? args[2] : a.elements[$0] }
                return .array(try SQLArray(dimensions: a.dimensions, lowerBounds: a.lowerBounds, elements: values))
            }
            let values = a.elements.indices.compactMap { matched[$0] ? nil : a.elements[$0] }
            if values.isEmpty { return try empty() }
            return .array(try SQLArray(dimensions: [values.count], lowerBounds: a.lowerBounds, elements: values))
        default: break
        }
        try require(["array_length", "array_lower", "array_upper"].contains(name) ? 2 : 1)
        if args.contains(where: \.isNull) { return .null }
        guard let a = try array(args[0]) else { return .null }
        switch name {
        case "cardinality": return .integer(Int64(a.elements.count))
        case "array_ndims": return a.dimensions.isEmpty ? .null : .integer(Int64(a.dimensions.count))
        case "array_dims":
            if a.dimensions.isEmpty { return .null }
            let text = zip(a.dimensions, a.lowerBounds).map { "[\($1):\(Int64($1) + Int64($0) - 1)]" }.joined()
            return .text(SQLText(validatedUTF8: Array(text.utf8)))
        case "array_length", "array_lower", "array_upper":
            guard case .integer(let dimension) = args[1] else { throw JedError("42804", "expected array dimension") }
            guard dimension >= 1 && dimension <= a.dimensions.count else { return .null }
            let index = Int(dimension - 1)
            if name == "array_length" { return .integer(Int64(a.dimensions[index])) }
            if name == "array_lower" { return .integer(Int64(a.lowerBounds[index])) }
            return .integer(Int64(a.lowerBounds[index]) + Int64(a.dimensions[index]) - 1)
        default: throw JedError("42883", "unknown array function \(name)")
        }
    }

    private static func toString(_ args: [SQLValue], element: TypeDescriptor, context: ScalarCasts.Context) throws -> SQLValue {
        guard let a = try array(args[0]), !args[1].isNull else { return .null }
        guard case .text(let separator) = args[1] else { throw JedError("42804", "expected text separator") }
        let nullString: SQLText?
        if args.count == 3 && !args[2].isNull {
            guard case .text(let text) = args[2] else { throw JedError("42804", "expected text null string") }; nullString = text
        } else { nullString = nil }
        context.meter?.charge(MemorySizes.payload(args[0])); try context.meter?.guardCost()
        var parts: [SQLText] = [], size: Int64 = 0
        for value in a.elements {
            let part: SQLText
            if value.isNull { guard let nullString else { continue }; part = nullString }
            else if case .scalar = element.type { part = try ScalarCasts.render(value, context: context, containerBoolean: true) }
            else { part = try ContainerText.render(value, type: element, context: context) }
            if !parts.isEmpty { size = saturatingAdd(size, Int64(separator.utf8.count)) }
            size = saturatingAdd(size, Int64(part.utf8.count)); parts.append(part)
        }
        guard size <= 0x3fff_ffff else { throw JedError("54000", "requested length too large") }
        context.meter?.charge(size); try context.meter?.guardCost(); try context.meter?.reserveScalar(size)
        var output: [UInt8] = []; output.reserveCapacity(Int(size))
        for (index, part) in parts.enumerated() { if index != 0 { output += separator.utf8 }; output += part.utf8 }
        return .text(SQLText(validatedUTF8: output))
    }

    private static func fromString(_ args: [SQLValue]) throws -> SQLValue {
        if args[0].isNull { return .null }
        guard case .text(let text) = args[0] else { throw JedError("42804", "expected text input") }
        let separator: SQLText?, nullString: SQLText?
        if args[1].isNull { separator = nil }
        else { guard case .text(let value) = args[1] else { throw JedError("42804", "expected text separator") }; separator = value }
        if args.count == 3 && !args[2].isNull {
            guard case .text(let value) = args[2] else { throw JedError("42804", "expected text null string") }; nullString = value
        } else { nullString = nil }
        let bytes = text.utf8
        if bytes.isEmpty { return try empty() }
        var values: [SQLValue] = []
        func emit(_ range: Range<Int>) {
            let text = SQLText(validatedUTF8: Array(bytes[range]))
            values.append(text == nullString ? .null : .text(text))
        }
        if let separator {
            let pattern = separator.utf8
            if pattern.isEmpty { emit(0..<bytes.count) }
            else {
                // KMP keeps adversarial repeated-prefix separators linear.
                var prefix = Array(repeating: 0, count: pattern.count), matched = 0
                if pattern.count > 1 {
                    for index in 1..<pattern.count {
                        while matched > 0 && pattern[index] != pattern[matched] { matched = prefix[matched - 1] }
                        if pattern[index] == pattern[matched] { matched += 1 }
                        prefix[index] = matched
                    }
                }
                matched = 0
                var start = 0
                for index in bytes.indices {
                    while matched > 0 && bytes[index] != pattern[matched] { matched = prefix[matched - 1] }
                    if bytes[index] == pattern[matched] { matched += 1 }
                    if matched == pattern.count {
                        emit(start..<index + 1 - pattern.count); start = index + 1; matched = 0
                    }
                }
                emit(start..<bytes.count)
            }
        } else {
            var start = 0
            while start < bytes.count {
                var end = start + 1
                while end < bytes.count && bytes[end] & 0xc0 == 0x80 { end += 1 }
                emit(start..<end); start = end
            }
        }
        return .array(try SQLArray(elements: values))
    }

    enum Bound { case omitted; case value(Int64?) }
    enum Subscript { case index(Int64?); case slice(lower: Bound, upper: Bound) }
    static func subscriptValue(_ source: SQLValue, subscripts: [Subscript]) throws -> SQLValue {
        guard let array = try array(source) else { return .null }
        let sliced = subscripts.contains { if case .slice = $0 { true } else { false } }
        if !sliced {
            guard subscripts.count == array.dimensions.count && !array.elements.isEmpty else { return .null }
            var offset = 0
            for (dimension, subscriptValue) in subscripts.enumerated() {
                guard case .index(let optional) = subscriptValue, let value = optional else { return .null }
                let lower = Int64(array.lowerBounds[dimension]), upper = lower + Int64(array.dimensions[dimension]) - 1
                guard value >= lower && value <= upper else { return .null }
                offset = offset * array.dimensions[dimension] + Int(value - lower)
            }
            return array.elements[offset]
        }
        for specification in subscripts {
            switch specification {
            case .index(nil), .slice(.value(nil), _), .slice(_, .value(nil)): return .null
            default: break
            }
        }
        guard subscripts.count <= array.dimensions.count && !array.elements.isEmpty else { return try empty() }
        var starts: [Int] = [], dimensions: [Int] = []
        for dimension in array.dimensions.indices {
            let lower = Int64(array.lowerBounds[dimension]), upper = lower + Int64(array.dimensions[dimension]) - 1
            var start = lower, end = upper
            if dimension < subscripts.count {
                switch subscripts[dimension] {
                case .index(let value): start = 1; end = value!
                case .slice(let l, let u):
                    if case .value(let value) = l { start = value! }
                    if case .value(let value) = u { end = value! }
                }
            }
            start = max(start, lower); end = min(end, upper)
            if start > end { return try empty() }
            starts.append(Int(start - lower)); dimensions.append(Int(end - start + 1))
        }
        let count = dimensions.reduce(1, *)
        var offsets = Array(repeating: 0, count: dimensions.count), values: [SQLValue] = []
        for _ in 0..<count {
            var sourceIndex = 0
            for index in dimensions.indices { sourceIndex = sourceIndex * array.dimensions[index] + starts[index] + offsets[index] }
            values.append(array.elements[sourceIndex])
            for index in dimensions.indices.reversed() {
                offsets[index] += 1
                if offsets[index] < dimensions[index] { break }
                offsets[index] = 0
            }
        }
        return .array(try SQLArray(dimensions: dimensions, lowerBounds: Array(repeating: 1, count: dimensions.count), elements: values))
    }

    typealias ComparisonWork = (String, SQLValue, SQLValue, TypeDescriptor) throws -> Void
    static func widthBucket(_ operand: SQLValue, thresholds: SQLValue, element: TypeDescriptor,
                            meter: CostMeter, comparisonWork: ComparisonWork,
                            collatedKey: KeyEncoding.CollatedKey? = nil) throws -> SQLValue {
        if operand.isNull { return .null }
        guard let array = try array(thresholds) else { return .null }
        guard array.dimensions.count <= 1 else { throw JedError("2202E", "thresholds must be one-dimensional array") }
        guard !array.elements.contains(where: \.isNull) else { throw JedError("22004", "thresholds array must not contain NULLs") }
        if case .f32(let bits) = operand, Float(bitPattern: bits).isNaN { return .integer(Int64(array.elements.count)) }
        if case .f64(let bits) = operand, Double(bitPattern: bits).isNaN { return .integer(Int64(array.elements.count)) }
        var left = 0, right = array.elements.count
        while left < right {
            let middle = left + (right - left) / 2, value = array.elements[middle]
            meter.charge(1); try meter.step()
            try comparisonWork("<", operand, value, element)
            if try ValueComparison.total(operand, value, type: element, collatedKey: collatedKey) < 0 { right = middle }
            else { left = middle + 1 }
        }
        return .integer(Int64(left))
    }

    /// The array form uses total-order comparison inside present record
    /// elements. rowWise is reserved for row-valued subquery quantifiers.
    static func quantified(_ operation: String, all: Bool, operand: SQLValue, source: SQLValue,
                           element: TypeDescriptor, rowWise: Bool = false, meter: CostMeter,
                           comparisonWork: ComparisonWork, collatedKey: KeyEncoding.CollatedKey? = nil) throws -> SQLValue {
        guard let array = try array(source) else { return .null }
        var unknown = false
        for value in array.elements {
            meter.charge(1); try meter.step()
            try comparisonWork(operation, operand, value, element)
            let answer: SQLValue
            if operand.isNull || value.isNull { answer = .null }
            else if rowWise { answer = try ValueComparison.sql(operation, operand, value, type: element, collatedKey: collatedKey) }
            else {
                let comparison = try ValueComparison.total(operand, value, type: element, collatedKey: collatedKey,
                                                           equality: ["=", "<>", "!="].contains(operation))
                switch operation {
                case "=": answer = .boolean(comparison == 0)
                case "<>", "!=": answer = .boolean(comparison != 0)
                case "<": answer = .boolean(comparison < 0)
                case "<=": answer = .boolean(comparison <= 0)
                case ">": answer = .boolean(comparison > 0)
                case ">=": answer = .boolean(comparison >= 0)
                default: throw JedError("42883", "unknown quantified comparison")
                }
            }
            if answer.isNull { unknown = true }
            else if case .boolean(let result) = answer, result != all { return .boolean(!all) }
        }
        return unknown ? .null : .boolean(all)
    }

    static func nullCount(_ args: [SQLValue], nonnull: Bool, arrayForm: Bool = false) throws -> SQLValue {
        let values: [SQLValue]
        if arrayForm {
            guard args.count == 1 else { throw JedError("42883", "invalid variadic arguments") }
            guard let array = try array(args[0]) else { return .null }
            values = array.elements
        } else { values = args }
        return .integer(Int64(values.reduce(0) { $0 + ($1.isNull != nonnull ? 1 : 0) }))
    }

    /// Retains the evaluated operand, then yields one existing element at a
    /// time. next() owns generated_row; the relation adapter must not charge it
    /// again. A collector reserves only the rows it chooses to retain.
    final class Rows {
        private var elements: [SQLValue]
        private var position = 0
        private let meter: CostMeter
        init(_ value: SQLValue, meter: CostMeter) throws { elements = try array(value)?.elements ?? []; self.meter = meter }
        func next() throws -> SQLValue? {
            guard position < elements.count else { close(); return nil }
            do {
                meter.charge(1); try meter.checkpoint()
                let value = elements[position]; position += 1
                if position == elements.count { close() }
                return value
            } catch { close(); throw error }
        }
        func close() { elements = []; position = 0 }
    }
}
