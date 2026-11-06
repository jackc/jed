#if os(Linux)
import Foundation
import Synchronization

internal struct PageServiceOwnerLimits: Sendable {
    var idleSessionMilliseconds: UInt64 = 0
    var idleWriteMilliseconds: UInt64 = 60_000
    var maxConnections: Int = 0
}

/// Background transport only. The owner's ordinary queries use DatabaseCore
/// directly, so serving does not add socket or lock syscalls to their path.
internal final class PageServiceOwner: Sendable {
    private struct State {
        var closed = false
        var nextConnection: UInt64 = 1
        var connections: [UInt64: PageServiceOwnerConnection] = [:]
        var admitted: Set<UInt64> = []
        var dropNextAcknowledgement = false
        var beforeNextAcknowledgement: (@Sendable () -> Void)?
        var draining = false
        var drainRequester: UInt64?
    }
    let core: DatabaseCore
    let limits: PageServiceOwnerLimits
    let yields: Bool
    private let listener: any ServiceListener
    private let state = Mutex(State())
    private let entropy: @Sendable (Int) throws -> [UInt8]
    private let ownership: @Sendable (UInt64) -> Bool
    private let ended: @Sendable (UInt64) -> Void
    private let poisoned: @Sendable () -> Void
    private let synchronous: @Sendable () -> Bool
    private let storageLimit: @Sendable () -> Int64
    private let timers: Bool
    private let dispatchWork: @Sendable (@escaping @Sendable () -> Void) -> Void

