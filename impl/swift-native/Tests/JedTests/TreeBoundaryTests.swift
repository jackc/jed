import Foundation
import Dispatch
import Synchronization
import XCTest
@testable import Jed

private final class TreeReadCounter: Sendable {
    let value = Mutex(0)
}

private final class TreeLoadResults: Sendable {
    let values = Mutex<[Result<PackedLeaf, any Error>]>([])
}

final class TreeBoundaryTests: XCTestCase {
    private func key(_ number: Int, width: Int = 4) -> [UInt8] {
        var writer = ByteWriter(); writer.u32(UInt32(number))
        return writer.bytes + [UInt8](repeating: 0xAB, count: max(0, width - 4))
    }

    private func orderedKeys(_ tree: BPlusTree, descending: Bool = false) throws -> [[UInt8]] {
        var cursor = TreeCursor(tree, descending: descending), result: [[UInt8]] = []
        while let record = try cursor.next() { result.append(record.key) }
        XCTAssertEqual(cursor.visitedNodes, tree.nodeCount)
        return result
    }

    private func validate(_ tree: BPlusTree) throws {
        func visit(_ child: TreeChild, lower: [UInt8]?, upper: [UInt8]?) throws -> (Int, Int) {
            let node = try tree.resolve(child)
            XCTAssertLessThanOrEqual(node.payloadSize, tree.codec.capacity)
            if node.isLeaf {
                XCTAssertGreaterThan(node.entryCount, 0, "committed leaf cannot be empty")
                if node.entryCount > 0 {
                    try BPlusTree.checkBounds(Array(node.key(at: 0)),
                        Array(node.key(at: node.entryCount - 1)), lower: lower, upper: upper)
                }
                return (1, 1)
            }
            XCTAssertEqual(node.children.count, node.keys.count + 1)
            var childHeight: Int?, nodes = 1
            for index in node.children.indices {
                let (height, count) = try visit(node.children[index],
                    lower: index == 0 ? lower : node.keys[index - 1],
                    upper: index == node.keys.count ? upper : node.keys[index])
                if let childHeight { XCTAssertEqual(height, childHeight) } else { childHeight = height }
                nodes += count
            }
            XCTAssertEqual(node.height, childHeight! + 1); XCTAssertEqual(node.nodeCount, nodes)
            return (node.height, nodes)
        }
        if let root = tree.root { _ = try visit(root, lower: nil, upper: nil) }
    }

    private func pool(_ store: MemoryBlockStore, codec: any PageCodec, reads: TreeReadCounter? = nil,
                      cacheBytes: Int? = 256) throws -> NodePool {
        try NodePool(codec: codec, cacheBytes: cacheBytes) { reference in
            reads?.value.withLock { $0 += 1 }
            let bytes = try store.read(at: UInt64(reference.index) * UInt64(codec.pageSize), count: codec.pageSize)
            return try codec.openBody(pageIndex: reference.index, sealed: bytes, expectedTag: reference.tag)
        }
    }

    private func write(_ pages: [StoredPage], to store: MemoryBlockStore, pageSize: Int) throws {
        for page in pages { try store.write(at: UInt64(page.index) * UInt64(pageSize), bytes: page.bytes) }
    }

    func testCacheResizePreservesCursorLeavesAndCapacityAcrossInvalidation() throws {
        let codec = try PlainPageCodec(pageSize: 256), device = MemoryBlockStore()
        var tree = BPlusTree(codec: codec, types: [])
        for id in 0..<200 { try tree.insert(PackedRecord(key: key(id), cells: [])) }
        var allocator = try PageAllocator(highWater: 2, free: [], firstBody: 2)
        let written = try tree.serialize(allocator: &allocator, fromScratch: true)
        try write(written.pages, to: device, pageSize: 256)
        let cache = try pool(device, codec: codec, cacheBytes: nil)
        let source = try BPlusTree.open(root: written.root, pageCount: allocator.highWater, types: [], pool: cache)
        XCTAssertEqual(try orderedKeys(source).count, 200)
        XCTAssertGreaterThan(cache.residentLeaves, 1)
        var cursor = TreeCursor(source)
        XCTAssertEqual(try cursor.next()?.key, key(0))
        try cache.resizeCache(bytes: 256)
        XCTAssertEqual(cache.capacity, 1); XCTAssertEqual(cache.residentLeaves, 1)
        for id in 1..<200 { XCTAssertEqual(try cursor.next()?.key, key(id)) }
        XCTAssertNil(try cursor.next())
        cache.invalidateAll(); XCTAssertEqual(cache.capacity, 1)
        try cache.resizeCache(bytes: nil)
        XCTAssertNil(cache.capacity)
        XCTAssertEqual(try orderedKeys(source).count, 200)
        XCTAssertGreaterThan(cache.residentLeaves, 1)
    }

