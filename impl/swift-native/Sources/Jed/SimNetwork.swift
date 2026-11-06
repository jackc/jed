#if os(Linux)
import Foundation

/// A host byte-stream network. Production protocol readers still decode every
/// frame; each write waits until its peer has processed it and requested more.
internal final class SimNetwork: @unchecked Sendable {
    fileprivate let condition = NSCondition()
    fileprivate var listeners: [String: SimListener] = [:]
    fileprivate var ends: [UInt64: SimStreamEnd] = [:]
    fileprivate var records: [String: ServiceOwnerRecord] = [:]
    private var next: UInt64 = 1
    private var actions = 0
    fileprivate var frames = 0, disconnectAt = 0
    fileprivate(set) var disconnected = false

    func dispatch(_ body: @escaping @Sendable () -> Void) {
        condition.lock(); actions += 1; condition.unlock()
        Thread.detachNewThread {
            body()
            self.condition.lock(); self.actions -= 1; self.condition.broadcast(); self.condition.unlock()
        }
    }
    func beginStep(disconnectAt: Int = 0) {
        condition.lock(); defer { condition.unlock() }
        frames = 0; self.disconnectAt = disconnectAt; disconnected = false
    }
    func owner(_ path: String) -> ServiceOwnerRecord? {
        condition.lock(); defer { condition.unlock() }; return records[path]
    }
    func writeOwner(_ path: String, _ record: ServiceOwnerRecord) throws {
        // The owner record uses the same bounded wire codec as native files.
        guard let decoded = ServiceOwnerRecord.decode(try record.encode()) else {
            throw JedError("58030", "invalid simulated owner record")
        }
        condition.lock(); records[path] = decoded; condition.unlock()
    }
    func listen(_ endpoint: String, process: Int) throws -> any ServiceListener {
        condition.lock(); defer { condition.unlock() }
        guard listeners[endpoint] == nil else { throw JedError("58030", "simulated endpoint is already bound") }
        let listener = SimListener(network: self, endpoint: endpoint, process: process)
        listeners[endpoint] = listener; return listener
    }
    func connect(_ endpoint: String, process: Int) throws -> any ServiceFrameTransport {
        condition.lock(); defer { condition.unlock() }
        guard let listener = listeners[endpoint], !listener.closed else { throw JedError("08006", "simulated connection refused") }
        let client = SimStreamEnd(network: self, id: next, process: process, endpoint: endpoint)
        let server = SimStreamEnd(network: self, id: next + 1, process: listener.process, endpoint: endpoint)
        next += 2; client.peer = server; server.peer = client
        ends[client.id] = client; ends[server.id] = server
        listener.pending.append(server); condition.broadcast()
        return PageFrameStream(client)
    }
    func remove(_ endpoint: String) {
        condition.lock(); defer { condition.unlock() }
        listeners[endpoint]?.closeLocked(); condition.broadcast()
    }
    func kill(_ process: Int) {
        condition.lock(); defer { condition.unlock() }
        for listener in Array(listeners.values) where listener.process == process { listener.closeLocked() }
        for end in ends.values where end.process == process { end.closeLocked() }
        condition.broadcast()
    }
    func disconnect(_ process: Int, endpoint: String) {
        condition.lock(); defer { condition.unlock() }
        for end in ends.values where end.process == process && end.endpoint == endpoint {
            end.dropLocked()
        }
        condition.broadcast()
    }
    func settle() throws {
        condition.lock(); defer { condition.unlock() }
        // This is a diagnostic bound for a broken host protocol, never a source
        // of simulated time or a successful trace outcome.
        let limit = Date(timeIntervalSinceNow: 10)
        while !quiescent {
            guard condition.wait(until: limit) else { throw SimViolation("simulated network did not become quiescent") }
        }
        for (id, end) in ends where end.finished && end.peer?.finished != false { ends.removeValue(forKey: id) }
    }
    private var quiescent: Bool {
        actions == 0 && listeners.values.allSatisfy { $0.pending.isEmpty } && ends.values.allSatisfy {
            $0.finished || !$0.closed && $0.waiting && $0.inbox.isEmpty
        }
    }
}

