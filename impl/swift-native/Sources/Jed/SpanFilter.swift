/// The shared packed-lane WHERE driver (packed-leaf.md §11 A4). Its AND tree
/// preserves scalar short circuiting. Supported leaves inspect validated
/// stored bytes; every other leaf uses the ordinary evaluator and row frame.
internal final class PackedLaneFilter {
    private enum Body {
        case absent
        case plain(ResolvedExpression, [Int])
        case encoded(PackedPredicate)
    }
    private let body: Body
    let hasEncodedPredicates: Bool

    init(predicate: ResolvedExpression?, types: [TypeDescriptor], touched: [Int], context: ExpressionContext) {
        guard let predicate else { body = .absent; hasEncodedPredicates = false; return }
        let tree = PackedPredicate.compile(predicate, types: types, touched: Set(touched), context: context)
        hasEncodedPredicates = tree.hasAtom
        body = hasEncodedPredicates ? .encoded(tree) : .plain(predicate, touched)
    }

    func keep(_ row: PackedLaneRow, context: ExpressionContext) throws -> Bool {
        switch body {
        case .absent: return true
        case .plain(let expression, let columns):
            try row.prepare(columns)
            return try context.matches(expression)
        case .encoded(let predicate): return try predicate.keep(row, context: context)
        }
    }
}

private enum PackedPredicate {
    indirect case conjunction(PackedPredicate, PackedPredicate)
    case atom(PackedPredicateAtom)
    case generic(ResolvedExpression, [Int])

    static func compile(_ expression: ResolvedExpression, types: [TypeDescriptor],
                        touched: Set<Int>, context: ExpressionContext) -> PackedPredicate {
        if case .binary("and", let left, let right, _) = expression.node {
            return .conjunction(compile(left, types: types, touched: touched, context: context),
                compile(right, types: types, touched: touched, context: context))
        }
        if let atom = PackedPredicateAtom.compile(expression, types: types, touched: touched, context: context) {
            return .atom(atom)
        }
        var reads: Set<Int> = []
        StoredExpressionDependencies.walk([expression]) {
            if case .column(let column) = $0.node, touched.contains(column) { reads.insert(column) }
        }
        return .generic(expression, reads.sorted())
    }

    var hasAtom: Bool {
        switch self {
        case .conjunction(let left, let right): left.hasAtom || right.hasAtom
        case .atom: true
        case .generic: false
        }
    }

    func keep(_ row: PackedLaneRow, context: ExpressionContext) throws -> Bool {
        switch self {
        case .conjunction(let left, let right):
            context.meter.charge(1); try context.meter.step()
            guard try left.keep(row, context: context) else { return false }
            return try right.keep(row, context: context)
        case .atom(let atom):
            if let result = try atom.keepStored(row, meter: context.meter) { return result }
            // A declined atom has charged nothing. Resolving its original
            // operand reports malformed/overflow/compression errors normally.
            _ = try row.value(atom.column)
            return try context.matches(atom.expression)
        case .generic(let expression, let columns):
            try row.prepare(columns)
            return try context.matches(expression)
        }
    }
}

private final class PackedPredicateAtom {
    private enum ArrayOperation { case contains, contained, overlaps }
    private enum Element { case text, bytes, integer(Int) }
    private enum Kind {
        case nullness(Bool)
        case equal(bytes: [UInt8], width: Int, text: Bool, negated: Bool, charge: Int64)
        /// A bare integer column against an integer literal or parameter:
        /// the evaluator's comparison code and the right-hand constant.
        case integer(code: Int, constant: Int64)
        case array(ArrayOperation, Element, [[UInt8]], hasNull: Bool, charge: Int64)
    }
    let expression: ResolvedExpression
    let column: Int
    private let kind: Kind
    /// Offset scratch is reused per atom. It retains no page or decoded value.
    private var elements: [Range<Int>] = []

    private init(expression: ResolvedExpression, column: Int, kind: Kind) {
        self.expression = expression; self.column = column; self.kind = kind
    }

