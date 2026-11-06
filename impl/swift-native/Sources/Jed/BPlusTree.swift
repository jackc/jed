/// A child is either an immutable resident node or a clean leaf address. The
/// bounds survive lazy loading so a misdirected leaf fails before yielding rows.
internal enum TreeChild: Sendable {
    case resident(TreeNode)
    case onDisk(PageReference, lower: [UInt8]?, upper: [UInt8]?)

    var height: Int { if case .resident(let node) = self { return node.height }; return 1 }
    var nodeCount: Int { if case .resident(let node) = self { return node.nodeCount }; return 1 }
    var containsRecords: Bool { if case .resident(let node) = self { return node.containsRecords }; return true }
    var reference: PageReference? {
        switch self { case .resident(let node): node.reference; case .onDisk(let reference, _, _): reference }
    }
}

/// Records reside in packed bytes, including dirty nodes. Decoded records are
/// bounded writer scratch. Fresh overflow bytes live beside their placeholder
/// cells until serialization, indexed by row and then declared column ordinal.
internal final class TreeNode: Sendable {
    let leaf: PackedLeaf?
    let pending: [Int: [Int: PendingOverflow]]
    let keys: [[UInt8]]
    let children: [TreeChild]
    let reference: PageReference?
    let payloadSize: Int
    let height: Int
    let nodeCount: Int
    let containsRecords: Bool

    var isLeaf: Bool { children.isEmpty }
    var entryCount: Int { isLeaf ? leaf?.count ?? 0 : keys.count }

    init(leaf: PackedLeaf?, pending: [Int: [Int: PendingOverflow]] = [:], reference: PageReference? = nil,
         emptyPayload: Int = 0) {
        self.leaf = leaf; self.pending = pending; self.reference = reference
        keys = []; children = []; payloadSize = leaf?.payloadSize ?? emptyPayload
        height = 1; nodeCount = 1
        containsRecords = leaf != nil
    }

    init(keys: [[UInt8]], children: [TreeChild], codec: any PageCodec, reference: PageReference? = nil) {
        leaf = nil; pending = [:]; self.keys = keys; self.children = children; self.reference = reference
        payloadSize = PackedInterior.payloadSize(keys: keys, codec: codec)
        height = 1 + (children.first?.height ?? 0)
        nodeCount = children.reduce(1) { $0 + $1.nodeCount }
        containsRecords = children.contains { $0.containsRecords }
    }

    func record(at index: Int) -> PackedRecord {
        let record = leaf!.record(at: index)
        return PackedRecord(key: record.key, cells: record.cells, overflows: pending[index] ?? [:])
    }

    func records() -> [PackedRecord] { (0..<entryCount).map { record(at: $0) } }

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
            if leaf!.key(at: middle).lexicographicallyPrecedes(key) { lower = middle + 1 }
            else { upper = middle }
        }
        return (lower, lower < entryCount && leaf!.key(at: lower).elementsEqual(key))
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
internal struct BPlusTree: Sendable {
    static let maximumDepth = 256
    let codec: any PageCodec
    let types: [TypeDescriptor]
    let pool: NodePool?
    private(set) var root: TreeChild?
    private(set) var count: Int64?

    init(codec: any PageCodec, types: [TypeDescriptor], pool: NodePool? = nil) {
        self.codec = codec; self.types = types; self.pool = pool; root = nil; count = 0
    }

    var nodeCount: Int { root?.nodeCount ?? 0 }
    var height: Int { root?.height ?? 0 }
    var isEmpty: Bool { root == nil }