fileprivate final class SimListener: ServiceListener, @unchecked Sendable {
    weak var network: SimNetwork?
    let endpoint: String, process: Int
    var pending: [SimStreamEnd] = [], closed = false
    init(network: SimNetwork, endpoint: String, process: Int) {
        self.network = network; self.endpoint = endpoint; self.process = process
    }
    func acceptConnection() throws -> any ServiceFrameTransport {
        guard let network else { throw JedError("08006", "simulated network ended") }
        network.condition.lock(); defer { network.condition.unlock() }
        while pending.isEmpty && !closed { network.condition.wait() }
        guard !closed else { throw JedError("08006", "simulated listener is closed") }
        let end = pending.removeFirst(); network.condition.broadcast(); return PageFrameStream(end)
    }
    func closeLocked() {
        guard !closed else { return }; closed = true
        // Pending ends have no protocol reader to acknowledge their cleanup.
        for end in pending { end.closeLocked(); end.finished = true }
        pending.removeAll()
        if network?.listeners[endpoint] === self { network?.listeners.removeValue(forKey: endpoint) }
    }
    func close() {
        guard let network else { return }
        network.condition.lock(); closeLocked(); network.condition.broadcast(); network.condition.unlock()
    }
}

/// `closed` wakes I/O, while `finished` confirms the real reader has released
/// its transactions and reply state. EOF alone must not satisfy quiescence.
fileprivate final class SimStreamEnd: PageTransport, ServiceReaderLifetime, @unchecked Sendable {
    weak var network: SimNetwork?, peer: SimStreamEnd?
    let id: UInt64, process: Int
    let endpoint: String
    var inbox: [UInt8] = []
    var eof = false, closed = false, waiting = false, finished = false
    init(network: SimNetwork, id: UInt64, process: Int, endpoint: String) { self.network = network; self.id = id; self.process = process; self.endpoint = endpoint }
    func read(maxBytes: Int, deadline: ContinuousClock.Instant?) throws -> [UInt8] {
        guard let network else { return [] }
        network.condition.lock(); defer { network.condition.unlock() }
        while true {
            if !inbox.isEmpty {
                let count = min(maxBytes, inbox.count), result = Array(inbox.prefix(count))
                inbox.removeFirst(count); waiting = false; return result
            }
            if eof || closed { waiting = false; return [] }
            waiting = true; network.condition.broadcast(); network.condition.wait()
        }
    }
    func write(_ bytes: [UInt8], deadline: ContinuousClock.Instant?) throws {
        guard let network else { throw JedError("08006", "simulated network ended") }
        network.condition.lock(); defer { network.condition.unlock() }
        guard !closed, !eof, let peer else { throw JedError("08006", "simulated connection is closed") }
        network.frames += 1
        if network.disconnectAt > 0 && network.frames == network.disconnectAt {
            network.disconnected = true; dropLocked(); network.condition.broadcast()
            throw JedError("08006", "simulated connection dropped")
        }
        peer.inbox.append(contentsOf: bytes); peer.waiting = false; network.condition.broadcast()
        while !(peer.waiting && peer.inbox.isEmpty) && !peer.closed && !closed { network.condition.wait() }
    }
    func dropLocked() {
        eof = true; waiting = false; peer?.eof = true; peer?.waiting = false
    }
    func closeLocked() {
        if closed { return }; closed = true; waiting = false
        peer?.eof = true; peer?.waiting = false
    }
    func close() {
        guard let network else { return }
        network.condition.lock(); closeLocked(); network.condition.broadcast(); network.condition.unlock()
    }
    func readerFinished() {
        guard let network else { return }
        network.condition.lock(); finished = true; network.condition.broadcast(); network.condition.unlock()
    }
}
#endif