    static func compile(_ expression: ResolvedExpression, types: [TypeDescriptor],
                        touched: Set<Int>, context: ExpressionContext) -> PackedPredicateAtom? {
        func column(_ value: ResolvedExpression) -> Int? {
            guard case .column(let column) = value.node, types.indices.contains(column), touched.contains(column) else { return nil }
            return column
        }
        switch expression.node {
        case .isNull(let operand, let negated):
            guard let column = column(operand), TypeRules.fields(types[column].type) == nil else { return nil }
            return PackedPredicateAtom(expression: expression, column: column, kind: .nullness(negated))
        case .binary(let operation, let left, let right, _):
            let selected: Int, other: ResolvedExpression, flipped: Bool
            if let index = column(left), column(right) == nil { selected = index; other = right; flipped = false }
            else if let index = column(right), column(left) == nil { selected = index; other = left; flipped = true }
            else { return nil }
            if case .scalar(let scalar) = types[selected].type, scalar.integerWidth != nil {
                // The evaluator charges one unit for the operator, nothing
                // for either operand, and compares an integer pair directly.
                let codes = ["=": 3, "<>": 4, "<": 5, "<=": 6, ">": 7, ">=": 8]
                let mirrored = [3: 3, 4: 4, 5: 7, 6: 8, 7: 5, 8: 6]
                // Either operand form evaluates to itself at no charge.
                let constant: SQLValue
                switch other.node {
                case .constant(let literal): constant = literal
                case .parameter(let index):
                    guard index > 0, index <= context.parameters.count else { return nil }
                    constant = context.parameters[index - 1]
                default: return nil
                }
                guard let code = codes[operation], case .integer(let value) = constant else { return nil }
                return PackedPredicateAtom(expression: expression, column: selected,
                    kind: .integer(code: flipped ? mirrored[code]! : code, constant: value))
            }
            guard operation == "=" || operation == "<>" else { return nil }
            guard let (constant, charge) = constant(other, context: context) else { return nil }
            let bytes: [UInt8], width: Int, text: Bool
            switch (types[selected].type, constant) {
            case (.scalar(.text), .text(let value)):
                bytes = value.utf8; width = value.scalarCount; text = true
            case (.scalar(.bytea), .bytea(let value)):
                bytes = value; width = value.count; text = false
            default: return nil
            }
            return PackedPredicateAtom(expression: expression, column: selected,
                kind: .equal(bytes: bytes, width: width, text: text, negated: operation == "<>", charge: saturatingAdd(1, charge)))
        case .call(let function):
            guard function.hostID == nil, function.metadata.category == "array", function.arguments.count == 2 else { return nil }
            let selected: Int, other: ResolvedExpression, first: Bool
            if let index = column(function.arguments[0]), column(function.arguments[1]) == nil {
                selected = index; other = function.arguments[1]; first = true
            } else if let index = column(function.arguments[1]), column(function.arguments[0]) == nil {
                selected = index; other = function.arguments[0]; first = false
            } else { return nil }
            let operation: ArrayOperation
            switch (function.name, first) {
            case ("@>", true), ("<@", false), ("contains", true), ("contained_by", false): operation = .contains
            case ("@>", false), ("<@", true), ("contains", false), ("contained_by", true): operation = .contained
            case ("&&", _), ("overlaps", _): operation = .overlaps
            default: return nil
            }
            guard case .array(let type) = types[selected].type, case .scalar(let scalar) = type,
                  let (constant, charge) = constant(other, context: context), case .array(let array) = constant else { return nil }
            let element: Element
            if scalar == .text { element = .text }
            else if scalar == .bytea { element = .bytes }
            else if let width = scalar.integerWidth { element = .integer(width) }
            else { return nil }
            var bodies: [[UInt8]] = [], hasNull = false
            for value in array.elements {
                if value.isNull { hasNull = true; continue }
                switch (element, value) {
                case (.text, .text(let value)): guard value.utf8.count <= Int(UInt16.max) else { return nil }
                case (.bytes, .bytea(let value)): guard value.count <= Int(UInt16.max) else { return nil }
                case (.integer, .integer(let value)): guard scalar.admits(value) else { return nil }
                default: return nil
                }
                guard let body = try? ValueCodec.encodeBody(value, type: TypeDescriptor(type)) else { return nil }
                bodies.append(body)
            }
            return PackedPredicateAtom(expression: expression, column: selected,
                kind: .array(operation, element, bodies, hasNull: hasNull, charge: saturatingAdd(function.cost, charge)))
        default: return nil
        }
    }

    /// Literal forms have no state/host calls or allocation reservations. Use
    /// a private meter so constructor work is replayed, never charged early.
    private static func constant(_ expression: ResolvedExpression, context: ExpressionContext) -> (SQLValue, Int64)? {
        func literal(_ value: ResolvedExpression) -> Bool {
            switch value.node {
            case .parameter: true
            case .constant(let constant):
                switch constant { case .null, .text, .bytea, .integer, .array: true; default: false }
            default: false
            }
        }
        switch expression.node {
        case .array(let children, let nested):
            guard !nested, children.allSatisfy(literal) else { return nil }
        default: guard literal(expression) else { return nil }
        }
        let meter = CostMeter()
        var current = context; current.meter = meter; current.scalar.meter = meter
        guard let value = try? current.evaluate(expression) else { return nil }
        return (value, meter.accrued)
    }

