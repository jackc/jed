internal struct RegexOptions: Sendable {
    var ignoreCase = false
    var dotAll = false
    var lineAnchors = false
    var newlineSensitive = false
    var quote = false
}

internal struct RegexClass: Sendable {
    let negated: Bool
    let ranges: [ClosedRange<UInt32>]
    init(negated: Bool = false, ranges: [ClosedRange<UInt32>]) {
        self.negated = negated
        var normalized: [ClosedRange<UInt32>] = []
        for range in ranges.sorted(by: { $0.lowerBound == $1.lowerBound ? $0.upperBound < $1.upperBound : $0.lowerBound < $1.lowerBound }) {
            if let last = normalized.last, range.lowerBound <= last.upperBound + 1 {
                normalized[normalized.count - 1] = last.lowerBound...max(last.upperBound, range.upperBound)
            } else { normalized.append(range) }
        }
        self.ranges = normalized
    }
    func admits(_ value: UInt32) -> Bool {
        var lo = 0, hi = ranges.count
        while lo < hi {
            let mid = lo + (hi - lo) / 2
            if ranges[mid].upperBound < value { lo = mid + 1 } else { hi = mid }
        }
        let inside = lo < ranges.count && ranges[lo].contains(value)
        return inside != negated
    }
    var positiveRanges: [ClosedRange<UInt32>] {
        if !negated { return ranges }
        var result: [ClosedRange<UInt32>] = [], next: UInt32 = 0
        for range in ranges {
            if next < range.lowerBound { result.append(next...(range.lowerBound - 1)) }
            next = range.upperBound + 1
        }
        if next <= 0x10ffff { result.append(next...0x10ffff) }
        return result
    }
    var listing: String { (negated ? "^" : "") + ranges.map { "\($0.lowerBound)-\($0.upperBound)" }.joined(separator: ",") }
}

internal enum RegexInstruction: Sendable {
    case char(UInt32), any, anyChar, characterClass(Int), split(Int, Int), jump(Int), save(Int)
    case start, end, lineStart, lineEnd, gate, match
    var listing: String {
        switch self {
        case .char(let n): "char \(n)"
        case .any: "any"
        case .anyChar: "anychar"
        case .characterClass(let k): "class \(k)"
        case .split(let x, let y): "split \(x) \(y)"
        case .jump(let x): "jmp \(x)"
        case .save(let n): "save \(n)"
        case .start: "assertstart"
        case .end: "assertend"
        case .lineStart: "assertlinestart"
        case .lineEnd: "assertlineend"
        case .gate: "gate"
        case .match: "match"
        }
    }
}

private enum RegexSyntax {
    case empty, char(UInt32), any, characterClass(RegexClass), anchor(RegexInstruction)
    case concat([Int]), alternate([Int]), group(Int, Int), repeatNode(Int, Int, Int?, Bool)
}
private struct RegexNode {
    let syntax: RegexSyntax
    let nullable: Bool
    let size: Int
}

