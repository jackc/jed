internal struct WindowFrame {
    enum Bound {
        case unbounded(preceding: Bool), current, offset(SQLValue, preceding: Bool)
        var hasOffset: Bool { if case .offset = self { true } else { false } }
    }
    let mode: String
    let start: Bound
    let end: Bound
    let exclusion: String

    static func resolve(_ syntax: SQLWindowFrame?, order: [WindowOrder], allowNegative: Bool = false) throws -> Self? {
        guard let syntax else { return nil }
        let end = syntax.end ?? .currentRow
        func preceding(_ bound: SQLFrameBound) -> Bool { if case .offset(_, true) = bound { true } else { false } }
        func following(_ bound: SQLFrameBound) -> Bool { if case .offset(_, false) = bound { true } else { false } }
        if case .unbounded(false) = syntax.start { throw JedError("42P20", "frame start cannot be UNBOUNDED FOLLOWING") }
        if case .unbounded(true) = end { throw JedError("42P20", "frame end cannot be UNBOUNDED PRECEDING") }
        if case .currentRow = syntax.start, preceding(end) { throw JedError("42P20", "frame starting from current row cannot have preceding rows") }
        if following(syntax.start) {
            if preceding(end) { throw JedError("42P20", "frame starting from following row cannot have preceding rows") }
            if case .currentRow = end { throw JedError("42P20", "frame starting from following row cannot end with current row") }
        }
        let offset = preceding(syntax.start) || following(syntax.start) || preceding(end) || following(end)
        if syntax.mode == "groups", order.isEmpty { throw JedError("42P20", "GROUPS mode requires an ORDER BY clause") }
        var key: ScalarType?
        if syntax.mode == "range", offset {
            guard order.count == 1 else { throw JedError("42P20", "RANGE offset requires exactly one ORDER BY key") }
            guard case .scalar(let type) = order[0].type.type,
                  [.i16, .i32, .i64, .decimal, .f32, .f64].contains(type) else {
                throw JedError("0A000", "RANGE offset is not supported for this ordering type")
            }
            key = type
        }
        func literal(_ expression: SQLExpression) throws -> (SQLDecimal, Bool) {
            switch expression.kind {
            case .parenthesized(let child): return try literal(child)
            case .number(let text):
                return (try SQLDecimal.parse(text), !text.contains(".") && !text.lowercased().contains("e"))
            case .unary(let operation, let child) where operation == "+" || operation == "-":
                let (value, integer) = try literal(child); return (operation == "-" ? value.negated() : value, integer)
            default:
                var work = [expression]
                while let item = work.popLast() {
                    if case .call(let call) = item.kind {
                        if call.over != nil || call.overName != nil { throw JedError("42P20", "window function is not allowed in frame bound") }
                        if AggregateQueryPlan.isAggregate(call) { throw JedError("42803", "aggregate is not allowed in frame bound") }
                    }
                    work.append(contentsOf: CatalogExpressionPolicy.children(item))
                }
                throw JedError("0A000", "frame offset must be a non-negative numeric literal")
            }
        }
        func bound(_ input: SQLFrameBound) throws -> Bound {
            switch input {
            case .unbounded(let preceding): return .unbounded(preceding: preceding)
            case .currentRow: return .current
            case .offset(let expression, let preceding):
                let (number, integer) = try literal(expression)
                if syntax.mode != "range" && !integer { throw JedError("0A000", "frame offset must be an integer literal") }
                if number.negative && !allowNegative { throw JedError("22013", "frame offset must not be negative") }
                let nonnegative = allowNegative ? number.absolute() : number
                let value: SQLValue
                if key == .decimal { value = .decimal(nonnegative) }
                else if key == .f32 || key == .f64 { value = try FloatText.fromDecimal(nonnegative, type: .f64) }
                else {
                    guard integer else { throw JedError("0A000", "frame offset must be an integer literal") }
                    guard let count = Int64(nonnegative.description) else { throw JedError("22003", "frame offset is out of range") }
                    value = .integer(count)
                }
                return .offset(value, preceding: preceding)
            }
        }
        return try Self(mode: syntax.mode, start: bound(syntax.start), end: bound(end), exclusion: syntax.exclusion ?? "no others")
    }
}

