import Foundation
import XCTest
@testable import Jed

#if os(Linux)
final class SimulationServiceBoundaryTests: XCTestCase {
    private func setup() throws -> (SimServiceWorld, Environment) {
        let clock = ManualClock(microseconds: 1_700_000_000_000_000)
        var builder = Environment.builder(); builder.setClockSource(clock)
        let environment = builder.build(), filesystem = try SimFilesystem(seed: 81, pageSize: 1024)
        let database = try FileDatabaseHost.createResolved(path: "sim.jed",
            options: CreateOptions(environment: environment, pageSize: 1024, locking: .none, flushIntervalMilliseconds: 0),
            catalogID: Database.allocateCatalogID(), random: SeededRandomSource(seed: 90), operations: filesystem)
        try database.close()
        return (SimServiceWorld(filesystem: filesystem, clock: clock), environment)
    }
    private func open(_ process: Int, world: SimServiceWorld, environment: Environment) throws -> Database {
        try ServiceDatabaseHost.openResolved(path: "sim.jed", options: OpenOptions(environment: environment,
            flushIntervalMilliseconds: 0, pageService: PageServiceOptions(shutdownGraceMilliseconds: 1000,
                idleWriteMilliseconds: 0)), catalogID: Database.allocateCatalogID(),
            platform: SimServicePlatform(world: world, process: process, scratchSeed: UInt64(process + 1)))
    }
    func testRealClientDeltaAndAsyncPinReleaseSettleThroughUnbufferedFrames() throws {
        let (world, environment) = try setup()
        let owner = try open(0, world: world, environment: environment)
        let client = try open(1, world: world, environment: environment)
        defer { try? client.close(); try? owner.close() }
        try world.network.settle()
        XCTAssertEqual((owner.host as? ServiceDatabaseHost)?.toolingRole, "owner")
        XCTAssertEqual((client.host as? ServiceDatabaseHost)?.toolingRole, "client")
        let pin = try client.core.beginRead(); try world.network.settle()
        XCTAssertEqual(owner.core.livePinCount, 1)
        pin.close(); try world.network.settle(); XCTAssertEqual(owner.core.livePinCount, 0)
        let lease = try client.core.acquireWriter(); defer { lease.close() }
        var snapshot = lease.snapshot, catalog = snapshot.catalog
        catalog.sequences.append(CatalogSequence(name: try SQLText("external")))
        try snapshot.updateCatalog(catalog, schemaChanged: true)
        let plan = try XCTUnwrap(lease.prepare(snapshot))
        try lease.persist(plan); lease.close(); try world.network.settle()
        XCTAssertEqual(owner.version, 2)
        let current = try owner.core.beginRead(); defer { current.close() }
        XCTAssertEqual(current.snapshot.catalog.sequences.map(\.name.description), ["external"])
    }
    func testCrashClosesOwnedDevicesThenClientPromotesAtNextAdmission() throws {
        let (world, environment) = try setup()
        let owner = try open(0, world: world, environment: environment)
        let client = try open(1, world: world, environment: environment)
        defer { try? client.close(); try? owner.close() }
        try world.network.settle()
        owner.core.storage.pager.revoke(); try? owner.close(); world.kill(0)
        try world.network.settle()
        XCTAssertNil(world.owner("sim.jed"))
        XCTAssertEqual((client.host as? ServiceDatabaseHost)?.toolingRole, "client")
        let pin = try client.core.beginRead(); pin.close(); try world.network.settle()
        XCTAssertEqual((client.host as? ServiceDatabaseHost)?.toolingRole, "owner")
        XCTAssertEqual(world.owner("sim.jed"), 1)
    }

    func testPoisonedOwnerStopsServingButRetainsPresenceUntilClose() throws {
        let (world, environment) = try setup()
        let owner = try open(0, world: world, environment: environment)
        let client = try open(1, world: world, environment: environment)
        defer { try? client.close(); try? owner.close() }
        let lease = try client.core.acquireWriter(); defer { lease.close() }
        var snapshot = lease.snapshot, catalog = snapshot.catalog
        catalog.sequences.append(CatalogSequence(name: try SQLText("uncertain")))
        try snapshot.updateCatalog(catalog, schemaChanged: true)
        let plan = try XCTUnwrap(lease.prepare(snapshot))
        owner.core.storage.pager.armFault(.sync(ordinal: 1))
        XCTAssertThrowsError(try lease.persist(plan)) { XCTAssertEqual(($0 as? JedError)?.code, "08007") }
        lease.close(); try world.network.settle()
        XCTAssertEqual(world.owner("sim.jed"), 0)
        XCTAssertEqual((owner.host as? ServiceDatabaseHost)?.toolingRole, "owner")
        XCTAssertThrowsError(try owner.checkOpen()) { XCTAssertEqual(($0 as? JedError)?.code, "58030") }
        try? owner.close(); try world.network.settle()
        XCTAssertNil(world.owner("sim.jed"))
        let pin = try client.core.beginRead(); pin.close(); try world.network.settle()
        XCTAssertEqual(world.owner("sim.jed"), 1)
    }
}
#endif
