import Synchronization
import XCTest
@testable import Jed

final class ScanPageCountBoundaryTests: XCTestCase {
    private func key(_ number: Int) -> [UInt8] {
        var writer = ByteWriter(); writer.u32(UInt32(number)); return writer.bytes
    }

    private func checkCounts(_ tree: BPlusTree, file: StaticString = #filePath, line: UInt = #line) {
        guard case .resident(let root) = tree.root, !root.isLeaf else {
            XCTFail("expected a tree with resident routing metadata", file: file, line: line); return
        }
        let separator = root.keys[0]
        let leftNodes = root.children[0].nodeCount
        let rightNodes = root.children.dropFirst().reduce(0) { $0 + $1.nodeCount }
        XCTAssertEqual(ScanAccess.full.pages(tree), Int64(tree.nodeCount), file: file, line: line)
        XCTAssertEqual(ScanAccess.empty.pages(tree), 0, file: file, line: line)
        XCTAssertEqual(ScanAccess(lower: nil, upper: TreeBound(separator, inclusive: false), isEmpty: false).pages(tree),
                       Int64(1 + leftNodes), file: file, line: line)
        XCTAssertEqual(ScanAccess(lower: TreeBound(separator), upper: nil, isEmpty: false).pages(tree),
                       Int64(1 + rightNodes), file: file, line: line)

        // A point touches one node per level, even when its key is absent.
        for point in [key(0), key(200), key(399), key(500), separator] {
            let bound = TreeBound(point)
            XCTAssertEqual(ScanAccess(lower: bound, upper: bound, isEmpty: false).pages(tree),
                           Int64(tree.height), file: file, line: line)
        }
        let first = TreeBound(key(0)), last = TreeBound(key(399))
        let intervals = ScanAccess(lower: nil, upper: nil, isEmpty: false, intervals: [
            ScanInterval(lower: first, upper: first), ScanInterval(lower: last, upper: last)
        ])
        XCTAssertEqual(intervals.pages(tree), Int64(2 * tree.height), file: file, line: line)

        // Only isEmpty bypasses the logical traversal. Contradictory bounds
        // charge the visited routing nodes until no child can overlap.
        XCTAssertEqual(ScanAccess(lower: TreeBound(separator), upper: TreeBound(separator, inclusive: false), isEmpty: false).pages(tree),
                       1, file: file, line: line)
        XCTAssertEqual(ScanAccess(lower: first, upper: TreeBound(key(0), inclusive: false), isEmpty: false).pages(tree),
                       Int64(tree.height), file: file, line: line)
    }

    func testPageCountsUseOnlyResidentRoutingMetadata() throws {
        let codec = try PlainPageCodec(pageSize: 256)
        var tree = BPlusTree(codec: codec, types: [])
        XCTAssertEqual(ScanAccess.full.pages(tree), 0)
        for id in 0..<400 { try tree.insert(PackedRecord(key: key(id), cells: [])) }
        XCTAssertGreaterThan(tree.height, 1)
        checkCounts(tree)

        let device = MemoryBlockStore(), reads = Mutex(0)
        var allocator = try PageAllocator(highWater: 2, free: [], firstBody: 2)
        let written = try tree.serialize(allocator: &allocator, fromScratch: true)
        for page in written.pages { try device.write(at: UInt64(page.index) * 256, bytes: page.bytes) }
        let pool = try NodePool(codec: codec, cacheBytes: 256) { reference in
            reads.withLock { $0 += 1 }
            return try codec.openBody(pageIndex: reference.index,
                sealed: device.read(at: UInt64(reference.index) * 256, count: 256), expectedTag: reference.tag)
        }
        let lazy = try BPlusTree.open(root: written.root, pageCount: allocator.highWater, types: [], pool: pool)
        reads.withLock { $0 = 0 }
        XCTAssertEqual(pool.residentLeaves, 0)
        checkCounts(lazy)
        XCTAssertEqual(reads.withLock { $0 }, 0)
        XCTAssertEqual(pool.residentLeaves, 0)
    }
}
