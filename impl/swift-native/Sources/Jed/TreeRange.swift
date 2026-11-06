internal extension BPlusTree {
    /// Logical nodes overlapped by a bounded traversal. Only resident routing
    /// metadata is consulted: an evicted leaf contributes one without a read.
    /// Middle children contribute cached subtree totals, so a span needs only
    /// its two boundary descents rather than a walk through the whole range.
    func overlapNodeCount(lower: TreeBound? = nil, upper: TreeBound? = nil) -> Int {
        func window(_ keys: [[UInt8]], value: [UInt8], inclusive: Bool) -> Int {
            var low = 0, high = keys.count
            while low < high {
                let middle = low + (high - low) / 2
                let before = keys[middle].lexicographicallyPrecedes(value) || (inclusive && keys[middle] == value)
                if before { low = middle + 1 } else { high = middle }
            }
            return low
        }
        func count(_ child: TreeChild, lower: TreeBound?, upper: TreeBound?) -> Int {
            if lower == nil && upper == nil { return child.nodeCount }
            guard case .resident(let node) = child, !node.isLeaf else { return 1 }
            // Equality always routes right at a separator. Only the upper
            // endpoint's inclusivity changes the final child's window.
            let first = lower.map { window(node.keys, value: $0.key, inclusive: true) } ?? 0
            let last = upper.map { window(node.keys, value: $0.key, inclusive: $0.inclusive) } ?? node.keys.count
            // A contradictory bound can exclude every child while retaining
            // the cost of visiting this interior node.
            guard first <= last else { return 1 }
            var total = 1
            for index in first...last {
                total += count(node.children[index], lower: index == first ? lower : nil, upper: index == last ? upper : nil)
            }
            return total
        }
        return root.map { count($0, lower: lower, upper: upper) } ?? 0
    }
}
