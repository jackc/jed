import Foundation
import Synchronization
import XCTest
@testable import Jed

private final class SpillTestStore: ScratchStore {
    let memory = MemoryBlockStore()
    struct State { var closes = 0; var failWrites = false; var failReads = false; var maximumRead = 0; var maximumWrite = 0 }
    let state = Mutex(State())
    func read(at offset: UInt64, count: Int) throws -> [UInt8] {
        try state.withLock { value in
            if value.failReads { throw JedError("58030", "injected scratch read") }
            value.maximumRead = max(value.maximumRead, count)
        }
        return try memory.read(at: offset, count: count)
    }
    func write(at offset: UInt64, bytes: [UInt8]) throws {
        try state.withLock { value in
            if value.failWrites { throw JedError("58030", "injected scratch write") }
            value.maximumWrite = max(value.maximumWrite, bytes.count)
        }
        try memory.write(at: offset, bytes: bytes)
    }
    func size() throws -> UInt64 { try memory.size() }
    func setSize(_ bytes: UInt64) throws { try memory.setSize(bytes) }
    func sync() throws { XCTFail("operator scratch must not issue a durability barrier") }
    func close() { state.withLock { $0.closes += 1 } }
}
private final class SpillTestHost: ScratchFileHost {
    let sealed = false
    let store = SpillTestStore()
    let creates = Mutex(0)
    func create(kind: String) throws -> any ScratchStore { creates.withLock { $0 += 1 }; return store }
}

