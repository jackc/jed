#if os(Linux)
import Foundation
import Synchronization

/// One reply can arrive from the reader thread while its initiating caller
/// blocks. A waiting begin_write owns no connection lock, so reads keep moving.
internal final class ServiceReply: @unchecked Sendable {
    private let condition = NSCondition()
    private var result: Result<ServiceMessage, JedError>?
    func finish(_ value: Result<ServiceMessage, JedError>) {
        condition.lock(); defer { condition.unlock() }
        guard result == nil else { return }; result = value; condition.broadcast()
    }
    func wait(cancellation: CancellationToken? = nil,
              deadline: ContinuousClock.Instant? = nil, timeoutCode: String = "08006",
              onCancel: @Sendable () -> Void = {}) throws -> ServiceMessage {
        condition.lock()
        while result == nil {
            if cancellation?.isCancelled == true {
                condition.unlock(); onCancel()
                throw JedError("57014", "canceling statement due to user request")
            }
            if let deadline, ContinuousClock.now >= deadline {
                condition.unlock(); onCancel()
                throw JedError(timeoutCode, "page-service reply wait timed out")
            }
            if cancellation == nil && deadline == nil { condition.wait() }
            else { _ = condition.wait(until: Date(timeIntervalSinceNow: 0.001)) }
        }
        let completed = result!; condition.unlock()
        return try completed.get()
    }
}

/// A client multiplexes all sessions over one socket. Frame assignment/send is
/// ordered independently of response waiting. The dedicated receive loop never
/// invokes SQL or decodes database pages.
internal final class PageServiceConnection: Sendable {
    private struct State {
        var next: UInt32 = 2
        var waiting: [UInt32: ServiceReply] = [:]
        var failure: JedError?
    }
    let hello: ServiceHelloReply
    private let socket: UnixServiceSocket
    private let state = Mutex(State())
    private let outbound = NSLock()
    private let notice: @Sendable (UInt8, UInt64) -> Void
    private let disconnected: @Sendable () -> Void

    private init(socket: UnixServiceSocket, hello: ServiceHelloReply,
                 notice: @escaping @Sendable (UInt8, UInt64) -> Void,
                 disconnected: @escaping @Sendable () -> Void) {
        self.socket = socket; self.hello = hello; self.notice = notice; self.disconnected = disconnected
    }
    static func open(socket: UnixServiceSocket, hello request: ServiceHello,
                     deadline: ContinuousClock.Instant,
                     notice: @escaping @Sendable (UInt8, UInt64) -> Void = { _, _ in },
                     disconnected: @escaping @Sendable () -> Void = {}) throws -> PageServiceConnection {
        do {
            try socket.send(ServiceFrame(request: 1, message: .hello(request)), pageSize: 0, deadline: deadline)
            guard let frame = try socket.readFrame(pageSize: 0, deadline: deadline), frame.request == 1 else {
                throw JedError("08006", "page-service hello ended without its reply")
            }
            if case .error(let code, let message, let detail, _) = frame.message {
                var error = JedError(code, message); error.detail = detail.isEmpty ? nil : detail; throw error
            }
            guard case .helloOK(let reply) = frame.message else { throw PageServiceProtocol.violation("hello received another reply kind") }
            guard reply.protocolVersion == PageServiceProtocol.version, reply.format == MetaPage.version else {
                throw JedError("0A000", "page-service protocol or file format is unsupported")
            }
            try PageEnvelope.validateSize(Int(reply.pageSize), encrypted: false, opening: true)
            guard reply.connection != 0, reply.maxReadPages > 0,
                  reply.maxReadPages <= PageServiceProtocol.readLimit(pageSize: Int(reply.pageSize)) else {
                throw PageServiceProtocol.violation("invalid hello limits")
            }
            let result = PageServiceConnection(socket: socket, hello: reply, notice: notice, disconnected: disconnected)
            Thread.detachNewThread { result.receive() }
            return result
        } catch { socket.close(); throw error }
    }
    var isClosed: Bool { state.withLock { $0.failure != nil } }