    func testCopiedCursorRetainsIndependentPositionAcrossEvictionAndClose() throws {
        let codec = try PlainPageCodec(pageSize: 256), device = MemoryBlockStore()
        var tree = BPlusTree(codec: codec, types: [])
        for id in 0..<400 { try tree.insert(PackedRecord(key: key(id), cells: [])) }
        var allocator = try PageAllocator(highWater: 2, free: [], firstBody: 2)
        let written = try tree.serialize(allocator: &allocator, fromScratch: true)
        try write(written.pages, to: device, pageSize: 256)
        let reads = TreeReadCounter(), cache = try pool(device, codec: codec, reads: reads)
        let source = try BPlusTree.open(root: written.root, pageCount: allocator.highWater, types: [], pool: cache)
        var original = TreeCursor(source, lower: TreeBound(key(0)), upper: TreeBound(key(175), inclusive: false))
        let first = try XCTUnwrap(original.nextPosition())
        XCTAssertEqual(first.record().key, key(0))
        XCTAssertGreaterThan(first.node.entryCount, 1)
        var copied = original
        for id in 1..<175 { XCTAssertEqual(try original.next()?.key, key(id)) }
        XCTAssertNil(try original.next())
        let expectedVisits = original.visitedNodes
        original.close(); cache.invalidateAll()

        // The copy still owns its first leaf after another cursor advances,
        // closes, and evicts the cache. It faults later leaves only on demand.
        let before = reads.value.withLock { $0 }
        XCTAssertEqual(try copied.next()?.key, key(1))
        XCTAssertEqual(reads.value.withLock { $0 }, before)
        for id in 2..<175 { XCTAssertEqual(try copied.next()?.key, key(id)) }
        XCTAssertNil(try copied.next())
        XCTAssertGreaterThan(reads.value.withLock { $0 }, before)
        XCTAssertEqual(copied.visitedNodes, expectedVisits)
        copied.rewind(); XCTAssertEqual(try copied.next()?.key, key(0))
        copied.close(); XCTAssertNil(try copied.next())
        XCTAssertNil(try original.next())
    }

    func testStreamReadAheadStartsAtOneAndFailedHintsRemainPhysicalOnly() throws {
        let codec = try PlainPageCodec(pageSize: 256), device = MemoryBlockStore()
        var tree = BPlusTree(codec: codec, types: [])
        for id in 0..<800 { try tree.insert(PackedRecord(key: key(id), cells: [])) }
        var allocator = try PageAllocator(highWater: 2, free: [], firstBody: 2)
        let written = try tree.serialize(allocator: &allocator, fromScratch: true)
        try write(written.pages, to: device, pageSize: 256)
        let batches = Mutex<[Int]>([])
        let read: @Sendable (PageReference) throws -> [UInt8] = { reference in
            try codec.openBody(pageIndex: reference.index,
                sealed: device.read(at: UInt64(reference.index) * 256, count: 256), expectedTag: reference.tag)
        }
        let cache = try NodePool(codec: codec, cacheBytes: 64 * 256, readMany: { references in
            batches.withLock { $0.append(references.count) }
            if references.count > 1 { throw JedError("58030", "injected speculative batch failure") }
            return try references.map(read)
        }, batchLimit: 5, read: read)
        let source = try BPlusTree.open(root: written.root, pageCount: allocator.highWater, types: [], pool: cache)
        var cursor = TreeCursor(source)
        XCTAssertEqual(try cursor.next()?.key, key(0))
        XCTAssertEqual(batches.withLock { $0 }, [1])
        for id in 1..<800 { XCTAssertEqual(try cursor.next()?.key, key(id)) }
        XCTAssertNil(try cursor.next())
        let counts = batches.withLock { $0 }
        XCTAssertTrue(counts.contains(where: { $0 > 1 }))
        XCTAssertTrue(counts.allSatisfy { $0 <= 5 })
        XCTAssertLessThanOrEqual(cache.residentLeaves, 64)
    }

