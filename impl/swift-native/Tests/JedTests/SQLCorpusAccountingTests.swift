import Foundation
import XCTest
@testable import Jed

final class SQLCorpusAccountingTests: XCTestCase {
    func testWholeRunDefaultsValidateWithoutChangingExplicitZero() throws {
        let unset = try SQLCorpusAccounting(environment: [:])
        XCTAssertNil(unset.queryMemory); XCTAssertNil(unset.workMemory)
        let zero = try SQLCorpusAccounting(environment: ["JED_CONFORMANCE_QUERY_MEMORY": "0", "JED_CONFORMANCE_WORK_MEM": "0"])
        XCTAssertEqual(zero.queryMemory, 0); XCTAssertEqual(zero.workMemory, 0)
        XCTAssertEqual(try SQLCorpusAccounting(environment: ["JED_CONFORMANCE_QUERY_MEMORY": "-1"]).queryMemory, -1)
        for (key, value) in [("JED_CONFORMANCE_QUERY_MEMORY", "bad"), ("JED_CONFORMANCE_QUERY_MEMORY", "9223372036854775808"),
                             ("JED_CONFORMANCE_WORK_MEM", "-1"), ("JED_CONFORMANCE_QUERY_MEMORY_PEAKS", "")] {
            XCTAssertThrowsError(try SQLCorpusAccounting(environment: [key: value]))
        }
    }

    func testEveryRecordGetsAnOrdinalAndItsOwnPeakAfterDirectiveOverride() throws {
        let output = FileManager.default.temporaryDirectory.appendingPathComponent("jed-memory-peaks-" + UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: output) }
        let accounting = try SQLCorpusAccounting(environment: ["JED_CONFORMANCE_QUERY_MEMORY": "1",
            "JED_CONFORMANCE_WORK_MEM": "256", "JED_CONFORMANCE_QUERY_MEMORY_PEAKS": output.path])
        let file = try SQLCorpusFile("""
            # requires: query.select
            statement error 54P05
            SELECT 1

            # max_query_memory_bytes: 0
            query I
            SELECT 1
            ----
            1

            # max_query_memory_bytes: 128
            query I
            SELECT 1
            ----
            1

            """)
        let runner = try SQLCorpusRunner(file: file, name: "adapter/peaks.test", accounting: accounting) { _, _ in
            throw SQLCorpusError("unexpected fixture")
        }
        defer { try? runner.close() }
        let report = runner.run(file)
        XCTAssertTrue(report.completed); XCTAssertTrue(report.failures.isEmpty, report.failures.joined(separator: "\n"))
        let lines = try String(contentsOf: output, encoding: .utf8).split(separator: "\n").map { $0.split(separator: "\t") }
        XCTAssertEqual(lines.count, 3)
        XCTAssertEqual(lines.map { String($0[0]) }, Array(repeating: "adapter/peaks.test", count: 3))
        XCTAssertEqual(lines.map { String($0[1]) }, ["1", "2", "3"])
        XCTAssertEqual(lines[0][2], "0") // Rejected reservations never alter the account.
        XCTAssertEqual(lines[1][2], "0") // Explicit unlimited overrides the active default.
        XCTAssertGreaterThan(try XCTUnwrap(Int64(lines[2][2])), 0)
        XCTAssertEqual(Set(lines.map { String($0[3]) }).count, 1)
    }
}
