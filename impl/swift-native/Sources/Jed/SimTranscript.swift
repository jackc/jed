import Synchronization

/// Canonical JSONL is constructed in field order rather than through a host
/// dictionary encoder. Hashes use the original UTF-8 bytes without normalization.
internal enum SimText {
    static func store(_ bytes: [UInt8]) throws -> MemoryBlockStore {
        let store = MemoryBlockStore(); try store.write(at: 0, bytes: bytes); return store
    }
    static func hash(_ bytes: some Sequence<UInt8>) -> UInt64 {
        bytes.reduce(UInt64(14_695_981_039_346_656_037)) { ($0 ^ UInt64($1)) &* 1_099_511_628_211 }
    }
    static func hex(_ value: UInt64) -> String { let text = String(value, radix: 16); return String(repeating: "0", count: 16 - text.count) + text }
    static func quote(_ text: String) -> String {
        var result = "\""
        for scalar in text.unicodeScalars {
            switch scalar.value {
            case 34: result += "\\\""
            case 92: result += "\\\\"
            case 8: result += "\\b"
            case 9: result += "\\t"
            case 10: result += "\\n"
            case 12: result += "\\f"
            case 13: result += "\\r"
            case 0..<32:
                let hex = String(scalar.value, radix: 16); result += "\\u" + String(repeating: "0", count: 4 - hex.count) + hex
            default: result.unicodeScalars.append(scalar)
            }
        }
        return result + "\""
    }
    static func less(_ lhs: String, _ rhs: String) -> Bool { lhs.utf8.lexicographicallyPrecedes(rhs.utf8) }
}

/// The environment retains one sanctioned random seam. Each trace session owns
/// its independent seeded stream; selecting a session never resets that stream.
internal final class SimRandomSource: RandomSource {
    private let active: Mutex<any RandomSource>
    init() { active = Mutex(SeededRandomSource(seed: 0)) }
    func select(_ source: any RandomSource) { active.withLock { $0 = source } }
    func bytes(count: Int) throws -> [UInt8] { let source = active.withLock { $0 }; return try source.bytes(count: count) }
}

internal struct SimQueryResult {
    let rows: [String]
    let firstColumn: [String]
    let affected: Int64
    let cost: Int64
    var hash: String { SimText.hex(SimText.hash(rows.joined(separator: "\u{1e}").utf8)) }
    static func run(_ session: Session, _ sql: String) throws -> SimQueryResult {
        let cursor = try session.query(sql); defer { cursor.close() }
        var rows: [String] = [], first: [String] = []
        while let row = try cursor.next() {
            var cells: [String] = []
            for (index, value) in row.enumerated() {
                if value.isNull { cells.append("NULL") }
                else {
                    guard let type = cursor.columns[index].type else { throw JedError.corruption("non-NULL result has unknown type") }
                    cells.append(try session.render(value, type: type).description)
                }
            }
            if let value = cells.first { first.append(value) }
            rows.append(cells.joined(separator: "\u{1f}"))
        }
        rows.sort(by: SimText.less)
        let affected = cursor.rowsAffected ?? -1
        return SimQueryResult(rows: rows, firstColumn: first, affected: affected, cost: cursor.cost)
    }
}
