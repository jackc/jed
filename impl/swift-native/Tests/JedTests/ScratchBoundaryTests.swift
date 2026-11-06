import Foundation
import Synchronization
import XCTest
@testable import Jed

private final class ScratchTestStore: ScratchStore {
    let memory = MemoryBlockStore()
    private let state = Mutex(false)
    var closed: Bool { state.withLock { $0 } }
    func read(at offset: UInt64, count: Int) throws -> [UInt8] { try memory.read(at: offset, count: count) }
    func write(at offset: UInt64, bytes: [UInt8]) throws { try memory.write(at: offset, bytes: bytes) }
    func size() throws -> UInt64 { try memory.size() }
    func setSize(_ bytes: UInt64) throws { try memory.setSize(bytes) }
    func sync() throws { XCTFail("scratch issued a barrier") }
    func close() { state.withLock { $0 = true } }
}
private final class ScratchTestHost: ScratchFileHost {
    let sealed = true
    let file = ScratchTestStore()
    func create(kind: String) throws -> any ScratchStore {
        try SealedScratchStore(inner: file, random: SeededRandomSource(seed: 55))
    }
}

final class ScratchBoundaryTests: XCTestCase {
    private func expectIO(_ body: () throws -> Void, file: StaticString = #filePath, line: UInt = #line) {
        XCTAssertThrowsError(try body(), file: file, line: line) {
            XCTAssertEqual(($0 as? JedError)?.code, "58030", file: file, line: line)
        }
    }
    func testSealedRandomAccessHolesAndTruncateNeverResetReplayCounters() throws {
        let inner = ScratchTestStore(), random = SeededRandomSource(seed: 16)
        let store = try SealedScratchStore(inner: inner, random: random)
        try store.setSize(16_384)
        XCTAssertEqual(try store.read(at: 4094, count: 6), [0, 0, 0, 0, 0, 0])
        try store.write(at: 4094, bytes: [1, 2, 3, 4, 5, 6])
        XCTAssertEqual(try store.read(at: 4094, count: 6), [1, 2, 3, 4, 5, 6])
        let old = try inner.read(at: 4136, count: 4136)
        try store.setSize(4097)
        try store.setSize(16_384)
        XCTAssertEqual(try store.read(at: 4094, count: 6), [1, 2, 3, 0, 0, 0])
        try store.setSize(4096); try store.setSize(8192)
        XCTAssertEqual(try store.read(at: 4096, count: 4), [0, 0, 0, 0])
        try store.write(at: 4096, bytes: [9, 8, 7, 6])
        let current = try inner.read(at: 4136, count: 4136)
        try inner.write(at: 4136, bytes: old)
        expectIO { _ = try store.read(at: 4096, count: 4) }
        try inner.write(at: 4136, bytes: current)
        XCTAssertEqual(try store.read(at: 4096, count: 4), [9, 8, 7, 6])
        try store.sync(); try store.close(); try store.close()
        XCTAssertTrue(inner.closed)
        expectIO { _ = try store.read(at: 0, count: 1) }
    }
    func testSealedAADRejectsMovedBlockAndCrossFileReplay() throws {
        let a = ScratchTestStore(), b = ScratchTestStore()
        let first = try SealedScratchStore(inner: a, random: SeededRandomSource(seed: 1))
        let second = try SealedScratchStore(inner: b, random: SeededRandomSource(seed: 2))
        try first.write(at: 0, bytes: Array(repeating: 0x61, count: 8192))
        try second.write(at: 0, bytes: Array(repeating: 0x62, count: 4096))
        let block = try a.read(at: 0, count: 4136)
        XCTAssertFalse(block[..<4096].allSatisfy { $0 == 0x61 })
        try a.write(at: 4136, bytes: block)
        expectIO { _ = try first.read(at: 4096, count: 1) }
        try b.write(at: 0, bytes: block)
        expectIO { _ = try second.read(at: 0, count: 1) }
    }
    func testScratchMigrationPreservesEveryLogicalByteAndThreshold() throws {
        let host = ScratchTestHost(), store = try SpillableScratchStore(budget: 8192, host: host)
        let values = try SeededRandomSource(seed: 8).bytes(count: 20_000)
        try store.write(at: 0, bytes: values)
        try store.spillIfNeeded(committedBytes: 8192); XCTAssertFalse(store.spilled)
        try store.spillIfNeeded(committedBytes: 8193); XCTAssertTrue(store.spilled)
        XCTAssertEqual(try store.read(at: 0, count: values.count), values)
        try store.write(at: 4095, bytes: [1, 2, 3]); XCTAssertEqual(try store.read(at: 4095, count: 3), [1, 2, 3])
        try store.setSize(4096); try store.setSize(8192)
        XCTAssertEqual(try store.read(at: 4095, count: 5), [1, 0, 0, 0, 0])
        try store.sync(); try store.close(); XCTAssertTrue(host.file.closed)
    }
    #if os(Linux) || canImport(Darwin)
    func testNativeScratchIsUnlinkedAndRebuildActuallySpills() throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("jed-swift-scratch-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: directory) }
        let host = NativeScratchFileHost(directory: directory.path, sealed: true,
            names: SeededRandomSource(seed: 7), keys: SeededRandomSource(seed: 9))
        let store = try SpillableScratchStore(budget: 1024, host: host)
        let url = try XCTUnwrap(Bundle.module.url(forResource: "encrypted_index_table", withExtension: "jed", subdirectory: "Fixtures"))
        let sourceStore = MemoryBlockStore(); try sourceStore.write(at: 0, bytes: Array(Data(contentsOf: url)))
        let source = try StorageDomain.open(store: sourceStore, catalogID: 83,
            credential: .hostKey(Array(repeating: 0x4a, count: 32)), validateCatalog: { _ in })
        var scratch = try ScratchDomain.create(store: store, codec: PlainPageCodec(pageSize: 1024), cacheBytes: 1024)
        let target = try DomainRebuilder.rebuild(source.committed, scratch: &scratch, stageBytes: 256)
        XCTAssertTrue(store.spilled)
        XCTAssertEqual(try FileManager.default.contentsOfDirectory(atPath: directory.path), [])
        XCTAssertEqual(target.tables.values.first?.count, 150)
        var cursor = target.tables.values.first!.cursor(), count = 0
        while try cursor.next() != nil { count += 1 }
        XCTAssertEqual(count, 150); XCTAssertLessThanOrEqual(target.pool.residentLeaves, 1)
        try store.close()
    }
    #endif
}