    /// nil means fall back without charging; false is a complete SQL WHERE
    /// rejection (including UNKNOWN). No span is treated as a decoded value.
    func keepStored(_ row: PackedLaneRow, meter: CostMeter) throws -> Bool? {
        switch kind {
        case .nullness(let negated):
            meter.charge(1); try meter.step()
            return try row.isNull(column) != negated
        case .integer(let code, let constant):
            // Declining (NULL, a writer leaf) charges nothing; the caller
            // then evaluates the original expression.
            guard let (leaf, index) = row.packedPosition(),
                  let value = leaf.fastInteger(row: index, column: column) else { return nil }
            meter.charge(1); try meter.step()
            switch code {
            case 3: return value == constant
            case 4: return value != constant
            case 5: return value < constant
            case 6: return value <= constant
            case 7: return value > constant
            default: return value >= constant
            }
        case .equal(let constant, let constantWidth, let text, let negated, let charge):
            if let (leaf, index) = row.packedPosition() {
                // The same decisions as below, read from the page in place.
                switch leaf.shortBody(row: index, column: column) {
                case .other: return nil
                case .null: meter.charge(charge); try meter.step(); return false
                case .inline(let range):
                    let width: Int
                    if text {
                        guard let count = Self.utf8Width(leaf.bytes, range) else { return nil }
                        width = count
                    } else { width = range.count }
                    meter.charge(charge); try meter.step()
                    meter.charge(Int64(max(0, min(width, constantWidth) - 1))); try meter.guardCost()
                    var equal = range.count == constant.count
                    if equal {
                        for offset in 0..<constant.count where leaf.bytes[range.lowerBound + offset] != constant[offset] {
                            equal = false; break
                        }
                    }
                    return equal != negated
                }
            }
            guard let cell = try row.storedCell(column) else { return nil }
            guard let bytes = cell.bytes else { meter.charge(charge); try meter.step(); return false }
            guard bytes.first == 0, let body = Self.shortBody(bytes.dropFirst()) else { return nil }
            let width: Int
            if text {
                guard let count = Self.utf8Width(body) else { return nil }
                width = count
            } else { width = body.count }
            meter.charge(charge); try meter.step()
            meter.charge(Int64(max(0, min(width, constantWidth) - 1))); try meter.guardCost()
            return body.elementsEqual(constant) != negated
        case .array(let operation, let element, let constant, let constantHasNull, let charge):
            if let (leaf, index) = row.packedPosition() {
                // The same decisions as below, read from the page in place.
                switch leaf.taggedBody(row: index, column: column) {
                case .other: return nil
                case .null: meter.charge(charge); try meter.step(); return false
                case .inline(let range):
                    guard let hasNull = arraySpans(leaf.bytes, range, element: element) else { return nil }
                    meter.charge(charge); try meter.step()
                    return arrayResult(operation, leaf.bytes, constant: constant, constantHasNull: constantHasNull, hasNull: hasNull)
                }
            }
            guard let cell = try row.storedCell(column) else { return nil }
            guard let bytes = cell.bytes else { meter.charge(charge); try meter.step(); return false }
            guard bytes.first == 0 else { return nil }
            let body = bytes.dropFirst()
            guard let hasNull = arraySpans(body, body.startIndex..<body.endIndex, element: element) else { return nil }
            meter.charge(charge); try meter.step()
            return arrayResult(operation, body, constant: constant, constantHasNull: constantHasNull, hasNull: hasNull)
        }
    }

    private static func shortBody(_ bytes: ArraySlice<UInt8>) -> ArraySlice<UInt8>? {
        var reader = ByteReader(bytes)
        guard let length = try? reader.u16(), Int(length) == reader.remaining else { return nil }
        return try? reader.take(Int(length))
    }