    func testDemandWaitingOnFailedHintRetriesItsOwnPage() throws {
        let codec = try PlainPageCodec(pageSize: 256)
        let payload = try PackedLeaf.encodePayload([PackedRecord(key: [7], cells: [])], types: [], capacity: codec.capacity)
        let bytes = try PageSealer.seal(codec: codec, index: 2, type: 2, count: 1, payload: payload).page.bytes
        let started = DispatchSemaphore(value: 0), release = DispatchSemaphore(value: 0)
        let hinted = DispatchSemaphore(value: 0), demanded = DispatchSemaphore(value: 0)
        let calls = TreeReadCounter(), results = TreeLoadResults()
        let cache = try NodePool(codec: codec, cacheBytes: 4096, readMany: { _ in
            started.signal()
            guard release.wait(timeout: .now() + 5) == .success else { throw JedError("XX000", "test hint timed out") }
            throw JedError("XX001", "corruption in a different hinted page")
        }, read: { _ in calls.value.withLock { $0 += 1 }; return bytes })
        DispatchQueue.global().async {
            cache.prefetch([PageReference(2), PageReference(3)], types: []); hinted.signal()
        }
        guard started.wait(timeout: .now() + 5) == .success else { XCTFail("hint did not start"); return }
        defer { release.signal() }
        DispatchQueue.global().async {
            let result = Result { try cache.load(PageReference(2), types: []) }
            results.values.withLock { $0.append(result) }; demanded.signal()
        }
        // The hinted page is still in flight, so an ordinary demand must wait.
        XCTAssertEqual(demanded.wait(timeout: .now() + .milliseconds(20)), .timedOut)
        XCTAssertEqual(calls.value.withLock { $0 }, 0)
        release.signal()
        XCTAssertEqual(hinted.wait(timeout: .now() + 5), .success)
        XCTAssertEqual(demanded.wait(timeout: .now() + 5), .success)
        let leaf = try results.values.withLock { try XCTUnwrap($0.first).get() }
        XCTAssertEqual(Array(leaf.key(at: 0)), [7])
        XCTAssertEqual(calls.value.withLock { $0 }, 1)
    }

