import Foundation
import Synchronization
import XCTest
@testable import Jed

private final class DomainTestDevice: BlockStore {
    let memory = MemoryBlockStore()
    private let events = Mutex<[String]>([])
    func read(at offset: UInt64, count: Int) throws -> [UInt8] { events.withLock { $0.append("read") }; return try memory.read(at: offset, count: count) }
    func write(at offset: UInt64, bytes: [UInt8]) throws { events.withLock { $0.append("write") }; try memory.write(at: offset, bytes: bytes) }
    func sync() throws { events.withLock { $0.append("sync") } }
    func size() throws -> UInt64 { try memory.size() }
    func setSize(_ bytes: UInt64) throws { events.withLock { $0.append("resize") }; try memory.setSize(bytes) }
    func reset() { events.withLock { $0.removeAll() } }
    var log: [String] { events.withLock { $0 } }
}

final class StorageDomainBoundaryTests: XCTestCase {
    private func name(_ text: String) throws -> SQLText { try SQLText(text) }
    private func key(_ value: Int64) throws -> [UInt8] { try KeyEncoding.encode(.integer(value), type: TypeDescriptor(.scalar(.i64))) }
    private func fixture(_ name: String) throws -> [UInt8] {
        let url = try XCTUnwrap(Bundle.module.url(forResource: name, withExtension: "jed", subdirectory: "Fixtures"))
        return Array(try Data(contentsOf: url))
    }
    private func expect(_ code: String, _ body: () throws -> Void, file: StaticString = #filePath, line: UInt = #line) {
        XCTAssertThrowsError(try body(), file: file, line: line) { XCTAssertEqual(($0 as? JedError)?.code, code, file: file, line: line) }
    }
    private func table(_ text: String, snapshot: inout DomainSnapshot, blob: Bool = false) throws {
        let name = try name(text), id = try self.name("id")
        var columns = [CatalogColumn(name: id, type: CatalogTypeUse(.scalar(.i64)), notNull: true)]
        if blob { columns.append(CatalogColumn(name: try self.name("b"), type: CatalogTypeUse(.scalar(.bytea)))) }
        var image = snapshot.catalog
        image.tables.append(CatalogTable(name: name, columns: columns, primaryKey: [0], root: PageSealer.nullReference(snapshot.pool.codec)))
        try snapshot.updateCatalog(image, schemaChanged: true)
        snapshot.setTableStore(name, snapshot.emptyStore(types: columns.map { TypeDescriptor($0.type.type == .scalar(.i64) ? .scalar(.i64) : .scalar(.bytea)) }))
    }
    private func put(_ id: Int64, table: String, snapshot: inout DomainSnapshot, blob: [UInt8]? = nil) throws {
        let name = try name(table)
        var store = try XCTUnwrap(snapshot.tables[name]), values: [RecordInput] = [.value(.integer(id))]
        if let blob { values.append(.value(.bytea(blob))) }
        let record = try RecordDisposition.plan(key: key(id), values: values, types: store.tree.types, codec: store.tree.codec)
        try store.put(record, netStaging: true); snapshot.setTableStore(name, store)
    }
    private func commit(_ working: DomainSnapshot, to domain: StorageDomain,
                        policy: DomainCommitPolicy = DomainCommitPolicy(), entropy: KeyEntropy? = nil) throws -> DomainSnapshot {
        let plan = try XCTUnwrap(domain.prepare(working, policy: policy, entropy: entropy, verifyLiveCount: true))
        return try domain.persist(plan)
    }