/// A flat syntax arena and explicit group stack avoid depending on the native
/// call stack for long alternations or deeply nested patterns.
private struct RegexParser {
    struct Frame { var branches: [Int] = []; var sequence: [Int] = []; let group: Int? }
    let scalars: [UInt32]
    let options: RegexOptions
    var position = 0, groups = 0
    var nodes: [RegexNode] = []
    static func invalid(_ detail: String) -> JedError { JedError("2201B", "invalid regular expression: \(detail)") }
    static func saturated(_ n: Int) -> Int { min(RegexProgram.maximumInstructions + 1, n) }
    mutating func append(_ syntax: RegexSyntax, nullable: Bool, size: Int) -> Int {
        nodes.append(RegexNode(syntax: syntax, nullable: nullable, size: Self.saturated(size))); return nodes.count - 1
    }
    mutating func sequence(_ items: [Int]) -> Int {
        if items.count == 1 { return items[0] }
        return append(.concat(items), nullable: items.allSatisfy { nodes[$0].nullable }, size: items.reduce(0) { Self.saturated($0 + nodes[$1].size) })
    }
    mutating func finish(_ frame: Frame) -> Int {
        var branches = frame.branches; branches.append(sequence(frame.sequence))
        let child: Int
        if branches.count == 1 { child = branches[0] }
        else { child = append(.alternate(branches), nullable: branches.contains { nodes[$0].nullable }, size: branches.reduce((branches.count - 1) * 2) { Self.saturated($0 + nodes[$1].size) }) }
        if let group = frame.group { return append(.group(group, child), nullable: nodes[child].nullable, size: nodes[child].size + 2) }
        return child
    }
    mutating func parse() throws -> Int {
        if options.quote {
            var children: [Int] = []
            for scalar in scalars { children.append(append(.char(scalar), nullable: false, size: 1)) }
            return sequence(children)
        }
        var frames = [Frame(group: nil)]
        while position < scalars.count {
            let scalar = scalars[position]
            if scalar == 124 {
                position += 1
                let branch = sequence(frames[frames.count - 1].sequence)
                frames[frames.count - 1].branches.append(branch); frames[frames.count - 1].sequence = []
                continue
            }
            if scalar == 40 {
                position += 1
                var group: Int? = nil
                if position < scalars.count && scalars[position] == 63 {
                    guard position + 1 < scalars.count && scalars[position + 1] == 58 else { throw Self.invalid("unsupported group syntax") }
                    position += 2
                } else { groups += 1; group = groups }
                frames.append(Frame(group: group)); continue
            }
            let atom: Int
            if scalar == 41 {
                guard frames.count > 1 else { throw Self.invalid("unbalanced parenthesis") }
                position += 1; atom = finish(frames.removeLast())
            } else {
                if [42, 43, 63].contains(scalar) || scalar == 123 && repetition(at: position) != nil { throw Self.invalid("quantifier operand missing") }
                position += 1
                switch scalar {
                case 46: atom = options.dotAll ? append(.characterClass(RegexClass(negated: true, ranges: [])), nullable: false, size: 1) : append(.any, nullable: false, size: 1)
                case 91: atom = append(.characterClass(try characterClass()), nullable: false, size: 1)
                case 92:
                    let escaped = try escape()
                    switch escaped {
                    case .literal(let c): atom = append(.char(c), nullable: false, size: 1)
                    case .characterClass(let c): atom = append(.characterClass(c), nullable: false, size: 1)
                    case .anchor(let instruction): atom = append(.anchor(instruction), nullable: true, size: 1)
                    }
                case 94: atom = append(.anchor(options.lineAnchors ? .lineStart : .start), nullable: true, size: 1)
                case 36: atom = append(.anchor(options.lineAnchors ? .lineEnd : .end), nullable: true, size: 1)
                default: atom = append(.char(scalar), nullable: false, size: 1)
                }
            }
            let quantified = try quantify(atom)
            frames[frames.count - 1].sequence.append(quantified)
        }
        guard frames.count == 1 else { throw Self.invalid("unbalanced parenthesis") }
        return finish(frames[0])
    }
    struct Repetition { let minimum: Int; let maximum: Int?; let end: Int; let reversed: Bool }
    func repetition(at start: Int) -> Repetition? {
        guard start < scalars.count && scalars[start] == 123 else { return nil }
        var cursor = start + 1
        func digits() -> [UInt32]? {
            let begin = cursor
            while cursor < scalars.count && scalars[cursor] >= 48 && scalars[cursor] <= 57 { cursor += 1 }
            guard cursor > begin else { return nil }
            let result = scalars[begin..<cursor].drop(while: { $0 == 48 })
            return result.isEmpty ? [48] : Array(result)
        }
        func count(_ digits: [UInt32]) -> Int {
            digits.reduce(0) { min(RegexProgram.maximumInstructions + 1, $0 * 10 + Int($1 - 48)) }
        }
        guard let minimum = digits(), cursor < scalars.count else { return nil }
        if scalars[cursor] == 125 { return Repetition(minimum: count(minimum), maximum: count(minimum), end: cursor + 1, reversed: false) }
        guard scalars[cursor] == 44 else { return nil }; cursor += 1
        if cursor < scalars.count && scalars[cursor] == 125 { return Repetition(minimum: count(minimum), maximum: nil, end: cursor + 1, reversed: false) }
        guard let maximum = digits(), cursor < scalars.count && scalars[cursor] == 125 else { return nil }
        let reversed = maximum.count < minimum.count || maximum.count == minimum.count && maximum.lexicographicallyPrecedes(minimum)
        let lower = count(minimum)
        let upper: Int
        if reversed { upper = 0 }
        else {
            let lowMagnitude = BigMagnitude(decimalDigits: minimum.map { UInt8($0) }[...])
            let highMagnitude = BigMagnitude(decimalDigits: maximum.map { UInt8($0) }[...])
            let difference = highMagnitude.subtracting(lowMagnitude)
            let optional = difference.decimalDigits > 5 ? RegexProgram.maximumInstructions + 1 : min(RegexProgram.maximumInstructions + 1, Int(difference.description)!)
            upper = lower + optional
        }
        return Repetition(minimum: lower, maximum: upper, end: cursor + 1, reversed: reversed)
    }
    mutating func quantify(_ atom: Int) throws -> Int {
        guard position < scalars.count else { return atom }
        var minimum: Int, maximum: Int?
        switch scalars[position] {
        case 42: minimum = 0; maximum = nil; position += 1
        case 43: minimum = 1; maximum = nil; position += 1
        case 63: minimum = 0; maximum = 1; position += 1
        case 123:
            guard let repetition = repetition(at: position) else { return atom }
            guard !repetition.reversed else { throw Self.invalid("invalid repetition count") }
            minimum = repetition.minimum; maximum = repetition.maximum; position = repetition.end
        default: return atom
        }
        var greedy = true
        if position < scalars.count && scalars[position] == 63 { greedy = false; position += 1 }
        let size: Int
        let child = nodes[atom]
        // Plus has its own one-copy loop lowering. A bounded {1,} uses one
        // mandatory copy and a separate star, so retain its spelling below.
        let source = scalars[position - (greedy ? 1 : 2)]
        if source == 43 { return append(.repeatNode(atom, -1, nil, greedy), nullable: child.nullable, size: child.size + 1) }
        if let maximum { size = child.size * minimum + (maximum - minimum) * (child.size + 1) }
        else { size = child.size * minimum + child.size + 2 }
        return append(.repeatNode(atom, minimum, maximum, greedy), nullable: minimum == 0 || child.nullable, size: size)
    }
    enum Escaped { case literal(UInt32), characterClass(RegexClass), anchor(RegexInstruction) }
    mutating func escape(inClass: Bool = false) throws -> Escaped {
        guard position < scalars.count else { throw Self.invalid("trailing backslash") }
        let c = scalars[position]; position += 1
        switch c {
        case 100, 68: return .characterClass(RegexClass(negated: c == 68, ranges: [48...57]))
        case 119, 87: return .characterClass(RegexClass(negated: c == 87, ranges: [48...57, 65...90, 95...95, 97...122]))
        case 115, 83: return .characterClass(RegexClass(negated: c == 83, ranges: [9...13, 32...32]))
        case 110: return .literal(10)
        case 116: return .literal(9)
        case 114: return .literal(13)
        case 102: return .literal(12)
        case 118: return .literal(11)
        case 65 where !inClass: return .anchor(.start)
        case 122 where !inClass: return .anchor(.end)
        default:
            if (48...57).contains(c) || (65...90).contains(c) || (97...122).contains(c) {
                throw Self.invalid("invalid escape \\" + String(UnicodeScalar(c)!))
            }
            return .literal(c)
        }
    }
    mutating func classItem() throws -> Escaped {
        guard position < scalars.count else { throw Self.invalid("unbalanced bracket expression") }
        let c = scalars[position]; position += 1
        if c == 92 { return try escape(inClass: true) }
        if c == 91 && position < scalars.count && [58, 46, 61].contains(scalars[position]) { throw Self.invalid("POSIX bracket expressions ([:class:], [.coll.], [=equiv=]) are not supported") }
        return .literal(c)
    }
    mutating func characterClass() throws -> RegexClass {
        var negated = false, first = true, ranges: [ClosedRange<UInt32>] = []
        if position < scalars.count && scalars[position] == 94 { negated = true; position += 1 }
        while position < scalars.count {
            if scalars[position] == 93 && !first {
                position += 1
                if negated && options.newlineSensitive && !options.dotAll { ranges.append(10...10) }
                return RegexClass(negated: negated, ranges: ranges)
            }
            let item = try classItem()
            if case .literal(let low) = item, position + 1 < scalars.count, scalars[position] == 45, scalars[position + 1] != 93 {
                position += 1
                let last = try classItem()
                guard case .literal(let high) = last, low <= high else { throw Self.invalid("invalid range in bracket expression") }
                ranges.append(low...high)
            } else {
                switch item {
                case .literal(let c): ranges.append(c...c)
                case .characterClass(let c): ranges.append(contentsOf: c.positiveRanges)
                case .anchor: preconditionFailure("class escape cannot be an anchor")
                }
            }
            first = false
        }
        throw Self.invalid("unbalanced bracket expression")
    }
}

