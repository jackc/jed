internal struct TreeBound: Sendable {
    let key: [UInt8]
    let inclusive: Bool
    init(_ key: [UInt8], inclusive: Bool = true) { self.key = key; self.inclusive = inclusive }
}

/// A retained position in an immutable leaf. Readers that need only selected
/// columns can borrow their cells without copying a key or constructing every
/// column of a PackedRecord. The node keeps both page bytes and pending values
/// alive independently of the cursor's next leaf or close.
internal struct TreePosition: Sendable {
    let node: TreeNode
    let row: Int

    func record() -> PackedRecord { node.record(at: row) }
    func cell(_ column: Int) -> StoredCell { node.cell(row: row, column: column) }
    func isNull(_ column: Int) -> Bool { node.isNull(row: row, column: column) }
    func materialize(_ column: Int, pager: Pager?) throws -> SQLValue {
        // Clean packed leaves decode common scalars straight from the page.
        if node.pending.isEmpty, let leaf = node.leaf, let value = try leaf.fastValue(row: row, column: column) {
            return value
        }
        let type = node.types[column]
        if !node.pending.isEmpty, let overflow = node.pending[row]?[column] {
            let raw = overflow.compressed ? try LZ4.decompress(overflow.bytes, rawLength: overflow.rawLength) : overflow.bytes
            return try ValueCodec.decodeRawBody(raw, type: type)
        }
        return try cell(column).materialize(type: type, pager: pager)
    }
}

/// Consecutive admitted rows of one leaf, in cursor order. A consumer holds
/// the node once for the whole run instead of once per row.
internal struct TreeRun: Sendable {
    let node: TreeNode
    let first: Int
    let count: Int
    let descending: Bool
    @inline(__always) func row(_ ordinal: Int) -> Int { descending ? first - ordinal : first + ordinal }
}

/// A snapshot cursor owns only its root, one interior frame per level, and its
/// current leaf. It never collects a range or follows sibling links.
internal struct TreeCursor: Sendable {
    private struct Frame: Sendable {
        let node: TreeNode
        var child: Int
    }
    private var tree: BPlusTree?
    private let lower: TreeBound?
    private let upper: TreeBound?
    private let descending: Bool
    private var stack: [Frame] = []
    private var leaf: TreeNode?
    private var row = 0
    private var started = false
    private var finished = false
    private var hintWindow = 1
    private(set) var visitedNodes = 0

    init(_ tree: BPlusTree, lower: TreeBound? = nil, upper: TreeBound? = nil, descending: Bool = false) {
        self.tree = tree; self.lower = lower; self.upper = upper; self.descending = descending
    }

    mutating func next() throws -> PackedRecord? {
        try nextPosition()?.record()
    }

    mutating func nextPosition() throws -> TreePosition? {
        guard !finished else { return nil }
        if !started {
            started = true
            guard let tree, let root = tree.root else { finish(); return nil }
            if let lower, let upper,
               upper.key.lexicographicallyPrecedes(lower.key) ||
                (upper.key == lower.key && (!upper.inclusive || !lower.inclusive)) {
                finish(); return nil
            }
            try descend(root, seek: descending ? upper : lower, tree: tree)
        }
        while let leaf {
            if row >= 0, row < leaf.entryCount {
                // The seek already satisfied the bound on the entry side, and
                // keys are strictly ordered, so only the exit bound can fail.
                if descending {
                    if let lower {
                        let order = leaf.compareKey(at: row, with: lower.key)
                        if order < 0 || (order == 0 && !lower.inclusive) { finish(); return nil }
                    }
                } else if let upper {
                    let order = leaf.compareKey(at: row, with: upper.key)
                    if order > 0 || (order == 0 && !upper.inclusive) { finish(); return nil }
                }
                let position = TreePosition(node: leaf, row: row)
                row += descending ? -1 : 1
                return position
            }
            // The tree is needed only at a leaf transition. Binding it before
            // the row loop copies its reference-bearing snapshot on every pull.
            guard let tree else { finish(); return nil }
            try advanceLeaf(tree)
        }
        finish(); return nil
    }