    func testSharedGoldenTreePagesRebuildByteExactly() throws {
        // The shared fixtures were built in ascending order. Rebuilding their
        // leaf records checks split boundaries, copy/push-up, directories,
        // post-order allocation, and the legal zero-key interior together.
        for (name, types, selectLast) in [
            ("pk_table", [TypeDescriptor(.scalar(.i32)), TypeDescriptor(.scalar(.i16))], false),
            ("tall_tree", [TypeDescriptor(.scalar(.i32)), TypeDescriptor(.scalar(.text))], false),
            ("max_sep_table", [], true)
        ] {
            let url = try XCTUnwrap(Bundle.module.url(forResource: name, withExtension: "jed", subdirectory: "Fixtures"))
            let bytes = Array(try Data(contentsOf: url)), codec = try PlainPageCodec(pageSize: 256)
            let store = MemoryBlockStore(); try store.write(at: 0, bytes: bytes)
            let pageCount = UInt32(bytes.count / 256)
            var candidates: Set<UInt32> = [], children: Set<UInt32> = []
            for index in 2..<pageCount {
                let page = Array(bytes[(Int(index) * 256)..<(Int(index + 1) * 256)])
                if page[0] == 2 || page[0] == 3 { candidates.insert(index) }
                if page[0] == 3 {
                    let interior = try PackedInterior(bytes: page, codec: codec, pageCount: pageCount)
                    children.formUnion(interior.children.map(\.index))
                }
            }
            let roots = candidates.subtracting(children).sorted()
            let root = try XCTUnwrap(selectLast ? roots.last : roots.first)
            let source = try BPlusTree.open(root: PageReference(root), pageCount: pageCount,
                                           types: types, pool: pool(store, codec: codec))
            var expected: [UInt32: [UInt8]] = [:]
            func collect(_ child: TreeChild) throws {
                let node = try source.resolve(child), reference = try XCTUnwrap(child.reference)
                expected[reference.index] = Array(bytes[(Int(reference.index) * 256)..<(Int(reference.index + 1) * 256)])
                for child in node.children { try collect(child) }
            }
            try collect(XCTUnwrap(source.root))
            var rebuilt = BPlusTree(codec: codec, types: types), cursor = TreeCursor(source)
            while let record = try cursor.next() { try rebuilt.insert(record) }
            var allocator = try PageAllocator(highWater: XCTUnwrap(expected.keys.min()), free: [], firstBody: 2)
            let result = try rebuilt.serialize(allocator: &allocator, fromScratch: true)
            XCTAssertEqual(result.root.index, root, name)
            XCTAssertEqual(result.pages.count, expected.count, name)
            for page in result.pages { XCTAssertEqual(page.bytes, expected[page.index], "\(name) page \(page.index)") }
            try validate(rebuilt)
        }
    }

    func testSnapshotsCursorBoundsAndCopyOnWriteDeletion() throws {
        let codec = try PlainPageCodec(pageSize: 256)
        var tree = BPlusTree(codec: codec, types: [])
        for ordinal in 0..<400 { try tree.insert(PackedRecord(key: key((ordinal * 113) % 400), cells: [])) }
        let snapshot = tree
        var pinned = TreeCursor(tree, lower: TreeBound(key(20), inclusive: false), upper: TreeBound(key(75), inclusive: false))
        XCTAssertEqual(try pinned.next()?.key, key(21))
        for ordinal in 0..<400 where ordinal % 2 == 0 { XCTAssertNotNil(try tree.remove(key(ordinal))) }
        XCTAssertEqual(try orderedKeys(tree), (0..<400).filter { $0 % 2 == 1 }.map { key($0) })
        XCTAssertEqual(try orderedKeys(snapshot, descending: true), (0..<400).reversed().map { key($0) })
        var remaining: [[UInt8]] = []
        while let row = try pinned.next() { remaining.append(row.key) }
        XCTAssertEqual(remaining, (22..<75).map { key($0) })
        pinned.rewind(); XCTAssertEqual(try pinned.next()?.key, key(21)); pinned.close(); XCTAssertNil(try pinned.next())
        var reverse = TreeCursor(snapshot, lower: TreeBound(key(20)), upper: TreeBound(key(75), inclusive: false), descending: true)
        var reverseKeys: [[UInt8]] = []
        while let row = try reverse.next() { reverseKeys.append(row.key) }
        XCTAssertEqual(reverseKeys, (20..<75).reversed().map { key($0) })
        try validate(tree)
        for ordinal in 0..<400 where ordinal % 2 == 1 { try tree.remove(key(ordinal)) }
        XCTAssertTrue(tree.isEmpty); XCTAssertEqual(tree.nodeCount, 0); XCTAssertEqual(tree.count, 0)
        XCTAssertEqual(snapshot.count, 400)
    }

