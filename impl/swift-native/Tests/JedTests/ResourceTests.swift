import XCTest
@testable import Jed

final class ResourceTests: XCTestCase {
    func testFirstCrossedCeilingSurvivesCounterSaturation() {
        let lifetime = LifetimeCost(limit: 10, accrued: 8)
        let meter = CostMeter(limit: 5, lifetime: lifetime)
        meter.charge(Int64.max)
        XCTAssertEqual(meter.accrued, Int64.max)
        XCTAssertEqual(lifetime.accrued, Int64.max)
        XCTAssertThrowsError(try meter.guardCost()) { XCTAssertEqual(($0 as? JedError)?.code, "54P02") }
        let tie = CostMeter(limit: 2, lifetime: LifetimeCost(limit: 10, accrued: 8))
        tie.charge(3)
        XCTAssertThrowsError(try tie.guardCost()) { XCTAssertEqual(($0 as? JedError)?.code, "54P01") }
    }

    func testMemoryEqualityAllowedAndCostWinsOnlyOnRejection() throws {
        let resources = StatementResources(queryLimit: 64)
        let meter = CostMeter(limit: 1, resources: resources)
        meter.charge(1)
        try meter.reserveQuery(64) // Successful admission adds no cost checkpoint.
        XCTAssertThrowsError(try meter.reserveQuery(1)) { XCTAssertEqual(($0 as? JedError)?.code, "54P01") }
        XCTAssertEqual(resources.query.used, 64)
        resources.query.release(64)
        XCTAssertEqual(resources.query.used, 0)
    }

    func testPendingFloorAndUnderflowInstrumentation() throws {
        let diagnostics = MemoryDiagnostics()
        let resources = StatementResources(queryLimit: 100, diagnostics: diagnostics)
        let meter = CostMeter(resources: resources)
        try meter.syncPending(40)
        try meter.reserveQuery(60)
        XCTAssertEqual(diagnostics.peak, 100)
        resources.query.release(61)
        XCTAssertEqual(resources.query.used, 40)
        XCTAssertEqual(diagnostics.underflows, 1)
        try meter.syncPending(10)
        XCTAssertEqual(resources.query.used, 10)
        XCTAssertThrowsError(try meter.syncPending(101))
        XCTAssertEqual(resources.query.floor, 10)
        XCTAssertEqual(resources.query.used, 10)
    }

    func testNestedMetersShareScalarAllowanceAndLaterStatementsDoNotResetIt() throws {
        let shared = StatementResources(scalarLimit: 10)
        let outer = CostMeter(resources: shared)
        let scratch = CostMeter(resources: shared)
        try outer.reserveScalar(6)
        try scratch.reserveScalar(4)
        let next = CostMeter(resources: StatementResources(scalarLimit: 10))
        try next.reserveScalar(10)
        XCTAssertThrowsError(try outer.reserveScalar(1)) { XCTAssertEqual(($0 as? JedError)?.code, "54P04") }
        XCTAssertEqual(shared.scalarUsed, 10)
    }

    func testCancellationDoesNotChangeCostsAndWinsAtCheckpoint() throws {
        let token = CancellationToken()
        let meter = CostMeter(limit: 1, cancellation: token)
        meter.charge(1)
        token.cancel()
        XCTAssertThrowsError(try meter.checkpoint()) { XCTAssertEqual(($0 as? JedError)?.code, "57014") }
        XCTAssertEqual(meter.accrued, 1)
    }
}
