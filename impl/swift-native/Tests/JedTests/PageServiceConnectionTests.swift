#if os(Linux)
import Foundation
import Dispatch
import Synchronization
import XCTest
@testable import Jed

final class PageServiceConnectionTests: XCTestCase {
    func testWaitingWriterReplyDoesNotBlockPageReplyAndEOFReleasesWaiters() throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("jed-swift-mux-" + UUID().uuidString)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: directory) }
        let listener = try UnixServiceSocket.listen(path: directory.appendingPathComponent("s").path)
        defer { listener.close() }
        let socket = try UnixServiceSocket.connect(path: directory.appendingPathComponent("s").path, deadline: .now.advanced(by: .seconds(2)))
        let server = try listener.accept(), done = DispatchSemaphore(value: 0), failure = Mutex<JedError?>(nil)
        Thread.detachNewThread {
            defer { server.close(); done.signal() }
            do {
                let hello = try XCTUnwrap(server.readFrame(pageSize: 0, deadline: .now.advanced(by: .seconds(2))))
                try server.send(ServiceFrame(request: hello.request, message: .helloOK(ServiceHelloReply(protocolVersion: 1,
                    format: MetaPage.version, pageSize: 1024, connection: 7, flags: 0, maxReadPages: 1024))), pageSize: 0)
                let writer = try XCTUnwrap(server.readFrame(pageSize: 1024, deadline: .now.advanced(by: .seconds(2))))
                let pages = try XCTUnwrap(server.readFrame(pageSize: 1024, deadline: .now.advanced(by: .seconds(2))))
                guard writer.message.code == 5, pages.message.code == 3 else { throw JedError("XX000", "unexpected test request order") }
                try server.send(ServiceFrame(request: pages.request, message: .pages([[UInt8](repeating: 0x4a, count: 1024)])), pageSize: 1024)
                let read = ServiceReadReply(point: ServiceCachePoint(incarnation: [UInt8](repeating: 1, count: 16), txid: 3),
                    invalidate: 1, indices: [], meta: [UInt8](repeating: 0, count: 1024))
                try server.send(ServiceFrame(request: writer.request, message: .writeBegun(ServiceWriteReply(read: read,
                    reuseGrant: true, pending: [], reuse: []))), pageSize: 1024)
                _ = try server.readFrame(pageSize: 1024, deadline: .now.advanced(by: .seconds(2)))
                // EOF instead of replying to commit_status releases its waiter.
            } catch { failure.withLock { $0 = error as? JedError ?? JedError("XX000", "test service failed") } }
        }
        let connection = try PageServiceConnection.open(socket: socket, hello: ServiceHello(), deadline: .now.advanced(by: .seconds(2)))
        defer { connection.close() }
        let writer = try connection.request(.beginWrite(.cold, timeout: 5000, flags: 0))
        let pages = try connection.request(.readPages(txid: 2, indices: [2]))
        guard case .pages(let values) = try pages.wait() else { return XCTFail("wrong page response") }
        XCTAssertEqual(values, [[UInt8](repeating: 0x4a, count: 1024)])
        guard case .writeBegun(let granted) = try writer.wait() else { return XCTFail("wrong writer response") }
        XCTAssertEqual(granted.read.point.txid, 3)
        let unanswered = try connection.request(.commitStatus(txid: 3, digest: 1))
        XCTAssertThrowsError(try unanswered.wait()) { XCTAssertEqual(($0 as? JedError)?.code, "08006") }
        XCTAssertEqual(done.wait(timeout: .now() + 2), .success)
        XCTAssertNil(failure.withLock { $0 })
    }
}
#endif