    func testKeyOnlyLeafAndSeparatorCapacityAreDistinct() throws {
        let codecs: [any PageCodec] = [try PlainPageCodec(pageSize: 256),
            try EncryptedPageCodec(pageSize: 1024, dataKey: Array(repeating: 7, count: 32))]
        for codec in codecs {
            let leafMaximum = codec.capacity - PackedLeaf.overhead(records: 1, types: [])
            let separatorMaximum = codec.capacity - (2 * PackedInterior.pointerWidth(codec) + 4)
            var tree = BPlusTree(codec: codec, types: [])
            let largest = key(1, width: leafMaximum)
            try tree.insert(PackedRecord(key: largest, cells: []))
            XCTAssertGreaterThan(largest.count, RecordDisposition.recordMaximum(capacity: codec.capacity, columns: 0))
            var allocator = try PageAllocator(highWater: codec.firstBodyPage, free: [], firstBody: codec.firstBodyPage)
            let written = try tree.serialize(allocator: &allocator, fromScratch: true,
                entropy: { Array(repeating: 0xA5, count: $0) })
            XCTAssertEqual(written.pages.count, 1)
            let store = MemoryBlockStore(); try write(written.pages, to: store, pageSize: codec.pageSize)
            let reopened = try BPlusTree.open(root: written.root, rowCount: 1, pageCount: allocator.highWater,
                types: [], pool: pool(store, codec: codec))
            XCTAssertEqual(try reopened.get(largest)?.key, largest)
            // A smaller left neighbor copies the large entry into an oversized
            // root separator. Keep the pending shape until serialization while
            // its committed predecessor remains independently readable.
            var pending = tree
            try pending.insert(PackedRecord(key: key(0), cells: []))
            XCTAssertEqual(pending.count, 2)
            let highWater = allocator.highWater
            XCTAssertThrowsError(try pending.serialize(allocator: &allocator, fromScratch: true,
                entropy: { Array(repeating: 0xA5, count: $0) })) {
                XCTAssertEqual(($0 as? JedError)?.code, "0A000")
            }
            XCTAssertEqual(allocator.highWater, highWater)
            XCTAssertEqual(tree.count, 1); XCTAssertNil(try tree.get(key(0)))
            pending = tree
            try pending.insert(PackedRecord(key: key(2, width: leafMaximum + 1), cells: []))
            XCTAssertThrowsError(try pending.serialize(allocator: &allocator, fromScratch: true,
                entropy: { Array(repeating: 0xA5, count: $0) })) {
                XCTAssertEqual(($0 as? JedError)?.code, "0A000")
            }
            // The reverse shape keeps the big entry in a leaf and copies only
            // its small right neighbor, so it remains valid.
            try tree.insert(PackedRecord(key: key(2), cells: [])); try validate(tree)
            var boundary = BPlusTree(codec: codec, types: [])
            try boundary.insert(PackedRecord(key: key(1, width: separatorMaximum), cells: []))
            try boundary.insert(PackedRecord(key: key(0, width: codec.capacity - separatorMaximum), cells: []))
            try validate(boundary)
            XCTAssertEqual(try boundary.resolve(XCTUnwrap(boundary.root)).payloadSize, codec.capacity)
        }
    }

    func testOversizedPendingLeafRetainsItsQ3ChargeWithoutRelaxingDiskValidation() throws {
        let codecs: [any PageCodec] = [try PlainPageCodec(pageSize: 256),
            try EncryptedPageCodec(pageSize: 1024, dataKey: Array(repeating: 7, count: 32))]
        for codec in codecs {
            var store = TableStore(tree: BPlusTree(codec: codec, types: []))
            let small = PackedRecord(key: key(1), cells: [])
            try store.put(small, netStaging: true); store.clearPending()
            let committed = store
            let large = PackedRecord(key: key(2, width: codec.capacity + 41), cells: [])
            try store.put(large, netStaging: true)
            XCTAssertEqual(store.pendingBytes, Int64(large.key.count))
            XCTAssertEqual(try store.get(large.key)?.key, large.key)
            XCTAssertEqual(try orderedKeys(store.tree), [small.key, large.key])
            let leaf = try store.tree.resolve(XCTUnwrap(store.tree.root))
            XCTAssertGreaterThan(leaf.payloadSize, codec.capacity)
            XCTAssertNil(leaf.leaf)
            XCTAssertThrowsError(try PackedLeaf.encodePayload([small, large],
                types: [], capacity: codec.capacity)) {
                XCTAssertEqual(($0 as? JedError)?.code, "0A000")
            }
            var allocation = try PageAllocator(highWater: codec.firstBodyPage, free: [], firstBody: codec.firstBodyPage)
            XCTAssertThrowsError(try store.tree.serialize(allocator: &allocation,
                entropy: { Array(repeating: 0xA5, count: $0) })) {
                XCTAssertEqual(($0 as? JedError)?.code, "0A000")
            }
            XCTAssertEqual(allocation.highWater, codec.firstBodyPage)
            XCTAssertEqual(committed.count, 1); XCTAssertNil(try committed.get(large.key))
            try store.remove(large.key)
            XCTAssertEqual(store.pendingBytes, 0)
            let written = try store.tree.serialize(allocator: &allocation,
                entropy: { Array(repeating: 0xA5, count: $0) })
            XCTAssertEqual(written.pages.count, 1)
            XCTAssertEqual(try written.tree.get(small.key)?.key, small.key)
        }
    }

