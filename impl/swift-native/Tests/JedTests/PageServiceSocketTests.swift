#if os(Linux)
import Foundation
import Dispatch
import Synchronization
import XCTest
@testable import Jed

final class PageServiceSocketTests: XCTestCase {
    private func directory() throws -> String {
        let url = FileManager.default.temporaryDirectory.appendingPathComponent("jed-swift-socket-" + UUID().uuidString)
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: false)
        addTeardownBlock { try FileManager.default.removeItem(at: url) }
        return url.path
    }
    func testNativeFramingRoundTripAndCloseInterruptsPendingRead() throws {
        let endpoint = try directory() + "/s"
        let listener = try UnixServiceSocket.listen(path: endpoint)
        defer { listener.close() }
        let client = try UnixServiceSocket.connect(path: endpoint, deadline: .now.advanced(by: .seconds(2)))
        defer { client.close() }
        let server = try listener.accept()
        try client.send(ServiceFrame(request: 1, message: .hello(ServiceHello())), pageSize: 0)
        let frame = try XCTUnwrap(server.readFrame(pageSize: 0, deadline: .now.advanced(by: .seconds(2))))
        XCTAssertEqual(frame.request, 1); XCTAssertEqual(frame.message.code, 1)
        try server.send(ServiceFrame(request: 1, message: .helloOK(ServiceHelloReply(protocolVersion: 1, format: 50,
            pageSize: 1024, connection: 9, flags: 0, maxReadPages: 1024))), pageSize: 0)
        XCTAssertEqual(try client.readFrame(pageSize: 0, deadline: .now.advanced(by: .seconds(2)))?.message.code, 0x41)
        let entered = DispatchSemaphore(value: 0), finished = DispatchSemaphore(value: 0)
        Thread.detachNewThread {
            entered.signal()
            _ = try? server.readFrame(pageSize: 1024)
            finished.signal()
        }
        XCTAssertEqual(entered.wait(timeout: .now() + 2), .success)
        server.close()
        XCTAssertEqual(finished.wait(timeout: .now() + 2), .success)
        XCTAssertNil(try client.readFrame(pageSize: 1024, deadline: .now.advanced(by: .seconds(2))))
    }
    func testListenerCloseInterruptsPendingAccept() throws {
        let listener = try UnixServiceSocket.listen(path: directory() + "/s")
        let entered = DispatchSemaphore(value: 0), accepted = DispatchSemaphore(value: 0), closed = DispatchSemaphore(value: 0)
        Thread.detachNewThread {
            entered.signal(); _ = try? listener.accept(); accepted.signal()
        }
        XCTAssertEqual(entered.wait(timeout: .now() + 2), .success)
        Thread.detachNewThread { listener.close(); closed.signal() }
        XCTAssertEqual(accepted.wait(timeout: .now() + 2), .success)
        XCTAssertEqual(closed.wait(timeout: .now() + 2), .success)
    }
    func testEndpointFallbackRejectsSymlinkOrNonPrivateBundleDirectory() throws {
        let directory = try directory(), database = directory + "/db", bundled = database + ".lock"
        try FileManager.default.createDirectory(atPath: bundled, withIntermediateDirectories: false)
        let incarnation = [UInt8](repeating: 0x19, count: 16)
        let selected = try XCTUnwrap(PageServiceEndpoint.choose(database: database, incarnation: incarnation))
        if selected.hasPrefix(bundled) {
            let parent = URL(fileURLWithPath: selected).deletingLastPathComponent().path
            XCTAssertEqual((try FileManager.default.attributesOfItem(atPath: parent)[.posixPermissions] as? NSNumber)?.intValue, 0o700)
            try FileManager.default.removeItem(atPath: parent)
            try FileManager.default.createSymbolicLink(atPath: parent, withDestinationPath: directory)
            let fallback = PageServiceEndpoint.choose(database: database, incarnation: incarnation)
            XCTAssertFalse(fallback?.hasPrefix(bundled) ?? false)
        }
    }
}
#endif
