import Foundation
import XCTest
@testable import Jed

final class SQLCorpusTemporaryTests: XCTestCase {
    #if os(Linux) || canImport(Darwin)
    func testForcedMemoryTEMPUsesRealScratchAndPreservesMemoryMain() throws {
        let accounting = try SQLCorpusAccounting(environment: ["JED_CONFORMANCE_TEMP_BUFFERS": "1"])
        let file = try SQLCorpusFile("""
            # requires: ddl.temp_table
            statement ok
            CREATE TEMP TABLE t(id i32)

            statement ok
            INSERT INTO t VALUES (1)

            query I
            SELECT count(*) FROM t
            ----
            1

            """)
        let runner = try SQLCorpusRunner(file: file, mode: .memory, accounting: accounting) { _, _ in
            throw SQLCorpusError("unexpected fixture")
        }
        let report = runner.run(file)
        XCTAssertTrue(report.completed); XCTAssertTrue(report.failures.isEmpty, report.failures.joined(separator: "\n"))
        XCTAssertGreaterThan(runner.temporarySpills, 0)
        try runner.close(); try runner.close()
        XCTAssertThrowsError(try SQLCorpusAccounting(environment: ["JED_CONFORMANCE_TEMP_BUFFERS": "-1"]))
    }
    #endif
}
