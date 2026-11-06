/// The shared bounded implication procedure. Planning consumes no SQL cost;
/// explicit continuation frames keep a flat AND/OR tree off the Swift stack.
internal enum PredicateImplication {
    private struct Atom {
        let expression: ResolvedExpression
        let operation: String
        let value: SQLValue
        let collated: Bool
    }
    private indirect enum Request {
        case conjunction([ResolvedExpression], ResolvedExpression)
        case clause(ResolvedExpression, ResolvedExpression)
        case atom(Atom, ResolvedExpression)
    }
    private enum Work {
        case request(Request)
        case all([Request], Int)
        case any([Request], Int)
    }
    static func implies(_ filters: [ResolvedExpression], predicate: ResolvedExpression, columnOffset: Int = 0) -> Bool {
        let premises = filters.flatMap { QueryEstimate.conjuncts($0) }
        var work: [Work] = [.request(.conjunction(premises, predicate))], result = false, remaining = 10_000
        func schedule(_ requests: [Request], all: Bool) {
            guard let first = requests.first else { result = all; return }
            work.append(all ? .all(requests, 1) : .any(requests, 1)); work.append(.request(first))
        }
        while let next = work.popLast() {
            switch next {
            case .all(let requests, let position):
                guard result, position < requests.count else { continue }
                work.append(.all(requests, position + 1)); work.append(.request(requests[position]))
            case .any(let requests, let position):
                guard !result, position < requests.count else { continue }
                work.append(.any(requests, position + 1)); work.append(.request(requests[position]))
            case .request(let request):
                guard remaining > 0 else { result = false; continue }
                remaining -= 1
                switch request {
                case .conjunction(let premises, let conclusion):
                    if let (a, b) = boolean(conclusion, "and") {
                        schedule([.conjunction(premises, a), .conjunction(premises, b)], all: true)
                    } else {
                        var choices = premises.map { Request.clause($0, conclusion) }
                        if let (a, b) = boolean(conclusion, "or") { choices += [.conjunction(premises, a), .conjunction(premises, b)] }
                        schedule(choices, all: false)
                    }
                case .clause(let premise, let conclusion):
                    if same(premise, conclusion, offset: columnOffset) { result = true }
                    else if let (a, b) = boolean(conclusion, "and") { schedule([.clause(premise, a), .clause(premise, b)], all: true) }
                    else if let (a, b) = boolean(premise, "or") { schedule([.clause(a, conclusion), .clause(b, conclusion)], all: true) }
                    else if let (a, b) = boolean(premise, "and") { schedule([.clause(a, conclusion), .clause(b, conclusion)], all: false) }
                    else if let atoms = quantified(premise) {
                        if atoms.isEmpty { result = false } else { schedule(atoms.map { .atom($0, conclusion) }, all: true) }
                    } else {
                        var choices: [Request] = []
                        if let (a, b) = boolean(conclusion, "or") { choices += [.clause(premise, a), .clause(premise, b)] }
                        if let premise = atom(premise) { choices.append(.atom(premise, conclusion)) }
                        schedule(choices, all: false)
                    }
                case .atom(let premise, let conclusion):
                    if let (a, b) = boolean(conclusion, "and") { schedule([.atom(premise, a), .atom(premise, b)], all: true) }
                    else if let (a, b) = boolean(conclusion, "or") { schedule([.atom(premise, a), .atom(premise, b)], all: false) }
                    else if case .isNull(let expression, true) = conclusion.node,
                            same(premise.expression, expression, offset: columnOffset) { result = true }
                    else if let wanted = atom(conclusion), same(premise.expression, wanted.expression, offset: columnOffset),
                            let order = compare(premise.value, wanted.value),
                            !(ordering(premise.operation) || ordering(wanted.operation)) || (!premise.collated && !wanted.collated) {
                        result = table(premise.operation, wanted.operation, order: order)
                    } else { result = false }
                }
            }
        }
        return result
    }
    private static func boolean(_ expression: ResolvedExpression, _ operation: String) -> (ResolvedExpression, ResolvedExpression)? {
        if case .binary(let op, let a, let b, _) = expression.node, op == operation { return (a, b) }; return nil
    }
    private static func ordering(_ operation: String) -> Bool { ["<", "<=", ">", ">="].contains(operation) }
    private static func constant(_ expression: ResolvedExpression) -> SQLValue? {
        guard case .constant(let value) = expression.node else { return nil }
        switch value {
        case .integer, .decimal, .text, .date, .timestamp, .timestamptz, .boolean, .uuid: return value
        default: return nil
        }
    }
    private static func atom(_ expression: ResolvedExpression) -> Atom? {
        guard case .binary(let operation, let a, let b, let type) = expression.node,
              ["=", "<>", "<", "<=", ">", ">="].contains(operation) else { return nil }
        let collated = type.collation.map { $0.description != "C" } ?? false
        if let value = constant(b) { return Atom(expression: a, operation: operation, value: value, collated: collated) }
        guard let value = constant(a) else { return nil }
        let flipped = ["<": ">", "<=": ">=", ">": "<", ">=": "<="][operation] ?? operation
        return Atom(expression: b, operation: flipped, value: value, collated: collated)
    }
    private static func quantified(_ expression: ResolvedExpression) -> [Atom]? {
        guard case .quantified("=", false, let source, let array, let type) = expression.node,
              TypeRules.fields(type.type) == nil else { return nil }
        let values: [SQLValue]
        switch array.node {
        case .constant(.array(let array)): values = array.elements
        case .array(let elements, false):
            guard elements.count <= 100 else { return nil }
            var result: [SQLValue] = []
            for element in elements {
                if case .constant(.null) = element.node { result.append(.null) }
                else if let value = constant(element) { result.append(value) }
                else { return nil }
            }
            values = result
        default: return nil
        }
        guard values.count <= 100 else { return nil }
        var atoms: [Atom] = []
        for value in values {
            if value.isNull { continue }
            let expression = ResolvedExpression(type: type, node: .constant(value), position: 0)
            guard constant(expression) != nil else { return nil }
            atoms.append(Atom(expression: source, operation: "=", value: value, collated: type.collation.map { $0.description != "C" } ?? false))
        }
        return atoms
    }
    private static func compare(_ a: SQLValue, _ b: SQLValue) -> Int? {
        func order<T: Comparable>(_ a: T, _ b: T) -> Int { a == b ? 0 : a < b ? -1 : 1 }
        switch (a, b) {
        case (.integer(let a), .integer(let b)): return order(a, b)
        case (.decimal(let a), .decimal(let b)): return a.compare(to: b)
        case (.text(let a), .text(let b)): return order(a, b)
        case (.date(let a), .date(let b)): return order(a, b)
        case (.timestamp(let a), .timestamp(let b)), (.timestamptz(let a), .timestamptz(let b)): return order(a, b)
        case (.boolean(let a), .boolean(let b)): return a == b ? 0 : a ? 1 : -1
        case (.uuid(let a), .uuid(let b)): return a.bytes == b.bytes ? 0 : a.bytes.lexicographicallyPrecedes(b.bytes) ? -1 : 1
        default: return nil
        }
    }
    private static func table(_ a: String, _ b: String, order: Int) -> Bool {
        switch a {
        case "=": switch b { case "=": return order == 0; case "<>": return order != 0; case "<": return order < 0; case "<=": return order <= 0; case ">": return order > 0; case ">=": return order >= 0; default: return false }
        case "<": return ["<", "<=", "<>"].contains(b) && order <= 0
        case "<=": return ["<", "<>"].contains(b) && order < 0 || b == "<=" && order <= 0
        case ">": return [">", ">=", "<>"].contains(b) && order >= 0
        case ">=": return [">", "<>"].contains(b) && order > 0 || b == ">=" && order >= 0
        case "<>": return b == "<>" && order == 0
        default: return false
        }
    }
    private static func same(_ a: ResolvedExpression, _ b: ResolvedExpression, offset: Int) -> Bool {
        var work = [(a, b)]
        while let (a, b) = work.popLast() {
            switch (a.node, b.node) {
            case (.column(let x), .column(let y)): if x != y + offset || a.type?.type != b.type?.type { return false }
            case (.binary(let x, let a1, let a2, let xt), .binary(let y, let b1, let b2, let yt)):
                guard x == y, !ordering(x) || xt.collation == yt.collation else { return false }
                work.append((a2, b2)); work.append((a1, b1))
            case (.unary(let x, let a), .unary(let y, let b)):
                guard x == y else { return false }; work.append((a, b))
            case (.isNull(let a, let x), .isNull(let b, let y)):
                guard x == y else { return false }; work.append((a, b))
            case (.quantified(let xo, let xa, let x, let xv, let xt), .quantified(let yo, let ya, let y, let yv, let yt)):
                guard xo == yo, xa == ya, !ordering(xo) || xt.collation == yt.collation else { return false }
                work.append((xv, yv)); work.append((x, y))
            case (.pattern(let x), .pattern(let y)):
                guard x.like == y.like, x.negated == y.negated, x.insensitive == y.insensitive, x.similar == y.similar else { return false }
                work.append((x.pattern, y.pattern)); work.append((x.subject, y.subject))
            case (.rangeOperation(let xo, let xa, let xb, let xt), .rangeOperation(let yo, let ya, let yb, let yt)):
                guard xo == yo, xt == yt else { return false }; work.append((xb, yb)); work.append((xa, ya))
            case (.subscriptValue(let x, let xb), .subscriptValue(let y, let yb)):
                guard xb.count == yb.count else { return false }
                work.append((x, y))
                for (x, y) in zip(xb, yb) {
                    guard x.isSlice == y.isSlice else { return false }
                    if let a = x.lower, let b = y.lower { work.append((a, b)) }
                    else if x.lower != nil || y.lower != nil { return false }
                    if let a = x.upper, let b = y.upper { work.append((a, b)) }
                    else if x.upper != nil || y.upper != nil { return false }
                }
            case (.constant(let x), .constant(let y)):
                if let result = compare(x, y) { if result != 0 { return false } }
                else if !ExpressionIdentity.same(a, b) { return false }
            default:
                if !ExpressionIdentity.same(a, b, columns: { $0 == $1 + offset }) { return false }
            }
        }
        return true
    }
}
