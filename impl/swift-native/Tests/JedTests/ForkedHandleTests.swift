#if os(Linux)
import XCTest
import Glibc
@testable import Jed

/// A handle inherited across fork is invalid (locking.md §3). The child may
/// read what is resident, which touches nothing shared, but it is refused as
/// soon as it could write: it would otherwise hold the parent's file locks.
final class ForkedHandleTests: XCTestCase {
    private func run(_ locking: FileLocking) throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("jed-fork-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let path = directory.appendingPathComponent("db.jed").path
        let database = try Database.create(options: CreateOptions(path: path, locking: locking))
        defer { try? database.close() }
        try database.execute("CREATE TABLE t(id i64 PRIMARY KEY, v i64)")
        try database.execute("INSERT INTO t VALUES (1, 10), (2, 20)")
        let session = try database.session()
        defer { session.close() }
        let read = try session.prepare("SELECT v FROM t WHERE id = $1")
        // Leave the row's page resident and the lookup plan cached.
        for _ in 0..<3 { _ = try session.queryPrepared(read, parameters: [.integer(1)]).collect() }

        let child = fork()
        XCTAssertGreaterThanOrEqual(child, 0)
        if child == 0 {
            // No XCTest, no cleanup: report through the exit status only.
            var status: Int32 = 0
            do {
                let rows = try session.queryPrepared(read, parameters: [.integer(1)]).collect()
                if rows.count != 1 { status = 10 }
            } catch { status = 11 }
            if status == 0 {
                do { try session.execute("INSERT INTO t VALUES (3, 30)"); status = 20 }
                catch let error as JedError { status = error.code == "55006" ? 0 : 21 }
                catch { status = 22 }
            }
            if status == 0 {
                do { try session.begin(); status = 30 }
                catch let error as JedError { status = error.code == "55006" ? 0 : 31 }
                catch { status = 32 }
            }
            _exit(status)
        }
        var status: Int32 = 0, finished = false
        for _ in 0..<400 {
            if waitpid(child, &status, WNOHANG) == child { finished = true; break }
            usleep(50_000)
        }
        if !finished { kill(child, SIGKILL); waitpid(child, &status, 0) }
        XCTAssertTrue(finished, "forked child did not finish")
        XCTAssertEqual(status & 0x7f, 0, "forked child was signalled")
        XCTAssertEqual((status >> 8) & 0xff, 0, "forked child exit status (10s read, 20s write, 30s begin)")
        // The parent is unaffected and the child's row never reached the file.
        try session.execute("INSERT INTO t VALUES (4, 40)")
        let rows = try session.query("SELECT id FROM t ORDER BY id").collect()
        XCTAssertEqual(rows.count, 3)
        guard case .integer(4) = rows[2][0] else { return XCTFail("parent write missing") }
    }
    func testForkedChildOfExclusiveOwnerCannotWrite() throws { try run(.exclusive) }
    func testForkedChildOfServiceOwnerCannotWrite() throws { try run(.automatic) }
}
#endif
