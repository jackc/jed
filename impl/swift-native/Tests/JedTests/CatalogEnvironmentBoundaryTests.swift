import Foundation
import XCTest
@testable import Jed

final class CatalogEnvironmentBoundaryTests: XCTestCase {
    private func fixture(_ name: String) throws -> [UInt8] {
        Array(try Data(contentsOf: XCTUnwrap(Bundle.module.url(forResource: name, withExtension: "jucd", subdirectory: "Fixtures"))))
    }
    func testSkewedStatisticsRetainCanonicalFactsWithoutReinterpretingOldOrder() throws {
        let name = try SQLText("unicode"), tableName = try SQLText("t")
        var builder = Environment.builder(); try builder.loadUnicodeData(fixture("unicode"))
        let environment = builder.build(), descriptor = TypeDescriptor(.scalar(.text))
        var type = CatalogTypeUse(.scalar(.text)); type.collation = name
        var image = CatalogImage()
        image.collations = [CatalogCollation(name: name, unicode: try SQLText("old"), cldr: try SQLText("old"), description: try SQLText(""))]
        image.tables = [CatalogTable(name: tableName, columns: [CatalogColumn(name: try SQLText("v"), type: type)], root: PageReference(0))]
        var stats = CatalogStatistics(table: tableName, column: 0, stale: false, distributionEligible: true,
            termEligible: false, analyzed: 2, nulls: 0, widthSum: 2, distinct: 2, sampleRows: 2, sampleNonnull: 2)
        let oldOrder = try ["z", "a"].map { try ValueCodec.encode(.text(SQLText($0)), type: descriptor) }
        stats.mcv = oldOrder.map { CatalogStatistics.MCV(frequency: 1, bytes: $0) }
        stats.histogram = oldOrder; image.statistics = [stats]
        let collected = try CatalogImage.collect(image.entries(), catalogID: 0,
            collatedKey: { _, _ in XCTFail("old keys must not be regenerated under a skewed bundle"); return [] },
            collationVerdict: { try CatalogEnvironment.verdict($0, environment: environment) })
        XCTAssertEqual(collected.statistics[0].histogram, oldOrder)
        XCTAssertEqual(collected.statistics[0].mcv.map(\.bytes), oldOrder)
        XCTAssertFalse(collected.statistics[0].stale)
        XCTAssertThrowsError(try CatalogImage.collect(image.entries(), catalogID: 0,
            collatedKey: { try environment.unicode.key($0, collation: $1) }, collationVerdict: { _ in true })) {
            XCTAssertEqual(($0 as? JedError)?.code, "XX001")
        }
        XCTAssertThrowsError(try CatalogImage.collect(image.entries(), catalogID: 0,
            collationVerdict: { try CatalogEnvironment.verdict($0, environment: Environment.builder().build()) })) {
            XCTAssertEqual(($0 as? JedError)?.code, "XX002")
        }
        image.statistics[0].mcv[0] = CatalogStatistics.MCV(frequency: 0, bytes: oldOrder[0])
        XCTAssertThrowsError(try CatalogImage.collect(image.entries(), catalogID: 0, collationVerdict: { _ in false })) {
            XCTAssertEqual(($0 as? JedError)?.code, "XX001")
        }
        image.statistics[0].mcv[0] = CatalogStatistics.MCV(frequency: 1, bytes: [0, 255])
        XCTAssertThrowsError(try CatalogImage.collect(image.entries(), catalogID: 0, collationVerdict: { _ in false })) {
            XCTAssertEqual(($0 as? JedError)?.code, "XX001")
        }
    }
}
