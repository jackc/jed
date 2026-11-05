internal struct GinPosting: Sendable {
    let term: [UInt8]
    let storageKey: [UInt8]
    var key: [UInt8] { term + storageKey }
}

/// The inverted store is an ordinary empty-payload B+tree. A posting edit only
/// decodes one run; it never materializes the complete posting list.
internal struct GinIndex: Sendable {
    private(set) var tree: BPlusTree
    init(tree: BPlusTree) throws {
        guard tree.types.isEmpty else { throw JedError.corruption("GIN entry tree must have no value columns") }
        self.tree = tree
    }
    static func build(postings: [GinPosting], codec: any PageCodec, pool: NodePool? = nil) throws -> GinIndex {
        let ordered = postings.sorted {
            let a = $0.key, b = $1.key
            return a == b ? $0.storageKey.count < $1.storageKey.count : a.lexicographicallyPrecedes(b)
        }
        var tree = BPlusTree(codec: codec, types: [], pool: pool), index = 0
        while index < ordered.count {
            let term = ordered[index].term
            var keys: [[UInt8]] = [], end = index
            while end < ordered.count, ordered[end].term == term {
                keys.append(ordered[end].storageKey); end += 1
            }
            for run in try GinPostingRun.pack(term: term, keys: keys) {
                guard try tree.insert(PackedRecord(key: run, cells: [])) == nil else {
                    throw JedError.corruption("duplicate GIN run key")
                }
            }
            index = end
        }
        return try GinIndex(tree: tree)
    }

    static func postings(values: [SQLValue], opclasses: [GinOpclass], storageKey: [UInt8]) throws -> [GinPosting] {
        guard !storageKey.isEmpty, values.count == opclasses.count, (1...32).contains(values.count) else {
            throw JedError.corruption("invalid GIN index tuple")
        }
        var postings: [GinPosting] = []
        for index in values.indices {
            for term in try opclasses[index].indexTerms(values[index]) {
                postings.append(GinPosting(term: try GinTerms.tagged(term, column: index, columnCount: values.count), storageKey: storageKey))
            }
        }
        return postings
    }

    func runAtOrAfter(term: [UInt8], key: [UInt8]) throws -> [UInt8]? {
        var cursor = TreeCursor(tree, lower: TreeBound(term + key),
            upper: GinTerms.successor(term).map { TreeBound($0, inclusive: false) })
        return try cursor.next()?.key
    }
    private func runBefore(term: [UInt8], key: [UInt8]) throws -> [UInt8]? {
        var cursor = TreeCursor(tree, lower: TreeBound(term), upper: TreeBound(term + key, inclusive: false), descending: true)
        return try cursor.next()?.key
    }
    private static func lowerBound(_ keys: [[UInt8]], _ key: [UInt8]) -> Int {
        var low = 0, high = keys.count
        while low < high {
            let middle = low + (high - low) / 2
            if keys[middle].lexicographicallyPrecedes(key) { low = middle + 1 } else { high = middle }
        }
        return low
    }
    private mutating func replace(_ old: [UInt8]?, with runs: [[UInt8]]) throws {
        var changed = tree
        if let old { _ = try changed.remove(old) }
        for run in runs { _ = try changed.insert(PackedRecord(key: run, cells: [])) }
        tree = changed
    }
    @discardableResult mutating func insert(term: [UInt8], key: [UInt8]) throws -> Bool {
        guard !key.isEmpty else { throw JedError.corruption("empty GIN storage key") }
        if let run = try runAtOrAfter(term: term, key: key) {
            var keys = try GinPostingRun.decode(termLength: term.count, bytes: run)
            let position = Self.lowerBound(keys, key)
            if position < keys.count, keys[position] == key { return false }
            keys.insert(key, at: position)
            try replace(run, with: GinPostingRun.split(term: term, keys: keys)); return true
        }
        if let previous = try runBefore(term: term, key: key) {
            let keys = try GinPostingRun.decode(termLength: term.count, bytes: previous) + [key]
            let grown = try GinPostingRun.encode(term: term, keys: keys)
            if grown.count <= GinPostingRun.maximumBytes { try replace(previous, with: [grown]); return true }
        }
        try replace(nil, with: [GinPostingRun.encode(term: term, keys: [key])]); return true
    }
    @discardableResult mutating func remove(term: [UInt8], key: [UInt8]) throws -> Bool {
        guard !key.isEmpty else { throw JedError.corruption("empty GIN storage key") }
        guard let run = try runAtOrAfter(term: term, key: key) else { return false }
        var keys = try GinPostingRun.decode(termLength: term.count, bytes: run)
        let position = Self.lowerBound(keys, key)
        guard position < keys.count, keys[position] == key else { return false }
        keys.remove(at: position)
        try replace(run, with: GinPostingRun.split(term: term, keys: keys)); return true
    }
}
