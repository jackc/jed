/// A child is either an immutable resident node or a clean leaf address. The
/// bounds survive lazy loading so a misdirected leaf fails before yielding rows.
/// Every write copies the child array of each interior node on its path, so
/// a child must cost one retain to copy: the clean-leaf payload is boxed.
internal enum TreeChild: Sendable {
    case resident(TreeNode)
    indirect case onDisk(PageReference, lower: [UInt8]?, upper: [UInt8]?)

    var height: Int { if case .resident(let node) = self { return node.height }; return 1 }
    var nodeCount: Int { if case .resident(let node) = self { return node.nodeCount }; return 1 }
    var containsRecords: Bool { if case .resident(let node) = self { return node.containsRecords }; return true }
    var reference: PageReference? {
        switch self { case .resident(let node): node.reference; case .onDisk(let reference, _, _): reference }
    }
}

/// Identity of one batch of tree edits (see `BPlusTree.beginBatch`).
internal final class TreeEditToken: Sendable {}

/// Clean leaves borrow packed bytes; writer leaves retain immutable stored
/// records until serialization. Fresh overflow bytes live beside their
/// placeholder cells, indexed by row and then declared column ordinal.
///
/// A node is immutable once any snapshot can reach it. The one exception is
/// an interior node created inside an edit batch: until that batch ends only
/// the batch's own tree reaches it, and the batch may update its routing
/// fields in place instead of copying its whole fan-out for every row. The
/// class is therefore Sendable by that discipline, not by construction.
internal final class TreeNode: @unchecked Sendable {
    let leaf: PackedLeaf?
    let recordLeaf: RecordLeaf?
    let pending: [Int: [Int: PendingOverflow]]
    fileprivate(set) var keys: [[UInt8]]
    fileprivate(set) var children: ChildList
    let reference: PageReference?
    fileprivate(set) var payloadSize: Int
    let height: Int
    fileprivate(set) var nodeCount: Int
    fileprivate(set) var containsRecords: Bool
    /// The batch that created this interior node, while it may still edit it.
    fileprivate let owner: TreeEditToken?

    var isLeaf: Bool { children.isEmpty }
    var entryCount: Int { isLeaf ? leaf?.count ?? recordLeaf?.records.count ?? 0 : keys.count }

    init(leaf: PackedLeaf?, pending: [Int: [Int: PendingOverflow]] = [:], reference: PageReference? = nil,
         emptyPayload: Int = 0) {
        self.leaf = leaf; self.pending = pending; self.reference = reference
        recordLeaf = nil; owner = nil
        keys = []; children = ChildList(); payloadSize = leaf?.payloadSize ?? emptyPayload
        height = 1; nodeCount = 1
        containsRecords = leaf != nil
    }

    init(records: RecordLeaf, reference: PageReference? = nil) {
        leaf = nil; recordLeaf = records; pending = records.pending; self.reference = reference; owner = nil
        keys = []; children = ChildList(); payloadSize = records.payloadSize
        height = 1; nodeCount = 1; containsRecords = !records.records.isEmpty
    }

    convenience init(keys: [[UInt8]], children: [TreeChild], codec: any PageCodec, reference: PageReference? = nil,
                     owner: TreeEditToken? = nil) {
        self.init(keys: keys, children: ChildList(children), codec: codec, reference: reference, owner: owner)
    }

    init(keys: [[UInt8]], children: ChildList, codec: any PageCodec, reference: PageReference? = nil,
         owner: TreeEditToken? = nil) {
        leaf = nil; recordLeaf = nil; pending = [:]; self.keys = keys; self.children = children; self.reference = reference
        self.owner = owner
        payloadSize = PackedInterior.payloadSize(keys: keys, codec: codec)
        height = 1 + (children.first?.height ?? 0)
        nodeCount = children.reduce(1) { $0 + $1.nodeCount }
        containsRecords = children.contains { $0.containsRecords }
    }

    /// Column descriptors of a non-empty leaf.
    var types: [TypeDescriptor] { leaf?.types ?? recordLeaf?.types ?? [] }

