internal struct GistEntry: Sendable {
    let bound: [GistBound]
    let storageKey: [UInt8]

    func leafKey(opclasses: [GistOpclass]) throws -> [UInt8] {
        try GistTuple.encode(bound, ops: opclasses) + storageKey
    }
    static func decode(_ bytes: [UInt8], opclasses: [GistOpclass]) throws -> GistEntry {
        var reader = ByteReader(bytes)
        let bound = try GistTuple.decode(&reader, ops: opclasses, leaf: true)
        guard reader.remaining > 0 else { throw JedError.corruption("GiST entry has no storage key") }
        return GistEntry(bound: bound, storageKey: Array(try reader.take(reader.remaining)))
    }
}

/// Every node is immutable after construction. Cached union and minimum row key
/// make sibling ordering independent of allocation and avoid recursive sort work.
internal final class GistNode: Sendable {
    let entries: [GistEntry]
    let children: [GistNode]
    let bound: [GistBound]
    let minimumStorageKey: [UInt8]
    let height: Int
    let nodeCount: Int
    let recordCount: Int
    var isLeaf: Bool { children.isEmpty }
    var entryCount: Int { isLeaf ? entries.count : children.count }

    init(entries: [GistEntry], opclasses: [GistOpclass]) throws {
        guard let first = entries.first else { throw JedError.corruption("empty GiST leaf") }
        self.entries = entries; children = []; height = 1; nodeCount = 1; recordCount = entries.count
        var union = first.bound, minimum = first.storageKey
        for entry in entries.dropFirst() {
            union = try GistTuple.union(union, entry.bound, ops: opclasses)
            if entry.storageKey.lexicographicallyPrecedes(minimum) { minimum = entry.storageKey }
        }
        bound = union; minimumStorageKey = minimum
    }
    init(children: [GistNode], opclasses: [GistOpclass]) throws {
        guard let first = children.first, children.allSatisfy({ $0.height == first.height }) else {
            throw JedError.corruption("empty or unbalanced GiST interior")
        }
        entries = []; self.children = children; height = first.height + 1
        nodeCount = 1 + children.reduce(0) { $0 + $1.nodeCount }
        recordCount = children.reduce(0) { $0 + $1.recordCount }
        var union = first.bound, minimum = first.minimumStorageKey
        for child in children.dropFirst() {
            union = try GistTuple.union(union, child.bound, ops: opclasses)
            if child.minimumStorageKey.lexicographicallyPrecedes(minimum) { minimum = child.minimumStorageKey }
        }
        bound = union; minimumStorageKey = minimum
    }
}

internal struct GistSearchResult: Sendable {
    let storageKeys: [[UInt8]]
    let nodesVisited: Int
    let interiorsVisited: Int
    /// `range @> empty` needs the ordinary table scan (gist.md §5). No partial
    /// candidates or charges are returned when this is true.
    let requiresFullScan: Bool
}

internal struct GistTree: Sendable {
    static let fanout = 4
    static let maximumDepth = 256
    let opclasses: [GistOpclass]
    let root: GistNode?
    var count: Int { root?.recordCount ?? 0 }
    var nodeCount: Int { root?.nodeCount ?? 0 }
    var isEmpty: Bool { root == nil }

    static func entryLess(_ a: GistEntry, _ b: GistEntry, ops: [GistOpclass]) throws -> Bool {
        let order = try GistTuple.compare(a.bound, b.bound, ops: ops)
        return order == 0 ? a.storageKey.lexicographicallyPrecedes(b.storageKey) : order < 0
    }
    static func childLess(_ a: GistNode, _ b: GistNode, ops: [GistOpclass]) throws -> Bool {
        let order = try GistTuple.compare(a.bound, b.bound, ops: ops)
        return order == 0 ? a.minimumStorageKey.lexicographicallyPrecedes(b.minimumStorageKey) : order < 0
    }

    /// Canonical whole-tree rebuild is the supported GiST storage contract.
    /// The caller supplies the final statement's leaf set, after all mutations.
    static func build(opclasses: [GistOpclass], leafKeys: [[UInt8]]) throws -> GistTree {
        try build(opclasses: opclasses, entries: leafKeys.map { try GistEntry.decode($0, opclasses: opclasses) })
    }
    static func build(opclasses: [GistOpclass], entries: [GistEntry]) throws -> GistTree {
        try GistTuple.validateCount(opclasses.count, opclasses)
        var keys: Set<[UInt8]> = []
        for entry in entries {
            try GistTuple.validateCount(entry.bound.count, opclasses)
            guard !entry.storageKey.isEmpty, keys.insert(entry.storageKey).inserted else {
                throw JedError.corruption("empty or duplicate GiST storage key")
            }
            for (op, bound) in zip(opclasses, entry.bound) { try op.validate(bound, leaf: true) }
        }
        let ordered = try entries.sorted { try entryLess($0, $1, ops: opclasses) }
        var root: GistNode?
        for entry in ordered {
            if let previous = root {
                let nodes = try insert(entry, into: previous, ops: opclasses)
                root = nodes.count == 1 ? nodes[0] : try GistNode(children: nodes.sorted {
                    try childLess($0, $1, ops: opclasses)
                }, opclasses: opclasses)
            } else { root = try GistNode(entries: [entry], opclasses: opclasses) }
        }
        return GistTree(opclasses: opclasses, root: root)
    }

