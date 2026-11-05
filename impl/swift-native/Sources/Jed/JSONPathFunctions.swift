/// A single execution owns its continuation and regex scratch. Calling next()
/// resumes the pending item; it never replays an earlier accessor or predicate.
internal final class JSONPathCursor {
    private var produce: (() throws -> SQLJSONValue?)?
    private let silent: Bool
    private var finished = false
    private(set) var suppressedError: JedError?
    init(silent: Bool, produce: @escaping () throws -> SQLJSONValue?) {
        self.silent = silent; self.produce = produce
    }
    func close() { finished = true; produce = nil }
    func next() throws -> SQLJSONValue? {
        guard !finished, let produce else { return nil }
        do {
            let result = try produce()
            if result == nil { close() }
            return result
        } catch {
            close()
            if silent, let error = error as? JedError, error.code.hasPrefix("22") { suppressedError = error; return nil }
            throw error
        }
    }
}

internal enum JSONPathFunctions {
    static func regex(_ pattern: SQLText, _ flags: SQLText, _ meter: CostMeter?) throws -> JSONPathRegex {
        let options = RegexOptions(ignoreCase: flags.utf8.contains(105), dotAll: flags.utf8.contains(115),
                                   lineAnchors: flags.utf8.contains(109), newlineSensitive: true,
                                   quote: flags.utf8.contains(113))
        let program = try RegexProgram.compile(pattern: pattern, options: options)
        if let meter { try program.chargeCompile(meter) }
        let scratch = RegexScratch()
        return JSONPathRegex(matches: { try program.isMatch($0, meter: $1, scratch: scratch) })
    }

    static func call(_ name: String, context: SQLJSONValue, path: SQLText,
                     variables: SQLJSONValue = .object([]), silent: Bool = false,
                     constant: Bool = false, operatorForm: Bool = false,
                     meter: CostMeter, regexCompiler: JSONPathRegexCompiler? = nil) throws -> SQLValue {
        guard case .object = variables else {
            var error = JedError("22023", "\"vars\" argument is not an object")
            error.detail = "Jsonpath parameters should be encoded as key-value pairs of \"vars\" object."
            throw error
        }
        let program = try JSONPathProgram.compile(path, meter: meter, regexCompiler: regexCompiler ?? regex)
        let evaluation = JSONPathEvaluation(program: program, context: context, variables: variables,
                                            variableBase: !operatorForm, meter: meter)
        let exists = name == "exists" || name == "jsonb_path_exists" || name == "jsonb_path_exists_tz" || name == "@?"
        let array = name == "array" || name == "jsonb_path_query_array" || name == "jsonb_path_query_array_tz"
        let producer = evaluation.cursor(program.expression, current: evaluation.root, existenceTest: exists)
        let result = JSONPathCursor(silent: silent || operatorForm) { try producer.next()?.value }
        var first: SQLJSONValue?, count = 0, items: [SQLJSONValue] = []
        while let value = try result.next() {
            if count == 0 { first = value }
            count = min(2, count + 1)
            if array { items.append(value) }
            if exists && !program.strict { break }
        }
        if exists { return result.suppressedError != nil ? .null : .boolean(first != nil) }
        switch name {
        case "match", "jsonb_path_match", "jsonb_path_match_tz", "@@":
            if count == 1, let first {
                if case .boolean(let truth) = first { return .boolean(truth) }
                if case .null = first { return .null }
            }
            if silent || operatorForm { return .null }
            throw JedError("22038", "single boolean result is expected")
        case "first", "jsonb_path_query_first", "jsonb_path_query_first_tz": return first.map(SQLValue.jsonb) ?? .null
        case "array", "jsonb_path_query_array", "jsonb_path_query_array_tz":
            let value = SQLJSONValue.array(items); try SQLJSON.validateDepth(value)
            return try JSONFunctions.chargeGrowth(.jsonb(value), constant: constant, meter: meter)
        default: throw JedError("42883", "unknown jsonpath query function")
        }
    }