    /// Same keys with one child replaced: sizes follow from the old node, so
    /// nothing is re-measured across the whole fan-out.
    /// `replacedCount` is the node count of the child being replaced, as it
    /// was when `node` last counted it.
    init(replacing index: Int, in node: TreeNode, with child: TreeNode, replacedCount: Int,
         owner: TreeEditToken? = nil) {
        leaf = nil; recordLeaf = nil; pending = [:]; keys = node.keys; reference = nil; self.owner = owner
        var children = node.children
        children[index] = .resident(child)
        self.children = children
        payloadSize = node.payloadSize; height = node.height
        nodeCount = node.nodeCount - replacedCount + child.nodeCount
        containsRecords = child.containsRecords || (node.containsRecords && children.contains { $0.containsRecords })
    }

    /// `init(replacing:in:with:)` applied to this node itself. Only the batch
    /// that owns the node calls it.
    /// `child` may be the resident child itself, already edited in place, so
    /// its former count comes from the caller and not from the slot.
    fileprivate func replaceChild(at index: Int, with child: TreeNode, replacedCount: Int) {
        children[index] = .resident(child)
        nodeCount += child.nodeCount - replacedCount
        containsRecords = child.containsRecords || (containsRecords && children.contains { $0.containsRecords })
    }

    /// Hands this node's routing arrays to its owning batch, leaving them
    /// unshared so the batch can edit them without a copy.
    fileprivate func takeRouting() -> (keys: [[UInt8]], children: ChildList) {
        let taken = (keys, children)
        keys = []; children = ChildList()
        return taken
    }

    /// `init(keys:children:codec:)` applied to this node itself: the same
    /// derived sizes. Siblings have equal depth, so the height stands.
    fileprivate func setRouting(keys: [[UInt8]], children: ChildList, codec: any PageCodec) {
        self.keys = keys; self.children = children
        payloadSize = PackedInterior.payloadSize(keys: keys, codec: codec)
        nodeCount = children.reduce(1) { $0 + $1.nodeCount }
        containsRecords = children.contains { $0.containsRecords }
    }

    /// The writer-record form of this leaf, or nil for an empty placeholder.
    func writerLeaf() -> RecordLeaf? {
        if let recordLeaf { return recordLeaf }
        return leaf.map { RecordLeaf(adopting: $0, pending: pending) }
    }

    func record(at index: Int) -> PackedRecord {
        if let recordLeaf { return recordLeaf.records[index] }
        let record = leaf!.record(at: index)
        return PackedRecord(key: record.key, cells: record.cells, overflows: pending[index] ?? [:])
    }

    func records() -> [PackedRecord] { recordLeaf?.records.flat ?? (0..<entryCount).map { record(at: $0) } }

    func key(at index: Int) -> ArraySlice<UInt8> {
        if let leaf { return leaf.key(at: index) }
        return recordLeaf!.records[index].key[...]
    }

    func cell(row: Int, column: Int) -> StoredCell {
        if let leaf { return leaf.cell(row: row, column: column) }
        return recordLeaf!.records[row].cells[column]
    }

    func isNull(row: Int, column: Int) -> Bool {
        if let leaf { return leaf.isNull(row: row, column: column) }
        return recordLeaf!.records[row].cells[column].isNull
    }

    /// Negative when the stored key at `index` sorts before `key`.
    func compareKey(at index: Int, with key: [UInt8]) -> Int {
        if let leaf { return leaf.compareKey(at: index, with: key) }
        let stored = recordLeaf!.records[index].key
        if stored == key { return 0 }
        return stored.lexicographicallyPrecedes(key) ? -1 : 1
    }

    func childSlot(_ key: [UInt8]) -> Int {
        var lower = 0, upper = keys.count
        while lower < upper {
            let middle = lower + (upper - lower) / 2
            if key.lexicographicallyPrecedes(keys[middle]) { upper = middle }
            else { lower = middle + 1 }
        }
        return lower
    }

    func leafSlot(_ key: [UInt8]) -> (index: Int, found: Bool) {
        var lower = 0, upper = entryCount
        while lower < upper {
            let middle = lower + (upper - lower) / 2
            if compareKey(at: middle, with: key) < 0 { lower = middle + 1 }
            else { upper = middle }
        }
        return (lower, lower < entryCount && compareKey(at: lower, with: key) == 0)
    }
}

