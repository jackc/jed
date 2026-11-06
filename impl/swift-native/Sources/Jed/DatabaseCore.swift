import Foundation
import Synchronization

/// A transaction owns a token, not a thread. The condition protects the token
/// and permits an independently requested cancellation to interrupt a waiter.
/// NSCondition is the sole synchronization for this class's mutable fields.
internal final class WriterGate: @unchecked Sendable {
    private let condition = NSCondition()
    private var owner: StorageRevision?

    func acquire(cancellation: CancellationToken? = nil, wait: Bool = true) throws -> StorageRevision {
        condition.lock(); defer { condition.unlock() }
        while owner != nil {
            if cancellation?.isCancelled == true { throw JedError("57014", "canceling statement due to user request") }
            guard wait else { throw JedError("55006", "database has an active writer") }
            // Waiting time is host coordination, never SQL clock access or cost.
            _ = condition.wait(until: Date(timeIntervalSinceNow: 0.01))
        }
        if cancellation?.isCancelled == true { throw JedError("57014", "canceling statement due to user request") }
        let token = StorageRevision(); owner = token; return token
    }
    func release(_ token: StorageRevision) {
        condition.lock(); defer { condition.unlock() }
        if owner === token { owner = nil; condition.broadcast() }
    }
    func owns(_ token: StorageRevision) -> Bool {
        condition.lock(); defer { condition.unlock() }; return owner === token
    }
}

/// One independently releasable reference to a published version. A cursor
/// retains its own pin instead of sharing a session's close operation.
internal final class SnapshotPin: Sendable {
    let snapshot: DomainSnapshot
    private let owner: DatabaseCore
    private let active = Mutex(true)
    fileprivate init(snapshot: DomainSnapshot, owner: DatabaseCore) { self.snapshot = snapshot; self.owner = owner }
    func retain() throws -> SnapshotPin {
        try active.withLock { active in
            guard active else { throw JedError("55000", "snapshot pin is closed") }
            return owner.pinRetained(snapshot)
        }
    }
    func close() {
        let release = active.withLock { value in let previous = value; value = false; return previous }
        if release { owner.releasePin(snapshot.txid) }
    }
    deinit { close() }
}

/// Storage-domain coordination shared by every session. Root publication and
/// pin registration use the same lock, including the persist→publish window.
/// SQL/session state and attachment routing stay above this storage identity.
internal final class DatabaseCore: Sendable {
    struct Registry: Sendable {
        var storage: StorageDomain
        var snapshot: DomainSnapshot
        var pins: [UInt64: Int] = [:]
    }
    var storage: StorageDomain { registry.withLock { $0.storage } }
    private let registry: Mutex<Registry>
    private let writer = WriterGate()

    init(storage: StorageDomain) { registry = Mutex(Registry(storage: storage, snapshot: storage.committed)) }
    var version: UInt64 { registry.withLock { $0.snapshot.txid } }
    var oldestLiveVersion: UInt64 { registry.withLock { $0.pins.keys.min() ?? $0.snapshot.txid } }
    var livePinCount: Int { registry.withLock { $0.pins.values.reduce(0, +) } }