    static func query(context: SQLJSONValue, path: SQLText, variables: SQLJSONValue = .object([]),
                      silent: Bool = false, meter: CostMeter,
                      regexCompiler: JSONPathRegexCompiler? = nil) throws -> JSONPathCursor {
        guard case .object = variables else { throw JedError("22023", "\"vars\" argument is not an object") }
        let program = try JSONPathProgram.compile(path, meter: meter, regexCompiler: regexCompiler ?? regex)
        let evaluation = JSONPathEvaluation(program: program, context: context, variables: variables, variableBase: true, meter: meter)
        let producer = evaluation.cursor(program.expression, current: evaluation.root)
        return JSONPathCursor(silent: silent) { try producer.next()?.value }
    }
}

private struct JSONPathItem {
    let value: SQLJSONValue
    var base: UInt64 = 0
    var offset = 0
    var last: Int?
    var suppressed = false
}

private enum JSONTruth { case yes, no, unknown }

private final class JSONPathItemCursor {
    let next: () throws -> JSONPathItem?
    init(_ next: @escaping () throws -> JSONPathItem?) { self.next = next }
}

private final class JSONPathEvaluation {
    let program: JSONPathProgram
    let root: JSONPathItem
    let variables: JSONPathItem
    let variableBase: Bool
    let meter: CostMeter
    var nextBase: UInt64
    var layout: [UInt64: [Int: [JSONPathItem]]] = [:]

    init(program: JSONPathProgram, context: SQLJSONValue, variables: SQLJSONValue,
         variableBase: Bool, meter: CostMeter) {
        self.program = program; root = JSONPathItem(value: context)
        self.variables = JSONPathItem(value: variables, base: 1)
        self.variableBase = variableBase; self.meter = meter; nextBase = variableBase ? 2 : 1
    }

    func charge(_ count: Int64 = 1) throws { meter.charge(count); try meter.step() }
    func jsonError(_ error: Error) -> Bool { (error as? JedError)?.code.hasPrefix("22") == true }
    /// Recursive expression nesting is bounded by the compiler. Accessor chains
    /// and fan-out use the explicit continuation stack below.
    func evaluate(_ expression: JSONPathExpression, current: JSONPathItem, existenceTest: Bool = false,
                  emit: (JSONPathItem) throws -> Bool) throws -> Bool {
        let producer = cursor(expression, current: current, existenceTest: existenceTest)
        while let item = try producer.next() { if try !emit(item) { return false } }
        return true
    }

    func cursor(_ expression: JSONPathExpression, current: JSONPathItem, existenceTest: Bool = false) -> JSONPathItemCursor {
        switch expression {
        case .path(let base, let steps):
            let source = cursor(base, current: current)
            var active: JSONPathItemCursor?
            return JSONPathItemCursor {
                while true {
                    if let item = try active?.next() { return item }
                    guard let item = try source.next() else { return nil }
                    active = self.walkCursor(item, steps: steps)
                }
            }
        case .unary(let op, let operand):
            let source = cursor(operand, current: current)
            var values: [JSONPathItem] = [], position = 0
            return JSONPathItemCursor {
                while true {
                    if position == values.count {
                        guard let item = try source.next() else { return nil }
                        values = self.unwrapped(item); position = 0
                        if values.isEmpty { continue }
                    }
                    let item = values[position]; position += 1
                    try self.charge()
                    guard case .number(let number) = item.value else {
                        if existenceTest && !self.program.strict { continue }
                        throw JedError("2203B", "operand of unary jsonpath operator \(op) is not a numeric value")
                    }
                    try self.charge()
                    return JSONPathItem(value: .number(op == "-" ? number.negated() : number), last: item.last, suppressed: item.suppressed)
                }
            }
        default:
            var complete = false
            return JSONPathItemCursor {
                if complete { return nil }; complete = true
                try self.meter.pollStep()
                return try self.singleton(expression, current: current)
            }
        }
    }