internal struct RegexProgram: Sendable {
    static let maximumInstructions = 32_768
    enum Wrapper: Sendable { case ordinary, gatedLeftmost, gatedRightmost }
    let instructions: [RegexInstruction]
    let classes: [RegexClass]
    let groupCount: Int
    let options: RegexOptions
    let fold: @Sendable (UInt32) -> UInt32
    var instructionCount: Int { instructions.count }
    var listing: [String] { instructions.map(\.listing) }
    var classListing: [String] { classes.map(\.listing) }
    static func asciiFold(_ scalar: UInt32) -> UInt32 { (65...90).contains(scalar) ? scalar + 32 : scalar }
    static func compile(pattern: SQLText, options: RegexOptions = RegexOptions(), wrapper: Wrapper = .ordinary,
                        fold: @escaping @Sendable (UInt32) -> UInt32 = RegexProgram.asciiFold) throws -> RegexProgram {
        var scalars = pattern.description.unicodeScalars.map(\.value)
        if options.ignoreCase {
            var escaped = false
            for i in scalars.indices {
                if !options.quote && escaped { escaped = false; continue }
                if !options.quote && scalars[i] == 92 { escaped = true; continue }
                scalars[i] = fold(scalars[i])
            }
        }
        var parser = RegexParser(scalars: scalars, options: options)
        let root = try parser.parse()
        let gated = wrapper != .ordinary
        guard parser.nodes[root].size <= maximumInstructions - (gated ? 7 : 6) else { throw JedError("54001", "regular expression program too large") }
        var compiler = RegexEmitter(nodes: parser.nodes)
        compiler.instructions = [.split(wrapper == .gatedRightmost ? 1 : 3, wrapper == .gatedRightmost ? 3 : 1), .anyChar, .jump(0)]
        if gated { compiler.instructions.append(.gate) }
        compiler.instructions.append(.save(0))
        compiler.emit(root)
        compiler.instructions.append(contentsOf: [.save(1), .match])
        precondition(compiler.instructions.count <= maximumInstructions)
        return RegexProgram(instructions: compiler.instructions, classes: compiler.classes, groupCount: parser.groups, options: options, fold: fold)
    }
    func chargeCompile(_ meter: CostMeter) throws { meter.charge(Int64(instructionCount)); try meter.guardCost() }
    func subjectScalars(_ text: SQLText) -> [UInt32] {
        let scalars = text.description.unicodeScalars.map(\.value)
        return options.ignoreCase ? scalars.map(fold) : scalars
    }
    func search(_ subject: SQLText, start: Int = 0, meter: CostMeter, scratch: RegexScratch? = nil) throws -> RegexMatch? {
        try search(scalars: subjectScalars(subject), start: start, meter: meter, scratch: scratch)
    }
    func search(scalars: [UInt32], start: Int = 0, gates: Set<Int>? = nil, meter: CostMeter, scratch: RegexScratch? = nil) throws -> RegexMatch? {
        try run(scalars, start: start, gates: gates, allEnds: false, meter: meter, scratch: scratch).match
    }
    func ends(scalars: [UInt32], gates: Set<Int>, meter: CostMeter, scratch: RegexScratch? = nil) throws -> Set<Int> {
        try run(scalars, start: 0, gates: gates, allEnds: true, meter: meter, scratch: scratch).ends
    }
    func isMatch(_ subject: SQLText, meter: CostMeter, scratch: RegexScratch? = nil) throws -> Bool { try search(subject, meter: meter, scratch: scratch) != nil }
}

