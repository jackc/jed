import Foundation
import XCTest
@testable import Jed

final class SQLCorpusVariantTests: XCTestCase {
    func testEncryptedModeRequiresDiskAndTranscript() throws {
        XCTAssertThrowsError(try SQLCorpusVariant(environment: ["JED_CONFORMANCE_ENCRYPTED": "yes"]))
        XCTAssertThrowsError(try SQLCorpusVariant(environment: ["JED_CONFORMANCE_ENCRYPTED": "1"]))
        let variant = try SQLCorpusVariant(environment: ["JED_CONFORMANCE_ENCRYPTED": "1", "JED_CONFORMANCE_TRANSCRIPT": "/unused/transcript"])
        XCTAssertThrowsError(try variant.validate(mode: .memory))
        try variant.validate(mode: .disk)
        XCTAssertEqual(try XCTUnwrap(variant.credential).pageServiceBytes(), [2] + Array(repeating: 0x4a, count: 32))
        XCTAssertNil(try SQLCorpusVariant(environment: [:]).credential)
    }

    func testTranscriptRetainsExactEscapesAndRepeatedOutputs() throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let path = directory.appendingPathComponent("transcript.tsv")
        let variant = try SQLCorpusVariant(environment: ["JED_CONFORMANCE_ENCRYPTED": "1", "JED_CONFORMANCE_TRANSCRIPT": path.path])
        for _ in 0..<2 {
            try variant.transcribe(name: "adapter.test", kind: "rows", sql: "EXPLAIN\nSELECT '\\t'", actual: "a\tb\nc\\d")
        }
        let expected = "adapter.test\trows\tEXPLAIN\\nSELECT '\\\\t'\ta\\tb\\nc\\\\d\n"
        XCTAssertEqual(try String(contentsOf: path, encoding: .utf8), expected + expected)
        XCTAssertThrowsError(try variant.transcribe(name: "bad\tfile", kind: "cost", sql: "", actual: "0"))
        XCTAssertTrue(SQLCorpusVariant.isExplain(" \nExPlAiN SELECT 1"))
        XCTAssertFalse(SQLCorpusVariant.isExplain("SELECT 'EXPLAIN'"))
        XCTAssertThrowsError(try SQLCorpusRunner.ordered(["incomplete"], columns: 2, sort: .rowsort))
    }

    func testEncryptedTranscriptDoesNotSuppressOrdinaryAssertions() throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let path = directory.appendingPathComponent("transcript.tsv")
        let variant = try SQLCorpusVariant(environment: ["JED_CONFORMANCE_ENCRYPTED": "1", "JED_CONFORMANCE_TRANSCRIPT": path.path])
        let database = try Database()
        defer { try? database.close() }
        let session = try database.session()
        defer { session.close() }
        var report = SQLCorpusRunner.Report()
        let record = SQLCorpusFile.Record(line: 1, kind: .query(tags: [73], sort: .nosort),
            sql: "SELECT 1", expected: ["deliberately wrong adapter expectation"], directives: [])
        SQLCorpusRunner.runRecord(record, session: session, report: &report, variant: variant, name: "adapter.test")
        XCTAssertEqual(report.failures.count, 1)
        let wrongError = SQLCorpusFile.Record(line: 2, kind: .statement(error: "XX999"),
            sql: "SELECT 1", expected: [], directives: [])
        SQLCorpusRunner.runRecord(wrongError, session: session, report: &report, variant: variant, name: "adapter.test")
        XCTAssertEqual(report.failures.count, 2)
        XCTAssertFalse(FileManager.default.fileExists(atPath: path.path))
    }
}