    init(core: DatabaseCore, listener: any ServiceListener, limits: PageServiceOwnerLimits = .init(),
         yields: Bool = false, entropy: @escaping @Sendable (Int) throws -> [UInt8],
         ownership: @escaping @Sendable (UInt64) -> Bool = { _ in false },
         ended: @escaping @Sendable (UInt64) -> Void = { _ in },
         poisoned: @escaping @Sendable () -> Void = {},
         synchronous: @escaping @Sendable () -> Bool = { true },
         storageLimit: @escaping @Sendable () -> Int64 = { 0 }, timers: Bool = true,
         dispatchWork: @escaping @Sendable (@escaping @Sendable () -> Void) -> Void = { Thread.detachNewThread($0) }) {
        self.core = core; self.listener = listener; self.limits = limits; self.yields = yields
        self.entropy = entropy; self.ownership = ownership; self.ended = ended; self.poisoned = poisoned
        self.synchronous = synchronous; self.storageLimit = storageLimit; self.timers = timers; self.dispatchWork = dispatchWork
    }
    func start() {
        Thread.detachNewThread { self.acceptLoop() }
        if timers { Thread.detachNewThread { self.timeoutLoop() } }
    }
    private func acceptLoop() {
        do {
            while !state.withLock({ $0.closed }) {
                let socket = try listener.acceptConnection()
                let connection = state.withLock { state -> PageServiceOwnerConnection? in
                    guard !state.closed, state.nextConnection != 0 else { return nil }
                    let id = state.nextConnection; state.nextConnection &+= 1
                    let connection = PageServiceOwnerConnection(owner: self, socket: socket, id: id)
                    state.connections[id] = connection; return connection
                }
                if let connection { connection.start() } else { socket.close(); socket.readerFinished() }
            }
        } catch { if !state.withLock({ $0.closed }) { fail() } }
    }
    private func timeoutLoop() {
        while true {
            let connections = state.withLock { state -> [PageServiceOwnerConnection]? in
                state.closed ? nil : Array(state.connections.values)
            }
            guard let connections else { return }
            let now = ContinuousClock.now
            for connection in connections { connection.checkTimeout(now) }
            Thread.sleep(forTimeInterval: 0.01)
        }
    }
    fileprivate func admit(_ id: UInt64) throws {
        try state.withLock { state in
            guard !state.closed else { throw JedError("08006", "page-service owner is closing") }
            if limits.maxConnections > 0, state.admitted.count >= limits.maxConnections {
                throw JedError("53300", "too many page-service connections")
            }
            state.admitted.insert(id)
        }
    }
    fileprivate func remove(_ id: UInt64) {
        state.withLock { state in state.connections.removeValue(forKey: id); state.admitted.remove(id) }
        ended(id)
    }
    fileprivate func requestOwnership(_ id: UInt64) -> Bool { ownership(id) }
    fileprivate func keyEntropy(_ count: Int) throws -> [UInt8] { try entropy(count) }
    fileprivate func afterAcknowledgement(_ lease: ServiceWriterLease) throws {
        try lease.afterAcknowledgement(entropy: entropy, synchronous: synchronous(), storageLimit: storageLimit())
    }
    fileprivate func dropAcknowledgement() -> Bool {
        let action = state.withLock { state -> (Bool, (@Sendable () -> Void)?) in
            let result = (state.dropNextAcknowledgement, state.beforeNextAcknowledgement)
            state.dropNextAcknowledgement = false; state.beforeNextAcknowledgement = nil
            return result
        }
        action.1?()
        return action.0
    }
    func beforeNextCommitAcknowledgement(_ action: @escaping @Sendable () -> Void) {
        state.withLock { $0.beforeNextAcknowledgement = action }
    }
    func dropNextCommitAcknowledgement() { state.withLock { $0.dropNextAcknowledgement = true } }
    func notice(kind: UInt8, connection: UInt64) {
        let connections = state.withLock { Array($0.connections.values) }
        for client in connections { client.notice(kind: kind, connection: connection) }
    }
    var isDraining: Bool { state.withLock { $0.draining } }
    fileprivate func isDraining(requester: UInt64) -> Bool {
        state.withLock { !$0.closed && $0.draining && $0.drainRequester == requester }
    }
    var hasClientTransactions: Bool {
        let clients = state.withLock { Array($0.connections.values) }
        return clients.contains { $0.hasTransaction }
    }
    /// Close the resident admission gate before acknowledging an ownership
    /// request. Notices/cancellation take connection locks and run separately.
    func markDraining(requester: UInt64) -> Bool {
        state.withLock { state in
            guard !state.closed, state.drainRequester == nil || state.drainRequester == requester else { return false }
            state.draining = true; state.drainRequester = requester
            return true
        }
    }
    func beginDrain(kind: UInt8, requester: UInt64 = 0) {
        let clients = state.withLock { state -> [PageServiceOwnerConnection] in
            guard !state.closed else { return [] }
            if requester == 0 {
                guard state.drainRequester != 0 else { return [] }
                state.draining = true; state.drainRequester = 0
            } else {
                // An asynchronous worker only announces its existing
                // reservation; it cannot resurrect or replace a canceled one.
                guard state.draining, state.drainRequester == requester else { return [] }
            }
            return Array(state.connections.values)
        }
        for client in clients { client.announceDrain(kind: kind, requester: requester) }
    }
    func clearDrainReservation(requester: UInt64) -> Bool {
        state.withLock { state in
            guard state.drainRequester == requester else { return false }
            state.draining = false; state.drainRequester = nil
            return true
        }
    }
    func announceDrainCancellation() {
        let clients = state.withLock { state in
            state.draining ? [] : Array(state.connections.values)
        }
        for client in clients { client.announceDrainCancellation() }
    }
    func cancelDrain(requester: UInt64) {
        if clearDrainReservation(requester: requester) { announceDrainCancellation() }
    }
    fileprivate func dispatch(_ body: @escaping @Sendable () -> Void) { dispatchWork(body) }
    fileprivate func fail() {
        // A failing connection may hold its own operation lock. Stop from a
        // separate worker so draining cannot recursively acquire that lock.
        dispatchWork { self.poisoned(); self.stop() }
    }
    func stop() {
        let connections = state.withLock { state -> [PageServiceOwnerConnection]? in
            guard !state.closed else { return nil }
            state.closed = true; return Array(state.connections.values)
        }
        guard let connections else { return }
        listener.close()
        for connection in connections { connection.close() }
        core.stopService()
    }
}

