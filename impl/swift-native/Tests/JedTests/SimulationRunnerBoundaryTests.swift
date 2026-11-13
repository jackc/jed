import Foundation
import XCTest
@testable import Jed

final class SimulationRunnerBoundaryTests: XCTestCase {
    func testFrozenTraceInventoryParsesWithoutSkipping() throws {
        let directory = try XCTUnwrap(Bundle.module.url(forResource: "Simulation", withExtension: nil, subdirectory: "Fixtures"))
        let paths = try FileManager.default.contentsOfDirectory(at: directory.appendingPathComponent("regress"), includingPropertiesForKeys: nil)
            .filter { $0.pathExtension == "sim" }.sorted { $0.path < $1.path }
        XCTAssertEqual(paths.count, 19)
        for path in paths { XCTAssertFalse(try SimTrace(String(contentsOf: path, encoding: .utf8)).steps.isEmpty, path.lastPathComponent) }
    }
    func testFrozenAnalyzeDropRegressionRunsTwiceDeterministically() throws {
        let path = try XCTUnwrap(Bundle.module.url(forResource: "analyze-then-drop-table", withExtension: "sim", subdirectory: "Fixtures/Simulation/regress"))
        let trace = try SimTrace(String(contentsOf: path, encoding: .utf8))
        let first = try SimRunner(trace).run(), second = try SimRunner(trace).run()
        XCTAssertEqual(first.violation, ""); XCTAssertEqual(second.violation, "")
        XCTAssertEqual(first.transcript, second.transcript); XCTAssertEqual(first.operationsHash, second.operationsHash)
        XCTAssertTrue(first.transcript.contains("\"op\":\"crash\"")); XCTAssertTrue(first.transcript.contains("\"op\":\"check\""))
    }
    func testCrashTranscriptHashesTheEmittedImageBeforeCredentialRepair() throws {
        let path = try XCTUnwrap(Bundle.module.url(forResource: "encryption-rekey-rename-not-durable",
            withExtension: "sim", subdirectory: "Fixtures/Simulation/regress"))
        let trace = try SimTrace(String(contentsOf: path, encoding: .utf8))
        let result = try SimRunner(trace).run()
        XCTAssertEqual(result.violation, "")
        let pageSize = try XCTUnwrap(Int(trace.header["page_size"] ?? ""))
        var checked = 0, repairBoundary = false
        for line in result.transcript.split(separator: "\n") {
            let row = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(line.utf8)) as? [String: Any])
            guard row["op"] as? String == "crash" else { continue }
            let step = try XCTUnwrap(row["i"] as? Int), pages = try XCTUnwrap(row["pages"] as? Int)
            let bytes = try XCTUnwrap(result.images["step\(step).img"])
            XCTAssertGreaterThanOrEqual(bytes.count, pages * pageSize)
            XCTAssertEqual(row["image"] as? String, SimText.hex(SimText.hash(bytes.prefix(pages * pageSize))), "step \(step)")
            checked += 1
            if step == 145 { repairBoundary = true }
        }
        XCTAssertGreaterThan(checked, 0); XCTAssertTrue(repairBoundary)
    }
    #if os(Linux)
    func testFrozenServiceDisconnectPreservesIndependentAttachmentConnection() throws {
        let path = try XCTUnwrap(Bundle.module.url(forResource: "service-disconnect-spares-served-connections",
            withExtension: "sim", subdirectory: "Fixtures/Simulation/regress"))
        let trace = try SimTrace(String(contentsOf: path, encoding: .utf8))
        let first = try SimRunner(trace).run(), second = try SimRunner(trace).run()
        XCTAssertEqual(first.violation, ""); XCTAssertEqual(second.violation, "")
        XCTAssertEqual(first.transcript, second.transcript); XCTAssertEqual(first.operationsHash, second.operationsHash)
        XCTAssertEqual(first.images, second.images)
    }
    #endif

}