    func singleton(_ expression: JSONPathExpression, current: JSONPathItem) throws -> JSONPathItem {
        switch expression {
        case .root:
            var item = root; item.last = current.last; item.suppressed = current.suppressed
            return item
        case .current: return current
        case .last:
            guard let last = current.last else { throw JedError("42601", "last is only allowed in array subscripts") }
            return JSONPathItem(value: .number(SQLDecimal(Int64(last))), suppressed: current.suppressed)
        case .variable(let key):
            guard variableBase else { throw JedError("42704", "could not find jsonpath variable \"\(key)\"") }
            guard case .object(let members) = variables.value else { throw JedError("22023", "\"vars\" argument is not an object") }
            var lo = 0, hi = members.count
            while lo < hi {
                let mid = lo + (hi - lo) / 2, candidate = members[mid].key
                if candidate.utf8.count < key.utf8.count || (candidate.utf8.count == key.utf8.count && candidate < key) { lo = mid + 1 } else { hi = mid }
            }
            guard lo < members.count && members[lo].key == key else { throw JedError("42704", "could not find jsonpath variable \"\(key)\"") }
            var item = child(variables, at: lo); item.last = current.last; item.suppressed = current.suppressed
            return item
        case .literal(let value): return JSONPathItem(value: value, last: current.last, suppressed: current.suppressed)
        case .predicate(let predicate):
            let truth = try predicateValue(predicate, current: current)
            return JSONPathItem(value: truth == .unknown ? .null : .boolean(truth == .yes), last: current.last, suppressed: current.suppressed)
        case .binary(let op, let left, let right):
            let a = try operands(left, current: current), b = try operands(right, current: current)
            guard a.count == 1, case .number(let lhs) = a[0].value else { throw JedError("22038", "left operand of jsonpath operator \(op) is not a single numeric value") }
            guard b.count == 1, case .number(let rhs) = b[0].value else { throw JedError("22038", "right operand of jsonpath operator \(op) is not a single numeric value") }
            try charge()
            let result = try DecimalFunctions.binary(op, lhs, rhs, meter: meter)
            try charge()
            return JSONPathItem(value: .number(result), last: current.last, suppressed: current.suppressed)
        case .path, .unary: throw JedError("XX000", "sequence expression requires a jsonpath cursor")
        }
    }

    private enum Work {
        case apply(JSONPathItem, Int, Bool)
        case sequence([JSONPathItem], Int, Int, Bool, Int?)
        case subscripts(JSONPathItem, [JSONPathItem], [(JSONPathExpression, JSONPathExpression?)], Int, Int)
        case descend(JSONPathItem, Int, Int?, Int?, Int)
        case keyvalues(JSONPathItem, [SQLJSONMember], [JSONPathItem], Int, Int)
        case releaseBase(UInt64)
    }

