/// Normal form preserves source child order while sorting only direct terms.
/// Thus selective AND children can stop later work without reordering costs.
internal indirect enum GinQueryTree: Sendable {
    case term([UInt8])
    case all([GinQueryTree])
    case any([GinQueryTree])

    func normalized() -> GinQueryTree? {
        let conjunction: Bool, children: [GinQueryTree]
        switch self {
        case .term: return self
        case .all(let nodes): conjunction = true; children = nodes
        case .any(let nodes): conjunction = false; children = nodes
        }
        var leaves: [[UInt8]] = [], rest: [GinQueryTree] = []
        func append(_ node: GinQueryTree) {
            switch node {
            case .term(let bytes): leaves.append(bytes)
            case .all(let nodes) where conjunction: for node in nodes { append(node) }
            case .any(let nodes) where !conjunction: for node in nodes { append(node) }
            default: rest.append(node)
            }
        }
        for child in children { if let node = child.normalized() { append(node) } }
        let nodes = GinTerms.distinct(leaves).map(GinQueryTree.term) + rest
        guard !nodes.isEmpty else { return nil }
        if nodes.count == 1 { return nodes[0] }
        return conjunction ? .all(nodes) : .any(nodes)
    }

    func tagged(column: Int, columnCount: Int) throws -> GinQueryTree {
        switch self {
        case .term(let bytes): return .term(try GinTerms.tagged(bytes, column: column, columnCount: columnCount))
        case .all(let children): return .all(try children.map { try $0.tagged(column: column, columnCount: columnCount) })
        case .any(let children): return .any(try children.map { try $0.tagged(column: column, columnCount: columnCount) })
        }
    }
}

internal enum GinStrategy: Sendable {
    case contains, overlaps, member, equal
    case jsonContains, jsonHasKey, jsonHasAnyKey, jsonHasAllKeys
}
internal enum GinQuery: Sendable { case empty, fullScan, gather(GinQueryTree) }

internal extension GinOpclass {
    func query(_ value: SQLValue, strategy: GinStrategy) throws -> GinQuery {
        if value.isNull { return .empty }
        var terms: [[UInt8]] = [], any = false
        if isJSON {
            switch (strategy, value) {
            case (.jsonContains, .jsonb(let document)):
                terms = try GinTerms.jsonIndex(document)
                if terms.isEmpty { return .fullScan }
            case (.jsonHasKey, .text(let text)): terms = [GinTerms.jsonKey(text)]
            case (.jsonHasAnyKey, .array(let array)), (.jsonHasAllKeys, .array(let array)):
                for value in array.elements where !value.isNull {
                    guard case .text(let text) = value else { throw JedError("42804", "GIN key query requires text elements") }
                    terms.append(GinTerms.jsonKey(text))
                }
                any = strategy == .jsonHasAnyKey
                if terms.isEmpty { return any ? .empty : .fullScan }
            default: throw JedError("42804", "GIN query does not match jsonb operator class")
            }
        } else if strategy == .member {
            guard let term = try queryTerm(value) else { return .empty }
            terms = [term]
        } else {
            guard strategy == .contains || strategy == .overlaps || strategy == .equal,
                  case .array(let array) = value else { throw JedError("42804", "GIN query does not match array operator class") }
            any = strategy == .overlaps
            for element in array.elements {
                if element.isNull { if strategy == .contains { return .empty }; continue }
                if let term = try queryTerm(element) { terms.append(term) }
                else if !any { return .empty }
            }
            if terms.isEmpty { return any ? .empty : .fullScan }
        }
        let children = GinTerms.distinct(terms).map(GinQueryTree.term)
        guard let tree = (any ? GinQueryTree.any(children) : .all(children)).normalized() else { return .empty }
        return .gather(tree)
    }
}