    func testEveryOrdinaryFrozenImageOpensAndCommitsIndependentRoots() throws {
        let paths = GeneratedSpec.sourceHashes.keys.filter {
            $0.hasPrefix("spec/fileformat/fixtures/") && !$0.contains("/cow_") && !$0.contains("/malformed_")
        }.sorted()
        XCTAssertEqual(paths.count, 87)
        for path in paths {
            let name = String(path.split(separator: "/").last!.dropLast(4))
            do {
                let bytes = try fixture(name), device = MemoryBlockStore(), encrypted = name.hasPrefix("encrypted_")
                try device.write(at: 0, bytes: bytes)
                let credential = encrypted ? try Credential.hostKey(Array(repeating: 0x4a, count: 32)) : nil
                let domain = try StorageDomain.open(store: device, catalogID: 1, cacheBytes: 1024, credential: credential, validateCatalog: { _ in })
                let before = domain.committed
                XCTAssertNil(try domain.prepare(before))
                var working = before
                // An explicit metadata revision exercises the commit layer
                // without pretending to execute a SQL statement.
                try working.updateCatalog(working.catalog, schemaChanged: false)
                var nonce: UInt64 = 1
                let entropy: KeyEntropy = { count in
                    XCTAssertEqual(count, 24); var bytes = Array(repeating: UInt8(0x72), count: 16)
                    var writer = ByteWriter(); writer.u64(nonce); nonce += 1; bytes += writer.bytes; return bytes
                }
                let after = try commit(working, to: domain, entropy: encrypted ? entropy : nil)
                XCTAssertEqual(after.txid, before.txid + 1, name)
                for table in before.catalog.tables {
                    XCTAssertEqual(after.catalog.tables.first(where: { $0.name == table.name })?.root, table.root, name)
                    for index in table.indexes where index.kind != 2 {
                        let rewritten = after.catalog.tables.first(where: { $0.name == table.name })!.indexes.first(where: { $0.name == index.name })!
                        XCTAssertEqual(rewritten.root, index.root, name)
                    }
                }
                let reopened = try StorageDomain.open(store: device, catalogID: 1, cacheBytes: 1024, credential: credential, validateCatalog: { _ in })
                XCTAssertEqual(reopened.committed.txid, after.txid, name)
                XCTAssertEqual(reopened.livePages, domain.livePages, name)
                XCTAssertLessThanOrEqual(domain.pool.residentLeaves, max(1, 1024 / domain.pager.pageSize))
            } catch { XCTFail("\(name): \(error)") }
        }
    }

    func testMalformedPersistedCountFailsPreflightWithoutWritesOrImageOutput() throws {
        let device = DomainTestDevice()
        try device.write(at: 0, bytes: fixture("malformed_row_count"))
        let domain = try StorageDomain.open(store: device, catalogID: 111, validateCatalog: { _ in })
        let name = try name("t")
        var working = domain.committed, store = try XCTUnwrap(working.tables[name])
        XCTAssertEqual(store.count, 25)
        var cursor = store.cursor(), removed = 0
        while let record = try cursor.next() { try store.remove(record.key); removed += 1 }
        XCTAssertEqual(removed, 20); XCTAssertEqual(store.count, 5); XCTAssertTrue(store.tree.isEmpty)
        working.setTableStore(name, store)
        device.reset()
        expect("XX001") { _ = try domain.prepare(working) }
        XCTAssertFalse(domain.pager.isPoisoned)
        XCTAssertTrue(device.log.allSatisfy { $0 == "read" })
        var emitted = 0
        expect("XX001") { _ = try DomainImage.write(working, txid: working.txid + 1) { _ in emitted += 1 } }
        XCTAssertEqual(emitted, 0)
        XCTAssertEqual(domain.committed.tables[name]?.count, 25)
    }

    func testOversizedPendingIndexFailsPreflightBeforeStorageAdmissionOrWrites() throws {
        let device = DomainTestDevice(), domain = try StorageDomain.create(store: device, kind: .file,
            catalogID: 112, pageSize: 256)
        var working = domain.committed
        try table("a", snapshot: &working); try put(1, table: "a", snapshot: &working)
        let before = try commit(working, to: domain), tableName = try name("a"), indexName = try name("a_id")
        working = before
        var image = working.catalog
        image.tables[0].indexes.append(CatalogIndex(name: indexName, keys: [.column(0)],
            root: PageSealer.nullReference(domain.pool.codec)))
        try working.updateCatalog(image, schemaChanged: true)
        var index = working.emptyStore(types: [])
        let oversized = PackedRecord(key: [UInt8](repeating: 0x11, count: domain.pool.codec.capacity + 1), cells: [])
        try index.put(oversized, netStaging: true); working.setIndexStore(indexName, index)
        XCTAssertEqual(working.pendingBytes, Int64(oversized.key.count))
        let oldBytes = try device.read(at: 0, count: Int(device.size()))
        let oldMeta = domain.pager.selected.meta, oldFree = try domain.pager.allocationSnapshot(reuseAllowed: true).persistedFree
        device.reset()
        var policy = DomainCommitPolicy(); policy.storageLimit = 1
        expect("0A000") { _ = try domain.prepare(working, policy: policy) }
        XCTAssertFalse(domain.pager.isPoisoned)
        XCTAssertTrue(device.log.allSatisfy { $0 == "read" })
        XCTAssertEqual(domain.pager.selected.meta.pageCount, oldMeta.pageCount)
        XCTAssertEqual(domain.pager.selected.meta.txid, oldMeta.txid)
        XCTAssertEqual(try domain.pager.allocationSnapshot(reuseAllowed: true).persistedFree, oldFree)
        XCTAssertEqual(try device.read(at: 0, count: Int(device.size())), oldBytes)
        XCTAssertEqual(domain.committed.tables[tableName]?.count, 1)
        XCTAssertNil(domain.committed.indexes[indexName])
        var emitted = 0
        expect("0A000") { _ = try DomainImage.write(working, txid: working.txid + 1) { _ in emitted += 1 } }
        XCTAssertEqual(emitted, 0)
    }

