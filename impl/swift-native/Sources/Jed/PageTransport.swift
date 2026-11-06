#if os(Linux)
import Foundation
import Glibc
import Synchronization
import CJedHost

/// Host-supplied duplex bytes for the page service. Reads return at most
/// maxBytes, with an empty result only at EOF. close must wake pending I/O.
/// The transport carries pages, never SQL or application function calls.
public protocol PageTransport: AnyObject, Sendable {
    func read(maxBytes: Int, deadline: ContinuousClock.Instant?) throws -> [UInt8]
    func write(_ bytes: [UInt8], deadline: ContinuousClock.Instant?) throws
    func close()
    var diagnostic: String { get }
}
extension PageTransport { public var diagnostic: String { "" } }

internal protocol ServiceFrameTransport: Sendable {
    func readFrame(pageSize: Int, deadline: ContinuousClock.Instant?) throws -> ServiceFrame?
    func send(_ frame: ServiceFrame, pageSize: Int, deadline: ContinuousClock.Instant?) throws
    func close()
    var diagnostic: String { get }
}
extension ServiceFrameTransport {
    func readFrame(pageSize: Int) throws -> ServiceFrame? { try readFrame(pageSize: pageSize, deadline: nil) }
    func send(_ frame: ServiceFrame, pageSize: Int) throws { try send(frame, pageSize: pageSize, deadline: nil) }
    var diagnostic: String { "" }
}
extension UnixServiceSocket: ServiceFrameTransport {}

internal final class PageFrameStream: ServiceFrameTransport, Sendable {
    let transport: any PageTransport
    private let outbound = NSLock()
    init(_ transport: any PageTransport) { self.transport = transport }
    var diagnostic: String { transport.diagnostic }
    private func exact(_ count: Int, eof: Bool, deadline: ContinuousClock.Instant?) throws -> [UInt8]? {
        var result: [UInt8] = []; result.reserveCapacity(count)
        while result.count < count {
            let bytes = try transport.read(maxBytes: count - result.count, deadline: deadline)
            guard bytes.count <= count - result.count else { throw JedError("08006", "page transport exceeded its requested read bound") }
            if bytes.isEmpty {
                if eof && result.isEmpty { return nil }
                throw JedError("08006", "page transport ended inside a frame")
            }
            result.append(contentsOf: bytes)
        }
        return result
    }
    func readRaw(deadline: ContinuousClock.Instant? = nil) throws -> [UInt8]? {
        guard let header = try exact(4, eof: true, deadline: deadline) else { return nil }
        var reader = ByteReader(header); let length = try reader.u32()
        try PageServiceProtocol.checkLength(length)
        return header + (try exact(Int(length), eof: false, deadline: deadline)!)
    }
    func sendRaw(_ bytes: [UInt8], deadline: ContinuousClock.Instant? = nil) throws {
        outbound.lock(); defer { outbound.unlock() }
        try transport.write(bytes, deadline: deadline)
    }
    func readFrame(pageSize: Int, deadline: ContinuousClock.Instant? = nil) throws -> ServiceFrame? {
        guard let bytes = try readRaw(deadline: deadline) else { return nil }
        do { return try ServiceFrame.decode(bytes, pageSize: pageSize) }
        catch {
            var reader = ByteReader(bytes); _ = try reader.take(5)
            throw ServiceFrameFailure(request: try reader.u32(), underlying: error)
        }
    }
    func send(_ frame: ServiceFrame, pageSize: Int, deadline: ContinuousClock.Instant? = nil) throws {
        try sendRaw(frame.encode(pageSize: pageSize), deadline: deadline)
    }
    func close() { transport.close() }
}

/// Owned nonblocking descriptors. Polling is bounded so close wakes a pipe read
/// without closing a descriptor still borrowed by another thread.
internal final class DescriptorPageTransport: PageTransport, @unchecked Sendable {
    private let lifetime = NSCondition()
    private var input: Int32, output: Int32
    private var active = 0, closing = false
    init(input: Int32, output: Int32) throws {
        self.input = input; self.output = output
        for fd in Set([input, output]).filter({ $0 >= 0 }) {
            guard fcntl(fd, F_SETFD, FD_CLOEXEC) == 0,
                  fcntl(fd, F_SETFL, fcntl(fd, F_GETFL) | O_NONBLOCK) == 0 else {
                for owned in Set([input, output]).filter({ $0 >= 0 }) { _ = Glibc.close(owned) }
                self.input = -1; self.output = -1
                throw JedError("08006", "could not configure page transport")
            }
        }
    }
    private func acquire(write: Bool) throws -> Int32 {
        lifetime.lock(); defer { lifetime.unlock() }
        let fd = write ? output : input
        guard !closing, fd >= 0 else { throw JedError("08006", "page transport is closed") }
        active += 1; return fd
    }
    private func release() { lifetime.lock(); active -= 1; lifetime.broadcast(); lifetime.unlock() }
    private func wait(_ fd: Int32, writing: Bool, deadline: ContinuousClock.Instant?) throws {
        while true {
            lifetime.lock(); let closed = closing; lifetime.unlock()
            guard !closed else { throw JedError("08006", "page transport is closed") }
            if let deadline, ContinuousClock.now >= deadline { throw JedError("08006", "page transport timed out") }
            var item = pollfd(fd: fd, events: Int16(writing ? POLLOUT : POLLIN), revents: 0)
            let ready = Glibc.poll(&item, 1, 10)
            if ready < 0 && errno == EINTR { continue }
            guard ready >= 0, item.revents & Int16(POLLNVAL) == 0 else { throw JedError("08006", "page transport poll failed") }
            if ready > 0 { return }
        }
    }
    func read(maxBytes: Int, deadline: ContinuousClock.Instant?) throws -> [UInt8] {
        guard maxBytes > 0, maxBytes <= PageServiceProtocol.maxFrameLength else { throw JedError("08006", "invalid transport read bound") }
        let fd = try acquire(write: false); defer { release() }
        var result = [UInt8](repeating: 0, count: min(maxBytes, 64 * 1024))
        while true {
            try wait(fd, writing: false, deadline: deadline)
            let count = result.withUnsafeMutableBytes { Glibc.read(fd, $0.baseAddress, $0.count) }
            if count < 0 && (errno == EINTR || errno == EAGAIN || errno == EWOULDBLOCK) { continue }
            guard count >= 0 else { throw JedError("08006", "page transport read failed") }
            result.removeLast(result.count - count); return result
        }
    }
    func write(_ bytes: [UInt8], deadline: ContinuousClock.Instant?) throws {
        let fd = try acquire(write: true); defer { release() }
        var offset = 0
        while offset < bytes.count {
            try wait(fd, writing: true, deadline: deadline)
            let count = bytes.withUnsafeBytes { jed_host_write_nosigpipe(fd, $0.baseAddress!.advanced(by: offset), $0.count - offset) }
            if count < 0 && (errno == EINTR || errno == EAGAIN || errno == EWOULDBLOCK) { continue }
            guard count > 0 else { throw JedError("08006", "page transport write failed") }
            offset += count
        }
    }
    func close() {
        lifetime.lock(); defer { lifetime.unlock() }
        if closing { while input >= 0 || output >= 0 { lifetime.wait() }; return }
        closing = true
        while active != 0 { lifetime.wait() }
        for fd in Set([input, output]).filter({ $0 >= 0 }) { _ = Glibc.close(fd) }
        input = -1; output = -1; lifetime.broadcast()
    }
    deinit { close() }
}

