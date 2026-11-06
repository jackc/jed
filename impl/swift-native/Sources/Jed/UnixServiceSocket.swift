#if os(Linux)
import Foundation
import Glibc

internal struct ServiceFrameFailure: Error {
    let request: UInt32
    let underlying: any Error
}

/// Narrow Unix stream transport. The condition protects descriptor lifetime:
/// shutdown wakes active operations before close can recycle the descriptor.
/// All pointer borrows cover only a synchronous POSIX call and owned Swift bytes.
internal final class UnixServiceSocket: @unchecked Sendable {
    private let lifetime = NSCondition()
    private var descriptor: Int32
    private var active = 0
    private var closing = false
    private let writes = NSLock()
    init(descriptor: Int32) { self.descriptor = descriptor }

    private static func failure(_ operation: String) -> JedError { JedError("08006", "page-service \(operation) failed (errno \(errno))") }
    private func acquire() throws -> Int32 {
        lifetime.lock(); defer { lifetime.unlock() }
        guard !closing, descriptor >= 0 else { throw JedError("08006", "page-service connection is closed") }
        active += 1; return descriptor
    }
    private func checkActive(_ fd: Int32) throws {
        lifetime.lock(); defer { lifetime.unlock() }
        guard !closing, descriptor == fd else { throw JedError("08006", "page-service connection is closed") }
    }
    private func release() {
        lifetime.lock(); defer { lifetime.unlock() }
        active -= 1; if active == 0 { lifetime.broadcast() }
    }
    func close() {
        lifetime.lock(); defer { lifetime.unlock() }
        if closing { while descriptor >= 0 { lifetime.wait() }; return }
        closing = true
        if descriptor >= 0 { _ = Glibc.shutdown(descriptor, Int32(SHUT_RDWR)) }
        while active != 0 { lifetime.wait() }
        if descriptor >= 0 { _ = Glibc.close(descriptor); descriptor = -1 }
        lifetime.broadcast()
    }
    deinit { close() }