    func walkCursor(_ initial: JSONPathItem, steps: [JSONPathStep]) -> JSONPathItemCursor {
        var stack: [Work] = [.apply(initial, 0, false)]
        return JSONPathItemCursor { [self] in
        while let work = stack.popLast() {
            try meter.pollStep()
            switch work {
            case .releaseBase(let base): layout.removeValue(forKey: base)
            case .sequence(let values, let cursor, let index, let emitted, let last):
                if cursor >= values.count { continue }
                stack.append(.sequence(values, cursor + 1, index, emitted, last))
                if emitted { try charge() }
                var item = values[cursor]
                if let last { item.last = last }
                stack.append(.apply(item, index, !emitted))
            case .subscripts(let array, let elements, let bounds, let cursor, let next):
                if cursor >= bounds.count { continue }
                let pair = bounds[cursor]
                let first = try subscriptIndex(pair.0, current: array)
                let last = try pair.1.map { try subscriptIndex($0, current: array) } ?? first
                stack.append(.subscripts(array, elements, bounds, cursor + 1, next))
                if program.strict && !array.suppressed && (first < 0 || last >= elements.count || first > last) {
                    throw JedError("22033", "jsonpath array subscript is out of bounds")
                }
                let lo = max(0, first), hi = min(elements.count - 1, last)
                if lo <= hi { stack.append(.sequence(Array(elements[lo...hi]), 0, next, true, array.last)) }
            case .descend(let item, let level, let first, let last, let next):
                if level > 0 { try charge() }
                let descendants = children(item)
                if level < (last ?? Int.max) {
                    for child in descendants.reversed() { stack.append(.descend(child, level + 1, first, last, next)) }
                }
                let leaf = SQLJSON.kind(item.value) != "array" && SQLJSON.kind(item.value) != "object"
                let selected = first == nil ? (last == nil && level > 0 && leaf) : level >= first! && level <= (last ?? Int.max)
                if selected {
                    var result = item; result.suppressed = true
                    try charge(); stack.append(.apply(result, next, false))
                }
            case .keyvalues(let object, let members, let values, let cursor, let next):
                if cursor >= members.count { continue }
                stack.append(.keyvalues(object, members, values, cursor + 1, next))
                let product = object.base.multipliedReportingOverflow(by: 10_000_000_000)
                let sum = product.partialValue.addingReportingOverflow(UInt64(object.offset))
                guard !product.overflow && !sum.overflow && nextBase < UInt64.max else {
                    throw JedError("22003", "jsonpath object identifier is out of range")
                }
                let id = sum.partialValue
                let result = SQLJSONValue.object([
                    SQLJSONMember(key: SQLText(validatedUTF8: [105, 100]), value: .number(try SQLDecimal.parse(String(id)))),
                    SQLJSONMember(key: SQLText(validatedUTF8: [107, 101, 121]), value: .string(members[cursor].key)),
                    SQLJSONMember(key: SQLText(validatedUTF8: [118, 97, 108, 117, 101]), value: values[cursor].value),
                ])
                let item = JSONPathItem(value: result, base: nextBase, last: object.last, suppressed: object.suppressed)
                nextBase += 1
                stack.append(.releaseBase(item.base))
                try charge(); stack.append(.apply(item, next, false))
            case .apply(let original, let index, let alreadyUnwrapped):
                if index == steps.count { return original }
                let step = steps[index]
                let unwrap: Bool
                switch step {
                case .member, .members, .filter: unwrap = true
                case .method(let name, _): unwrap = name != "type" && name != "size"
                default: unwrap = false
                }
                if !program.strict && unwrap && !alreadyUnwrapped, case .array = original.value {
                    stack.append(.sequence(children(original), 0, index, false, nil)); continue
                }
                try charge()
                let next = index + 1
                switch step {
                case .member(let name):
                    guard case .object(let members) = original.value else {
                        if !program.strict || original.suppressed { continue }
                        throw JedError("2203A", "jsonpath member accessor can only be applied to an object")
                    }
                    var found: Int?
                    for (position, member) in members.enumerated() { try charge(); if member.key == name { found = position; break } }
                    if let found { stack.append(.sequence([child(original, at: found)], 0, next, true, nil)) }
                    else if program.strict && !original.suppressed { throw JedError("2203A", "JSON object does not contain key \"\(name)\"") }
                case .members:
                    guard case .object = original.value else {
                        if !program.strict || original.suppressed { continue }; throw JedError("2203C", "jsonpath wildcard member accessor can only be applied to an object")
                    }
                    stack.append(.sequence(children(original), 0, next, true, nil))
                case .elements, .subscripts:
                    var array = original, values: [JSONPathItem]
                    if case .array = original.value { values = children(original) }
                    else {
                        if program.strict { if original.suppressed { continue }; throw JedError("22039", "jsonpath array accessor can only be applied to an array") }
                        values = [original]
                    }
                    array.last = values.count - 1
                    if case .elements = step { stack.append(.sequence(values, 0, next, true, array.last)) }
                    else if case .subscripts(let bounds) = step { stack.append(.subscripts(array, values, bounds, 0, next)) }
                case .filter(let predicate):
                    if try predicateValue(predicate, current: original) == .yes { stack.append(.sequence([original], 0, next, true, nil)) }
                case .any(let first, let last): stack.append(.descend(original, 0, first, last, next))
                case .method(let name, let arguments):
                    if name == "keyvalue" {
                        guard case .object(let members) = original.value else { throw JedError("2203C", "jsonpath item method .keyvalue() can only be applied to an object") }
                        stack.append(.keyvalues(original, members, children(original), 0, next))
                    } else if let value = try method(name, args: arguments, item: original) {
                        stack.append(.sequence([JSONPathItem(value: value, last: original.last, suppressed: original.suppressed)], 0, next, true, nil))
                    }
                }
            }
        }
        return nil
        }
    }