/// Explicit host process transport, for example ["ssh", "host", "jed", "pages", path].
/// Arguments never receive the credential: it travels only in each hello frame.
private final class TransportDiagnostics: Sendable {
    let bytes = Mutex<[UInt8]>([])
}

public final class ProcessPageTransport: PageTransport, @unchecked Sendable {
    private let process: Int32
    private let stream: DescriptorPageTransport
    private let errors: DescriptorPageTransport
    private let errorBytes = TransportDiagnostics()
    private let closed = Mutex(false)
    private let diagnosticsDone = DispatchGroup()
    public init(command: [String]) throws {
        guard let program = command.first, !program.isEmpty, command.allSatisfy({ !$0.utf8.contains(0) }) else {
            throw JedError("22023", "empty or invalid page transport command")
        }
        var arguments = command.map { strdup($0) }
        defer { for argument in arguments { free(argument) } }
        guard arguments.allSatisfy({ $0 != nil }) else { throw JedError("08006", "could not allocate transport arguments") }
        arguments.append(nil)
        var child: Int32 = -1, input: Int32 = -1, output: Int32 = -1, errors: Int32 = -1
        let launched = arguments.withUnsafeMutableBufferPointer {
            jed_host_spawn_transport($0.baseAddress!, &child, &input, &output, &errors)
        }
        guard launched == 0 else {
            throw JedError(errno == ENOSYS ? "0A000" : "08006", "could not safely start page transport (errno \(errno))")
        }
        process = child
        do {
            // Each constructor consumes its descriptors, including on failure.
            let readFD = output, writeFD = input; output = -1; input = -1
            stream = try DescriptorPageTransport(input: readFD, output: writeFD)
            let errorFD = errors; errors = -1
            do { self.errors = try DescriptorPageTransport(input: errorFD, output: -1) }
            catch { stream.close(); throw error }
        } catch {
            for fd in [input, output, errors] where fd >= 0 { _ = Glibc.close(fd) }
            Self.reap(child)
            throw error
        }
        let retained = self.errors, diagnostics = errorBytes, finished = diagnosticsDone
        finished.enter()
        Thread.detachNewThread {
            defer { retained.close(); finished.leave() }
            do {
                while true {
                    let bytes = try retained.read(maxBytes: 4096, deadline: nil)
                    if bytes.isEmpty { return }
                    diagnostics.bytes.withLock { stored in
                        let remaining = max(0, 64 * 1024 - stored.count)
                        stored.append(contentsOf: bytes.prefix(remaining))
                    }
                }
            } catch { }
        }
    }
    public var diagnostic: String { errorBytes.bytes.withLock { String(decoding: $0, as: UTF8.self) } }
    public func read(maxBytes: Int, deadline: ContinuousClock.Instant?) throws -> [UInt8] { try stream.read(maxBytes: maxBytes, deadline: deadline) }
    public func write(_ bytes: [UInt8], deadline: ContinuousClock.Instant?) throws { try stream.write(bytes, deadline: deadline) }
    public func close() {
        guard closed.withLock({ value in let old = value; value = true; return !old }) else { return }
        stream.close()
        Self.reap(process)
        _ = diagnosticsDone.wait(timeout: .now() + .milliseconds(100))
        errors.close()
    }
    /// The kernel-created process descriptor remains tied to this child even
    /// if the embedding host reaps it or ignores SIGCHLD. No PID is signalled.
    private static func reap(_ process: Int32) {
        defer { _ = Glibc.close(process) }
        if jed_host_wait_transport(process, 0) == 1 { return }
        _ = jed_host_signal_transport(process, SIGTERM)
        if jed_host_wait_transport(process, 100) == 1 { return }
        _ = jed_host_signal_transport(process, SIGKILL)
        _ = jed_host_wait_transport(process, -1)
    }
    deinit { close() }
}
#endif
