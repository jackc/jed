/// Expand authored grouping syntax without exceeding the shared 4096-set gate.
/// Column/expression identity is resolved later; duplicate sets remain present.
internal enum GroupingExpansion {
    static let maximum = 4096
    static func product(_ left: [[SQLExpression]], _ right: [[SQLExpression]]) throws -> [[SQLExpression]] {
        guard right.isEmpty || left.count <= maximum / right.count else { throw JedError("54001", "too many grouping sets") }
        return left.flatMap { a in right.map { a + $0 } }
    }
    static func expand(_ groups: [SQLGroup]) throws -> [[SQLExpression]] {
        var result: [[SQLExpression]] = [[]]
        for group in groups { result = try product(result, expand(group)) }
        return result
    }
    private static func expand(_ group: SQLGroup) throws -> [[SQLExpression]] {
        switch group {
        case .set(let keys): return [keys]
        case .sets(let groups):
            var result: [[SQLExpression]] = []
            for group in groups {
                let items = try expand(group)
                guard items.count <= maximum - result.count else { throw JedError("54001", "too many grouping sets") }
                result += items
            }
            return result
        case .rollup(let groups):
            var result: [[SQLExpression]] = []
            for count in stride(from: groups.count, through: 0, by: -1) {
                let items = try expand(Array(groups.prefix(count)))
                guard items.count <= maximum - result.count else { throw JedError("54001", "too many grouping sets") }
                result += items
            }
            return result
        case .cube(let groups):
            guard groups.count <= 12 else { throw JedError("54001", "too many grouping sets") }
            var result: [[SQLExpression]] = []
            for mask in 0..<(1 << groups.count) {
                let selected = groups.indices.filter { mask & (1 << $0) != 0 }.map { groups[$0] }
                let items = try expand(selected)
                guard items.count <= maximum - result.count else { throw JedError("54001", "too many grouping sets") }
                result += items
            }
            return result
        }
    }
}