    func unwrapped(_ item: JSONPathItem) -> [JSONPathItem] {
        if !program.strict, case .array = item.value { return children(item) }; return [item]
    }
    func operands(_ expression: JSONPathExpression, current: JSONPathItem, unwrap: Bool = true) throws -> [JSONPathItem] {
        var items: [JSONPathItem] = []
        _ = try evaluate(expression, current: current) { item in items += unwrap ? self.unwrapped(item) : [item]; return true }
        return items
    }
    func subscriptIndex(_ expression: JSONPathExpression, current: JSONPathItem) throws -> Int {
        let items = try operands(expression, current: current, unwrap: false)
        guard items.count == 1, case .number(let number) = items[0].value,
              let integer = Int32(try number.truncated().description) else { throw JedError("22033", "jsonpath array subscript is not a single numeric value") }
        return Int(integer)
    }

    func predicateValue(_ predicate: JSONPathPredicate, current: JSONPathItem) throws -> JSONTruth {
        switch predicate {
        case .and(let a, let b):
            let left = try predicateValue(a, current: current), right = try predicateValue(b, current: current)
            if left == .no || right == .no { return .no }; return left == .unknown || right == .unknown ? .unknown : .yes
        case .or(let a, let b):
            let left = try predicateValue(a, current: current), right = try predicateValue(b, current: current)
            if left == .yes || right == .yes { return .yes }; return left == .unknown || right == .unknown ? .unknown : .no
        case .not(let p): let value = try predicateValue(p, current: current); return value == .yes ? .no : value == .no ? .yes : .unknown
        case .unknown(let p): return try predicateValue(p, current: current) == .unknown ? .yes : .no
        case .exists(let expression):
            do {
                var found = false
                _ = try evaluate(expression, current: current, existenceTest: true) { _ in found = true; return self.program.strict }
                return found ? .yes : .no
            } catch { if jsonError(error) { return .unknown }; throw error }
        case .compare(let op, let left, let right):
            return try pairPredicate(left, right, current: current) { try self.compare(op, $0, $1) }
        case .starts(let left, let right):
            return try pairPredicate(left, right, current: current, unwrapRight: false) { a, b in
                guard case .string(let a) = a, case .string(let b) = b else { return .unknown }
                try self.charge(Int64(max(0, min(a.scalarCount, b.scalarCount) - 1)))
                return a.utf8.starts(with: b.utf8) ? .yes : .no
            }
        case .regex(let expression, _, _, let regex):
            let values: [JSONPathItem]
            do { values = try operands(expression, current: current) }
            catch { if jsonError(error) { return .unknown }; throw error }
            var answer = JSONTruth.no
            for item in values {
                try charge()
                let truth: JSONTruth
                if case .string(let text) = item.value { truth = try regex.matches(text, meter) ? .yes : .no } else { truth = .unknown }
                if program.strict && truth == .unknown { return .unknown }
                if !program.strict && truth == .yes { return .yes }
                if truth == .yes || (truth == .unknown && answer == .no) { answer = truth }
            }
            return answer
        }
    }
    func pairPredicate(_ left: JSONPathExpression, _ right: JSONPathExpression, current: JSONPathItem,
                       unwrapRight: Bool = true, compare: (SQLJSONValue, SQLJSONValue) throws -> JSONTruth) throws -> JSONTruth {
        let a: [JSONPathItem], b: [JSONPathItem]
        do { a = try operands(left, current: current); b = try operands(right, current: current, unwrap: unwrapRight) }
        catch { if jsonError(error) { return .unknown }; throw error }
        var answer = JSONTruth.no
        for lhs in a { for rhs in b {
            try charge(); let truth = try compare(lhs.value, rhs.value)
            if program.strict && truth == .unknown { return .unknown }
            if !program.strict && truth == .yes { return .yes }
            if truth == .yes || (truth == .unknown && answer == .no) { answer = truth }
        } }
        return answer
    }
    func compare(_ operation: String, _ left: SQLJSONValue, _ right: SQLJSONValue) throws -> JSONTruth {
        let leftKind = SQLJSON.kind(left), rightKind = SQLJSON.kind(right)
        if leftKind == "null" || rightKind == "null" {
            if leftKind != rightKind { return operation == "!=" || operation == "<>" ? .yes : .no }
        } else if leftKind != rightKind || leftKind == "array" || leftKind == "object" { return .unknown }
        if case .string(let a) = left, case .string(let b) = right { try charge(Int64(max(0, min(a.scalarCount, b.scalarCount) - 1))) }
        if case .number(let a) = left, case .number(let b) = right { try DecimalFunctions.chargeWork("compare", a, b, meter: meter) }
        let order = SQLJSON.compare(left, right, root: false), answer: Bool
        switch operation { case "==": answer = order == 0; case "!=", "<>": answer = order != 0; case "<": answer = order < 0; case "<=": answer = order <= 0; case ">": answer = order > 0; default: answer = order >= 0 }
        return answer ? .yes : .no
    }

