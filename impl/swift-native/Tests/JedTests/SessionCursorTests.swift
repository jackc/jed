import XCTest
@testable import Jed

final class SessionCursorTests: XCTestCase {
    func testTransactionEndClosesUnreadRowsAndReleasesPins() throws {
        for writable in [false, true] {
            for commit in [false, true] {
                let database = try Database(), session = try database.session()
                try session.begin(writable: writable)
                let first = try session.query("SELECT 1"), second = try session.query("SELECT 2")
                XCTAssertEqual(first.cost, 0); XCTAssertFalse(first.isClosed)
                XCTAssertGreaterThanOrEqual(database.core.livePinCount, 2)
                if commit { try session.commit() } else { try session.rollback() }
                XCTAssertTrue(first.isClosed); XCTAssertTrue(second.isClosed)
                XCTAssertNil(try first.next()); XCTAssertNil(try second.next())
                XCTAssertEqual(first.cost, 0); XCTAssertEqual(second.cost, 0)
                XCTAssertEqual(database.core.livePinCount, 0)
                XCTAssertEqual(session.status, .idle)
            }
        }
    }

    func testSessionCloseEndsBlockCursorsButAutocommitCursorOwnsItsPin() throws {
        let database = try Database(), session = try database.session()
        try session.begin(writable: false)
        let inBlock = try session.query("SELECT 1")
        session.close()
        XCTAssertTrue(inBlock.isClosed); XCTAssertEqual(database.core.livePinCount, 0)
        let other = try database.session(), autocommit = try other.query("SELECT 2")
        other.close()
        XCTAssertFalse(autocommit.isClosed)
        XCTAssertEqual(database.core.livePinCount, 1)
        XCTAssertNotNil(try autocommit.next())
        XCTAssertNil(try autocommit.next())
        XCTAssertEqual(database.core.livePinCount, 0)
    }

    func testBlockDoesNotRetainDroppedOrAlreadyClosedCursorResources() throws {
        let database = try Database(), session = try database.session()
        try session.begin(writable: false)
        let baseline = database.core.livePinCount
        weak var dropped: Rows?
        do {
            let rows = try session.query("SELECT 1")
            dropped = rows
            XCTAssertEqual(database.core.livePinCount, baseline + 1)
        }
        XCTAssertNil(dropped)
        XCTAssertEqual(database.core.livePinCount, baseline)
        let rows = try session.query("SELECT 2")
        rows.close(); rows.close()
        XCTAssertEqual(database.core.livePinCount, baseline)
        try session.rollback()
        XCTAssertEqual(database.core.livePinCount, 0)
    }
}