    /// The rest of the current leaf that lies inside the bounds, visiting
    /// exactly the rows and nodes `nextPosition` would, in the same order.
    mutating func nextRun() throws -> TreeRun? {
        guard !finished else { return nil }
        if !started {
            started = true
            guard let tree, let root = tree.root else { finish(); return nil }
            if let lower, let upper,
               upper.key.lexicographicallyPrecedes(lower.key) ||
                (upper.key == lower.key && (!upper.inclusive || !lower.inclusive)) {
                finish(); return nil
            }
            try descend(root, seek: descending ? upper : lower, tree: tree)
        }
        while let leaf {
            let entries = leaf.entryCount
            if row >= 0, row < entries {
                if descending {
                    var stop = 0
                    if let lower {
                        let slot = leaf.leafSlot(lower.key)
                        stop = slot.index + (slot.found && !lower.inclusive ? 1 : 0)
                    }
                    guard stop <= row else { finish(); return nil }
                    let run = TreeRun(node: leaf, first: row, count: row - stop + 1, descending: true)
                    // A bound inside this leaf ends the scan; otherwise the
                    // next call moves to the preceding leaf.
                    if stop > 0 { finish() } else { row = -1 }
                    return run
                }
                var end = entries
                if let upper {
                    let slot = leaf.leafSlot(upper.key)
                    end = slot.index + (slot.found && upper.inclusive ? 1 : 0)
                }
                guard end > row else { finish(); return nil }
                let run = TreeRun(node: leaf, first: row, count: end - row, descending: false)
                if end < entries { finish() } else { row = entries }
                return run
            }
            guard let tree else { finish(); return nil }
            try advanceLeaf(tree)
        }
        finish(); return nil
    }

    mutating func rewind() {
        guard tree != nil else { return }
        stack.removeAll(keepingCapacity: true); leaf = nil; row = 0
        started = false; finished = false; visitedNodes = 0; hintWindow = 1
    }

    mutating func close() {
        finish(); tree = nil; stack.removeAll(keepingCapacity: false)
    }

    private mutating func finish() { leaf = nil; stack.removeAll(keepingCapacity: true); finished = true }

    private mutating func descend(_ first: TreeChild, seek: TreeBound?, tree: BPlusTree) throws {
        var child = first
        while true {
            let node = try tree.resolve(child); visitedNodes += 1
            if node.isLeaf {
                leaf = node
                if let seek {
                    let slot = node.leafSlot(seek.key)
                    if descending { row = slot.index - (slot.found && seek.inclusive ? 0 : 1) }
                    else { row = slot.index + (slot.found && !seek.inclusive ? 1 : 0) }
                } else { row = descending ? node.entryCount - 1 : 0 }
                return
            }
            let index = seek.map { node.childSlot($0.key) } ?? (descending ? node.children.count - 1 : 0)
            hint(node, index: index, tree: tree)
            stack.append(Frame(node: node, child: index)); child = node.children[index]
        }
    }

    private mutating func hint(_ node: TreeNode, index: Int, tree: BPlusTree) {
        guard let pool = tree.pool, pool.readAheadEnabled,
              case .onDisk = node.children[index] else { return }
        var references: [PageReference] = [], next = index
        while node.children.indices.contains(next), references.count < hintWindow {
            if let lower, next < node.keys.count, !lower.key.lexicographicallyPrecedes(node.keys[next]) { break }
            if let upper, next > 0 {
                let boundary = node.keys[next - 1]
                if upper.key.lexicographicallyPrecedes(boundary) || (boundary == upper.key && !upper.inclusive) { break }
            }
            guard case .onDisk(let reference, _, _) = node.children[next] else { break }
            references.append(reference); next += descending ? -1 : 1
        }
        pool.prefetch(references, types: tree.types)
        hintWindow = min(64, hintWindow * 2)
    }

    private mutating func advanceLeaf(_ tree: BPlusTree) throws {
        leaf = nil
        while var frame = stack.popLast() {
            frame.child += descending ? -1 : 1
            if frame.child >= 0, frame.child < frame.node.children.count {
                hint(frame.node, index: frame.child, tree: tree)
                stack.append(frame)
                try descend(frame.node.children[frame.child], seek: nil, tree: tree)
                return
            }
        }
    }
}