    func method(_ name: String, args: [String], item: JSONPathItem) throws -> SQLJSONValue? {
        let value = item.value
        func invalid() -> JedError { JedError("22036", "argument of jsonpath item method .\(name)() is invalid") }
        switch name {
        case "type": return .string(SQLText(validatedUTF8: Array(SQLJSON.kind(value).utf8)))
        case "size":
            if case .array(let elements) = value { return .number(SQLDecimal(Int64(elements.count))) }
            if program.strict { if item.suppressed { return nil }; throw JedError("22039", "jsonpath item method .size() can only be applied to an array") }
            return .number(SQLDecimal(1))
        case "abs", "floor", "ceiling":
            guard case .number(let number) = value else { throw JedError("22036", "jsonpath item method .\(name)() can only be applied to a numeric value") }
            return .number(try DecimalFunctions.call(name: name, args: [number], meter: meter))
        case "string":
            switch value { case .string: return value; case .number, .boolean: return .string(try SQLJSON.render(value)); default: throw invalid() }
        case "boolean":
            switch value {
            case .boolean: return value
            case .number(let number): guard let integer = Int32(number.description) else { throw invalid() }; return .boolean(integer != 0)
            case .string(let text):
                let word = text.asciiLowercased.description
                if !word.isEmpty && ("true".hasPrefix(word) || "yes".hasPrefix(word) || word == "on" || word == "1") { return .boolean(true) }
                if !word.isEmpty && ("false".hasPrefix(word) || "no".hasPrefix(word) || word == "off" || word == "of" || word == "0") { return .boolean(false) }
                throw invalid()
            default: throw invalid()
            }
        case "number", "decimal", "integer", "bigint":
            let number: SQLDecimal
            do {
                if case .number(let input) = value { number = input }
                else if case .string(let text) = value {
                    if name == "integer" || name == "bigint" {
                        var bytes = text.utf8[...]
                        while let byte = bytes.first, byte == 32 || (9...13).contains(byte) { bytes = bytes.dropFirst() }
                        while let byte = bytes.last, byte == 32 || (9...13).contains(byte) { bytes = bytes.dropLast() }
                        if bytes.first == 43 || bytes.first == 45 { bytes = bytes.dropFirst() }
                        let prefixed = bytes.count >= 2 && bytes.first == 48 && [98, 66, 111, 79, 120, 88].contains(bytes[bytes.index(after: bytes.startIndex)])
                        guard prefixed || (!bytes.contains(46) && !bytes.contains(101) && !bytes.contains(69)) else { throw invalid() }
                    }
                    number = try SQLDecimal.parse(text.description)
                } else { throw invalid() }
                if name == "integer" || name == "bigint" {
                    let integer = try number.integerValue()
                    if name == "integer" && (integer < Int32.min || integer > Int32.max) { throw invalid() }
                    return .number(SQLDecimal(integer))
                }
            } catch { throw invalid() }
            if name == "decimal", let raw = args.first {
                guard let precision = Int32(raw), let scale = Int32(args.count > 1 ? args[1] : "0") else { throw invalid() }
                guard (1...1000).contains(precision) else { throw JedError("22023", "invalid precision for jsonpath item method .decimal()") }
                guard (-1000...1000).contains(scale) else { throw JedError("22023", "invalid scale for jsonpath item method .decimal()") }
                let rounded = number.rescaledUncapped(to: Int(scale))
                guard rounded.isZero || rounded.precision - rounded.scale <= Int(precision - scale) else { throw invalid() }
                return .number(rounded)
            }
            return .number(number)
        case "double":
            let text: String
            if case .number(let number) = value { text = number.description }
            else if case .string(let string) = value { text = string.description }
            else { throw invalid() }
            let bits: UInt64
            do {
                guard case .f64(let value) = try FloatText.parse(SQLText(text), type: .f64), Double(bitPattern: value).isFinite else { throw invalid() }
                bits = value
            } catch { throw invalid() }
            if case .number = value { return value }
            let exact = try BinaryFloatDecimal.exact(bits64: bits)
            let rounded = BinaryFloatDecimal.roundedEven(exact, scale: 15 - (exact.precision - exact.scale))
            return .number(rounded.rescaledUncapped(to: DecimalFunctions.minimumScale(rounded), round: false))
        default: throw JedError("0A000", "jsonpath item method is not supported")
        }
    }