internal struct TreeWrite: Sendable {
    let tree: BPlusTree
    let root: PageReference
    let pages: [StoredPage]
    /// Clean subtree boundaries, not their transitive closure.
    let keptRoots: Set<UInt32>
    /// Heads of inherited chains in freshly written leaves.
    let keptOverflowHeads: [PageReference]
}

/// Value snapshots share immutable nodes. Every mutation builds a replacement
/// path before publishing root/count, so a failure leaves this tree unchanged.
///
/// Inside an edit batch the replacement path reuses interior nodes the batch
/// itself created. The steps, and so the resulting tree, are exactly those of
/// the unbatched edits. What is given up is the failure guarantee for a
/// removal: an I/O or corruption error raised while rebalancing can leave the
/// batch's tree half edited, so a batch whose edit throws must be discarded.
internal struct BPlusTree: Sendable {
    static let maximumDepth = 256
    let codec: any PageCodec
    let types: [TypeDescriptor]
    let layout: LeafLayout
    let pool: NodePool?
    private(set) var root: TreeChild?
    private(set) var count: Int64?
    private var editToken: TreeEditToken?

    /// Starts a run of edits applied to this value alone. The caller must not
    /// copy the tree, or read it through another copy, until `endBatch`.
    mutating func beginBatch() { editToken = TreeEditToken() }
    mutating func endBatch() { editToken = nil }

    /// `replacedCount` must be read before the child is edited: within a
    /// batch the edit may update that same child object in place.
    private func replacing(_ index: Int, in node: TreeNode, with child: TreeNode, replacedCount: Int) -> TreeNode {
        if let editToken, node.owner === editToken {
            node.replaceChild(at: index, with: child, replacedCount: replacedCount)
            return node
        }
        return TreeNode(replacing: index, in: node, with: child, replacedCount: replacedCount, owner: editToken)
    }

    init(codec: any PageCodec, types: [TypeDescriptor], pool: NodePool? = nil, layout: LeafLayout = .index) {
        self.codec = codec; self.types = types; self.layout = layout; self.pool = pool; root = nil; count = 0
    }

    var nodeCount: Int { root?.nodeCount ?? 0 }
    var height: Int { root?.height ?? 0 }
    var isEmpty: Bool { root == nil }

    static func open(root reference: PageReference, rowCount: Int64? = nil, pageCount: UInt32,
                     types: [TypeDescriptor], pool: NodePool, layout: LeafLayout = .index) throws -> BPlusTree {
        var tree = BPlusTree(codec: pool.codec, types: types, pool: pool, layout: layout)
        guard rowCount.map({ $0 >= 0 && (($0 == 0) == (reference.index == 0)) }) ?? true else {
            throw JedError.corruption("tree root disagrees with row count")
        }
        tree.count = rowCount
        if reference.index == 0 {
            guard reference.tag.allSatisfy({ $0 == 0 }) else { throw JedError.corruption("nonzero empty-tree tag") }
            return tree
        }
        var seen: Set<UInt32> = []
        tree.root = try tree.loadSkeleton(reference, pageCount: pageCount, lower: nil, upper: nil,
                                         depth: 1, seen: &seen)
        return tree
    }

    private func loadSkeleton(_ reference: PageReference, pageCount: UInt32, lower: [UInt8]?, upper: [UInt8]?,
                              depth: Int, seen: inout Set<UInt32>) throws -> TreeChild {
        guard depth <= Self.maximumDepth, reference.index >= codec.firstBodyPage,
              reference.index < pageCount, seen.insert(reference.index).inserted else {
            throw JedError.corruption("invalid, repeated, or excessively deep tree page")
        }
        let bytes = try pool!.readBody(reference)
        guard bytes.count == codec.pageSize - codec.reservedBytes else { throw JedError.corruption("invalid tree page size") }
        if bytes[0] == 2 {
            // Classify only. PAX directories and values stay deferred until fault.
            return .onDisk(reference, lower: lower, upper: upper)
        }
        let packed = try PackedInterior(bytes: bytes, codec: codec, pageCount: pageCount)
        try Self.checkBounds(packed.keys.first, packed.keys.last, lower: lower, upper: upper)
        var children: [TreeChild] = []
        let first = try loadSkeleton(packed.children[0], pageCount: pageCount, lower: lower,
            upper: packed.keys.first ?? upper, depth: depth + 1, seen: &seen)
        children.append(first)
        for index in 1..<packed.children.count {
            let low = packed.keys[index - 1], high = index < packed.keys.count ? packed.keys[index] : upper
            if first.height == 1 {
                let child = packed.children[index]
                guard seen.insert(child.index).inserted else { throw JedError.corruption("tree page reached twice") }
                children.append(.onDisk(child, lower: low, upper: high))
            } else {
                let child = try loadSkeleton(packed.children[index], pageCount: pageCount,
                    lower: low, upper: high, depth: depth + 1, seen: &seen)
                guard child.height == first.height else { throw JedError.corruption("tree leaves have unequal depth") }
                children.append(child)
            }
        }
        return .resident(TreeNode(keys: packed.keys, children: children, codec: codec, reference: reference))
    }

