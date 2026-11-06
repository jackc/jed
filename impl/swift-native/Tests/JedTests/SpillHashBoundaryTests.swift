import Synchronization
import XCTest
@testable import Jed

private final class SpillHashTestStore: ScratchStore {
    let memory = MemoryBlockStore()
    let closes = Mutex(0)
    let failReads = Mutex(false)
    func read(at offset: UInt64, count: Int) throws -> [UInt8] {
        if failReads.withLock({ $0 }) { throw JedError("58030", "injected scratch read failure") }
        return try memory.read(at: offset, count: count)
    }
    func write(at offset: UInt64, bytes: [UInt8]) throws { try memory.write(at: offset, bytes: bytes) }
    func size() throws -> UInt64 { try memory.size() }
    func setSize(_ bytes: UInt64) throws { try memory.setSize(bytes) }
    func sync() throws { XCTFail("operator scratch issued a durability barrier") }
    func close() { closes.withLock { $0 += 1 } }
}
private final class SpillHashTestHost: ScratchFileHost {
    let sealed: Bool
    let files = Mutex<[SpillHashTestStore]>([])
    let failCreate: Int
    init(sealed: Bool = false, failCreate: Int = 0) { self.sealed = sealed; self.failCreate = failCreate }
    func create(kind: String) throws -> any ScratchStore {
        if files.withLock({ $0.count + 1 }) == failCreate { throw JedError("58030", "injected scratch create failure") }
        let store = SpillHashTestStore()
        let index = files.withLock { $0.append(store); return $0.count }
        if sealed { return try SealedScratchStore(inner: store, random: SeededRandomSource(seed: UInt64(index))) }
        return store
    }
    func assertClosed(file: StaticString = #filePath, line: UInt = #line) {
        for store in files.withLock({ $0 }) { XCTAssertEqual(store.closes.withLock { $0 }, 1, file: file, line: line) }
    }
}

