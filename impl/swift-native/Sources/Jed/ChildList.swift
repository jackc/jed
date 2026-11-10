/// An interior node's children, in fixed chunks.
///
/// A write replaces one child of each interior node on its path, and every
/// such node is built beside the one it replaces. With a flat array that
/// copies a reference to every child; here it copies the chunk list and the
/// one chunk that changed. Every chunk but the last holds exactly
/// `chunkSize` children, so indexing stays a shift and a mask.
internal struct ChildList: RandomAccessCollection, MutableCollection, Sendable {
    private static let shift = 5
    private static let chunkSize = 1 << shift
    private static let mask = chunkSize - 1

    private var chunks: [[TreeChild]]
    private(set) var count: Int

    init() { chunks = []; count = 0 }

    init<Children: Collection>(_ children: Children) where Children.Element == TreeChild {
        var chunks: [[TreeChild]] = [], current: [TreeChild] = []
        chunks.reserveCapacity((children.count + Self.mask) >> Self.shift)
        current.reserveCapacity(Swift.min(children.count, Self.chunkSize))
        for child in children {
            current.append(child)
            if current.count == Self.chunkSize {
                chunks.append(current); current = []; current.reserveCapacity(Self.chunkSize)
            }
        }
        if !current.isEmpty { chunks.append(current) }
        self.chunks = chunks; count = children.count
    }

    var startIndex: Int { 0 }
    var endIndex: Int { count }

    subscript(index: Int) -> TreeChild {
        get {
            precondition(index >= 0 && index < count, "child index out of range")
            return chunks[index >> Self.shift][index & Self.mask]
        }
        set {
            precondition(index >= 0 && index < count, "child index out of range")
            chunks[index >> Self.shift][index & Self.mask] = newValue
        }
    }

    /// Inserts before the child at `index`; `index == count` appends. Later
    /// chunks each pass one child along to stay full.
    mutating func insert(_ child: TreeChild, at index: Int) {
        precondition(index >= 0 && index <= count, "child index out of range")
        var chunk = index >> Self.shift
        if chunk == chunks.count { chunks.append([]) }
        chunks[chunk].insert(child, at: index & Self.mask)
        while chunks[chunk].count > Self.chunkSize {
            let overflow = chunks[chunk].removeLast()
            chunk += 1
            if chunk == chunks.count { chunks.append([]) }
            chunks[chunk].insert(overflow, at: 0)
        }
        count += 1
    }

    @discardableResult mutating func remove(at index: Int) -> TreeChild {
        precondition(index >= 0 && index < count, "child index out of range")
        var chunk = index >> Self.shift
        let removed = chunks[chunk].remove(at: index & Self.mask)
        while chunk + 1 < chunks.count {
            chunks[chunk].append(chunks[chunk + 1].removeFirst())
            chunk += 1
        }
        if chunks[chunk].isEmpty { chunks.removeLast() }
        count -= 1
        return removed
    }
}