    static func open(root reference: PageReference, rowCount: Int64? = nil, pageCount: UInt32,
                     types: [TypeDescriptor], pool: NodePool) throws -> BPlusTree {
        var tree = BPlusTree(codec: pool.codec, types: types, pool: pool)
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
            let leaf = try pool.load(reference, types: types)
            try Self.checkBounds(Array(leaf.key(at: 0)), Array(leaf.key(at: leaf.count - 1)), lower: lower, upper: upper)
            return TreeNode(leaf: leaf, reference: reference)
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
        guard record.cells.map(\.type) == types else {
            throw JedError("0A000", "record differs from its tree's type contract")
        }
        guard count != Int64.max else { throw JedError("54000", "tree row count exhausted") }
        let result: Rebuild
        var previous: PackedRecord?
        if let root { result = try inserting(resolve(root), record: record, previous: &previous) }
        else { result = .whole(try makeLeaf([record])) }
        let newRoot: TreeNode
        switch result {
        case .whole(let node): newRoot = node
        case .split(let left, let separator, let right):
            newRoot = TreeNode(keys: [separator], children: [.resident(left), .resident(right)], codec: codec)
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
        case split(TreeNode, [UInt8], TreeNode)
    }

    private func makeLeaf(_ records: [PackedRecord], reference: PageReference? = nil) throws -> TreeNode {
        if records.isEmpty {
            return TreeNode(leaf: nil, emptyPayload: PackedLeaf.overhead(records: 0, types: types))
        }
        let leaf = try PackedLeaf.pending(records, types: types, codec: codec)
        var pending: [Int: [Int: PendingOverflow]] = [:]
        for (row, record) in records.enumerated() where !record.overflows.isEmpty { pending[row] = record.overflows }
        return TreeNode(leaf: leaf, pending: pending, reference: reference)
    }

    /// Exact format.md fan-out selection. nil is the first delete abandon guard.
    static func splitPoint(range: ClosedRange<Int>, payload: Int, capacity: Int, rightEdge: Bool,
                           left: (Int) -> Int, right: (Int) -> Int) -> Int? {
        guard let minimum = range.first(where: { right($0) <= capacity }),
              let maximum = range.reversed().first(where: { left($0) <= capacity }), minimum <= maximum else { return nil }
        if rightEdge { return maximum }
        let balanced = range.first(where: { 2 * left($0) >= payload }) ?? maximum
        return max(minimum, min(balanced, maximum))
    }

    private func buildLeaf(_ records: [PackedRecord], edited: Int?) throws -> Rebuild {
        var prefix = [0]
        for record in records { prefix.append(prefix.last! + record.weight) }
        let n = records.count, total = prefix[n]
        let payload = total + PackedLeaf.overhead(records: n, types: types)
        if payload <= codec.capacity { return .whole(try makeLeaf(records)) }
        guard n >= 2, let middle = Self.splitPoint(range: 1...(n - 1), payload: payload,
            capacity: codec.capacity, rightEdge: edited == n - 1,
            left: { prefix[$0] + PackedLeaf.overhead(records: $0, types: types) },
            right: { total - prefix[$0] + PackedLeaf.overhead(records: n - $0, types: types) }) else {
            return .whole(try makeLeaf(records))
        }
        return .split(try makeLeaf(Array(records[..<middle])), records[middle].key,
                      try makeLeaf(Array(records[middle...])))
    }

    private func buildInterior(_ keys: [[UInt8]], children: [TreeChild], edited: Int?) -> Rebuild? {
        let n = keys.count, payload = PackedInterior.payloadSize(keys: keys, codec: codec)
        if payload <= codec.capacity || n < 2 { return .whole(TreeNode(keys: keys, children: children, codec: codec)) }
        let middle: Int
        if n == 2 { middle = 1 }
        else {
            var prefix = [0]
            for key in keys { prefix.append(prefix.last! + key.count) }
            let width = PackedInterior.pointerWidth(codec)
            guard let split = Self.splitPoint(range: 1...(n - 2), payload: payload, capacity: codec.capacity,
                rightEdge: edited == n - 1,
                left: { (width + 4) * $0 + width + prefix[$0] },
                right: { (width + 4) * (n - 1 - $0) + width + prefix[n] - prefix[$0 + 1] }) else { return nil }
            middle = split
        }
        let left = TreeNode(keys: Array(keys[..<middle]), children: Array(children[...middle]), codec: codec)
        let right = TreeNode(keys: Array(keys[(middle + 1)...]), children: Array(children[(middle + 1)...]), codec: codec)
        return .split(left, keys[middle], right)
    }

    private func inserting(_ node: TreeNode, record: PackedRecord, previous: inout PackedRecord?) throws -> Rebuild {
        if node.isLeaf {
            var records = node.records()
            let slot = node.leafSlot(record.key)
            if slot.found { previous = records[slot.index]; records[slot.index] = record }
            else { records.insert(record, at: slot.index) }
            return try buildLeaf(records, edited: slot.index)
        }
        let index = node.childSlot(record.key)
        let result = try inserting(resolve(node.children[index]), record: record, previous: &previous)
        var keys = node.keys, children = node.children
        switch result {
        case .whole(let child): children[index] = .resident(child)
        case .split(let left, let separator, let right):
            keys.insert(separator, at: index); children[index] = .resident(left)
            children.insert(.resident(right), at: index + 1)
        }
        // No admissible interior split is normally only a delete-merge case.
        // An already oversized pending separator may also reach it: preserve
        // the pending shape and let commit preflight reject it safely.
        return buildInterior(keys, children: children, edited: index)
            ?? .whole(TreeNode(keys: keys, children: children, codec: codec))
    }

    private func removing(_ node: TreeNode, key: [UInt8]) throws -> (TreeNode, PackedRecord?) {
        if node.isLeaf {
            let slot = node.leafSlot(key)
            guard slot.found else { return (node, nil) }
            var records = node.records()
            let old = records.remove(at: slot.index)
            return (try makeLeaf(records), old)
        }
        let index = node.childSlot(key)
        let (child, old) = try removing(resolve(node.children[index]), key: key)
        guard old != nil else { return (node, nil) }
        var children = node.children; children[index] = .resident(child)
        // A zero-key interior has no sibling for its sole leaf. If that
        // descendant drains, carry the empty unary subtree upward until a
        // parent can remove its edge. Pulling a separator down into it would
        // otherwise retain an empty leaf after the degenerate split.
        if !child.isLeaf, !child.containsRecords, children.count > 1 {
            var keys = node.keys
            children.remove(at: index)
            keys.remove(at: min(index, keys.count - 1))
            return (TreeNode(keys: keys, children: children, codec: codec), old)
        }
        let parent = TreeNode(keys: node.keys, children: children, codec: codec)
        guard child.payloadSize < codec.capacity / 2, children.count > 1 else { return (parent, old) }
        let pair = index + 1 < children.count ? index : index - 1
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
                children: left.children + right.children, edited: nil) else { return parent }
            result = merged
        }
        var keys = parent.keys, children = parent.children
        switch result {
        case .whole(let merged):
            keys.remove(at: index); children[index] = .resident(merged); children.remove(at: index + 1)
        case .split(let newLeft, let separator, let newRight):
            // A longer replacement separator cannot force a delete-path split.
            guard parent.payloadSize - keys[index].count + separator.count <= codec.capacity else { return parent }
            keys[index] = separator; children[index] = .resident(newLeft); children[index + 1] = .resident(newRight)
        }
        return TreeNode(keys: keys, children: children, codec: codec)
    }

    /// A mixed resident root is re-faulted after persistence when any unchanged
    /// external value was inherited from disk. That converts the entire leaf
    /// to stored references, including fresh values serialized beside it.
    /// A root containing only resident values keeps those payloads instead.
    mutating func refaultRoot(afterWriting written: Set<UInt32>,
                             read: (PageReference, [TypeDescriptor]) throws -> PackedLeaf) throws {
        guard pool != nil, case .resident(let node) = root, node.isLeaf,
              let reference = node.reference, written.contains(reference.index) else { return }
        for row in 0..<node.entryCount {
            let record = node.record(at: row)
            for column in record.cells.indices where record.overflows[column] == nil {
                if try record.cells[column].largeFacts(encrypted: codec.reservedBytes != 0)?.external != nil {
                    let leaf = try read(reference, types)
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
        // tree retains packed leaves until the destination binds its own pool;
        // never send its newly assigned ids to the source's cache/device.
        var result = fromScratch ? BPlusTree(codec: codec, types: types) : self
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
            payload = try PackedLeaf.encodePayload(records, types: types, capacity: codec.capacity)
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