    func testNearCapacitySeparatorsSurviveEveryDeleteAndSerialization() throws {
        let codec = try PlainPageCodec(pageSize: 256)
        var tree = BPlusTree(codec: codec, types: [])
        // Length 112 forces the N=2 -> m=1 interior split; varying lengths
        // additionally exercise both deterministic merge-abandon conditions.
        for variable in [false, true] {
            let keys = (0..<80).map { key($0, width: variable ? 4 + ($0 * 37) % 109 : 112) }
            for i in 0..<80 { try tree.insert(PackedRecord(key: keys[(i * 37) % 80], cells: [])) }
            try validate(tree)
            for i in 0..<80 {
                XCTAssertNotNil(try tree.remove(keys[(i * 53) % 80]))
                try validate(tree)
                var allocator = try PageAllocator(highWater: 2, free: [], firstBody: 2)
                _ = try tree.serialize(allocator: &allocator, fromScratch: true)
            }
            XCTAssertTrue(tree.isEmpty)
        }
    }

    func testDeletingLastRecordBelowZeroSeparatorInteriorPrunesItsEmptySubtree() throws {
        let codec = try PlainPageCodec(pageSize: 256)
        var tree = BPlusTree(codec: codec, types: [])
        for i in 0..<6 { try tree.insert(PackedRecord(key: key(i, width: 112), cells: [])) }
        XCTAssertEqual(tree.height, 3)
        try tree.remove(key(4, width: 112)); try tree.remove(key(5, width: 112))
        XCTAssertEqual(tree.height, 2)
        XCTAssertEqual(try orderedKeys(tree), (0..<4).map { key($0, width: 112) })
        try validate(tree)
        var allocator = try PageAllocator(highWater: 2, free: [], firstBody: 2)
        let written = try tree.serialize(allocator: &allocator, fromScratch: true)
        XCTAssertEqual(written.pages.count, 3)
    }

    func testLazyOpenBoundedPoolAndCacheIndependentLogicalVisits() throws {
        let codec = try PlainPageCodec(pageSize: 256), store = MemoryBlockStore()
        var original = BPlusTree(codec: codec, types: [])
        for i in 0..<2000 { try original.insert(PackedRecord(key: key(i, width: 24), cells: [])) }
        var allocator = try PageAllocator(highWater: 2, free: [], firstBody: 2)
        let written = try original.serialize(allocator: &allocator, fromScratch: true)
        try write(written.pages, to: store, pageSize: 256)
        let reads = TreeReadCounter(), cache = try pool(store, codec: codec, reads: reads, cacheBytes: 1)
        let opened = try BPlusTree.open(root: written.root, rowCount: 2000, pageCount: allocator.highWater, types: [], pool: cache)
        XCTAssertEqual(cache.residentLeaves, 0)
        XCTAssertLessThan(reads.value.withLock { $0 }, written.pages.count / 2)
        XCTAssertEqual(opened.nodeCount, original.nodeCount)
        XCTAssertEqual(try orderedKeys(opened), try orderedKeys(original))
        XCTAssertEqual(cache.residentLeaves, 1)
        XCTAssertEqual(try orderedKeys(opened, descending: true), try orderedKeys(original, descending: true))
        XCTAssertEqual(cache.residentLeaves, 1)
        let pinnedPool = try pool(store, codec: codec, cacheBytes: nil)
        let pinned = try BPlusTree.open(root: written.root, rowCount: 2000, pageCount: allocator.highWater, types: [], pool: pinnedPool)
        _ = try orderedKeys(pinned)
        XCTAssertGreaterThan(pinnedPool.residentLeaves, 1)
    }