    func testFreshFileReclaimsBootstrapPageBeforeSecondAllocation() throws {
        let device = MemoryBlockStore()
        let fresh = try StorageDomain.create(store: device, kind: .file, catalogID: 100, pageSize: 1024)
        var first = fresh.committed
        try table("a", snapshot: &first)
        first = try commit(first, to: fresh)
        XCTAssertEqual(fresh.pager.selected.meta.root.index, 3)
        XCTAssertEqual(fresh.pager.selected.meta.pageCount, 4)
        XCTAssertEqual(fresh.pager.selected.meta.freeList.index, 0)
        XCTAssertEqual(try fresh.pager.allocationSnapshot(reuseAllowed: true).persistedFree, [2])

        // The one-page residue intentionally has no on-disk free-list page.
        // Reopening the same durable bytes must not manufacture that residue.
        let reopenedDevice = MemoryBlockStore()
        try reopenedDevice.write(at: 0, bytes: device.read(at: 0, count: Int(device.size())))
        let reopened = try StorageDomain.open(store: reopenedDevice, catalogID: 101, validateCatalog: { _ in })
        XCTAssertEqual(try reopened.pager.allocationSnapshot(reuseAllowed: true).persistedFree, [])
        var reopenedSecond = reopened.committed
        try table("b", snapshot: &reopenedSecond)
        _ = try commit(reopenedSecond, to: reopened)
        XCTAssertEqual(reopened.pager.selected.meta.root.index, 4)
        XCTAssertEqual(reopened.pager.selected.meta.pageCount, 5)

        var second = first
        try table("b", snapshot: &second)
        _ = try commit(second, to: fresh)
        XCTAssertEqual(fresh.pager.selected.meta.root.index, 2)
        XCTAssertEqual(fresh.pager.selected.meta.pageCount, 4)
    }

    func testNoopHasNoIOAndNewStoresUseLazyPaging() throws {
        let device = DomainTestDevice(), domain = try StorageDomain.create(store: device, kind: .file, catalogID: 2, pageSize: 256, cacheBytes: 256)
        device.reset(); XCTAssertNil(try domain.prepare(domain.committed)); XCTAssertTrue(device.log.isEmpty)
        var working = domain.committed
        try table("a", snapshot: &working); try table("b", snapshot: &working)
        for id in 0..<120 { try put(Int64(id), table: "a", snapshot: &working) }
        try put(9, table: "b", snapshot: &working)
        let first = try commit(working, to: domain), a = try name("a"), b = try name("b")
        XCTAssertTrue(first.tables[a]!.tree.pool === domain.pool)
        let untouched = first.tables[b]!.tree.root?.reference
        var oldCursor = first.tables[a]!.cursor(), changed = first
        var edited = changed.tables[a]!
        try edited.remove(key(5)); changed.setTableStore(a, edited)
        var policy = DomainCommitPolicy(); policy.oldestPinnedVersion = first.txid
        let next = try commit(changed, to: domain, policy: policy)
        XCTAssertEqual(next.tables[b]!.tree.root?.reference, untouched)
        XCTAssertNotNil(try first.tables[a]!.get(key(5)))
        XCTAssertNil(try next.tables[a]!.get(key(5)))
        var rows = 0; while try oldCursor.next() != nil { rows += 1 }
        XCTAssertEqual(rows, 120); XCTAssertEqual(next.tables[a]!.count, 119)
        XCTAssertLessThanOrEqual(domain.pool.residentLeaves, 1)
        device.reset(); XCTAssertNil(try domain.prepare(next)); XCTAssertTrue(device.log.isEmpty)
    }