    static func checkBounds(_ first: [UInt8]?, _ last: [UInt8]?, lower: [UInt8]?, upper: [UInt8]?) throws {
        if let first, let lower, first.lexicographicallyPrecedes(lower) {
            throw JedError.corruption("tree key precedes its parent bound")
        }
        if let last, let upper, !last.lexicographicallyPrecedes(upper) {
            throw JedError.corruption("tree key exceeds its parent bound")
        }
    }

    func resolve(_ child: TreeChild) throws -> TreeNode {
        switch child {
        case .resident(let node): return node
        case .onDisk(let reference, let lower, let upper):
            guard let pool else { throw JedError.corruption("tree leaf has no paging source") }
            let leaf = try pool.load(reference, types: types, layout: layout)
            // The same two checks as `checkBounds`, against the page in place.
            if let lower, leaf.compareKey(at: 0, with: lower) < 0 {
                throw JedError.corruption("tree key precedes its parent bound")
            }
            if let upper, leaf.compareKey(at: leaf.count - 1, with: upper) >= 0 {
                throw JedError.corruption("tree key exceeds its parent bound")
            }
            return TreeNode(leaf: leaf, reference: reference)
        }
    }

    /// Where `key` is stored, by the same descent as `get` without building
    /// its record, together with the number of nodes on the path. That
    /// count is `overlapNodeCount` for the one-key interval `[key, key]`:
    /// equality routes right at a separator in both walks.
    func probe(_ key: [UInt8]) throws -> (nodes: Int, position: TreePosition?) {
        guard var child = root else { return (0, nil) }
        var nodes = 0
        while true {
            let node = try resolve(child)
            nodes += 1
            if node.isLeaf {
                let slot = node.leafSlot(key)
                return (nodes, slot.found ? TreePosition(node: node, row: slot.index) : nil)
            }
            child = node.children[node.childSlot(key)]
        }
    }

    func get(_ key: [UInt8]) throws -> PackedRecord? {
        guard var child = root else { return nil }
        while true {
            let node = try resolve(child)
            if node.isLeaf {
                let slot = node.leafSlot(key)
                return slot.found ? node.record(at: slot.index) : nil
            }
            child = node.children[node.childSlot(key)]
        }
    }

    @discardableResult
    mutating func insert(_ record: PackedRecord) throws -> PackedRecord? {
        // Page fit is a commit-preflight condition. Keep an unsplittable
        // pending node so its staged record still participates in Q3 admission.
        // Cells carry no descriptor, so the tree's own metadata is always the
        // decode contract. Admission checks the incoming record's shape once.
        let canonical = try RecordLeaf.admit(record, types: types)
        guard count != Int64.max else { throw JedError("54000", "tree row count exhausted") }
        let result: Rebuild
        var previous: PackedRecord?
        if let root { result = try inserting(resolve(root), record: canonical, previous: &previous) }
        else { result = .whole(try makeLeaf([canonical])) }
        let newRoot: TreeNode
        switch result {
        case .whole(let node): newRoot = node
        case .split(let nodes, let separators):
            var grown = buildInterior(separators, children: nodes.map(TreeChild.resident), edited: nil)
                ?? .whole(TreeNode(keys: separators, children: nodes.map(TreeChild.resident), codec: codec, owner: editToken))
            while case .split(let nodes, let separators) = grown {
                grown = buildInterior(separators, children: nodes.map(TreeChild.resident), edited: nil)
                    ?? .whole(TreeNode(keys: separators, children: nodes.map(TreeChild.resident), codec: codec, owner: editToken))
            }
            guard case .whole(let node) = grown else { throw JedError("XX000", "invalid tree rebuild") }
            newRoot = node
        }
        guard newRoot.height <= Self.maximumDepth else { throw JedError("54000", "tree nesting limit exceeded") }
        root = .resident(newRoot)
        if previous == nil, let count { self.count = count + 1 }
        return previous
    }