    func testFreshOverflowMetadataSurvivesSplitsAndExportCopiesChains() throws {
        let codec = try PlainPageCodec(pageSize: 256), store = MemoryBlockStore()
        let type = TypeDescriptor(.scalar(.bytea)), cache = try pool(store, codec: codec)
        var tree = BPlusTree(codec: codec, types: [type], pool: cache)
        var state: UInt64 = 19, raw: [UInt8] = []
        for _ in 0..<600 { state = state &* 6364136223846793005 &+ 1; raw.append(UInt8(truncatingIfNeeded: state >> 32)) }
        for ordinal in 0..<45 {
            let record = try RecordDisposition.plan(key: key(ordinal), values: [.value(.bytea(raw))], types: [type], codec: codec)
            XCTAssertFalse(record.overflows.isEmpty)
            try tree.insert(record)
        }
        var cursor = TreeCursor(tree)
        while let record = try cursor.next() {
            if case .bytea(let bytes) = try record.materialize(column: 0, type: type) { XCTAssertEqual(bytes, raw) }
            else { XCTFail("wrong pending value type") }
        }
        var allocator = try PageAllocator(highWater: 2, free: [], firstBody: 2)
        let written = try tree.serialize(allocator: &allocator)
        try write(written.pages, to: store, pageSize: 256)
        XCTAssertEqual(written.pages.first?.bytes[0], 4, "incremental chains precede their leaf")
        var cleanCursor = TreeCursor(written.tree)
        let first = try XCTUnwrap(cleanCursor.next())
        let head = try XCTUnwrap(first.cells[0].largeFacts(encrypted: false)?.external)
        XCTAssertEqual(try OverflowChain.read(head, length: raw.count, pageCount: allocator.highWater,
            codec: codec, read: cache.readBody), raw)
        var exportAllocation = try PageAllocator(highWater: 2, free: [], firstBody: 2)
        let exported = try written.tree.serialize(allocator: &exportAllocation, fromScratch: true)
        XCTAssertEqual(exported.pages.first?.bytes[0], 2, "from-scratch leaf id precedes its chains")
        XCTAssertTrue(exported.keptRoots.isEmpty); XCTAssertTrue(exported.keptOverflowHeads.isEmpty)
        let exportedStore = MemoryBlockStore(); try write(exported.pages, to: exportedStore, pageSize: 256)
        let exportedPool = try pool(exportedStore, codec: codec)
        let reopened = try BPlusTree.open(root: exported.root, rowCount: 45, pageCount: exportAllocation.highWater,
                                         types: [type], pool: exportedPool)
        var after = TreeCursor(reopened), count = 0
        while let record = try after.next() {
            let facts = try XCTUnwrap(record.cells[0].largeFacts(encrypted: false))
            XCTAssertEqual(try OverflowChain.read(XCTUnwrap(facts.external), length: facts.stored,
                pageCount: exportAllocation.highWater, codec: codec, read: exportedPool.readBody), raw)
            count += 1
        }
        XCTAssertEqual(count, 45)
    }

