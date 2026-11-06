/// Textual expansion limits belong to one authored outer view reference.
/// Replanning a body reuses its ancestry path, so planner retries and pushdown
/// do not consume extra expansion units. Sibling written references reset it.
internal final class ViewExpansionState {
    private struct Site: Hashable { let name: SQLText; let offset: Int }
    private var path: [Site] = []
    private var seen: Set<[Site]> = []
    private var active: Set<SQLText> = []
    private var base = 0
    func withBody<T>(_ name: SQLText, referenceDepth: Int, offset: Int, bodyDepth: Int,
                     _ body: () throws -> T) throws -> T {
        let nextBase = base + referenceDepth
        guard nextBase <= SQLParser.maximumNesting, bodyDepth <= SQLParser.maximumNesting - nextBase else {
            throw JedError("54001", "statement too complex: nesting depth exceeds the maximum of 256")
        }
        guard !active.contains(name) else { throw JedError("42P17", "recursive view definition") }
        if path.isEmpty { seen.removeAll(keepingCapacity: true) }
        let next = path + [Site(name: name, offset: offset)]
        if !seen.contains(next) {
            guard seen.count < 1024 else { throw JedError("54001", "statement too complex: a view reference expands to more than 1024 views") }
            seen.insert(next)
        }
        let previousBase = base
        path = next; active.insert(name); base = nextBase
        defer { path.removeLast(); active.remove(name); base = previousBase }
        return try body()
    }
}
