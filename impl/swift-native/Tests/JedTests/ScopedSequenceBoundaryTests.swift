import XCTest
@testable import Jed

final class ScopedSequenceBoundaryTests: XCTestCase {
    private func domain(_ id: UInt64, sequence: CatalogSequence) throws -> DomainSnapshot {
        let codec = try PlainPageCodec(pageSize: 8192)
        let pool = try NodePool(codec: codec, cacheBytes: nil) { _ in throw JedError("XX000", "empty resident domain cannot read") }
        var image = CatalogImage(); image.sequences = [sequence]
        return try DomainSnapshot(catalogID: id, txid: 0, catalog: image, pool: pool, tables: [:], indexes: [:], gistTrees: [:])
    }
    func testGroupAppliesOnlyResolvedOwnerAndKeepsTemporaryFirstLookup() throws {
        let name = try SQLText("counter")
        let main = try domain(17, sequence: CatalogSequence(name: name, start: 10, last: 10))
        let temp = try domain(29, sequence: CatalogSequence(name: name, start: 50, last: 50))
        var roots = QueryDomains(snapshots: [DomainNames.main: main, DomainNames.temporary: temp])
        let meter = CostMeter()
        let service = SequenceExecution(domains: roots, targetDomain: DomainNames.temporary,
            history: SequenceHistory(), meter: meter, writable: true, writableDomains: [DomainNames.temporary])
        var context = ExpressionContext(meter: meter); service.install(into: &context)
        let call = try XCTUnwrap(context.functions.stateCall)
        if case .integer(let value) = try call("nextval", [.text(name)]) { XCTAssertEqual(value, 50) }
        else { XCTFail("expected sequence value") }
        try service.apply(to: &roots)
        XCTAssertEqual(roots.snapshots[DomainNames.main]?.catalog.sequences.first?.last, 10)
        XCTAssertFalse(roots.snapshots[DomainNames.main]?.catalog.sequences.first?.called ?? true)
        XCTAssertTrue(roots.snapshots[DomainNames.main]?.catalogRevision === main.catalogRevision)
        XCTAssertTrue(roots.snapshots[DomainNames.temporary]?.catalog.sequences.first?.called ?? false)
        XCTAssertEqual(service.history.values[name], 50)
        XCTAssertEqual(meter.accrued, 1)
    }
    func testReadOnlyOwnerCannotBeAdvancedFromWritableTemporaryContext() throws {
        let name = try SQLText("durable_counter")
        let main = try domain(17, sequence: CatalogSequence(name: name))
        let roots = QueryDomains(snapshots: [DomainNames.main: main])
        let meter = CostMeter()
        let service = SequenceExecution(domains: roots, history: SequenceHistory(), meter: meter,
            writable: true, writableDomains: [DomainNames.temporary])
        var context = ExpressionContext(meter: meter); service.install(into: &context)
        let call = try XCTUnwrap(context.functions.stateCall)
        XCTAssertThrowsError(try call("nextval", [.text(name)])) { XCTAssertEqual(($0 as? JedError)?.code, "25006") }
        XCTAssertTrue(service.history.values.isEmpty)
    }
}