private struct RegexEmitter {
    enum Task { case node(Int), instruction(RegexInstruction), star(Int, Bool) }
    let nodes: [RegexNode]
    var instructions: [RegexInstruction] = []
    var classes: [RegexClass] = []
    mutating func emit(_ root: Int) {
        var work: [Task] = [.node(root)]
        while let task = work.popLast() {
            let start = instructions.count
            switch task {
            case .instruction(let instruction): instructions.append(instruction)
            case .star(let child, let greedy):
                let end = start + nodes[child].size + 2
                instructions.append(.split(greedy ? start + 1 : end, greedy ? end : start + 1))
                if nodes[child].nullable { work.append(.instruction(.split(greedy ? start + 1 : end, greedy ? end : start + 1))) }
                else { work.append(.instruction(.jump(start))) }
                work.append(.node(child))
            case .node(let index):
                switch nodes[index].syntax {
                case .empty: break
                case .char(let c): instructions.append(.char(c))
                case .any: instructions.append(.any)
                case .characterClass(let c): instructions.append(.characterClass(classes.count)); classes.append(c)
                case .anchor(let anchor): instructions.append(anchor)
                case .concat(let children): work.append(contentsOf: children.reversed().map(Task.node))
                case .group(let group, let child):
                    instructions.append(.save(group * 2)); work.append(.instruction(.save(group * 2 + 1))); work.append(.node(child))
                case .alternate(let branches):
                    let end = start + nodes[index].size
                    var tasks: [Task] = [], cursor = start
                    for (i, child) in branches.enumerated() {
                        if i + 1 < branches.count {
                            tasks.append(.instruction(.split(cursor + 1, cursor + nodes[child].size + 2)))
                            tasks.append(.node(child)); tasks.append(.instruction(.jump(end)))
                            cursor += nodes[child].size + 2
                        } else { tasks.append(.node(child)) }
                    }
                    work.append(contentsOf: tasks.reversed())
                case .repeatNode(let child, let minimum, let maximum, let greedy):
                    if minimum == -1 {
                        let end = start + nodes[child].size + 1
                        work.append(.instruction(.split(greedy ? start : end, greedy ? end : start))); work.append(.node(child))
                        continue
                    }
                    var tasks: [Task] = [], cursor = start
                    if nodes[child].size != 0 {
                        for _ in 0..<minimum { tasks.append(.node(child)); cursor += nodes[child].size }
                    }
                    if let maximum {
                        for _ in minimum..<maximum {
                            let end = cursor + nodes[child].size + 1
                            tasks.append(.instruction(.split(greedy ? cursor + 1 : end, greedy ? end : cursor + 1)))
                            tasks.append(.node(child)); cursor = end
                        }
                    } else { tasks.append(.star(child, greedy)) }
                    work.append(contentsOf: tasks.reversed())
                }
            }
        }
    }
}