/// operation serializes protocol state with idle revocation and EOF cleanup.
/// The blocking writer-gate wait runs outside it; all other requests retain
/// their ordinary stream order. No fields below escape that synchronization.
private final class PageServiceOwnerConnection: @unchecked Sendable {
    private let owner: PageServiceOwner
    private let socket: any ServiceFrameTransport
    private let id: UInt64
    private let operation = NSRecursiveLock()
    private var closed = false
    private var greeted = false
    private var readOnly = false
    private var lastActivity = ContinuousClock.now
    private var reads: [UInt64: [ServiceReadPin]] = [:]
    private var waiting: CancellationToken?
    private var waitingFrame: ServiceFrame?
    private var queued: [ServiceFrame] = []
    private var writer: ServiceWriterLease?
    private var delta: ServiceIncomingDelta?
    private var deltaError: JedError?
    private var revoked: UInt64?

    init(owner: PageServiceOwner, socket: any ServiceFrameTransport, id: UInt64) {
        self.owner = owner; self.socket = socket; self.id = id
    }
    func start() { Thread.detachNewThread { self.run() } }
    private func send(_ request: UInt32, _ message: ServiceMessage) throws {
        try socket.send(ServiceFrame(request: request, message: message), pageSize: owner.core.storage.pager.pageSize)
    }
    private func error(_ request: UInt32, _ error: JedError, fatal: Bool) {
        try? send(request, .error(code: error.code, message: error.message, detail: error.detail ?? "", flags: fatal ? 1 : 0))
    }
    private func run() {
        defer { close() }
        do {
            while let frame = try socket.readFrame(pageSize: owner.core.storage.pager.pageSize) {
                operation.lock()
                do {
                    if closed { operation.unlock(); return }
                    lastActivity = .now
                    try handle(frame)
                    operation.unlock()
                } catch {
                    let failure = error as? JedError ?? JedError("58030", "page-service owner failed")
                    if owner.core.storage.pager.isPoisoned { owner.fail(); operation.unlock(); return }
                    let fatal = !["25006", "55P03", "25P03"].contains(failure.code)
                    self.error(frame.request, failure, fatal: fatal)
                    operation.unlock()
                    if fatal { return }
                }
            }
        } catch let failure as ServiceFrameFailure {
            let value = failure.underlying as? JedError ?? PageServiceProtocol.violation("invalid frame")
            error(failure.request, value, fatal: true)
        } catch { /* EOF and physical transport failure release the connection. */ }
    }
    private func handle(_ frame: ServiceFrame) throws {
        guard frame.request != 0 else { throw PageServiceProtocol.violation("client request id is zero") }
        if !greeted {
            guard case .hello(let hello) = frame.message else { throw PageServiceProtocol.violation("first request is not hello") }
            guard hello.credential == nil else { throw PageServiceProtocol.violation("local owner must not receive a credential") }
            guard hello.minimum <= PageServiceProtocol.version, hello.maximum >= PageServiceProtocol.version,
                  hello.format == MetaPage.version else {
                var failure = JedError("0A000", "unsupported page-service protocol or file format")
                failure.detail = "supported protocol 1, file format \(MetaPage.version)"; throw failure
            }
            try owner.admit(id); greeted = true; readOnly = hello.flags & 1 != 0
            let size = owner.core.storage.pager.pageSize
            let flags: UInt8 = (readOnly ? 1 : 0) | (owner.core.storage.readOnly ? 2 : 0) | (owner.yields ? 4 : 0)
            try send(frame.request, .helloOK(ServiceHelloReply(protocolVersion: PageServiceProtocol.version,
                format: MetaPage.version, pageSize: UInt32(size), connection: id, flags: flags,
                maxReadPages: UInt32(PageServiceProtocol.readLimit(pageSize: size)))))
            return
        }
        if delta != nil || deltaError != nil {
            switch frame.message {
            case .commitPages, .commitEnd: break
            default: throw PageServiceProtocol.violation("request interleaves a commit delta")
            }
        }
        switch frame.message {
        case .beginRead, .beginWrite:
            if owner.isDraining { queued.append(frame); return }
        default: break
        }
        switch frame.message {
        case .beginRead(let point):
            let pin = try owner.core.serviceBeginRead(point)
            reads[pin.selected.meta.txid, default: []].append(pin)
            try send(frame.request, .readBegun(pin.reply))
        case .readPages(let txid, let indices):
            if revoked == txid { revoked = nil; throw JedError("25P03", "idle write transaction was revoked") }
            let selected = reads[txid]?.first?.selected ?? (writer?.selected.meta.txid == txid ? writer?.selected : nil)
            guard let selected, !indices.isEmpty,
                  indices.count <= PageServiceProtocol.readLimit(pageSize: selected.meta.pageSize),
                  indices.allSatisfy({ $0 >= 2 && $0 < selected.meta.pageCount }) else {
                throw PageServiceProtocol.violation("read_pages is outside a pinned snapshot")
            }
            let pages = try indices.map { try owner.core.storage.pager.readStored($0) }
            try send(frame.request, .pages(pages))
        case .endRead(let txid):
            guard var pins = reads[txid], let pin = pins.popLast() else { throw PageServiceProtocol.violation("end_read has no matching pin") }
            if pins.isEmpty { reads.removeValue(forKey: txid) } else { reads[txid] = pins }
            pin.close()
        case .beginWrite(let point, let timeout, let flags):
            if revoked != nil { revoked = nil; throw JedError("25P03", "idle write transaction was revoked") }
            guard writer == nil, waiting == nil else { throw PageServiceProtocol.violation("connection already holds or awaits a writer") }
            guard !readOnly, !owner.core.storage.readOnly else { throw JedError("25006", "page-service connection or owner is read only") }
            let cancellation = CancellationToken(); waiting = cancellation; waitingFrame = frame
            owner.dispatch { self.acquireWriter(frame.request, point: point, timeout: timeout, relaxed: flags & 1 != 0, cancellation: cancellation) }
        case .commitBegin(let begin):
            if revoked != nil { return }
            guard let writer else { throw PageServiceProtocol.violation("commit_begin has no writer") }
            do { delta = try writer.begin(begin) }
            catch let failure as JedError where failure.code == "08P01" { deltaError = failure }
        case .commitPages(let pages):
            if revoked != nil || deltaError != nil { return }
            guard let writer, delta != nil else { throw PageServiceProtocol.violation("commit_pages has no delta") }
            for page in pages {
                do { try delta!.receive(page) }
                catch let failure as JedError where failure.code == "08P01" { deltaError = failure; return }
                try writer.write(page)
            }
        case .commitEnd:
            if revoked != nil { revoked = nil; throw JedError("25P03", "idle write transaction was revoked") }
            guard let writer else { throw PageServiceProtocol.violation("commit_end has no writer") }
            if let deltaError { throw deltaError }
            guard let delta else { throw PageServiceProtocol.violation("commit_end has no delta") }
            try delta.complete()
            // This operation owns the lease across the point of no return,
            // acknowledgement and any cap flush, including a dropped socket.
            defer { self.delta = nil; self.writer = nil; writer.close() }
            let txid = try writer.finish(delta, entropy: owner.keyEntropy)
            if owner.dropAcknowledgement() { socket.close() }
            var sendFailure: (any Error)?
            do { try send(frame.request, .committed(txid)) } catch { sendFailure = error }
            try owner.afterAcknowledgement(writer)
            if let sendFailure { throw sendFailure }
        case .abort:
            if revoked != nil { revoked = nil; return }
            guard let writer else { throw PageServiceProtocol.violation("abort has no write transaction") }
            self.writer = nil; delta = nil; deltaError = nil; writer.close()
        case .commitStatus(let txid, let digest):
            try send(frame.request, .commitStatusReply(owner.core.serviceStatus(txid: txid, digest: digest)))
        case .requestOwnership:
            try send(frame.request, .ownershipReply(owner.requestOwnership(id) ? 0 : 1))
        default: throw PageServiceProtocol.violation("unexpected client message")
        }
    }
    private func acquireWriter(_ request: UInt32, point: ServiceCachePoint, timeout: UInt64,
                               relaxed: Bool, cancellation: CancellationToken) {
        let result = Result { try owner.core.serviceBeginWrite(point, relaxed: relaxed,
            timeoutMilliseconds: timeout, cancellation: cancellation) }
        operation.lock(); defer { operation.unlock() }
        let original = waitingFrame; waiting = nil; waitingFrame = nil
        if closed { if case .success(let lease) = result { lease.close() }; return }
        if owner.isDraining || cancellation.isCancelled {
            if case .success(let lease) = result { lease.close() }
            if let original { queued.append(original) }
            if !owner.isDraining { resumeAdmission() }
            return
        }
        do {
            let lease = try result.get(); writer = lease; lastActivity = .now
            try send(request, .writeBegun(lease.reply))
        } catch {
            if owner.core.storage.pager.isPoisoned { owner.fail(); return }
            let failure = error as? JedError ?? JedError("08006", "writer acquisition failed")
            self.error(request, failure, fatal: failure.code != "55P03")
            if failure.code != "55P03" { close() }
        }
    }
    func checkTimeout(_ now: ContinuousClock.Instant) {
        operation.lock(); defer { operation.unlock() }
        guard !closed else { return }
        let limits = owner.limits
        if let writer, limits.idleWriteMilliseconds > 0,
           now >= lastActivity.advanced(by: .milliseconds(Int64(clamping: limits.idleWriteMilliseconds))) {
            revoked = writer.selected.meta.txid; self.writer = nil; delta = nil; deltaError = nil
            writer.close()
        } else if reads.isEmpty, writer == nil, waiting == nil, limits.idleSessionMilliseconds > 0,
                  now >= lastActivity.advanced(by: .milliseconds(Int64(clamping: limits.idleSessionMilliseconds))) {
            close()
        }
    }
    var hasTransaction: Bool {
        operation.lock(); defer { operation.unlock() }
        return !reads.isEmpty || writer != nil
    }
    func announceDrain(kind: UInt8, requester: UInt64) {
        operation.lock(); defer { operation.unlock() }
        // State may have changed while this connection's current operation
        // finished. Check under the recipient lock, before pausing or sending.
        guard !closed, owner.isDraining(requester: requester) else { return }
        waiting?.cancel()
        notice(kind: kind, connection: requester)
    }
    func announceDrainCancellation() {
        operation.lock(); defer { operation.unlock() }
        guard !closed, !owner.isDraining else { return }
        resumeAdmission()
        notice(kind: 2, connection: 0)
    }
    func resumeAdmission() {
        operation.lock(); defer { operation.unlock() }
        guard !closed, !owner.isDraining else { return }
        let requests = queued; queued.removeAll()
        for frame in requests {
            do { try handle(frame) }
            catch {
                let failure = error as? JedError ?? JedError("08006", "deferred page-service request failed")
                self.error(frame.request, failure, fatal: true); close(); return
            }
        }
    }
    func notice(kind: UInt8, connection: UInt64) {
        operation.lock(); defer { operation.unlock() }
        guard !closed, greeted else { return }
        do { try send(0, .notice(kind: kind, connection: connection)) } catch { close() }
    }
    func close() {
        // Shutdown wakes a blocked receive/send before waiting for an operation.
        socket.close()
        operation.lock(); defer { operation.unlock() }
        guard !closed else { return }; closed = true
        waiting?.cancel(); waiting = nil; waitingFrame = nil; queued.removeAll()
        for pins in reads.values { for pin in pins { pin.close() } }; reads.removeAll()
        writer?.close(); writer = nil; delta = nil; deltaError = nil
        owner.remove(id)
        socket.readerFinished()
    }
}
#endif