final class SpillBoundaryTests: XCTestCase {
    private func expect(_ code: String, _ body: () throws -> Void, file: StaticString = #filePath, line: UInt = #line) {
        XCTAssertThrowsError(try body(), file: file, line: line) {
            XCTAssertEqual(($0 as? JedError)?.code, code, file: file, line: line)
        }
    }
    private func integer(_ row: [SQLValue]?) -> Int64? {
        guard let row, row.count == 1, case .integer(let value) = row[0] else { return nil }; return value
    }
    func testPrivateCodecRetainsRepresentationsAndRejectsMalformedFrames() throws {
        let array = try SQLArray(dimensions: [2, 1], lowerBounds: [-4, 7], elements: [.integer(9), .null])
        let decimal = try SQLDecimal.parse("-12345678901234567890.12000")
        let json = SQLJSONValue.object([SQLJSONMember(key: try SQLText("key"), value: .array([.number(decimal), .null]))])
        let row: [SQLValue] = [.null, .integer(Int64.min), .boolean(true), .text(try SQLText("ée\u{301}x")),
            .decimal(decimal), .bytea([0, 255, 3]), .uuid(try SQLUUID(bytes: Array(0..<16))),
            .timestamp(Int64.min), .timestamptz(Int64.max), .date(Int32.min),
            .interval(SQLInterval(months: -1, days: 7, microseconds: -19)),
            .f32(0xffc01234), .f64(0x8000000000000000), .f64(0x7ff8000000001234),
            .composite([.null, .array(array)]), .array(array),
            .range(SQLRange(lower: .integer(2), upper: .integer(5), upperInclusive: true)),
            .range(SQLRange(empty: true)), .json(try SQLText(" { \"x\": 1 } ")),
            .jsonb(json), .jsonpath(try SQLText("strict $.x"))]
        let bytes = try SpillRowCodec.encode(row), decoded = try SpillRowCodec.decode(bytes)
        XCTAssertEqual(decoded.count, row.count)
        guard case .decimal(let restored) = decoded[4], case .f32(let single) = decoded[11],
              case .f64(let zero) = decoded[12], case .f64(let nan) = decoded[13],
              case .array(let restoredArray) = decoded[15], case .text(let text) = decoded[3],
              case .json(let raw) = decoded[18] else { return XCTFail("scratch changed a representation tag") }
        XCTAssertEqual(restored, decimal); XCTAssertEqual(single, 0xffc01234)
        XCTAssertEqual(zero, 0x8000000000000000); XCTAssertEqual(nan, 0x7ff8000000001234)
        XCTAssertEqual(restoredArray.dimensions, [2, 1]); XCTAssertEqual(restoredArray.lowerBounds, [-4, 7])
        XCTAssertEqual(text.utf8, Array("ée\u{301}x".utf8)); XCTAssertEqual(raw.description, " { \"x\": 1 } ")
        XCTAssertEqual(try SpillRowCodec.encode(decoded), bytes)
        for malformed in [Array(bytes.dropLast()), bytes + [0], Array(repeating: UInt8(255), count: 8), [1,0,0,0,0,0,0,0,255]] {
            expect("58030") { _ = try SpillRowCodec.decode(malformed) }
        }
        var nested: SQLValue = .integer(1)
        for _ in 0..<256 { nested = .composite([nested]) }
        let nestedBytes = try SpillRowCodec.encode([nested])
        XCTAssertEqual(try SpillRowCodec.encode(SpillRowCodec.decode(nestedBytes)), nestedBytes)
    }
    func testFramedRowsHaveIndependentBoundedReadersAndCloseOnce() throws {
        let host = SpillTestHost(), file = try SpillRowFile(host: host, kind: "test")
        let large = Array(repeating: UInt8(0xa9), count: 180_000)
        try file.append([.bytea(large)])
        let prefix = file.cursor()
        try file.append([.integer(2)])
        let all = file.cursor()
        for cursor in [prefix, all] {
            guard case .bytea(let value) = try XCTUnwrap(cursor.next())[0] else { return XCTFail("missing bytea") }
            XCTAssertEqual(value, large)
        }
        XCTAssertNil(try prefix.next()); prefix.close()
        XCTAssertEqual(integer(try all.next()), 2); XCTAssertNil(try all.next())
        XCTAssertLessThanOrEqual(host.store.state.withLock { $0.maximumRead }, 65_536)
        XCTAssertLessThanOrEqual(host.store.state.withLock { $0.maximumWrite }, 65_536)
        let stale = file.cursor(); try file.close(); try file.close()
        expect("58030") { _ = try stale.next() }
        XCTAssertEqual(host.store.state.withLock { $0.closes }, 1)
    }
    func testSpoolExactThresholdRejectionAndOutputChargeLifetime() throws {
        let row: [SQLValue] = [.integer(1)], bytes = MemorySizes.row(row)
        let diagnostics = MemoryDiagnostics(), resources = StatementResources(queryLimit: bytes * 3, diagnostics: diagnostics)
        let meter = CostMeter(resources: resources), host = SpillTestHost()
        let spool = RowSpool(config: SpillConfig(workMemoryBytes: bytes, host: host, meter: meter))
        try spool.append(row); XCTAssertFalse(spool.isSpilled); XCTAssertEqual(resources.query.used, bytes)
        try spool.append([.integer(2)]); XCTAssertTrue(spool.isSpilled); XCTAssertEqual(resources.query.used, 0)
        XCTAssertEqual(diagnostics.peak, bytes * 2)
        let a = try spool.cursor(), b = try spool.cursor()
        XCTAssertEqual(integer(try a.next()), 1); XCTAssertEqual(integer(try b.next()), 1)
        a.close(); XCTAssertEqual(integer(try b.next()), 2); b.close()
        let output = try spool.takeOutput(); XCTAssertEqual(integer(try output.next()), 1); output.close()
        XCTAssertEqual(host.store.state.withLock { $0.closes }, 1)
        XCTAssertEqual(diagnostics.underflows, 0)

        let rejectedHost = SpillTestHost(), rejectedResources = StatementResources(queryLimit: bytes)
        let rejected = RowSpool(config: SpillConfig(workMemoryBytes: bytes * 100, host: rejectedHost,
            meter: CostMeter(resources: rejectedResources)))
        try rejected.append(row); try rejected.append(row)
        XCTAssertTrue(rejected.isSpilled); XCTAssertEqual(rejectedResources.query.used, 0); rejected.close()
        let resident = RowSpool(config: SpillConfig(workMemoryBytes: 0, host: nil, meter: meter))
        try resident.append(row); let residentOutput = try resident.takeOutput()
        XCTAssertEqual(resources.query.used, bytes); residentOutput.close(); XCTAssertEqual(resources.query.used, 0)
    }
    func testSpoolFailureReleasesOnlyItsChargeAndNeverFallsBack() throws {
        let bytes = MemorySizes.row([.integer(1)]), diagnostics = MemoryDiagnostics()
        let resources = StatementResources(queryLimit: bytes * 4, diagnostics: diagnostics), meter = CostMeter(resources: resources)
        try resources.query.reserve(bytes)
        let host = SpillTestHost(), spool = RowSpool(config: SpillConfig(workMemoryBytes: bytes, host: host, meter: meter))
        try spool.append([.integer(1)]); host.store.state.withLock { $0.failWrites = true }
        expect("58030") { try spool.append([.integer(2)]) }
        expect("58030") { try spool.append([.integer(3)]) }
        spool.close(); XCTAssertEqual(resources.query.used, bytes)
        XCTAssertEqual(host.store.state.withLock { $0.closes }, 1); XCTAssertEqual(diagnostics.underflows, 0)
        resources.query.release(bytes)
        let noSpill = RowSpool(config: SpillConfig(workMemoryBytes: 0, host: host,
            meter: CostMeter(resources: StatementResources(queryLimit: 1))))
        expect("54P05") { try noSpill.append([.integer(1)]) }; noSpill.close()
    }
    #if os(Linux) || canImport(Darwin)
    func testNativePlainAndSealedLargeFramesRemovePrivateFiles() throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("jed-spill-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: directory) }
        for sealed in [false, true] {
            let host = NativeScratchFileHost(directory: directory.path, sealed: sealed,
                names: SeededRandomSource(seed: sealed ? 2 : 1), keys: SeededRandomSource(seed: 9))
            let file = try SpillRowFile(host: host, kind: "frames")
            let text = try SQLText(String(repeating: "é", count: 40_000))
            try file.append([.text(text)]); let cursor = file.cursor()
            guard case .text(let decoded) = try XCTUnwrap(cursor.next())[0] else { return XCTFail("missing text") }
            XCTAssertEqual(decoded, text); XCTAssertNil(try cursor.next()); cursor.close(); try file.close()
            XCTAssertEqual(try FileManager.default.contentsOfDirectory(atPath: directory.path), [])
        }
    }
    #endif
}