/// Bounded persistent capture trees: each Save copies at most four nodes of
/// sixteen entries. Flat captures keep the common <=7-group path compact.
fileprivate final class RegexCaptureNode: Sendable {
    let values: [Int]
    let children: [RegexCaptureNode?]
    init(values: [Int] = [], children: [RegexCaptureNode?] = []) { self.values = values; self.children = children }
    static func replacing(_ root: RegexCaptureNode?, index: Int, value: Int, depth: Int) -> RegexCaptureNode {
        if depth == 1 {
            var values = root?.values ?? Array(repeating: -1, count: 16)
            values[index & 15] = value; return RegexCaptureNode(values: values)
        }
        var children = root?.children ?? Array(repeating: nil, count: 16)
        let branch = index >> ((depth - 1) * 4) & 15
        children[branch] = replacing(children[branch], index: index, value: value, depth: depth - 1)
        return RegexCaptureNode(children: children)
    }
    static func read(_ root: RegexCaptureNode?, index: Int, depth: Int) -> Int {
        guard let root else { return -1 }
        if depth == 1 { return root.values[index & 15] }
        return read(root.children[index >> ((depth - 1) * 4) & 15], index: index, depth: depth - 1)
    }
}
fileprivate struct RegexCaptures: Sendable {
    var flat: [Int]
    var root: RegexCaptureNode?
    let depth: Int
    init(count: Int) {
        var depth = 1, span = 16
        while span < count { span *= 16; depth += 1 }
        self.depth = depth; flat = depth == 1 ? Array(repeating: -1, count: count) : []
    }
    func setting(_ index: Int, to value: Int) -> RegexCaptures {
        var result = self
        if depth == 1 { result.flat[index] = value }
        else { result.root = RegexCaptureNode.replacing(root, index: index, value: value, depth: depth) }
        return result
    }
    func read(_ index: Int) -> Int { depth == 1 ? flat[index] : RegexCaptureNode.read(root, index: index, depth: depth) }
}
internal struct RegexMatch: Sendable {
    fileprivate let captures: RegexCaptures
    let groupCount: Int
    func span(_ group: Int = 0) -> Range<Int>? {
        guard group >= 0 && group <= groupCount else { return nil }
        let start = captures.read(group * 2), end = captures.read(group * 2 + 1)
        return start >= 0 && end >= start ? start..<end : nil
    }
    var captureSpans: [[Int]] { (0...groupCount).map { [captures.read($0 * 2), captures.read($0 * 2 + 1)] } }
}