    func testPendingLedgerNetSavepointAndGINRunOwnership() throws {
        let codec = try PlainPageCodec(pageSize: 256), type = TypeDescriptor(.scalar(.i64))
        let record = try RecordDisposition.plan(key: key(1), values: [.value(.integer(1))], types: [type], codec: codec)
        var store = TableStore(tree: BPlusTree(codec: codec, types: [type]))
        try store.put(record, netStaging: true); XCTAssertEqual(store.pendingBytes, 16)
        try store.put(record, netStaging: true); XCTAssertEqual(store.pendingBytes, 16)
        let saved = store; store.forgetPendingKeys()
        try store.put(record, netStaging: true); XCTAssertEqual(store.pendingBytes, 32)
        try store.remove(record.key); XCTAssertEqual(store.pendingBytes, 16)
        XCTAssertNotNil(try saved.get(record.key)); XCTAssertEqual(saved.pendingBytes, 16)
        store = saved; store.forgetPendingKeys(); try store.remove(record.key); XCTAssertEqual(store.pendingBytes, 16)
        store.clearPending(); XCTAssertEqual(store.pendingBytes, 0)
        var gin = TableStore(tree: BPlusTree(codec: codec, types: []))
        for id in 0..<30 { try gin.editGIN(term: [1, 2, 3], key: key(Int64(id)), insert: true, netStaging: true) }
        var cursor = gin.cursor(), expected: Int64 = 0
        while let row = try cursor.next() { expected += Int64(row.key.count) }
        XCTAssertEqual(gin.pendingBytes, expected)
        for id in 0..<30 { try gin.editGIN(term: [1, 2, 3], key: key(Int64(id)), insert: false, netStaging: true) }
        XCTAssertEqual(gin.pendingBytes, 0); XCTAssertTrue(gin.tree.isEmpty)
    }

    func testMemoryLimitForcedCompactionAndRepairStayBeforeWrites() throws {
        let device = DomainTestDevice(), domain = try StorageDomain.create(store: device, catalogID: 3, pageSize: 256)
        XCTAssertEqual(domain.committed.txid, 0)
        var working = domain.committed; try table("t", snapshot: &working)
        var initialLimit = DomainCommitPolicy(); initialLimit.storageLimit = 1
        device.reset(); expect("54P06") { _ = try domain.prepare(working, policy: initialLimit) }
        XCTAssertFalse(device.log.contains("write"))
        try put(1, table: "t", snapshot: &working)
        _ = try commit(working, to: domain)
        XCTAssertEqual(domain.pager.selected.meta.pageCount, 5)
        var policy = DomainCommitPolicy(); policy.storageLimit = 6 * 256
        working = domain.committed; try put(1, table: "t", snapshot: &working)
        let freeBeforePrecheck = try domain.pager.allocationSnapshot(reuseAllowed: true).persistedFree
        device.reset(); try domain.precheck(working, policy: policy)
        XCTAssertFalse(device.log.contains("write")); XCTAssertFalse(device.log.contains("sync"))
        let freeAfterPrecheck = try domain.pager.allocationSnapshot(reuseAllowed: true).persistedFree
        XCTAssertGreaterThan(freeAfterPrecheck.count, freeBeforePrecheck.count)
        XCTAssertEqual(domain.pager.selected.meta.pageCount, 5)
        XCTAssertEqual(domain.committed.txid, 1)
        _ = try commit(working, to: domain, policy: policy)
        XCTAssertEqual(domain.pager.selected.meta.pageCount, 6)
        for _ in 0..<12 {
            working = domain.committed; try put(1, table: "t", snapshot: &working)
            _ = try commit(working, to: domain, policy: policy)
            XCTAssertLessThanOrEqual(domain.pager.selected.meta.pageCount, 6)
        }
        XCTAssertTrue(domain.pager.selected.entries.isEmpty); XCTAssertEqual(domain.pager.selected.meta.freeList.index, 0)
        policy.storageLimit = 1
        working = domain.committed
        for id in 2..<30 { try put(Int64(id), table: "t", snapshot: &working) }
        device.reset(); expect("54P06") { _ = try domain.prepare(working, policy: policy) }
        XCTAssertFalse(device.log.contains("write")); XCTAssertFalse(domain.pager.isPoisoned)
        working = domain.committed
        let t = try name("t"); var emptied = working.tables[t]!
        try emptied.remove(key(1)); working.setTableStore(t, emptied)
        _ = try commit(working, to: domain, policy: policy)
        XCTAssertTrue(domain.committed.tables[t]!.tree.isEmpty)
    }