    func align(_ offset: Int) -> Int { (offset + 3) & ~3 }
    /// Build every container's child offsets in one traversal of a base value.
    /// Repeated root operands and keyvalue continuations reuse the same layout.
    func buildLayout(_ value: SQLJSONValue, from start: Int, base: UInt64, offsets: inout [Int: [JSONPathItem]]) -> Int {
        switch value {
        case .null, .boolean: return start
        case .string(let text): return start + text.utf8.count
        case .number(let number):
            let digits = Array(number.magnitude.description.utf8)
            var trailing = 0
            if !number.isZero { for digit in digits.reversed() { if digit != 48 { break }; trailing += 1 } }
            let high = number.base10000Weight
            let low = SQLDecimal.floorDiv(-number.scale + trailing, 4)
            let groups = number.isZero ? 0 : high - low + 1
            return align(start) + 4 + (number.scale <= 63 && (-64...63).contains(high) ? 2 : 4) + 2 * groups
        case .array, .object:
            let values: [SQLJSONValue]; var offset: Int
            if case .array(let elements) = value {
                values = elements; offset = align(start) + 4 + 4 * elements.count
            } else if case .object(let members) = value {
                values = members.map(\.value)
                offset = align(start) + 4 + 8 * members.count + members.reduce(0) { $0 + $1.key.utf8.count }
            } else { return start }
            var children: [JSONPathItem] = []
            for child in values {
                switch child { case .array, .object, .number: offset = align(offset); default: break }
                children.append(JSONPathItem(value: child, base: base, offset: offset))
                offset = buildLayout(child, from: offset, base: base, offsets: &offsets)
            }
            offsets[align(start)] = children
            return offset
        }
    }
    func rawChildren(_ item: JSONPathItem) -> [JSONPathItem] {
        switch item.value { case .array, .object: break; default: return [] }
        if layout[item.base] == nil {
            var offsets: [Int: [JSONPathItem]] = [:]
            _ = buildLayout(item.value, from: item.offset, base: item.base, offsets: &offsets)
            layout[item.base] = offsets
        }
        return layout[item.base]?[item.offset] ?? []
    }
    func child(_ item: JSONPathItem, at index: Int) -> JSONPathItem {
        var value = rawChildren(item)[index]; value.last = item.last; value.suppressed = item.suppressed
        return value
    }
    func children(_ item: JSONPathItem) -> [JSONPathItem] {
        let values = rawChildren(item)
        if item.last == nil && !item.suppressed { return values }
        return values.map { child in
            var result = child; result.last = item.last; result.suppressed = item.suppressed
            return result
        }
    }
}
