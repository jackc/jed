import Foundation
import XCTest
@testable import Jed

final class ExpressionCollationBoundaryTests: XCTestCase {
    private func resolver() throws -> ExpressionResolver {
        var unicode = TypeDescriptor(.scalar(.text)); unicode.collation = try SQLText("unicode")
        let plain = TypeDescriptor(.scalar(.text))
        let resolver = ExpressionResolver(columns: [
            ExpressionColumn(name: try SQLText("a"), qualifier: nil, type: unicode, index: 0),
            ExpressionColumn(name: try SQLText("b"), qualifier: nil, type: plain, index: 1),
            ExpressionColumn(name: try SQLText("x"), qualifier: nil, type: plain, index: 2, collation: .conflicted)])
        let url = try XCTUnwrap(Bundle.module.url(forResource: "unicode", withExtension: "jucd", subdirectory: "Fixtures"))
        try resolver.unicode.load(Array(Data(contentsOf: url)))
        return resolver
    }
    func testQueryProvenanceSurvivesSyntheticColumnsAndKeepsStorageRuleSeparate() throws {
        let resolver = try resolver()
        func resolve(_ text: String) throws -> ResolvedExpression { try resolver.resolve(SQLParser(text).parseExpressionOnly()) }
        let unicode = try SQLText("unicode"), c = try SQLText("C")
        XCTAssertEqual(try resolve("lower(a)").collation, .implicit(unicode))
        XCTAssertEqual(try resolve("lower(a COLLATE \"C\")").collation, .explicit(c))
        let mixed = try resolve("coalesce(a,b)")
        XCTAssertEqual(mixed.collation, .indeterminate(unicode, c))
        XCTAssertEqual(try resolve("ARRAY[a,b]").collation, mixed.collation)
        XCTAssertEqual(mixed.collation?.asColumn, .conflicted)
        XCTAssertEqual(try resolve("x::text").collation, .conflicted)
        XCTAssertEqual(try resolve("x::varchar(8)").collation, .some(.none))
        XCTAssertEqual(try resolve("coalesce(x,'')").collation, .some(.none))
        XCTAssertThrowsError(try mixed.collation?.orderingName()) { XCTAssertEqual(($0 as? JedError)?.code, "42P21") }
        XCTAssertThrowsError(try CollationDerivation.conflicted.orderingName()) { XCTAssertEqual(($0 as? JedError)?.code, "42P22") }
        XCTAssertThrowsError(try resolve("a=b")) { XCTAssertEqual(($0 as? JedError)?.code, "42P22") }
        _ = try resolve("a COLLATE \"C\" = b")
        resolver.storageCollation = true
        XCTAssertEqual(try resolve("lower(a)").collation, .some(.none))
        XCTAssertEqual(try resolve("a || ''").collation, .implicit(unicode))
        XCTAssertEqual(try resolve("a COLLATE \"C\"").collation, .explicit(c))
    }

    func testEqualityBypassesCollationTablesRecursivelyAndOrderingChargesVisitedPairs() throws {
        var element = TypeDescriptor(.scalar(.text)); element.collation = try SQLText("unicode")
        var type = TypeDescriptor(.array(.scalar(.text))); type.collation = element.collation
        let first = SQLValue.array(try SQLArray(dimensions: [2], lowerBounds: [1], elements: [.text(SQLText("a")), .text(SQLText("ignored"))]))
        let second = SQLValue.array(try SQLArray(dimensions: [2], lowerBounds: [1], elements: [.text(SQLText("bb")), .text(SQLText("ignored"))]))
        let meter = CostMeter()
        var context = ExpressionContext(meter: meter)
        var keys = 0
        context.collatedKey = { text, _ in keys += 1; return text.utf8 }
        try context.comparisonWork("<", first, second, type: type)
        XCTAssertEqual(meter.accrued, 3)
        XCTAssertEqual(keys, 2) // The decisive first pair stops the walk.
        let forbidden: KeyEncoding.CollatedKey = { _, _ in throw JedError("XX000", "equality must not read a collation table") }
        guard case .boolean(false) = try ValueComparison.sql("=", first, second, type: type, collatedKey: forbidden) else { return XCTFail("byte equality expected") }
        let nested = TypeDescriptor(.record([TypeField(name: try SQLText("f"), descriptor: type)]))
        XCTAssertEqual(try ValueComparison.total(.composite([first]), .composite([first]), type: nested, collatedKey: forbidden, equality: true), 0)
    }
}