final class SpillHashBoundaryTests: XCTestCase {
    private func bytes(_ integer: Int) -> [UInt8] { var writer = ByteWriter(); writer.u64(UInt64(integer)); return writer.bytes }
    private func integer(_ row: [SQLValue]?) -> Int64? {
        guard let row, row.count == 1, case .integer(let value) = row[0] else { return nil }; return value
    }
    func testDiskHashSplitsFullCollisionsAndReusesReplacementBlocks() throws {
        let host = SpillHashTestHost(), map = try SpillHashStore(host: host)
        for i in 0..<300 { try map.put(bytes(i), value: bytes(i + 1000), hash: i < 150 ? 7 : UInt64(i)) }
        XCTAssertGreaterThan(map.bucketCount, 16); XCTAssertEqual(map.count, 300)
        for i in 0..<300 { XCTAssertEqual(try map.get(bytes(i), hash: i < 150 ? 7 : UInt64(i)), bytes(i + 1000)) }
        XCTAssertNil(try map.get(bytes(999), hash: 7))
        let large = Array(repeating: UInt8(19), count: 4000)
        try map.put(bytes(0), value: large, hash: 7)
        let peak = map.allocatedBytes
        for i in 0..<50 { try map.put(bytes(0), value: i % 2 == 0 ? bytes(i) : large, hash: 7) }
        XCTAssertEqual(map.allocatedBytes, peak)
        XCTAssertEqual(try map.get(bytes(0), hash: 7), large); XCTAssertEqual(map.count, 300)
        map.close(); map.close(); host.assertClosed()
    }
    func testStateMapChargesTupleStateAndEqualityKeysAcrossSpill() throws {
        let diagnostics = MemoryDiagnostics(), resources = StatementResources(queryLimit: 100_000, diagnostics: diagnostics)
        let meter = CostMeter(resources: resources), host = SpillHashTestHost()
        let key: [SQLValue] = [.decimal(try SQLDecimal.parse("1.00"))]
        let short: [SQLValue] = [.text(try SQLText("abcd"))]
        let initial = GeneratedSpec.memoryEntry + MemorySizes.value(key[0]) + MemorySizes.value(short[0])
        let map = SpillStateMap(config: SpillConfig(workMemoryBytes: initial + 2, host: host, meter: meter))
        try map.put(key, value: short); XCTAssertEqual(map.residentBytes, initial); XCTAssertEqual(resources.query.used, initial)
        try map.put([.decimal(SQLDecimal(1))], value: [.text(try SQLText("ab"))])
        XCTAssertEqual(resources.query.used, initial - 2)
        try map.put(key, value: [.text(try SQLText("abcdefgh"))])
        XCTAssertTrue(map.isSpilled); XCTAssertEqual(resources.query.used, 0)
        guard case .text(let value) = try XCTUnwrap(map.get([.decimal(SQLDecimal(1))]))[0] else { return XCTFail("state missing") }
        XCTAssertEqual(value.description, "abcdefgh")
        XCTAssertTrue(try map.insert([.null])); XCTAssertFalse(try map.insert([.null]))
        XCTAssertEqual(try map.get([.null])?.count, 0)
        XCTAssertEqual(resources.query.used, 0); map.close(); host.assertClosed(); XCTAssertEqual(diagnostics.underflows, 0)
    }
    func testHashRowsRetainHotKeyOrderWithFixedFileCountAndEarlyClose() throws {
        for sealed in [false, true] {
            let host = SpillHashTestHost(sealed: sealed), diagnostics = MemoryDiagnostics()
            let resources = StatementResources(queryLimit: 1, diagnostics: diagnostics)
            let table = HashRowTable(config: SpillConfig(workMemoryBytes: 4096, host: host, meter: CostMeter(resources: resources)))
            for i in 0..<180 { try table.append(hash: i % 3 == 0 ? 9 : 7, row: [.integer(Int64(i))]) }
            XCTAssertTrue(table.isSpilled); XCTAssertEqual(resources.query.used, 0); XCTAssertEqual(diagnostics.peak, 0)
            XCTAssertEqual(host.files.withLock { $0.count }, 3)
            let first = try table.cursor(hash: 7), second = try table.cursor(hash: 7), empty = try table.cursor(hash: 100)
            XCTAssertNil(try empty.next()); XCTAssertEqual(integer(try first.next()), 1); first.close()
            var result: [Int64] = []
            while let row = try second.next() { result.append(try XCTUnwrap(integer(row))) }
            XCTAssertEqual(result, (0..<180).filter { $0 % 3 != 0 }.map(Int64.init))
            let interrupted = try table.cursor(hash: 9); XCTAssertEqual(integer(try interrupted.next()), 0)
            table.close()
            XCTAssertThrowsError(try interrupted.next()) { XCTAssertEqual(($0 as? JedError)?.code, "58030") }
            host.assertClosed(); XCTAssertEqual(diagnostics.underflows, 0)
        }
    }
    func testHashFailuresPoisonReadersAndCleanPartialCreation() throws {
        let failedHost = SpillHashTestHost(failCreate: 2)
        XCTAssertThrowsError(try SpillHashStore(host: failedHost)) { XCTAssertEqual(($0 as? JedError)?.code, "58030") }
        failedHost.assertClosed()
        let diagnostics = MemoryDiagnostics(), resources = StatementResources(queryLimit: 1000, diagnostics: diagnostics)
        let host = SpillHashTestHost(), map = SpillStateMap(config: SpillConfig(workMemoryBytes: 1,
            host: host, meter: CostMeter(resources: resources)))
        try map.put([.integer(1)], value: [.integer(2)])
        XCTAssertEqual(resources.query.used, 0)
        host.files.withLock { $0[0] }.failReads.withLock { $0 = true }
        XCTAssertThrowsError(try map.get([.integer(1)])) { XCTAssertEqual(($0 as? JedError)?.code, "58030") }
        host.files.withLock { $0[0] }.failReads.withLock { $0 = false }
        XCTAssertThrowsError(try map.get([.integer(1)])) { XCTAssertEqual(($0 as? JedError)?.code, "58030") }
        map.close(); host.assertClosed(); XCTAssertEqual(diagnostics.underflows, 0)

        let malformedHost = SpillHashTestHost(), malformed = try SpillHashStore(host: malformedHost)
        try malformed.put([1], value: [2], hash: 7)
        // The first node lives after the reserved null offset. A forged key
        // length must fail before allocating/reading beyond its size class.
        var forged = ByteWriter(); forged.u64(UInt64.max)
        try malformedHost.files.withLock { $0[1] }.memory.write(at: 24, bytes: forged.bytes)
        XCTAssertThrowsError(try malformed.get([1], hash: 7)) { XCTAssertEqual(($0 as? JedError)?.code, "58030") }
        malformed.close(); malformedHost.assertClosed()

        let freeHost = SpillHashTestHost(), freeMap = try SpillHashStore(host: freeHost)
        try freeMap.put([1], value: [2], hash: 1)
        try freeMap.put([1], value: Array(repeating: 2, count: 100), hash: 1)
        let priorSize = freeMap.allocatedBytes
        // Replacing the first 64-byte node leaves a reusable block at offset8.
        // Corrupting its disk link must not permit an out-of-bounds allocation.
        try freeHost.files.withLock { $0[1] }.memory.write(at: 8, bytes: forged.bytes)
        XCTAssertThrowsError(try freeMap.put([2], value: [3], hash: 2)) { XCTAssertEqual(($0 as? JedError)?.code, "58030") }
        XCTAssertEqual(freeMap.allocatedBytes, priorSize)
        freeMap.close(); freeHost.assertClosed()
    }
    func testResidentHashRowsChargeEntriesAndReleaseOnce() throws {
        let diagnostics = MemoryDiagnostics(), resources = StatementResources(queryLimit: 10_000, diagnostics: diagnostics)
        let row: [SQLValue] = [.integer(3)], bytes = GeneratedSpec.memoryEntry + MemorySizes.row(row)
        let table = HashRowTable(config: SpillConfig(workMemoryBytes: 0, host: nil, meter: CostMeter(resources: resources)))
        try table.append(hash: 7, row: row); try table.append(hash: 7, row: row)
        XCTAssertEqual(resources.query.used, 2 * bytes)
        let cursor = try table.cursor(hash: 7); XCTAssertEqual(integer(try cursor.next()), 3)
        table.releaseCharge(); XCTAssertEqual(resources.query.used, 0)
        XCTAssertEqual(integer(try cursor.next()), 3); XCTAssertNil(try cursor.next())
        table.close(); XCTAssertEqual(diagnostics.underflows, 0)
    }
}