    /// Match SQLText's valid, NUL-free UTF-8 contract while counting codepoints
    /// directly in a page slice. No String or owned byte array is constructed.
    private static func utf8Width(_ bytes: ArraySlice<UInt8>) -> Int? {
        utf8Width(bytes, bytes.startIndex..<bytes.endIndex)
    }
    @inline(__always)
    private static func utf8Width<Bytes: RandomAccessCollection>(_ bytes: Bytes, _ range: Range<Int>) -> Int?
    where Bytes.Element == UInt8, Bytes.Index == Int {
        var position = range.lowerBound, count = 0
        while position < range.upperBound {
            let first = bytes[position]
            if first > 0 && first < 0x80 { position += 1; count += 1; continue }
            let length: Int
            if first >= 0xc2 && first < 0xe0 { length = 2 }
            else if first >= 0xe0 && first < 0xf0 { length = 3 }
            else if first >= 0xf0 && first <= 0xf4 { length = 4 }
            else { return nil }
            guard length <= range.upperBound - position else { return nil }
            for offset in 1..<length where bytes[position + offset] & 0xc0 != 0x80 { return nil }
            let second = bytes[position + 1]
            if first == 0xe0 && second < 0xa0 || first == 0xed && second >= 0xa0
                || first == 0xf0 && second < 0x90 || first == 0xf4 && second >= 0x90 { return nil }
            position += length; count += 1
        }
        return count
    }

    /// Validate every structural rule of ValueCodec's array body before an
    /// atom charges or compares anything. Offsets are indices into `bytes`;
    /// NULL elements match nothing, and dimensions do not affect containment.
    private func arraySpans<Bytes: RandomAccessCollection>(_ bytes: Bytes, _ range: Range<Int>, element: Element) -> Bool?
    where Bytes.Element == UInt8, Bytes.Index == Int {
        // Build in a local so the loop does not touch this object per element.
        var spans = elements
        elements = []
        spans.removeAll(keepingCapacity: true)
        defer { elements = spans }
        var at = range.lowerBound
        let end = range.upperBound
        func u32(_ index: Int) -> UInt32 {
            UInt32(bytes[index]) << 24 | UInt32(bytes[index + 1]) << 16 | UInt32(bytes[index + 2]) << 8 | UInt32(bytes[index + 3])
        }
        guard end - at >= 2 else { return nil }
        let dimensions = Int(bytes[at]), flags = bytes[at + 1]
        at += 2
        guard flags <= 1, dimensions != 0 || flags == 0 else { return nil }
        var count = dimensions == 0 ? 0 : 1
        for _ in 0..<dimensions {
            guard end - at >= 8 else { return nil }
            let length = Int(u32(at)), lower = Int32(bitPattern: u32(at + 4))
            at += 8
            guard length > 0, length <= 134_217_727, count <= 134_217_727 / length,
                  Int64(lower) + Int64(length) - 1 <= Int32.max else { return nil }
            count *= length
        }
        let bitmap = at
        if flags == 1 {
            let size = (count + 7) / 8
            guard end - at >= size else { return nil }
            at += size
            if count % 8 != 0, size > 0, bytes[at - 1] & (UInt8.max >> (count % 8)) != 0 { return nil }
        }
        var hasNull = false
        for index in 0..<count {
            if flags == 1, bytes[bitmap + index / 8] & (0x80 >> (index % 8)) != 0 {
                hasNull = true; continue
            }
            let start = at
            switch element {
            case .integer(let width):
                guard end - at >= width else { return nil }
                at += width
            case .text, .bytes:
                guard end - at >= 2 else { return nil }
                let length = Int(bytes[at]) << 8 | Int(bytes[at + 1])
                at += 2
                guard end - at >= length else { return nil }
                if case .text = element, Self.utf8Width(bytes, at..<(at + length)) == nil { return nil }
                at += length
            }
            spans.append(start..<at)
        }
        guard at == end else { return nil }
        return hasNull
    }

    @inline(__always)
    private static func equal<Bytes: RandomAccessCollection>(_ bytes: Bytes, _ range: Range<Int>, _ constant: [UInt8]) -> Bool
    where Bytes.Element == UInt8, Bytes.Index == Int {
        guard range.count == constant.count else { return false }
        for offset in 0..<constant.count where bytes[range.lowerBound + offset] != constant[offset] { return false }
        return true
    }

    private func arrayResult<Bytes: RandomAccessCollection>(_ operation: ArrayOperation, _ bytes: Bytes, constant: [[UInt8]],
                                                           constantHasNull: Bool, hasNull: Bool) -> Bool
    where Bytes.Element == UInt8, Bytes.Index == Int {
        let spans = elements
        switch operation {
        case .contains:
            if constantHasNull { return false }
            return constant.allSatisfy { value in spans.contains { Self.equal(bytes, $0, value) } }
        case .contained:
            if hasNull { return false }
            return spans.allSatisfy { range in constant.contains { Self.equal(bytes, range, $0) } }
        case .overlaps:
            return spans.contains { range in constant.contains { Self.equal(bytes, range, $0) } }
        }
    }
}
