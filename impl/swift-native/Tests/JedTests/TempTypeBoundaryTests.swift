import XCTest
@testable import Jed

final class TempTypeBoundaryTests: XCTestCase {
    func testBorrowedNominalTypeSurvivesNameReuseWithoutCatalogCopy() throws {
        let codec = try PlainPageCodec(pageSize: 8192)
        let pool = try NodePool(codec: codec, cacheBytes: nil) { _ in
            throw JedError("XX000", "empty resident trees do not read a device")
        }
        let name = try SQLText("payload"), tableName = try SQLText("scratch")
        let field = TypeField(name: try SQLText("value"), descriptor: TypeDescriptor(.scalar(.i32)))
        let original = CompositeType(catalogID: 17, id: 3, name: name, fields: [field])
        let replacement = CompositeType(catalogID: 17, id: 4, name: name, fields: [field])
        var temporary = try DomainSnapshot(catalogID: 29, txid: 0, catalog: CatalogImage(), pool: pool,
            tables: [:], indexes: [:], gistTrees: [:])
        try temporary.borrowTypes([name: original])
        let table = CatalogTable(name: tableName, columns: [
            CatalogColumn(name: try SQLText("data"), type: CatalogTypeUse(.composite(name)))
        ], root: PageSealer.nullReference(codec))
        var image = temporary.catalog; image.tables.append(table)
        try temporary.updateCatalog(image, schemaChanged: true)
        temporary.setTableStore(tableName, temporary.emptyStore(types: [TypeDescriptor(.composite(original))]))
        try temporary.borrowTypes([name: replacement])
        XCTAssertTrue(temporary.catalog.composites.isEmpty)
        XCTAssertEqual(temporary.types[name], replacement)
        XCTAssertEqual(try temporary.tableTypes(table), [TypeDescriptor(.composite(original))])
        _ = try DomainCommitPlanner.layout(temporary, highWater: codec.firstBodyPage, free: [])
        XCTAssertThrowsError(try image.validateReferences(catalogID: temporary.catalogID)) {
            XCTAssertEqual(($0 as? JedError)?.code, "XX001")
        }
        try image.validateReferences(catalogID: temporary.catalogID, borrowedTypes: temporary.borrowedTypes)
        try temporary.borrowTypes([:])
        XCTAssertEqual(try temporary.tableTypes(table), [TypeDescriptor(.composite(original))])
    }
    func testBorrowedViewNamesRemainPrivateValidationFacts() throws {
        let codec = try PlainPageCodec(pageSize: 8192)
        let pool = try NodePool(codec: codec, cacheBytes: nil) { _ in
            throw JedError("XX000", "empty resident trees do not read a device")
        }
        let tableName = try SQLText("source"), columnName = try SQLText("value")
        var mainImage = CatalogImage()
        mainImage.tables = [CatalogTable(name: tableName, columns: [
            CatalogColumn(name: columnName, type: CatalogTypeUse(.scalar(.i32)))
        ], root: PageSealer.nullReference(codec))]
        let main = try DomainSnapshot(catalogID: 40, txid: 0, catalog: mainImage, pool: pool,
            tables: [:], indexes: [:], gistTrees: [:])
        var temporary = try DomainSnapshot(catalogID: 41, txid: 0, catalog: CatalogImage(), pool: pool,
            tables: [:], indexes: [:], gistTrees: [:])
        try temporary.borrow(main)
        var image = temporary.catalog
        image.views = [CatalogView(name: try SQLText("private_view"), columns: [.init(name: columnName)],
            definition: try SQLText("select value from source"),
            dependencies: [.init(kind: 0, name: tableName, columns: [columnName])])]
        try temporary.updateCatalog(image, schemaChanged: true)
        XCTAssertTrue(temporary.catalog.tables.isEmpty)
        XCTAssertTrue(temporary.tables.isEmpty)
        XCTAssertThrowsError(try image.validateReferences(catalogID: temporary.catalogID)) {
            XCTAssertEqual(($0 as? JedError)?.code, "XX001")
        }
        let emptyMain = try DomainSnapshot(catalogID: 40, txid: 1, catalog: CatalogImage(), pool: pool,
            tables: [:], indexes: [:], gistTrees: [:])
        try temporary.borrow(emptyMain)
        _ = try DomainCommitPlanner.layout(temporary, highWater: codec.firstBodyPage, free: [])
        var corrupt = temporary.catalog
        corrupt.views[0].dependencies[0].columns = [try SQLText("never_existed")]
        XCTAssertThrowsError(try temporary.updateCatalog(corrupt, schemaChanged: true)) {
            XCTAssertEqual(($0 as? JedError)?.code, "XX001")
        }
    }

}
