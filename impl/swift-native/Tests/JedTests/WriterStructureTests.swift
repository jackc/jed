import XCTest
@testable import Jed

/// Internal invariants of the structures that keep single-row writes cheap.
/// None is observable from SQL: each must behave exactly like the plain
/// collection or the unbatched edit it replaces.
final class WriterStructureTests: XCTestCase {
    private struct Random {
        var state: UInt64
        mutating func next(_ bound: Int) -> Int {
            state = state &* 6364136223846793005 &+ 1442695040888963407
            return Int((state >> 33) % UInt64(bound))
        }
    }
    private func key(_ number: Int) -> [UInt8] {
        var writer = ByteWriter(); writer.u32(UInt32(number)); return writer.bytes
    }

    func testPendingKeyMapMatchesDictionaryAndCopiesAreIndependent() {
        var random = Random(state: 7), map = PendingKeyMap(), model: [[UInt8]: Int64] = [:]
        var copies: [(PendingKeyMap, [[UInt8]: Int64])] = []
        for step in 0..<6000 {
            let name = key(random.next(700)), value = Int64(random.next(1000))
            if random.next(3) == 0 {
                XCTAssertEqual(map.removeValue(forKey: name), model.removeValue(forKey: name))
            } else {
                XCTAssertEqual(map.updateValue(value, forKey: name), model.updateValue(value, forKey: name))
            }
            XCTAssertEqual(map.count, model.count)
            if step % 500 == 0 { copies.append((map, model)) }
        }
        for candidate in 0..<700 { XCTAssertEqual(map[key(candidate)], model[key(candidate)]) }
        // Later edits of the original never reached an earlier copy.
        for (copy, expected) in copies {
            XCTAssertEqual(copy.count, expected.count)
            for candidate in 0..<700 { XCTAssertEqual(copy[key(candidate)], expected[key(candidate)]) }
        }
        map.removeAll()
        XCTAssertTrue(map.isEmpty); XCTAssertNil(map[key(1)])
    }

    func testRecordListMatchesArrayAndCopiesAreIndependent() {
        var random = Random(state: 11), next = 0
        var model: [PackedRecord] = (0..<70).map { _ in next += 1; return PackedRecord(key: key(next), cells: []) }
        var list = RecordList(model)
        let original = list, originalKeys = model.map(\.key)
        for _ in 0..<4000 {
            switch model.isEmpty ? 0 : random.next(3) {
            case 0:
                next += 1
                let record = PackedRecord(key: key(next), cells: []), index = random.next(model.count + 1)
                list.insert(record, at: index); model.insert(record, at: index)
            case 1:
                let index = random.next(model.count)
                XCTAssertEqual(list.remove(at: index).key, model.remove(at: index).key)
            default:
                next += 1
                let record = PackedRecord(key: key(next), cells: []), index = random.next(model.count)
                list.replace(at: index, with: record); model[index] = record
            }
            XCTAssertEqual(list.count, model.count)
        }
        XCTAssertEqual(list.flat.map(\.key), model.map(\.key))
        for index in model.indices { XCTAssertEqual(list[index].key, model[index].key) }
        XCTAssertEqual(original.flat.map(\.key), originalKeys)
        XCTAssertTrue(RecordList().isEmpty)
    }

    func testChildListMatchesArrayAndCopiesAreIndependent() {
        func leaf() -> TreeChild { .resident(TreeNode(leaf: nil)) }
        func same(_ list: ChildList, _ model: [TreeChild]) -> Bool {
            guard list.count == model.count else { return false }
            for index in model.indices {
                guard case .resident(let a) = list[index], case .resident(let b) = model[index], a === b else { return false }
            }
            return true
        }
        var random = Random(state: 31)
        var model: [TreeChild] = (0..<150).map { _ in leaf() }
        var list = ChildList(model)
        let original = list, originalModel = model
        for _ in 0..<3000 {
            switch model.isEmpty ? 0 : random.next(3) {
            case 0:
                let child = leaf(), index = random.next(model.count + 1)
                list.insert(child, at: index); model.insert(child, at: index)
            case 1:
                let index = random.next(model.count)
                list.remove(at: index); model.remove(at: index)
            default:
                let child = leaf(), index = random.next(model.count)
                list[index] = child; model[index] = child
            }
            XCTAssertTrue(same(list, model))
        }
        XCTAssertTrue(same(original, originalModel))
        XCTAssertTrue(same(ChildList(Array(list)), model))
        XCTAssertTrue(ChildList().isEmpty)
    }

