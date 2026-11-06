import Foundation
import XCTest
@testable import Jed

final class ExternalSortBoundaryTests: XCTestCase {
    private func compare(_ left: [SQLValue], _ right: [SQLValue]) throws -> Int {
        try ValueComparison.total(left[0], right[0], type: TypeDescriptor(.scalar(.i64)))
    }
    func testResidentRunKeepsItsChargeUntilTheConsumerCloses() throws {
        let resources = StatementResources(queryLimit: 1_000_000), meter = CostMeter(resources: resources)
        let sorter = ExternalRowSorter(workMemoryBytes: 1, host: nil, meter: meter, compare: compare)
        try sorter.append([.integer(2)]); try sorter.append([.integer(1)])
        let held = resources.query.used
        XCTAssertEqual(held, 2 * MemorySizes.row([.integer(1)]))
        XCTAssertNotNil(try sorter.next()); XCTAssertEqual(resources.query.used, held)
        XCTAssertNotNil(try sorter.next()); XCTAssertNil(try sorter.next())
        XCTAssertEqual(resources.query.used, held); sorter.close(); XCTAssertEqual(resources.query.used, 0)
    }
    #if os(Linux) || canImport(Darwin)
    func testBinaryCompactionKeepsStableTiesWithRejectedReservationsAndSealedFiles() throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("jed-sort-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: directory) }
        for sealed in [false, true] {
            let host = NativeScratchFileHost(directory: directory.path, sealed: sealed,
                names: SeededRandomSource(seed: 13), keys: SeededRandomSource(seed: 17))
            let diagnostics = MemoryDiagnostics()
            let resources = StatementResources(queryLimit: 1, diagnostics: diagnostics)
            let sorter = ExternalRowSorter(workMemoryBytes: 1, host: host, meter: CostMeter(resources: resources), compare: compare)
            for index in 0..<129 {
                try sorter.append([.integer(Int64(index % 7)), .integer(Int64(index))])
                XCTAssertLessThanOrEqual(try FileManager.default.contentsOfDirectory(atPath: directory.path).count, 8)
            }
            XCTAssertEqual(sorter.count, 129); XCTAssertEqual(resources.query.used, 0)
            var previous: (key: Int64, ordinal: Int64)?
            var count = 0
            while let row = try sorter.next() {
                guard case .integer(let key) = row[0], case .integer(let ordinal) = row[1] else { return XCTFail("malformed sorter row") }
                if let previous {
                    XCTAssertGreaterThanOrEqual(key, previous.key)
                    if key == previous.key { XCTAssertGreaterThan(ordinal, previous.ordinal) }
                }
                previous = (key, ordinal); count += 1
            }
            XCTAssertEqual(count, 129); sorter.close()
            XCTAssertEqual(resources.query.used, 0); XCTAssertEqual(diagnostics.underflows, 0)
            XCTAssertEqual(try FileManager.default.contentsOfDirectory(atPath: directory.path), [])
        }
    }
    func testEarlyCloseAndThrowingComparisonReleaseRunsAndReservations() throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("jed-sort-close-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: directory) }
        let host = NativeScratchFileHost(directory: directory.path, sealed: false,
            names: SeededRandomSource(seed: 23), keys: SeededRandomSource(seed: 29))
        let resources = StatementResources(queryLimit: 1_000_000), meter = CostMeter(resources: resources)
        let sorter = ExternalRowSorter(workMemoryBytes: MemorySizes.row([.integer(1)]), host: host, meter: meter, compare: compare)
        for value in 0..<9 { try sorter.append([.integer(Int64(value))]) }
        XCTAssertNotNil(try sorter.next()); sorter.close(); sorter.close()
        XCTAssertEqual(resources.query.used, 0)
        XCTAssertEqual(try FileManager.default.contentsOfDirectory(atPath: directory.path), [])
        let failure = ExternalRowSorter(workMemoryBytes: 1, host: host, meter: meter) { _, _ in throw JedError("58030", "injected comparison failure") }
        try failure.append([.integer(1)])
        XCTAssertThrowsError(try failure.append([.integer(2)]))
        failure.close(); XCTAssertEqual(resources.query.used, 0)
        XCTAssertEqual(try FileManager.default.contentsOfDirectory(atPath: directory.path), [])
    }
    #endif
}
