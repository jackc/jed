import Foundation
import XCTest
@testable import Jed

final class ConcurrencyCorpusBoundaryTests: XCTestCase {
    func testWholeFrozenConcurrencySchedules() throws {
        let root = try XCTUnwrap(Bundle.module.resourceURL?.appendingPathComponent("Fixtures/Corpus/concurrency"))
        let files = try FileManager.default.contentsOfDirectory(at: root, includingPropertiesForKeys: nil)
            .filter { $0.pathExtension == "test" }.sorted { $0.lastPathComponent < $1.lastPathComponent }
        XCTAssertEqual(files.count, 7, "the frozen baseline has seven complete schedules")
        var steps = 0, records = 0, assertions = 0, handoffs = 0, completed = 0
        for file in files {
            SQLCorpusTrace.write("Concurrency file begin: \(file.lastPathComponent)")
            let schedule = try ConcurrencyCorpusFile(String(contentsOf: file, encoding: .utf8))
            let runner = try ConcurrencyCorpusRunner(file: schedule)
            let report = runner.run(schedule)
            try runner.close()
            steps += report.steps; records += report.sql.records
            assertions += report.assertions; handoffs += report.handoffs
            if report.sql.completed { completed += 1 }
            for failure in report.sql.failures { XCTFail("\(file.lastPathComponent):\(failure)") }
            XCTAssertTrue(report.sql.completed, "\(file.lastPathComponent): schedule was not fully consumed")
            SQLCorpusTrace.write("Concurrency file end: \(file.lastPathComponent)")
        }
        print("Frozen concurrency: \(completed)/\(files.count) complete schedules, \(steps) steps, \(records) SQL records, \(assertions) version/watermark assertions, \(handoffs) queued writer handoffs")
    }

    func testScheduleParserRetainsRecordBoundariesAndLocations() throws {
        let file = try ConcurrencyCorpusFile("""
        # format: concurrency
        # requires: txn.shared
        # attach: a
        # max_storage_bytes: 8192 a
        open r read
        on r query I rowsort
        SELECT 1
        ----
        1
        close r
        expect version 0 # inline schedule comment
        """)
        XCTAssertEqual(file.setup.map(\.name), ["attach", "max_storage_bytes"])
        XCTAssertEqual(file.steps.map(\.line), [5, 6, 10, 11])
        guard case .record(let name, let record) = file.steps[1].operation else { return XCTFail("lost scheduled record") }
        XCTAssertEqual(name, "r"); XCTAssertEqual(record.line, 6)
        XCTAssertEqual(record.sql, "SELECT 1"); XCTAssertEqual(record.expected, ["1"])
        XCTAssertThrowsError(try ConcurrencyCorpusFile("# format: concurrency\n# requires: txn.shared\n# seed: 1\n"))
        XCTAssertThrowsError(try ConcurrencyCorpusFile("# format: concurrency\n# requires: txn.shared\nopen r read blocks\n"))
    }

    func testMalformedGateScheduleFailsBeforeBlockingAndCleansUp() throws {
        for controls in ["open w write blocks", "open w write\nopen next write",
                         "open w write\nopen a write blocks\nopen b write blocks"] {
            let file = try ConcurrencyCorpusFile("# format: concurrency\n# requires: txn.shared\n" + controls + "\n")
            let runner = try ConcurrencyCorpusRunner(file: file)
            let report = runner.run(file)
            XCTAssertFalse(report.sql.completed); XCTAssertEqual(report.sql.failures.count, 1)
            try runner.close()
        }
    }
}