    private func issue(_ message: ServiceMessage, reply: Bool) throws -> ServiceReply? {
        // The caller holds outbound until the frame has reached the stream.
        let waiter = reply ? ServiceReply() : nil
        let request = try state.withLock { state -> UInt32 in
            if let failure = state.failure { throw failure }
            let request = state.next
            state.next = request == UInt32.max ? 1 : request + 1
            guard state.waiting[request] == nil else { throw JedError("54000", "page-service request id space exhausted") }
            if let waiter { state.waiting[request] = waiter }
            return request
        }
        do { try socket.send(ServiceFrame(request: request, message: message), pageSize: Int(hello.pageSize)) }
        catch { fail(Self.connectionError(error)); throw Self.connectionError(error) }
        return waiter
    }
    /// Returns immediately after sending, so callers can protect pin selection
    /// and transmission together but wait without holding their pin mutex.
    func request(_ message: ServiceMessage) throws -> ServiceReply {
        outbound.lock(); defer { outbound.unlock() }
        return try issue(message, reply: true)!
    }
    func notify(_ message: ServiceMessage) throws {
        outbound.lock(); defer { outbound.unlock() }
        _ = try issue(message, reply: false)
    }
    /// No request may interleave with a delta. The caller knows whether a lost
    /// connection happened before the complete end frame, which chooses ordinary
    /// failure versus commit-status resolution.
    func sendCommit(_ begin: ServiceCommitBegin, body: [StoredPage], endSent: inout Bool) throws -> ServiceReply {
        outbound.lock(); defer { outbound.unlock() }
        endSent = false
        // Refuse an oversized manifest before emitting any commit frame.
        let pageSize = Int(hello.pageSize)
        guard begin.manifest.count <= (PageServiceProtocol.maxFrameLength - 17 - pageSize) / (pageSize + 4) else {
            throw JedError("54000", "commit manifest exceeds page-service frame limit")
        }
        _ = try ServiceFrame(request: 1, message: .commitBegin(begin)).encode(pageSize: Int(hello.pageSize))
        _ = try issue(.commitBegin(begin), reply: false)
        let capacity = (PageServiceProtocol.maxFrameLength - 9) / (Int(hello.pageSize) + 4)
        let ordered = body.sorted { $0.index < $1.index }
        var index = 0
        while index < ordered.count {
            let last = min(ordered.count, index + capacity)
            _ = try issue(.commitPages(Array(ordered[index..<last])), reply: false); index = last
        }
        let reply = try issue(.commitEnd, reply: true)!
        endSent = true
        return reply
    }
    private static func connectionError(_ error: any Error) -> JedError {
        if let error = error as? JedError { return error }
        return JedError("08006", "page-service connection failed")
    }
    private func receive() {
        do {
            while let frame = try socket.readFrame(pageSize: Int(hello.pageSize)) {
                if frame.request == 0, case .notice(let kind, let connection) = frame.message {
                    notice(kind, connection); continue
                }
                let waiter = state.withLock { $0.waiting.removeValue(forKey: frame.request) }
                if case .error(let code, let message, let detail, let flags) = frame.message {
                    var error = JedError(code, message); error.detail = detail.isEmpty ? nil : detail
                    waiter?.finish(.failure(error))
                    if flags & 1 != 0 { fail(JedError("08006", "page-service owner closed the connection")); return }
                    guard waiter != nil else { throw PageServiceProtocol.violation("error answers an unknown request") }
                } else {
                    guard let waiter else { throw PageServiceProtocol.violation("reply answers an unknown request") }
                    waiter.finish(.success(frame.message))
                }
            }
            fail(JedError("08006", "page-service connection ended"))
        } catch { fail(Self.connectionError(error)) }
    }
    private func fail(_ error: JedError) {
        let waiting = state.withLock { state -> [ServiceReply]? in
            guard state.failure == nil else { return nil }
            state.failure = error
            let waiting = Array(state.waiting.values); state.waiting.removeAll()
            return waiting
        }
        guard let waiting else { return }
        for waiter in waiting { waiter.finish(.failure(error)) }
        socket.close()
        // Notification may initiate re-election, which must not recursively
        // acquire an outbound lock still held by a failed sender.
        Thread.detachNewThread { [disconnected] in disconnected() }
    }
    func close() { fail(JedError("08006", "page-service connection is closed")) }
    deinit { socket.close() }
}
#endif
