#if os(Linux)
import Foundation
import Glibc
import Synchronization
import XCTest
@testable import Jed

private final class FragmentedTransport: PageTransport {
    let source: Mutex<[UInt8]>
    let bounds = Mutex<[Int]>([])
    init(_ bytes: [UInt8]) { source = Mutex(bytes) }
    func read(maxBytes: Int, deadline: ContinuousClock.Instant?) throws -> [UInt8] {
        bounds.withLock { $0.append(maxBytes) }
        return source.withLock { bytes in
            let n = min(3, min(maxBytes, bytes.count)), result = Array(bytes.prefix(n))
            bytes.removeFirst(n); return result
        }
    }
    func write(_ bytes: [UInt8], deadline: ContinuousClock.Instant?) throws {}
    func close() {}
}

final class PageTransportBoundaryTests: XCTestCase {
    private func pipe() throws -> (Int32, Int32) {
        var values: [Int32] = [-1, -1]
        guard values.withUnsafeMutableBufferPointer({ Glibc.pipe($0.baseAddress!) }) == 0 else {
            throw JedError("58030", "test pipe creation failed")
        }
        return (values[0], values[1])
    }
    func testFragmentedFramesAndLengthRefusalBeforeBodyRead() throws {
        let expected = ServiceFrame(request: 1, message: .hello(ServiceHello()))
        let bytes = try expected.encode(pageSize: 0), transport = FragmentedTransport(bytes)
        let stream = PageFrameStream(transport)
        let decoded = try XCTUnwrap(stream.readFrame(pageSize: 0))
        XCTAssertEqual(try decoded.encode(pageSize: 0), bytes)
        XCTAssertNil(try stream.readFrame(pageSize: 0))
        let malformed = FragmentedTransport([0x01, 0, 0, 1] + [UInt8](repeating: 0, count: 20))
        XCTAssertThrowsError(try PageFrameStream(malformed).readFrame(pageSize: 0)) {
            XCTAssertEqual(($0 as? JedError)?.code, "08P01")
        }
        XCTAssertEqual(malformed.source.withLock { $0.count }, 20)
    }
    func testPipeCloseWakesReaderAndBrokenWriteDoesNotSignalProcess() throws {
        let (readFD, writeFD) = try pipe()
        let reader = try DescriptorPageTransport(input: readFD, output: -1)
        let done = DispatchSemaphore(value: 0), failure = Mutex<String?>(nil)
        Thread.detachNewThread {
            defer { done.signal() }
            do { _ = try reader.read(maxBytes: 16, deadline: nil) }
            catch { failure.withLock { $0 = (error as? JedError)?.code } }
        }
        reader.close()
        XCTAssertEqual(done.wait(timeout: .now() + 2), .success)
        XCTAssertEqual(failure.withLock { $0 }, "08006")
        let writer = try DescriptorPageTransport(input: -1, output: writeFD); defer { writer.close() }
        XCTAssertThrowsError(try writer.write([1], deadline: nil)) {
            XCTAssertEqual(($0 as? JedError)?.code, "08006")
        }
    }
    func testProcessFailurePreservesBoundedStderrAndOpenDeadline() throws {
        let options = OpenOptions(fileLockTimeoutMilliseconds: 100)
        XCTAssertThrowsError(try Database.openRemote(command: ["/bin/sh", "-c", "printf native-transport-diagnostic >&2; exit 1"], options: options)) {
            XCTAssertEqual(($0 as? JedError)?.code, "08006")
            XCTAssertTrue(($0 as? JedError)?.message.contains("native-transport-diagnostic") ?? false)
        }
        let start = ContinuousClock.now
        XCTAssertThrowsError(try Database.openRemote(command: ["/bin/sleep", "5"], options: options)) {
            XCTAssertEqual(($0 as? JedError)?.code, "08006")
        }
        XCTAssertLessThan(start.duration(to: .now), .seconds(2))
    }
}
#endif