    @discardableResult
    mutating func remove(_ key: [UInt8]) throws -> PackedRecord? {
        guard let root else { return nil }
        let (rebuilt, removed) = try removing(resolve(root), key: key)
        guard let removed else { return nil }
        var replacement: TreeChild? = .resident(rebuilt)
        // Repeated collapse also handles an already-degenerate root chain.
        while let child = replacement {
            let node = try resolve(child)
            if node.isLeaf { if node.entryCount == 0 { replacement = nil }; break }
            if node.keys.isEmpty { replacement = node.children[0] } else { break }
        }
        self.root = replacement
        if let count { self.count = count - 1 }
        return removed
    }

    private enum Rebuild {
        case whole(TreeNode)
        case split([TreeNode], [[UInt8]])
    }

    private func makeLeaf(_ records: [PackedRecord], reference: PageReference? = nil) throws -> TreeNode {
        TreeNode(records: try RecordLeaf(records, types: types, layout: layout), reference: reference)
    }

    /// Exact format.md fan-out selection. nil is the first delete abandon guard.
    static func splitPoint(range: ClosedRange<Int>, payload: Int, capacity: Int, rightEdge: Bool,
                           left: (Int) -> Int, right: (Int) -> Int) -> Int? {
        func first(_ predicate: (Int) -> Bool) -> Int? {
            var low = range.lowerBound, high = range.upperBound + 1
            while low < high {
                let middle = low + (high - low) / 2
                if predicate(middle) { high = middle } else { low = middle + 1 }
            }
            return range.contains(low) ? low : nil
        }
        guard let minimum = first({ right($0) <= capacity }) else { return nil }
        let maximum = (first({ left($0) > capacity }) ?? (range.upperBound + 1)) - 1
        guard minimum <= maximum else { return nil }
        if rightEdge { return maximum }
        let balanced = first({ 2 * left($0) >= payload }) ?? maximum
        return max(minimum, min(balanced, maximum))
    }

    private func buildLeaf(_ records: [PackedRecord], edited: Int?) throws -> Rebuild {
        let n = records.count
        let whole = try makeLeaf(records)
        if whole.payloadSize <= codec.capacity || n < 2 { return .whole(whole) }
        func measure(_ range: Range<Int>) -> Int { PackedLeaf.measure(Array(records[range]), types: types, layout: layout) }
        if let middle = Self.splitPoint(range: 1...(n - 1), payload: whole.payloadSize,
            capacity: codec.capacity, rightEdge: edited == n - 1,
            left: { measure(0..<$0) }, right: { measure($0..<n) }) {
            return .split([try makeLeaf(Array(records[..<middle])), try makeLeaf(Array(records[middle...]))], [records[middle].key])
        }
        var low = 1, high = n
        while low < high {
            let middle = low + (high - low) / 2
            if measure(0..<middle) <= codec.capacity { low = middle + 1 } else { high = middle }
        }
        let middle = low - 1
        guard middle > 0 else { return .whole(whole) }
        let left = try makeLeaf(Array(records[..<middle]))
        let remainder = try buildLeaf(Array(records[middle...]), edited: edited.flatMap { $0 >= middle ? $0 - middle : nil })
        switch remainder {
        case .whole(let right):
            guard right.payloadSize <= codec.capacity else { return .whole(whole) }
            return .split([left, right], [records[middle].key])
        case .split(let nodes, let separators): return .split([left] + nodes, [records[middle].key] + separators)
        }
    }