    /// A leaf-at-a-time fold runs only when no ceiling or token can observe a
    /// row. With a ceiling set (never reached here) the same statements take
    /// the row-at-a-time path, and must give the same rows. (An integer SUM
    /// cannot overflow from a table this small, so that fallback is not
    /// reachable here.)
    func testRunFoldAgreesWithRowFold() throws {
        let database = try Database()
        defer { try? database.close() }
        try database.execute("CREATE TABLE t(id i32 PRIMARY KEY, a i64, b i16, c i32)")
        try database.execute("INSERT INTO t SELECT g, CASE WHEN g % 7 = 0 THEN NULL ELSE g::i64 * 1000 END, " +
            "(g % 100 - 50)::i16, CASE WHEN g % 3 = 0 THEN NULL ELSE (500 - g)::i32 END FROM generate_series(1, 900) s(g)")
        try database.execute("CREATE TABLE big(id i32 PRIMARY KEY, v i64)")
        try database.execute("INSERT INTO big SELECT g, CASE WHEN g = 700 THEN 9223372036854775807 ELSE g::i64 END " +
            "FROM generate_series(1, 900) s(g)")
        let statements = [
            "SELECT count(*), count(a), sum(a), min(a), max(a) FROM t",
            "SELECT sum(b), min(b), max(b), count(c), sum(c), min(c), max(c) FROM t",
            "SELECT count(*), sum(a) FROM t WHERE id BETWEEN 100 AND 640",
            "SELECT min(c), max(c) FROM t WHERE id > 5000",
            "SELECT sum(v) FROM big",
            "SELECT count(*), min(v), max(v) FROM big",
        ]
        func outcome(_ sql: String, limit: Int64) throws -> String {
            let session = try database.session()
            defer { session.close() }
            session.maxCost = limit
            do {
                let rows = try session.query(sql)
                var lines: [String] = []
                while let row = try rows.next() { lines.append(String(describing: try SpillRowCodec.encode(row))) }
                return lines.joined(separator: "|")
            } catch let error as JedError { return "error " + error.code }
        }
        for sql in statements {
            let batched = try outcome(sql, limit: 0), stepped = try outcome(sql, limit: 1_000_000_000)
            XCTAssertEqual(batched, stepped, sql)
        }
    }

    /// One line per node, in order: enough to tell any two shapes apart.
    private func shape(_ tree: BPlusTree) throws -> [String] {
        var lines: [String] = []
        func visit(_ child: TreeChild, depth: Int) throws {
            let node = try tree.resolve(child)
            if node.isLeaf {
                let keys = (0..<node.entryCount).map { Array(node.key(at: $0)) }
                lines.append("\(depth) leaf \(node.payloadSize) \(keys)")
            } else {
                lines.append("\(depth) interior \(node.payloadSize) \(node.nodeCount) \(node.containsRecords) \(node.keys)")
                for next in node.children { try visit(next, depth: depth + 1) }
            }
        }
        if let root = tree.root { try visit(root, depth: 0) }
        lines.append("count \(String(describing: tree.count)) nodes \(tree.nodeCount) height \(tree.height)")
        return lines
    }

    func testBatchedEditsBuildTheSameTreeAndLeaveEarlierSnapshotsUntouched() throws {
        let codec = try PlainPageCodec(pageSize: 256)
        var random = Random(state: 23), base = BPlusTree(codec: codec, types: [])
        for ordinal in 0..<1500 { try base.insert(PackedRecord(key: key((ordinal * 733) % 3000), cells: [])) }
        XCTAssertGreaterThanOrEqual(base.height, 3)
        let baseShape = try shape(base)
        // Runs of removals that drain and merge leaves, then insertions that
        // split them, with several batches so nodes outlive their batch.
        var plain = base, batched = base, middle: [(BPlusTree, [String])] = []
        for round in 0..<6 {
            var edits: [(remove: Bool, key: [UInt8])] = []
            let start = random.next(2400)
            for offset in 0..<400 { edits.append((true, key(start + offset))) }
            for _ in 0..<300 { edits.append((random.next(4) == 0, key(random.next(3200)))) }
            if round % 2 == 1 { for offset in 0..<400 { edits.append((false, key(start + offset))) } }
            batched.beginBatch()
            for edit in edits {
                if edit.remove {
                    XCTAssertEqual(try plain.remove(edit.key)?.key, try batched.remove(edit.key)?.key)
                } else {
                    let record = PackedRecord(key: edit.key, cells: [])
                    XCTAssertEqual(try plain.insert(record)?.key, try batched.insert(record)?.key)
                }
            }
            batched.endBatch()
            let expected = try shape(plain)
            XCTAssertEqual(try shape(batched), expected, "round \(round)")
            middle.append((batched, expected))
        }
        // Later batches never edited a tree an earlier batch had finished.
        XCTAssertEqual(try shape(base), baseShape)
        for (tree, expected) in middle { XCTAssertEqual(try shape(tree), expected) }
    }
}