    func testRejectedMemoryCommitRetainsCompactionForUnlimitedRetry() throws {
        let device = DomainTestDevice(), domain = try StorageDomain.create(store: device, catalogID: 112, pageSize: 256)
        var working = domain.committed
        try table("t", snapshot: &working); try put(1, table: "t", snapshot: &working)
        _ = try commit(working, to: domain)
        working = domain.committed; try put(1, table: "t", snapshot: &working)
        _ = try commit(working, to: domain)
        let before = domain.committed, highWater = domain.pager.selected.meta.pageCount
        XCTAssertEqual(highWater, 7)
        XCTAssertTrue(try domain.pager.allocationSnapshot(reuseAllowed: true).persistedFree.isEmpty)
        let bytes = try device.memory.read(at: 0, count: Int(device.memory.size()))
        working = before
        for id in 2...60 { try put(Int64(id), table: "t", snapshot: &working) }
        var limited = DomainCommitPolicy(); limited.storageLimit = 1; limited.oldestPinnedVersion = before.txid
        device.reset()
        expect("54P06") { _ = try domain.prepare(working, policy: limited) }
        XCTAssertTrue(device.log.allSatisfy { $0 == "read" })
        XCTAssertFalse(domain.pager.isPoisoned)
        XCTAssertEqual(domain.committed.txid, before.txid)
        XCTAssertEqual(domain.pager.selected.meta.pageCount, highWater)
        XCTAssertEqual(try device.memory.read(at: 0, count: Int(device.memory.size())), bytes)
        let reclaimed = try domain.pager.allocationSnapshot(reuseAllowed: true).persistedFree
        XCTAssertEqual(reclaimed, [2, 3, 4])
        XCTAssertEqual(domain.freeGeneration, before.txid)
        // Clearing the limit must use the already reclaimed pages, even though
        // unlimited admission does not itself run a forced reachability walk.
        let retry = try XCTUnwrap(domain.prepare(working))
        let written = Set(retry.pages.body.map(\.index))
        XCTAssertTrue(Set(reclaimed).isSubset(of: written))
        XCTAssertEqual(retry.pages.manifest.meta.pageCount,
            highWater + UInt32(retry.pages.body.count - reclaimed.count))
        _ = try domain.persist(retry)
        let name = try name("t")
        XCTAssertEqual(domain.committed.tables[name]?.count, 60)
        XCTAssertEqual(before.tables[name]?.count, 1)
        XCTAssertNotNil(try before.tables[name]?.get(key(1)))
        XCTAssertNil(try before.tables[name]?.get(key(60)))
    }

    func testOverflowSharingAndLivePageDeltaAcrossPinnedReclamation() throws {
        let domain = try StorageDomain.create(store: MemoryBlockStore(), kind: .file, catalogID: 4, pageSize: 256, cacheBytes: 256)
        var working = domain.committed; try table("t", snapshot: &working, blob: true)
        var seed: UInt64 = 123, blob: [UInt8] = []
        for _ in 0..<1300 { seed = seed &* 6364136223846793005 &+ 1; blob.append(UInt8(truncatingIfNeeded: seed >> 32)) }
        try put(1, table: "t", snapshot: &working, blob: blob)
        let pinned = try commit(working, to: domain), t = try name("t"), pinnedRecord = try XCTUnwrap(pinned.tables[t]!.get(key(1)))
        let pointer = try XCTUnwrap(pinnedRecord.cells[1].largeFacts(encrypted: false)?.external)
        let originalLive = domain.livePages
        for ordinal in 0..<45 {
            working = domain.committed; var store = working.tables[t]!
            let record = try XCTUnwrap(store.get(key(1)))
            let replaced = try RecordDisposition.plan(key: record.key, values: [.value(.integer(Int64(ordinal))), .kept(record.cells[1])],
                types: store.tree.types, codec: store.tree.codec, pager: domain.pager)
            try store.put(replaced, netStaging: true); working.setTableStore(t, store)
            var policy = DomainCommitPolicy(); policy.oldestPinnedVersion = pinned.txid
            _ = try commit(working, to: domain, policy: policy)
            XCTAssertEqual(domain.livePages, originalLive)
            XCTAssertEqual(try domain.committed.tables[t]!.get(key(1))!.cells[1].largeFacts(encrypted: false)?.external, pointer)
        }
        guard case .bytea(let restored) = try pinnedRecord.materialize(column: 1, pager: domain.pager) else { return XCTFail("expected bytea") }
        XCTAssertEqual(restored, blob)
        working = domain.committed; var store = working.tables[t]!
        try store.remove(key(1)); working.setTableStore(t, store)
        _ = try commit(working, to: domain)
        XCTAssertEqual(domain.livePages, 1)
        XCTAssertGreaterThan(try domain.pager.allocationSnapshot(reuseAllowed: true).persistedFree.count, 0)
    }