    private func buildInterior(_ keys: [[UInt8]], children: [TreeChild], edited: Int?, multiway: Bool = true) -> Rebuild? {
        let n = keys.count, payload = PackedInterior.payloadSize(keys: keys, codec: codec)
        if payload <= codec.capacity || n < 2 { return .whole(TreeNode(keys: keys, children: children, codec: codec, owner: editToken)) }
        let middle: Int
        if n == 2 { middle = 1 }
        else {
            var prefix = [0]
            for key in keys { prefix.append(prefix.last! + key.count) }
            let width = PackedInterior.pointerWidth(codec)
            guard let split = Self.splitPoint(range: 1...(n - 2), payload: payload, capacity: codec.capacity,
                rightEdge: edited == n - 1,
                left: { (width + 4) * $0 + width + prefix[$0] },
                right: { (width + 4) * (n - 1 - $0) + width + prefix[n] - prefix[$0 + 1] }) else {
                guard multiway, let m = (1...(n - 2)).reversed().first(where: { (width + 4) * $0 + width + prefix[$0] <= codec.capacity }),
                      let remainder = buildInterior(Array(keys[(m + 1)...]), children: Array(children[(m + 1)...]),
                        edited: edited.flatMap { $0 > m ? $0 - m - 1 : nil }) else { return nil }
                let left = TreeNode(keys: Array(keys[..<m]), children: Array(children[...m]), codec: codec, owner: editToken)
                switch remainder {
                case .whole(let right):
                    guard right.payloadSize <= codec.capacity else { return nil }
                    return .split([left, right], [keys[m]])
                case .split(let nodes, let separators): return .split([left] + nodes, [keys[m]] + separators)
                }
            }
            middle = split
        }
        let left = TreeNode(keys: Array(keys[..<middle]), children: Array(children[...middle]), codec: codec, owner: editToken)
        let right = TreeNode(keys: Array(keys[(middle + 1)...]), children: Array(children[(middle + 1)...]), codec: codec, owner: editToken)
        return .split([left, right], [keys[middle]])
    }

    private func inserting(_ node: TreeNode, record: PackedRecord, previous: inout PackedRecord?) throws -> Rebuild {
        if node.isLeaf {
            let slot = node.leafSlot(record.key)
            // One record changes: derive the new leaf from the old one rather
            // than re-admitting and re-measuring every record it holds.
            if let base = node.writerLeaf() {
                if slot.found { previous = base.records[slot.index] }
                let edited = try base.placing(record, at: slot.index, replacing: slot.found)
                if edited.payloadSize <= codec.capacity { return .whole(TreeNode(records: edited)) }
                return try buildLeaf(edited.records.flat, edited: slot.index)
            }
            var records = node.records()
            if slot.found { previous = records[slot.index]; records[slot.index] = record }
            else { records.insert(record, at: slot.index) }
            return try buildLeaf(records, edited: slot.index)
        }
        let index = node.childSlot(record.key)
        let replacedCount = node.children[index].nodeCount
        let result = try inserting(resolve(node.children[index]), record: record, previous: &previous)
        if case .whole(let child) = result {
            return .whole(replacing(index, in: node, with: child, replacedCount: replacedCount))
        }
        var keys = node.keys, children = node.children
        switch result {
        case .whole(let child): children[index] = .resident(child)
        case .split(let nodes, let separators):
            keys.insert(contentsOf: separators, at: index); children[index] = .resident(nodes[0])
            for ordinal in 1..<nodes.count { children.insert(.resident(nodes[ordinal]), at: index + ordinal) }
        }
        // No admissible interior split is normally only a delete-merge case.
        // An already oversized pending separator may also reach it: preserve
        // the pending shape and let commit preflight reject it safely.
        let lastInserted: Int
        if case .split(_, let separators) = result { lastInserted = index + separators.count - 1 } else { lastInserted = index }
        return buildInterior(keys, children: Array(children), edited: lastInserted)
            ?? .whole(TreeNode(keys: keys, children: children, codec: codec, owner: editToken))
    }

