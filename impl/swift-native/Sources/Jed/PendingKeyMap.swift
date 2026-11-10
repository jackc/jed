/// A persistent hash trie from staged storage keys to their accounted bytes.
///
/// Every statement works on a private copy of its stores so that a failure
/// leaves the transaction's state untouched. A dictionary shared that way is
/// copied whole on its first edit, which made each single-row statement cost
/// time proportional to the rows already staged by its transaction. Here a
/// copy shares all nodes and an edit copies only the nodes on one path; nodes
/// that are not shared are edited in place.
///
/// Order is never observed: the map answers only exact lookups, so the
/// per-process seed of `hashValue` cannot reach results, cost or bytes.
internal struct PendingKeyMap: Sendable {
    private enum Slot {
        case entry([UInt8], Int64)
        case child(Node)
        /// Keys whose whole hash is equal, below the last trie level.
        case bucket([([UInt8], Int64)])
        /// Transient: a child is detached while it is edited, so that the
        /// parent's reference does not make it look shared.
        case detached
    }
    private final class Node: @unchecked Sendable {
        var bitmap: UInt32
        var slots: [Slot]
        init(bitmap: UInt32 = 0, slots: [Slot] = []) { self.bitmap = bitmap; self.slots = slots }
    }
    private static let bitsPerLevel = 5
    private static let lastShift = 60

    private var root = Node()
    private(set) var count = 0

    var isEmpty: Bool { count == 0 }

    mutating func removeAll() { root = Node(); count = 0 }

    /// Sets the value for `key` and returns the value it replaced.
    @discardableResult mutating func updateValue(_ value: Int64, forKey key: [UInt8]) -> Int64? {
        let old = Self.update(&root, key: key, hash: UInt64(bitPattern: Int64(key.hashValue)), shift: 0, value: value)
        if old == nil { count += 1 }
        return old
    }

    @discardableResult mutating func removeValue(forKey key: [UInt8]) -> Int64? {
        // Look first: an absent key must not copy a shared path.
        guard self[key] != nil else { return nil }
        let old = Self.remove(&root, key: key, hash: UInt64(bitPattern: Int64(key.hashValue)), shift: 0)
        if old != nil { count -= 1 }
        return old
    }

    subscript(key: [UInt8]) -> Int64? {
        let hash = UInt64(bitPattern: Int64(key.hashValue))
        var node = root, shift = 0
        while true {
            let bit = UInt32(1) << UInt32((hash >> UInt64(shift)) & 31)
            guard node.bitmap & bit != 0 else { return nil }
            switch node.slots[(node.bitmap & (bit - 1)).nonzeroBitCount] {
            case .entry(let existing, let value): return existing == key ? value : nil
            case .bucket(let entries): return entries.first { $0.0 == key }?.1
            case .child(let child): node = child; shift += Self.bitsPerLevel
            case .detached: return nil
            }
        }
    }

    private static func unique(_ node: inout Node) {
        if !isKnownUniquelyReferenced(&node) { node = Node(bitmap: node.bitmap, slots: node.slots) }
    }

    /// Returns the child at `index` as the only reference to it from this
    /// node, so an unshared child tests as unique and is edited in place.
    private static func detachChild(_ node: Node, at index: Int) -> Node? {
        guard case .child(let child) = node.slots[index] else { return nil }
        node.slots[index] = .detached
        return child
    }

    private static func update(_ node: inout Node, key: [UInt8], hash: UInt64, shift: Int, value: Int64) -> Int64? {
        unique(&node)
        let bit = UInt32(1) << UInt32((hash >> UInt64(shift)) & 31)
        let index = (node.bitmap & (bit - 1)).nonzeroBitCount
        guard node.bitmap & bit != 0 else {
            node.slots.insert(.entry(key, value), at: index); node.bitmap |= bit
            return nil
        }
        if var child = detachChild(node, at: index) {
            let old = update(&child, key: key, hash: hash, shift: shift + bitsPerLevel, value: value)
            node.slots[index] = .child(child)
            return old
        }
        switch node.slots[index] {
        case .entry(let existing, let old):
            if existing == key { node.slots[index] = .entry(key, value); return old }
            if shift >= lastShift {
                node.slots[index] = .bucket([(existing, old), (key, value)])
            } else {
                // Push the resident entry one level down, then add beside it.
                var child = Node()
                let next = shift + bitsPerLevel
                _ = update(&child, key: existing, hash: UInt64(bitPattern: Int64(existing.hashValue)), shift: next, value: old)
                _ = update(&child, key: key, hash: hash, shift: next, value: value)
                node.slots[index] = .child(child)
            }
            return nil
        case .bucket(var entries):
            if let position = entries.firstIndex(where: { $0.0 == key }) {
                let old = entries[position].1
                entries[position].1 = value; node.slots[index] = .bucket(entries)
                return old
            }
            entries.append((key, value)); node.slots[index] = .bucket(entries)
            return nil
        case .child, .detached:
            return nil
        }
    }

    /// The caller has checked that `key` is present. Emptied children are
    /// removed; a child left with one entry is kept, which lookups tolerate.
    private static func remove(_ node: inout Node, key: [UInt8], hash: UInt64, shift: Int) -> Int64? {
        unique(&node)
        let bit = UInt32(1) << UInt32((hash >> UInt64(shift)) & 31)
        guard node.bitmap & bit != 0 else { return nil }
        let index = (node.bitmap & (bit - 1)).nonzeroBitCount
        func drop() { node.slots.remove(at: index); node.bitmap &= ~bit }
        if var child = detachChild(node, at: index) {
            let old = remove(&child, key: key, hash: hash, shift: shift + bitsPerLevel)
            if child.slots.isEmpty { drop() } else { node.slots[index] = .child(child) }
            return old
        }
        switch node.slots[index] {
        case .entry(let existing, let old):
            guard existing == key else { return nil }
            drop(); return old
        case .bucket(var entries):
            guard let position = entries.firstIndex(where: { $0.0 == key }) else { return nil }
            let old = entries.remove(at: position).1
            if entries.isEmpty { drop() } else { node.slots[index] = .bucket(entries) }
            return old
        case .child, .detached:
            return nil
        }
    }
}