    private static func insert(_ entry: GistEntry, into node: GistNode, ops: [GistOpclass]) throws -> [GistNode] {
        if node.isLeaf {
            let rows = try (node.entries + [entry]).sorted { try entryLess($0, $1, ops: ops) }
            if rows.count <= fanout { return [try GistNode(entries: rows, opclasses: ops)] }
            let middle = (rows.count + 1) / 2
            return [try GistNode(entries: Array(rows[..<middle]), opclasses: ops),
                    try GistNode(entries: Array(rows[middle...]), opclasses: ops)]
        }
        var slot = 0, best: [UInt8]?
        for index in node.children.indices {
            let union = try GistTuple.union(node.children[index].bound, entry.bound, ops: ops)
            let bytes = try GistTuple.encode(union, ops: ops)
            if best == nil || bytes.lexicographicallyPrecedes(best!) { slot = index; best = bytes }
        }
        var children = node.children
        let replacements = try insert(entry, into: children[slot], ops: ops)
        children[slot] = replacements[0]
        if replacements.count == 2 { children.append(replacements[1]) }
        children = try children.sorted { try childLess($0, $1, ops: ops) }
        if children.count <= fanout { return [try GistNode(children: children, opclasses: ops)] }
        let middle = (children.count + 1) / 2
        return [try GistNode(children: Array(children[..<middle]), opclasses: ops),
                try GistNode(children: Array(children[middle...]), opclasses: ops)]
    }

    func leafKeys() throws -> [[UInt8]] {
        var keys: [[UInt8]] = []
        func visit(_ node: GistNode) throws {
            for entry in node.entries { keys.append(try entry.leafKey(opclasses: opclasses)) }
            for child in node.children { try visit(child) }
        }
        if let root { try visit(root) }
        return keys
    }

    func search(queries: [GistQuery], strategies: [GistStrategy]) throws -> GistSearchResult {
        try GistTuple.validateCount(queries.count, opclasses)
        try GistTuple.validateCount(strategies.count, opclasses)
        var needsScan = false, impossible = false
        for index in opclasses.indices {
            switch (opclasses[index].isRange, queries[index], strategies[index]) {
            case (true, .range(let range), .contains): needsScan = needsScan || range.isEmpty
            case (true, .range(let range), .overlaps): impossible = impossible || range.isEmpty
            case (false, .scalar(let key), .equal):
                _ = try opclasses[index].matches(.scalar(min: key, max: key), query: queries[index], strategy: .equal, leaf: true)
            default: throw JedError("0A000", "GiST strategy does not match its operator class")
            }
        }
        if impossible {
            return GistSearchResult(storageKeys: [], nodesVisited: 0, interiorsVisited: 0, requiresFullScan: false)
        }
        if needsScan { return GistSearchResult(storageKeys: [], nodesVisited: 0, interiorsVisited: 0, requiresFullScan: true) }
        // The resident empty root has no durable page, but searching it is one
        // logical visit in the frozen cost contract. Empty-overlap and NULL
        // probes are eliminated by the caller before this descent.
        guard let root else { return GistSearchResult(storageKeys: [], nodesVisited: 1, interiorsVisited: 0, requiresFullScan: false) }
        var keys: [[UInt8]] = [], nodes = 0, interiors = 0
        func matches(_ bound: [GistBound], leaf: Bool) throws -> Bool {
            for index in opclasses.indices {
                if try !opclasses[index].matches(bound[index], query: queries[index], strategy: strategies[index], leaf: leaf) { return false }
            }
            return true
        }
        func visit(_ node: GistNode) throws {
            nodes += 1
            if node.isLeaf {
                for entry in node.entries where try matches(entry.bound, leaf: true) { keys.append(entry.storageKey) }
            } else {
                interiors += 1
                for child in node.children where try matches(child.bound, leaf: false) { try visit(child) }
            }
        }
        try visit(root)
        // The executor point-fetches in primary/storage-key order. Sorting is
        // independent of physical page identifiers and does not alter visit cost.
        keys.sort { $0.lexicographicallyPrecedes($1) }
        return GistSearchResult(storageKeys: keys, nodesVisited: nodes, interiorsVisited: interiors, requiresFullScan: false)
    }
}