    private func removing(_ node: TreeNode, key: [UInt8]) throws -> (TreeNode, PackedRecord?) {
        if node.isLeaf {
            let slot = node.leafSlot(key)
            guard slot.found else { return (node, nil) }
            if let base = node.writerLeaf() {
                return (TreeNode(records: base.removing(at: slot.index)), base.records[slot.index])
            }
            var records = node.records()
            let old = records.remove(at: slot.index)
            return (try makeLeaf(records), old)
        }
        let index = node.childSlot(key)
        let replacedCount = node.children[index].nodeCount
        let (child, old) = try removing(resolve(node.children[index]), key: key)
        guard old != nil else { return (node, nil) }
        // A zero-key interior has no sibling for its sole leaf. If that
        // descendant drains, carry the empty unary subtree upward until a
        // parent can remove its edge. Pulling a separator down into it would
        // otherwise retain an empty leaf after the degenerate split.
        if !child.isLeaf, !child.containsRecords, node.children.count > 1 {
            var keys = node.keys, children = node.children
            children.remove(at: index)
            keys.remove(at: min(index, keys.count - 1))
            return (TreeNode(keys: keys, children: children, codec: codec, owner: editToken), old)
        }
        let parent = replacing(index, in: node, with: child, replacedCount: replacedCount)
        guard child.payloadSize < codec.capacity / 2, node.children.count > 1 else { return (parent, old) }
        let pair = index + 1 < node.children.count ? index : index - 1
        return (try merge(parent, at: pair), old)
    }

    private func merge(_ parent: TreeNode, at index: Int) throws -> TreeNode {
        let left = try resolve(parent.children[index]), right = try resolve(parent.children[index + 1])
        guard left.isLeaf == right.isLeaf, left.height == right.height else {
            throw JedError.corruption("tree siblings have unequal depth")
        }
        let result: Rebuild
        if left.isLeaf { result = try buildLeaf(left.records() + right.records(), edited: nil) }
        else {
            guard let merged = buildInterior(left.keys + [parent.keys[index]] + right.keys,
                children: Array(left.children) + Array(right.children), edited: nil, multiway: false) else { return parent }
            result = merged
        }
        if case .split(_, let separators) = result {
            guard separators.count == 1 else { return parent }
            let separator = separators[0]
            // A longer replacement separator cannot force a delete-path split.
            guard parent.payloadSize - parent.keys[index].count + separator.count <= codec.capacity else { return parent }
        }
        let owned = editToken != nil && parent.owner === editToken
        var (keys, children) = owned ? parent.takeRouting() : (parent.keys, parent.children)
        switch result {
        case .whole(let merged):
            keys.remove(at: index); children[index] = .resident(merged); children.remove(at: index + 1)
        case .split(let nodes, let separators):
            keys[index] = separators[0]; children[index] = .resident(nodes[0]); children[index + 1] = .resident(nodes[1])
        }
        if owned {
            parent.setRouting(keys: keys, children: children, codec: codec)
            return parent
        }
        return TreeNode(keys: keys, children: children, codec: codec, owner: editToken)
    }

    /// A mixed resident root is re-faulted after persistence when any unchanged
    /// external value was inherited from disk. That converts the entire leaf
    /// to stored references, including fresh values serialized beside it.
    /// A root containing only resident values keeps those payloads instead.
    mutating func refaultRoot(afterWriting written: Set<UInt32>,
                             read: (PageReference, [TypeDescriptor], LeafLayout) throws -> PackedLeaf) throws {
        guard pool != nil, case .resident(let node) = root, node.isLeaf,
              let reference = node.reference, written.contains(reference.index) else { return }
        for row in 0..<node.entryCount {
            let record = node.record(at: row)
            for column in record.cells.indices where record.overflows[column] == nil {
                if try record.cells[column].largeFacts(encrypted: codec.reservedBytes != 0)?.external != nil {
                    let leaf = try read(reference, types, layout)
                    root = .resident(TreeNode(leaf: leaf, reference: reference))
                    return
                }
            }
        }
    }

