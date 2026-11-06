import XCTest
@testable import Jed

final class CatalogGraphTests: XCTestCase {
    func testStoredTokenTextRetainsDecimalCoefficientSpelling() throws {
        // These are byte-contract cases from format.md's token rendering table,
        // independent of expression evaluation and numeric value normalization.
        for (source, expected) in [("price>=0.50", "price >= 0.50"), ("0001.20", "0001.20"),
                                   ("1_0.00e-2", ".1000"), ("0x0010 != 00016", "16 <> 16")] {
            XCTAssertEqual(try CatalogSQL.render(Array(source.utf8)), try SQLText(expected))
        }
    }
    private func chain(_ count: Int, reverseNames: Bool, array: Bool = false) throws -> CatalogImage {
        var image = CatalogImage()
        for depth in 1...count {
            let number = reverseNames ? count + 1 - depth : depth
            let name = try SQLText("c" + (number < 10 ? "0" : "") + String(number))
            var type = CatalogType.scalar(.i32)
            if let previous = image.composites.last?.name {
                type = array ? .array(.composite(previous)) : .composite(previous)
            }
            image.composites.append(CatalogComposite(name: name,
                fields: [CatalogColumn(name: try SQLText("field"), type: CatalogTypeUse(type))]))
        }
        return image
    }
    func testCompositeDepthChecksDescentAndMemoizedShortcuts() throws {
        for reverse in [false, true] {
            for array in [false, true] {
                let admitted = try chain(32, reverseNames: reverse, array: array)
                XCTAssertEqual(try admitted.resolvedTypes(catalogID: 1).count, 32)
                let rejected = try chain(33, reverseNames: reverse, array: array)
                XCTAssertThrowsError(try rejected.resolvedTypes(catalogID: 1)) { error in
                    XCTAssertEqual((error as? JedError)?.code, "XX001")
                }
            }
        }
    }
    func testRejectedCatalogUpdateLeavesIdentityAndMetadataUnchanged() throws {
        let pool = try NodePool(codec: PlainPageCodec(pageSize: 8192), cacheBytes: nil) { _ in
            throw JedError("XX000", "an empty catalog must not read a page")
        }
        let original = try chain(32, reverseNames: false)
        var snapshot = try DomainSnapshot(catalogID: 7, txid: 0, catalog: original, pool: pool,
                                          tables: [:], indexes: [:], gistTrees: [:])
        let revision = snapshot.catalogRevision, identity = snapshot.types[try SQLText("c32")]!
        XCTAssertThrowsError(try snapshot.updateCatalog(chain(33, reverseNames: false), schemaChanged: true)) { error in
            XCTAssertEqual((error as? JedError)?.code, "XX001")
        }
        XCTAssertTrue(snapshot.catalogRevision === revision)
        XCTAssertEqual(snapshot.types.count, 32)
        XCTAssertEqual(snapshot.types[try SQLText("c32")], identity)
    }
}
