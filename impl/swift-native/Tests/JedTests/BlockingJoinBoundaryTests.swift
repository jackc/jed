import Foundation
import XCTest
@testable import Jed

final class BlockingJoinBoundaryTests: XCTestCase {
    #if os(Linux) || canImport(Darwin)
    func testHotKeyProbeSpillsBeforeONWithoutChangingCostOrOrder() throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("jed-join-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: directory) }
        let host = NativeScratchFileHost(directory: directory.path, sealed: true,
            names: SeededRandomSource(seed: 41), keys: SeededRandomSource(seed: 43))
        var costs: [Int64] = []
        for spilling in [false, true] {
            let diagnostics = MemoryDiagnostics()
            let resources = StatementResources(queryLimit: spilling ? 1 : 1_000_000, diagnostics: diagnostics)
            let meter = CostMeter(resources: resources), context = ExpressionContext(meter: meter)
            let config = SpillConfig(workMemoryBytes: spilling ? 128 : 0, host: spilling ? host : nil, meter: meter)
            let rows = RowSpool(config: config)
            for index in 0..<129 { try rows.append([.integer(7), .integer(Int64(index))]) }
            try rows.append([.null, .integer(999)])
            let keys = [(column: 0, type: TypeDescriptor(.scalar(.i64)))]
            let table = try BoundedHashJoinTable(rows: rows, keys: keys, config: config, context: context)
            rows.close()
            let matches = try table.probe([.integer(7)], keys: keys)
            XCTAssertEqual(matches.count, 129)
            XCTAssertEqual(matches.isSpilled, spilling)
            if spilling {
                XCTAssertEqual(resources.query.used, 0)
                XCTAssertLessThanOrEqual(try FileManager.default.contentsOfDirectory(atPath: directory.path).count, 4)
            }
            let cursor = try matches.cursor()
            for index in 0..<129 {
                let row = try XCTUnwrap(cursor.next())
                guard case .integer(let actual) = row[1] else { return XCTFail("lost build ordinal") }
                XCTAssertEqual(actual, Int64(index))
            }
            XCTAssertNil(try cursor.next()); cursor.close(); matches.close()
            let nullMatches = try table.probe([.null], keys: keys)
            XCTAssertEqual(nullMatches.count, 0); nullMatches.close()
            costs.append(meter.accrued)
            // Closing after just the first output must release the whole hot
            // bucket's scratch and the candidate spool, independently.
            let early = try table.probe([.integer(7)], keys: keys)
            let output = try early.takeOutput(); XCTAssertNotNil(try output.next()); output.close()
            table.close(); table.close()
            XCTAssertEqual(resources.query.used, 0); XCTAssertEqual(diagnostics.underflows, 0)
            XCTAssertEqual(try FileManager.default.contentsOfDirectory(atPath: directory.path), [])
        }
        XCTAssertEqual(costs[0], costs[1])
    }
    #endif
}