    /// Prepares bytes without device writes or mutation of any published node.
    /// Whole-image export renumbers and copies inherited chains; incremental
    /// commit retains clean subtrees and unchanged chain pointers.
    func serialize(allocator: inout PageAllocator, fromScratch: Bool = false,
                   entropy: KeyEntropy? = nil) throws -> TreeWrite {
        var allocation = allocator, pages: [StoredPage] = [], kept: Set<UInt32> = []
        var chains: [PageReference] = []
        // A whole-image export has a different page-address space. Its returned
        // tree retains encoded records until the destination binds its own pool;
        // never send its newly assigned ids to the source's cache/device.
        var result = fromScratch ? BPlusTree(codec: codec, types: types, layout: layout) : self
        result.count = count
        if let root {
            result.root = try serialize(root, isRoot: true, allocator: &allocation, fromScratch: fromScratch, entropy: entropy,
                pages: &pages, kept: &kept, chains: &chains)
        }
        allocator = allocation
        return TreeWrite(tree: result, root: result.root?.reference ?? PageSealer.nullReference(codec),
                         pages: pages, keptRoots: kept, keptOverflowHeads: chains)
    }

    private func serialize(_ child: TreeChild, isRoot: Bool = false, allocator: inout PageAllocator, fromScratch: Bool,
                           entropy: KeyEntropy?, pages: inout [StoredPage], kept: inout Set<UInt32>,
                           chains: inout [PageReference]) throws -> TreeChild {
        if !fromScratch, let reference = child.reference { kept.insert(reference.index); return child }
        let node = try resolve(child)
        var children: [TreeChild] = []
        for child in node.children {
            children.append(try serialize(child, allocator: &allocator, fromScratch: fromScratch, entropy: entropy,
                pages: &pages, kept: &kept, chains: &chains))
        }
        guard node.payloadSize <= codec.capacity else { throw JedError("0A000", "tree node exceeds page capacity") }
        // From-scratch: leaf's own id precedes its chains. Incremental: fresh
        // chains precede the leaf. Sealing always binds children/chains first.
        let earlyIndex = fromScratch ? try allocator.allocate() : nil
        let payload: [UInt8]
        var records: [PackedRecord] = [], overflowPages: [StoredPage] = []
        if node.isLeaf {
            guard node.entryCount > 0 else { throw JedError.corruption("cannot persist an empty leaf") }
            for row in 0..<node.entryCount {
                var record = node.record(at: row)
                var pending = record.overflows
                for column in types.indices where pending[column] == nil {
                    guard let facts = try record.cells[column].largeFacts(encrypted: codec.reservedBytes != 0),
                          let head = facts.external else { continue }
                    if fromScratch {
                        guard let pool else { throw JedError.corruption("inherited overflow has no paging source") }
                        let content = try OverflowChain.read(head, length: facts.stored,
                            pageCount: pool.pager?.selected.meta.pageCount ?? UInt32.max,
                            codec: pool.codec, read: pool.readBody)
                        pending[column] = PendingOverflow(bytes: content, rawLength: facts.raw, compressed: facts.compressed)
                    } else { chains.append(head) }
                }
                if !pending.isEmpty { record = PackedRecord(key: record.key, cells: record.cells, overflows: pending) }
                let sealed = try record.sealOverflow(allocator: &allocator, codec: codec, entropy: entropy)
                // The baseline root leaf stays resident. Preserve only payloads
                // already resident before this commit; inherited disk pointers
                // remain references. Non-root leaves shed these after sealing.
                let retained = isRoot && !fromScratch ? record.overflows : [:]
                records.append(PackedRecord(key: sealed.record.key, cells: sealed.record.cells, overflows: retained))
                overflowPages.append(contentsOf: sealed.pages)
            }
            payload = try PackedLeaf.encodePayload(records, types: types, capacity: codec.capacity, layout: layout)
        } else {
            payload = try PackedInterior.encodePayload(keys: node.keys, children: children.map { $0.reference! }, codec: codec)
        }
        let index = try earlyIndex ?? allocator.allocate()
        let sealed = try PageSealer.seal(codec: codec, index: index, type: node.isLeaf ? 2 : 3,
            count: UInt32(node.entryCount), payload: payload, entropy: entropy)
        if fromScratch { pages.append(sealed.page); pages.append(contentsOf: overflowPages) }
        else { pages.append(contentsOf: overflowPages); pages.append(sealed.page) }
        if node.isLeaf {
            if pool != nil, !fromScratch, !isRoot { return .onDisk(sealed.reference, lower: records.first?.key, upper: nil) }
            return .resident(try makeLeaf(records, reference: sealed.reference))
        }
        return .resident(TreeNode(keys: node.keys, children: children, codec: codec, reference: sealed.reference))
    }
}