    func pinLatest() -> SnapshotPin {
        registry.withLock { state in
            state.pins[state.snapshot.txid, default: 0] += 1
            return SnapshotPin(snapshot: state.snapshot, owner: self)
        }
    }
    fileprivate func pinRetained(_ snapshot: DomainSnapshot) -> SnapshotPin {
        registry.withLock { state in state.pins[snapshot.txid, default: 0] += 1 }
        return SnapshotPin(snapshot: snapshot, owner: self)
    }
    fileprivate func releasePin(_ version: UInt64) {
        registry.withLock { state in
            guard let count = state.pins[version] else { return }
            if count == 1 { state.pins.removeValue(forKey: version) }
            else { state.pins[version] = count - 1 }
        }
    }
    func acquireWriter(cancellation: CancellationToken? = nil, wait: Bool = true) throws -> WriterLease {
        guard !storage.readOnly else { throw JedError("25006", "cannot set transaction read-write mode on a read-only database") }
        let token = try writer.acquire(cancellation: cancellation, wait: wait)
        return WriterLease(core: self, token: token, snapshot: registry.withLock { $0.snapshot })
    }
    /// Maintenance/close must fail immediately while a transaction holds the
    /// gate. A read-only handle may flush prior committed batches through this
    /// same exclusion seam; the operation itself checks its storage capability.
    func maintenance<T>(_ operation: () throws -> T) throws -> T {
        let token = try writer.acquire(wait: false); defer { writer.release(token) }
        return try operation()
    }
    /// The watermark remains locked through streaming and replacement. A reader
    /// can pin either address space, never a root whose file has been replaced.
    func replaceStorage(_ operation: (StorageDomain, DomainSnapshot) throws -> StorageDomain) throws {
        let token = try writer.acquire(wait: false); defer { writer.release(token) }
        try registry.withLock { state in
            guard state.pins.isEmpty else { throw JedError("55006", "database has active readers") }
            let replacement = try operation(state.storage, state.snapshot)
            state.storage = replacement; state.snapshot = replacement.committed
        }
    }
    fileprivate func check(_ lease: WriterLease) throws {
        guard lease.core === self, writer.owns(lease.token) else { throw JedError("55000", "writer lease is closed") }
    }
    fileprivate func policy(_ requested: DomainCommitPolicy) -> DomainCommitPolicy {
        registry.withLock { state in
            var result = requested
            let next = state.snapshot.txid == UInt64.max ? UInt64.max : state.snapshot.txid + 1
            result.oldestPinnedVersion = state.pins.keys.min() ?? next
            result.canCompactCommitted = result.canCompactCommitted && (state.pins.keys.min() ?? state.snapshot.txid) >= state.snapshot.txid
            return result
        }
    }
    fileprivate func publish(_ snapshot: DomainSnapshot) {
        registry.withLock { state in state.snapshot = snapshot }
    }
    fileprivate func releaseWriter(_ token: StorageRevision) { writer.release(token) }
}

/// The owner serializes calls on this lease. Prepared commits are immutable and
/// may be preflighted across every changed domain before any domain persists.
internal final class WriterLease {
    fileprivate let core: DatabaseCore
    fileprivate let token: StorageRevision
    let snapshot: DomainSnapshot
    fileprivate init(core: DatabaseCore, token: StorageRevision, snapshot: DomainSnapshot) {
        self.core = core; self.token = token; self.snapshot = snapshot
    }
    func pin(_ working: DomainSnapshot) throws -> SnapshotPin {
        try core.check(self)
        guard working.identity === snapshot.identity, working.txid == snapshot.txid else { throw JedError("55000", "snapshot does not belong to this transaction") }
        return core.pinRetained(working)
    }
    func precheck(_ working: DomainSnapshot, policy: DomainCommitPolicy = .init()) throws {
        try core.check(self); try core.storage.precheck(working, policy: core.policy(policy))
    }
    func prepare(_ working: DomainSnapshot, policy: DomainCommitPolicy = .init(), entropy: KeyEntropy? = nil) throws -> PreparedDomainCommit? {
        try core.check(self)
        return try core.storage.prepare(working, policy: core.policy(policy), entropy: entropy)
    }
    @discardableResult func persist(_ prepared: PreparedDomainCommit, synchronous: Bool = true,
                                   syncDirectory: () throws -> Void = {}, afterPersist: () throws -> Void = {}) throws -> DomainSnapshot {
        try core.check(self)
        let replacement = try core.storage.persist(prepared, synchronous: synchronous, syncDirectory: syncDirectory)
        // The hook is a deterministic host-boundary test seam. Publication must
        // still happen if an observer throws after durable storage succeeded.
        defer { core.publish(replacement) }
        try afterPersist()
        return replacement
    }
    func stageMemory(_ prepared: PreparedDomainCommit) throws -> StagedDomainCommit {
        try core.check(self)
        return try core.storage.stageMemory(prepared)
    }
    @discardableResult func adoptMemory(_ staged: StagedDomainCommit) throws -> DomainSnapshot {
        try core.check(self)
        let replacement = try core.storage.adoptMemory(staged)
        core.publish(replacement)
        return replacement
    }
    func close() { core.releaseWriter(token) }
    deinit { close() }
}
