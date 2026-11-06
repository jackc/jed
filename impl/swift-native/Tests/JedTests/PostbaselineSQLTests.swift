import Foundation
import XCTest
import JedCrypto
@testable import Jed

/// Explicit corrections after the experiment's frozen baseline. These shared
/// files are counted separately and never enter GeneratedSpec's baseline hash.
final class PostbaselineSQLTests: XCTestCase {
    func testSharedStatisticsKMVBoundaryCorrection() throws {
        try run("statistics_kmv_boundary", digest: "bba8932e0e99b38d4e588367a5227d235b3e499753601971d5c6cd5034b07858", records: 14)
    }
    func testSharedGINRootStringCorrection() throws {
        try run("gin_jsonb_root_string", digest: "197f69977c2888947dbd317c5a1f26b5f08d741a16c879a7169f3241cacfd046", records: 7)
    }
    private func run(_ name: String, digest expectedDigest: String, records: Int) throws {
        let path = try XCTUnwrap(Bundle.module.url(forResource: name, withExtension: "test",
                                                   subdirectory: "Fixtures/Regressions"))
        let bytes = Array(try Data(contentsOf: path))
        let digest = JedCrypto.sha256(bytes).map { String(format: "%02x", $0) }.joined()
        XCTAssertEqual(digest, expectedDigest)
        let file = try SQLCorpusFile(String(decoding: bytes, as: UTF8.self))
        let runner = try SQLCorpusRunner(file: file) { name, ext in
            let resource = try XCTUnwrap(Bundle.module.url(forResource: name, withExtension: ext, subdirectory: "Fixtures"))
            return Array(try Data(contentsOf: resource))
        }
        let report = runner.run(file)
        try runner.close()
        XCTAssertEqual(report.records, records)
        XCTAssertTrue(report.completed)
        for failure in report.failures { XCTFail("\(name).test:\(failure)") }
        print("Postbaseline shared SQL regression \(name): 1 file, \(report.records) records, \(report.cells) cells, \(report.failures.count) failures")
    }
}