/// One search owner retains this scratch across calls. Sharing a program across
/// workers is safe; each worker supplies its own independently stamped scratch.
internal final class RegexScratch {
    fileprivate var marks: [UInt32] = []
    fileprivate var generation: UInt32 = 0
    fileprivate func next(count: Int) {
        if marks.count != count { marks = Array(repeating: 0, count: count); generation = 0 }
        generation &+= 1
        if generation == 0 { marks = Array(repeating: 0, count: count); generation = 1 }
    }
}

extension RegexProgram {
    private struct Thread { let pc: Int; let captures: RegexCaptures }
    private func closure(_ seed: Thread, into list: inout [Thread], at position: Int, input: [UInt32], gates: Set<Int>?, scratch: RegexScratch, meter: CostMeter) {
        var work = [seed]
        while let thread = work.popLast() {
            if scratch.marks[thread.pc] == scratch.generation { continue }
            scratch.marks[thread.pc] = scratch.generation; meter.charge(1)
            switch instructions[thread.pc] {
            case .jump(let target): work.append(Thread(pc: target, captures: thread.captures))
            case .split(let first, let second):
                work.append(Thread(pc: second, captures: thread.captures)); work.append(Thread(pc: first, captures: thread.captures))
            case .save(let slot): work.append(Thread(pc: thread.pc + 1, captures: thread.captures.setting(slot, to: position)))
            case .start: if position == 0 { work.append(Thread(pc: thread.pc + 1, captures: thread.captures)) }
            case .end: if position == input.count { work.append(Thread(pc: thread.pc + 1, captures: thread.captures)) }
            case .lineStart: if position == 0 || input[position - 1] == 10 { work.append(Thread(pc: thread.pc + 1, captures: thread.captures)) }
            case .lineEnd: if position == input.count || input[position] == 10 { work.append(Thread(pc: thread.pc + 1, captures: thread.captures)) }
            case .gate: if gates?.contains(position) == true { work.append(Thread(pc: thread.pc + 1, captures: thread.captures)) }
            default: list.append(thread)
            }
        }
    }
    fileprivate func run(_ input: [UInt32], start: Int, gates: Set<Int>?, allEnds: Bool, meter: CostMeter, scratch supplied: RegexScratch?) throws -> (match: RegexMatch?, ends: Set<Int>) {
        guard start >= 0 && start <= input.count else { return (nil, []) }
        let scratch = supplied ?? RegexScratch()
        scratch.next(count: instructions.count)
        var current: [Thread] = [], matched: RegexMatch? = nil, ends: Set<Int> = []
        closure(Thread(pc: 0, captures: RegexCaptures(count: 2 * (groupCount + 1))), into: &current, at: start, input: input, gates: gates, scratch: scratch, meter: meter)
        for position in start...input.count {
            var next: [Thread] = []
            scratch.next(count: instructions.count)
            for thread in current {
                let instruction = instructions[thread.pc]
                if case .match = instruction {
                    if allEnds { ends.insert(position); continue }
                    matched = RegexMatch(captures: thread.captures, groupCount: groupCount); break
                }
                guard position < input.count else { continue }
                let accepts: Bool
                switch instruction {
                case .char(let c): accepts = input[position] == c
                case .any: accepts = input[position] != 10
                case .anyChar: accepts = true
                case .characterClass(let index): accepts = classes[index].admits(input[position])
                default: preconditionFailure("epsilon instruction remained in consume list")
                }
                if accepts { closure(Thread(pc: thread.pc + 1, captures: thread.captures), into: &next, at: position + 1, input: input, gates: gates, scratch: scratch, meter: meter) }
            }
            current = next; try meter.guardCost()
            if current.isEmpty { break }
        }
        return (matched, ends)
    }
}