/// Partition-local peer extents and frame arithmetic. Offsets use wide integer
/// or uncapped exact decimal arithmetic; NULL placement remains a sort boundary.
internal struct WindowFrameIndex {
    let size: Int
    let starts: [Int]
    let ends: [Int]
    let group: [Int]
    let groups: [Range<Int>]

    init(size: Int, peers: (Int, Int) throws -> Bool) rethrows {
        self.size = size
        var ranges: [Range<Int>] = [], first = 0
        for position in 0..<size where position > 0 {
            if try !peers(first, position) { ranges.append(first..<position); first = position }
        }
        if size > 0 { ranges.append(first..<size) }
        var starts = Array(repeating: 0, count: size), ends = starts, group = starts
        for (index, range) in ranges.enumerated() {
            for position in range { starts[position] = range.lowerBound; ends[position] = range.upperBound; group[position] = index }
        }
        self.starts = starts; self.ends = ends; self.group = group; groups = ranges
    }
    func excludes(_ position: Int, candidate: Int, frame: WindowFrame?) -> Bool {
        switch frame?.exclusion {
        case "current row": return candidate == position
        case "group": return group[candidate] == group[position]
        case "ties": return candidate != position && group[candidate] == group[position]
        default: return false
        }
    }
    func bounds(_ position: Int, frame: WindowFrame?, order: [WindowOrder], value: (Int, WindowOrder) -> SQLValue) throws -> Range<Int> {
        guard let frame else { return 0..<(order.isEmpty ? size : ends[position]) }
        func clamp(_ value: Int128) -> Int { Int(max(0, min(Int128(size), value))) }
        func edge(_ bound: WindowFrame.Bound, lower: Bool) throws -> Int {
            switch bound {
            case .unbounded(let preceding): return preceding ? 0 : size
            case .current:
                if frame.mode == "rows" { return lower ? position : position + 1 }
                return lower ? starts[position] : ends[position]
            case .offset(let offset, let preceding):
                if frame.mode == "rows" {
                    guard case .integer(let count) = offset else { throw JedError("XX000", "invalid ROWS bound") }
                    let target = Int128(position) + (preceding ? -Int128(count) : Int128(count))
                    return clamp(target + (lower ? 0 : 1))
                }
                if frame.mode == "groups" {
                    guard case .integer(let count) = offset else { throw JedError("XX000", "invalid GROUPS bound") }
                    let target = Int128(group[position]) + (preceding ? -Int128(count) : Int128(count))
                    if target < 0 { return 0 }; if target >= Int128(groups.count) { return size }
                    return lower ? groups[Int(target)].lowerBound : groups[Int(target)].upperBound
                }
                guard let term = order.first else { throw JedError("XX000", "missing RANGE key") }
                let current = value(position, term)
                if current.isNull { return lower ? starts[position] : ends[position] }
                let subtract = preceding != term.descending
                func compare(_ other: SQLValue) throws -> Int {
                    if other.isNull { return term.nullsFirst ? -1 : 1 }
                    let result: Int
                    switch (other, current, offset) {
                    case (.integer(let a), .integer(let b), .integer(let n)):
                        let bound = Int128(b) + (subtract ? -Int128(n) : Int128(n))
                        result = Int128(a) < bound ? -1 : Int128(a) > bound ? 1 : 0
                    case (.decimal(let a), .decimal(let b), .decimal(let n)):
                        result = a.compare(to: b.addingUncapped(subtract ? n.negated() : n))
                    default:
                        let a = try AggregateStatistics.number(other), b = try AggregateStatistics.number(current)
                        let n = try AggregateStatistics.number(offset), bound = subtract ? b - n : b + n
                        result = a.isNaN ? (bound.isNaN ? 0 : 1) : bound.isNaN ? -1 : a < bound ? -1 : a > bound ? 1 : 0
                    }
                    return term.descending ? -result : result
                }
                var low = 0, high = size
                while low < high {
                    let middle = low + (high - low) / 2
                    let result = try compare(value(middle, term))
                    if lower ? result < 0 : result <= 0 { low = middle + 1 } else { high = middle }
                }
                return low
            }
        }
        let lower = try edge(frame.start, lower: true), upper = try edge(frame.end, lower: false)
        return lower..<max(lower, upper)
    }
}