    func testResidentRootRewritesFreshChainsUntilReload() throws {
        let device = MemoryBlockStore(), domain = try StorageDomain.create(store: device, kind: .file, catalogID: 40, pageSize: 256)
        var working = domain.committed
        try table("t", snapshot: &working, blob: true)
        var seed: UInt64 = 901, blob: [UInt8] = []
        for _ in 0..<600 { seed = seed &* 6364136223846793005 &+ 1; blob.append(UInt8(truncatingIfNeeded: seed >> 32)) }
        for id: Int64 in 1...3 { try put(id, table: "t", snapshot: &working, blob: blob) }
        _ = try commit(working, to: domain)
        let t = try name("t"), old = domain.committed
        guard case .resident(let root) = old.tables[t]!.tree.root else { return XCTFail("root leaf must stay resident") }
        XCTAssertTrue(root.isLeaf)
        XCTAssertNotNil(try old.tables[t]!.get(key(1))!.overflows[1])
        working = old; var store = working.tables[t]!
        try store.remove(key(3)); working.setTableStore(t, store)
        let plan = try XCTUnwrap(domain.prepare(working, verifyLiveCount: true))
        XCTAssertEqual(plan.pages.body.filter { $0.bytes[0] == 4 }.count, 6)
        _ = try domain.persist(plan)
        guard case .bytea(let pinned) = try old.tables[t]!.get(key(3))!.materialize(column: 1, pager: domain.pager) else {
            return XCTFail("pinned overflow value changed kind")
        }
        XCTAssertEqual(pinned, blob)
        let reopened = try StorageDomain.open(store: device, catalogID: 40, validateCatalog: { _ in })
        working = reopened.committed; store = working.tables[t]!
        XCTAssertTrue(try store.get(key(1))!.overflows.isEmpty)
        try store.remove(key(2)); working.setTableStore(t, store)
        let inherited = try XCTUnwrap(reopened.prepare(working, verifyLiveCount: true))
        XCTAssertEqual(inherited.pages.body.filter { $0.bytes[0] == 4 }.count, 0)
        _ = try reopened.persist(inherited)
        guard case .bytea(let recovered) = try reopened.committed.tables[t]!.get(key(1))!.materialize(column: 1, pager: reopened.pager) else {
            return XCTFail("reopened overflow value changed kind")
        }
        XCTAssertEqual(recovered, blob)
    }

    func testMultiDomainPreflightAndFailureLeaveOtherDevicesUntouched() throws {
        let aDevice = DomainTestDevice(), bDevice = DomainTestDevice()
        let a = try StorageDomain.create(store: aDevice, kind: .file, catalogID: 5, pageSize: 256)
        let b = try StorageDomain.create(store: bDevice, kind: .file, catalogID: 6, pageSize: 256)
        var aw = a.committed, bw = b.committed
        try table("t", snapshot: &aw); try put(1, table: "t", snapshot: &aw)
        try table("t", snapshot: &bw); try put(1, table: "t", snapshot: &bw)
        aDevice.reset(); bDevice.reset()
        try a.precheck(aw, policy: DomainCommitPolicy())
        var policy = DomainCommitPolicy(); policy.storageLimit = 1; policy.domainName = try name("other")
        expect("54P06") { try b.precheck(bw, policy: policy) }
        XCTAssertFalse(aDevice.log.contains("write")); XCTAssertFalse(bDevice.log.contains("write"))
        XCTAssertEqual(a.committed.txid, 1); XCTAssertEqual(b.committed.txid, 1)
        let prepared = try XCTUnwrap(a.prepare(aw))
        a.pager.armFault(.body(ordinal: 1, tearBytes: 0))
        expect("58030") { _ = try a.persist(prepared) }
        XCTAssertTrue(a.pager.isPoisoned); XCTAssertEqual(a.committed.txid, 1)
        XCTAssertFalse(b.pager.isPoisoned)
    }

