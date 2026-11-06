import Foundation
import XCTest
import JedCrypto
@testable import Jed

/// Explicit corrections after the experiment's frozen baseline. These shared
/// files are counted separately and never enter GeneratedSpec's baseline hash.
final class PostbaselineSQLTests: XCTestCase {
    func testSharedStatisticsKMVBoundaryCorrection() throws {
        let path = try XCTUnwrap(Bundle.module.url(forResource: "statistics_kmv_boundary", withExtension: "test",
                                                   subdirectory: "Fixtures/Regressions"))
        let bytes = Array(try Data(contentsOf: path))
        let digest = JedCrypto.sha256(bytes).map { String(format: "%02x", $0) }.joined()
        XCTAssertEqual(digest, "bba8932e0e99b38d4e588367a5227d235b3e499753601971d5c6cd5034b07858")
        let file = try SQLCorpusFile(String(decoding: bytes, as: UTF8.self))
        let runner = try SQLCorpusRunner(file: file) { name, ext in
            let resource = try XCTUnwrap(Bundle.module.url(forResource: name, withExtension: ext, subdirectory: "Fixtures"))
            return Array(try Data(contentsOf: resource))
        }
        let report = runner.run(file)
        try runner.close()
        XCTAssertEqual(report.records, 14)
        for failure in report.failures { XCTFail("statistics_kmv_boundary.test:\(failure)") }
        print("Postbaseline shared SQL regression: 1 file, \(report.records) records, \(report.cells) cells, \(report.failures.count) failures")
    }
}