    private static func address(_ path: String) throws -> sockaddr_un {
        let bytes = Array(path.utf8)
        guard !bytes.isEmpty, !bytes.contains(0), bytes.count <= PageServiceProtocol.endpointMaxBytes else {
            throw JedError("08006", "invalid page-service endpoint path")
        }
        var address = sockaddr_un(); address.sun_family = sa_family_t(AF_UNIX)
        withUnsafeMutableBytes(of: &address.sun_path) { storage in
            storage.initializeMemory(as: UInt8.self, repeating: 0)
            storage.copyBytes(from: bytes)
        }
        return address
    }
    private static func newDescriptor() throws -> Int32 {
        let fd = Glibc.socket(AF_UNIX, Int32(SOCK_STREAM.rawValue | SOCK_CLOEXEC.rawValue | SOCK_NONBLOCK.rawValue), 0)
        guard fd >= 0 else { throw failure("socket") }
        return fd
    }
    private static func wait(_ fd: Int32, events: Int16, deadline: ContinuousClock.Instant?) throws {
        while true {
            let milliseconds: Int32
            if let deadline {
                let remaining = ContinuousClock.now.duration(to: deadline)
                guard remaining > .zero else { throw JedError("08006", "page-service transport timeout") }
                let parts = remaining.components
                let seconds = min(parts.seconds, Int64(Int32.max) / 1000)
                milliseconds = Int32(min(Int64(Int32.max), seconds * 1000 + max(1, parts.attoseconds / 1_000_000_000_000_000)))
            } else { milliseconds = -1 }
            var item = pollfd(fd: fd, events: events, revents: 0)
            let ready = Glibc.poll(&item, 1, milliseconds)
            if ready < 0, errno == EINTR { continue }
            guard ready > 0 else {
                if ready == 0 { throw JedError("08006", "page-service transport timeout") }
                throw failure("poll")
            }
            if item.revents & Int16(POLLNVAL) != 0 { throw JedError("08006", "page-service descriptor is invalid") }
            // EOF/error must be observed by recv/send, including a final frame
            // that arrived together with POLLHUP.
            return
        }
    }
    static func connect(path: String, deadline: ContinuousClock.Instant) throws -> UnixServiceSocket {
        let fd = try newDescriptor()
        do {
            var address = try address(path)
            let result = withUnsafePointer(to: &address) {
                $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { Glibc.connect(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size)) }
            }
            if result != 0 {
                guard errno == EINPROGRESS || errno == EAGAIN else { throw failure("connect") }
                try wait(fd, events: Int16(POLLOUT), deadline: deadline)
                var code: Int32 = 0, length = socklen_t(MemoryLayout<Int32>.size)
                guard getsockopt(fd, SOL_SOCKET, SO_ERROR, &code, &length) == 0 else { throw failure("connect status") }
                guard code == 0 else { throw JedError("08006", "page-service connect failed (errno \(code))") }
            }
            return UnixServiceSocket(descriptor: fd)
        } catch { _ = Glibc.close(fd); throw error }
    }
    /// Only a presence-EX holder may create a listener or unlink its old path.
    static func listen(path: String) throws -> UnixServiceSocket {
        let fd = try newDescriptor()
        do {
            var address = try address(path)
            if path.withCString({ Glibc.unlink($0) }) != 0, errno != ENOENT { throw failure("remove stale socket") }
            let result = withUnsafePointer(to: &address) {
                $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { Glibc.bind(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size)) }
            }
            guard result == 0, Glibc.listen(fd, 128) == 0 else { throw failure("listen") }
            return UnixServiceSocket(descriptor: fd)
        } catch { _ = Glibc.close(fd); throw error }
    }
    func accept() throws -> UnixServiceSocket {
        let fd = try acquire(); defer { release() }
        while true {
            // Linux can report HUP then EAGAIN after listener shutdown. Check
            // the lifetime state before retrying so close can drain this lease.
            try checkActive(fd)
            try Self.wait(fd, events: Int16(POLLIN), deadline: nil)
            let peer = Glibc.accept(fd, nil, nil)
            if peer < 0, errno == EINTR || errno == EAGAIN || errno == EWOULDBLOCK { continue }
            guard peer >= 0 else { throw Self.failure("accept") }
            guard fcntl(peer, F_SETFD, FD_CLOEXEC) == 0, fcntl(peer, F_SETFL, O_NONBLOCK) == 0 else {
                _ = Glibc.close(peer); throw Self.failure("configure accepted socket")
            }
            return UnixServiceSocket(descriptor: peer)
        }
    }
    /// Nil is clean EOF before the length prefix. EOF within a frame is 08006.
    func readFrame(pageSize: Int, deadline: ContinuousClock.Instant? = nil) throws -> ServiceFrame? {
        let fd = try acquire(); defer { release() }
        guard let lengthBytes = try readExactly(fd, count: 4, allowEOF: true, deadline: deadline) else { return nil }
        var header = ByteReader(lengthBytes); let length = try header.u32()
        try PageServiceProtocol.checkLength(length)
        let body = try readExactly(fd, count: Int(length), allowEOF: false, deadline: deadline)!
        do { return try ServiceFrame.decode(lengthBytes + body, pageSize: pageSize) }
        catch {
            var header = ByteReader(body)
            _ = try header.u8()
            let request = try header.u32()
            throw ServiceFrameFailure(request: request, underlying: error)
        }
    }
    private func readExactly(_ fd: Int32, count: Int, allowEOF: Bool,
                             deadline: ContinuousClock.Instant?) throws -> [UInt8]? {
        var bytes = [UInt8](repeating: 0, count: count), position = 0
        while position < count {
            try Self.wait(fd, events: Int16(POLLIN), deadline: deadline)
            let read = bytes.withUnsafeMutableBufferPointer {
                Glibc.recv(fd, $0.baseAddress!.advanced(by: position), count - position, 0)
            }
            if read < 0, errno == EINTR || errno == EAGAIN || errno == EWOULDBLOCK { continue }
            if read == 0 {
                if position == 0, allowEOF { return nil }
                throw JedError("08006", "page-service connection ended inside a frame")
            }
            guard read > 0 else { throw Self.failure("receive") }
            position += read
        }
        return bytes
    }
    func send(_ frame: ServiceFrame, pageSize: Int, deadline: ContinuousClock.Instant? = nil) throws {
        let bytes = try frame.encode(pageSize: pageSize)
        writes.lock(); defer { writes.unlock() }
        let fd = try acquire(); defer { release() }
        var position = 0
        while position < bytes.count {
            try Self.wait(fd, events: Int16(POLLOUT), deadline: deadline)
            let sent = bytes.withUnsafeBufferPointer {
                Glibc.send(fd, $0.baseAddress!.advanced(by: position), bytes.count - position, Int32(MSG_NOSIGNAL))
            }
            if sent < 0, errno == EINTR || errno == EAGAIN || errno == EWOULDBLOCK { continue }
            guard sent > 0 else { throw Self.failure("send") }
            position += sent
        }
    }
}
#endif