    func testPoolSingleFlightAndInvalidatedLoadsCannotReenterCache() throws {
        let codec = try PlainPageCodec(pageSize: 256)
        func page(_ key: UInt8) throws -> [UInt8] {
            let payload = try PackedLeaf.encodePayload([PackedRecord(key: [key], cells: [])], types: [], capacity: codec.capacity)
            return try PageSealer.seal(codec: codec, index: 2, type: 2, count: 1, payload: payload).page.bytes
        }
        let before = try page(1), after = try page(2)
        let calls = TreeReadCounter(), started = DispatchSemaphore(value: 0), release = DispatchSemaphore(value: 0)
        let completed = DispatchSemaphore(value: 0), results = TreeLoadResults()
        let cache = try NodePool(codec: codec, cacheBytes: 256) { _ in
            let ordinal = calls.value.withLock { $0 += 1; return $0 }
            if ordinal == 1 {
                started.signal()
                guard release.wait(timeout: .now() + 5) == .success else { throw JedError("XX000", "test load timed out") }
                return before
            }
            return after
        }
        DispatchQueue.global().async {
            let result = Result { try cache.load(PageReference(2), types: []) }
            results.values.withLock { $0.append(result) }; completed.signal()
        }
        guard started.wait(timeout: .now() + 5) == .success else { XCTFail("load did not start"); return }
        defer { release.signal() }
        cache.invalidate([2])
        XCTAssertEqual(Array(try cache.load(PageReference(2), types: []).key(at: 0)), [2])
        release.signal()
        XCTAssertEqual(completed.wait(timeout: .now() + 5), .success)
        let old = try results.values.withLock { try XCTUnwrap($0.first).get() }
        XCTAssertEqual(Array(old.key(at: 0)), [1])
        XCTAssertEqual(Array(try cache.load(PageReference(2), types: []).key(at: 0)), [2])
        XCTAssertEqual(calls.value.withLock { $0 }, 2)

        let reads = TreeReadCounter(), parallel = TreeLoadResults()
        let shared = try NodePool(codec: codec, cacheBytes: 256) { _ in
            reads.value.withLock { $0 += 1 }; return before
        }
        DispatchQueue.concurrentPerform(iterations: 32) { _ in
            let result = Result { try shared.load(PageReference(2), types: []) }
            parallel.values.withLock { $0.append(result) }
        }
        let leaves = try parallel.values.withLock { try $0.map { try $0.get() } }
        XCTAssertEqual(leaves.count, 32); XCTAssertEqual(reads.value.withLock { $0 }, 1)
        XCTAssertTrue(leaves.allSatisfy { $0 === leaves[0] })
    }

    func testInteriorValidationAndLazyLeafParentBounds() throws {
        let codec = try PlainPageCodec(pageSize: 256)
        let zero = try PackedInterior.encodePayload(keys: [], children: [PageReference(2)], codec: codec)
        let zeroPage = try PageSealer.seal(codec: codec, index: 3, type: 3, count: 0, payload: zero).page.bytes
        let decoded = try PackedInterior(bytes: zeroPage, codec: codec, pageCount: 4)
        XCTAssertEqual(decoded.keys.count, 0); XCTAssertEqual(decoded.payloadSize, 4)
        for offset in [4, 16, 255] {
            var corrupt = zeroPage; corrupt[offset] ^= 0x80
            try PageEnvelope.putU32(PageEnvelope.checksum(corrupt, at: 12), into: &corrupt, at: 12)
            XCTAssertThrowsError(try PackedInterior(bytes: corrupt, codec: codec, pageCount: 4)) {
                XCTAssertEqual(($0 as? JedError)?.code, "XX001")
            }
        }
        let store = MemoryBlockStore()
        // Left leaf's key crosses the parent's separator. The skeleton may
        // classify that page at open, but its directory stays lazy until fault.
        for (index, key) in [(UInt32(2), UInt8(90)), (UInt32(3), UInt8(80))] {
            let payload = try PackedLeaf.encodePayload([PackedRecord(key: [key], cells: [])], types: [], capacity: codec.capacity)
            let page = try PageSealer.seal(codec: codec, index: index, type: 2, count: 1, payload: payload).page
            try write([page], to: store, pageSize: 256)
        }
        let payload = try PackedInterior.encodePayload(keys: [[50]], children: [PageReference(2), PageReference(3)], codec: codec)
        try write([PageSealer.seal(codec: codec, index: 4, type: 3, count: 1, payload: payload).page], to: store, pageSize: 256)
        let tree = try BPlusTree.open(root: PageReference(4), rowCount: 2, pageCount: 5, types: [], pool: pool(store, codec: codec))
        XCTAssertThrowsError(try tree.get([10])) { XCTAssertEqual(($0 as? JedError)?.code, "XX001") }
    }
}
