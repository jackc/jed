import XCTest
@testable import Jed

final class DomainReadBundleBoundaryTests: XCTestCase {
    func testInstallKeepsPriorDomainValuesAndRetainedPinsIndependent() throws {
        let database = try Database(), attachment = try Database()
        defer { try? database.close(); try? attachment.close() }
        let bundle = DomainReadBundle(pins: [DomainNames.main: try database.core.beginRead()])
        defer { bundle.close() }
        let initial = bundle.domains, retained = try bundle.retain()
        defer { retained.close() }
        let before = try initial.snapshot(DomainNames.main).txid
        try database.execute("CREATE TABLE later(id i32)")
        bundle.install(try database.core.beginRead(), for: DomainNames.main)
        let extra = try SQLText("extra")
        bundle.install(try attachment.core.beginRead(), for: extra)
        XCTAssertEqual(try bundle.domains.snapshot(DomainNames.main).txid, before + 1)
        XCTAssertEqual(try initial.snapshot(DomainNames.main).txid, before)
        XCTAssertEqual(try retained.domains.snapshot(DomainNames.main).txid, before)
        XCTAssertNil(initial.snapshots[extra]); XCTAssertNil(retained.domains.snapshots[extra])
        XCTAssertEqual(database.core.livePinCount, 2)
        XCTAssertEqual(attachment.core.livePinCount, 1)

        let sibling = try bundle.retain(); defer { sibling.close() }
        bundle.close(); bundle.close()
        try retained.checkRole(); try sibling.checkRole()
        XCTAssertEqual(try sibling.domains.snapshot(extra).catalogID, attachment.core.routingSnapshot.catalogID)
        XCTAssertEqual(database.core.livePinCount, 2)
        XCTAssertEqual(attachment.core.livePinCount, 1)
        retained.close(); sibling.close()
        XCTAssertEqual(database.core.livePinCount, 0)
        XCTAssertEqual(attachment.core.livePinCount, 0)
    }

    func testFailedRetainReleasesEarlierPinsAndCachedRootsStillCheckRoles() throws {
        let first = try Database(), last = try Database()
        defer { try? first.close(); try? last.close() }
        let a = try SQLText("a"), z = try SQLText("z")
        let firstPin = try first.core.beginRead(), lastPin = try last.core.beginRead()
        let bundle = DomainReadBundle(pins: [z: lastPin, a: firstPin])
        defer { bundle.close() }
        lastPin.close()
        XCTAssertThrowsError(try bundle.retain()) { XCTAssertEqual(($0 as? JedError)?.code, "55000") }
        XCTAssertEqual(first.core.livePinCount, 1)
        XCTAssertEqual(last.core.livePinCount, 0)
        try last.core.quiesceRole(flush: false)
        XCTAssertThrowsError(try bundle.checkRole()) { XCTAssertEqual(($0 as? JedError)?.code, "08006") }
        bundle.close()
        XCTAssertEqual(first.core.livePinCount, 0)
    }
}