    func testNominalIdentitiesSurviveCatalogReorderingAndSavepointCopies() throws {
        let domain = try StorageDomain.create(store: MemoryBlockStore(), catalogID: 7)
        var working = domain.committed, image = working.catalog
        let z = try name("z"), a = try name("a"), x = try name("x")
        image.composites = [.init(name: z, fields: [.init(name: x, type: .init(.scalar(.i64)))])]
        try working.updateCatalog(image, schemaChanged: true)
        let old = try XCTUnwrap(working.types[z]), saved = working
        image.composites.append(.init(name: a, fields: [.init(name: x, type: .init(.scalar(.i32)))]))
        try working.updateCatalog(image, schemaChanged: true)
        XCTAssertEqual(working.types[z], old); XCTAssertNotEqual(working.types[a]?.id, old.id)
        XCTAssertNil(saved.types[a])
        image.composites.removeAll { $0.name == z }; try working.updateCatalog(image, schemaChanged: true)
        image.composites.append(.init(name: z, fields: [.init(name: x, type: .init(.scalar(.i64)))]))
        try working.updateCatalog(image, schemaChanged: true)
        XCTAssertNotEqual(working.types[z], old)
    }
    func testMemoryAttachmentStageAbandonsAccountingAfterMainFailureAndAdoptsWithoutIO() throws {
        let device = DomainTestDevice(), memory = try StorageDomain.create(store: device, catalogID: 8, pageSize: 256)
        var working = memory.committed; try table("t", snapshot: &working); try put(1, table: "t", snapshot: &working)
        _ = try commit(working, to: memory)
        let before = memory.committed, high = memory.pager.selected.meta.pageCount
        let free = try memory.pager.allocationSnapshot(reuseAllowed: true).persistedFree
        let oldMeta = try device.read(at: 0, count: 512)
        working = before; try put(2, table: "t", snapshot: &working)
        let plan = try XCTUnwrap(memory.prepare(working, verifyLiveCount: true))
        device.reset()
        let abandoned = try memory.stageMemory(plan)
        XCTAssertTrue(device.log.contains("write")); XCTAssertFalse(device.log.contains("sync"))
        XCTAssertEqual(memory.committed.txid, before.txid); XCTAssertEqual(memory.pager.selected.meta.pageCount, high)
        XCTAssertEqual(try memory.pager.allocationSnapshot(reuseAllowed: true).persistedFree, free)
        XCTAssertEqual(try device.read(at: 0, count: 512), oldMeta)
        let t = try name("t")
        XCTAssertNil(try memory.committed.tables[t]!.get(key(2)))

        let main = try StorageDomain.create(store: MemoryBlockStore(), kind: .file, catalogID: 9, pageSize: 256)
        var mainWorking = main.committed; try table("m", snapshot: &mainWorking); try put(1, table: "m", snapshot: &mainWorking)
        let mainPlan = try XCTUnwrap(main.prepare(mainWorking))
        main.pager.armFault(.body(ordinal: 1, tearBytes: 0))
        expect("58030") { _ = try main.persist(mainPlan) }
        XCTAssertFalse(memory.pager.isPoisoned); XCTAssertEqual(memory.committed.txid, before.txid)
        XCTAssertNotNil(try before.tables[t]!.get(key(1)))

        // A new transaction may overwrite the abandoned destination slots.
        // Its unique stage token prevents adoption of the superseded bytes.
        working = before; try put(3, table: "t", snapshot: &working)
        let retry = try memory.stageMemory(XCTUnwrap(memory.prepare(working, verifyLiveCount: true)))
        device.reset()
        expect("55006") { _ = try memory.adoptMemory(abandoned) }
        let adopted = try memory.adoptMemory(retry)
        XCTAssertTrue(device.log.isEmpty)
        XCTAssertEqual(adopted.txid, before.txid + 1)
        XCTAssertNil(try adopted.tables[t]!.get(key(2))); XCTAssertNotNil(try adopted.tables[t]!.get(key(3)))
        expect("55006") { _ = try memory.adoptMemory(retry) }
    }

}
