import Foundation
import Synchronization

/// A transaction owns a token, not a thread. The condition protects the token
/// and permits an independently requested cancellation to interrupt a waiter.
/// NSCondition is the sole synchronization for this class's mutable fields.
internal final class WriterGate: @unchecked Sendable {
    private let condition = NSCondition()
    private var owner: StorageRevision?
    private var serviceOwner = false
    private var localMaintenance: StorageRevision?

    func acquire(cancellation: CancellationToken? = nil, wait: Bool = true, timeoutMilliseconds: UInt64 = 0, service: Bool = false) throws -> StorageRevision {
        let deadline = timeoutMilliseconds == 0 ? nil : ContinuousClock.now.advanced(by: .milliseconds(Int64(clamping: timeoutMilliseconds)))
        condition.lock(); defer { condition.unlock() }
        while owner != nil || localMaintenance != nil {
            if cancellation?.isCancelled == true { throw JedError("57014", "canceling statement due to user request") }
            guard wait else { throw JedError("55006", "database has an active writer") }
            if let deadline, ContinuousClock.now >= deadline { throw JedError("55P03", "canceling statement due to lock timeout") }
            // Waiting time is host coordination, never SQL clock access or cost.
            _ = condition.wait(until: Date(timeIntervalSinceNow: 0.01))
        }
        if cancellation?.isCancelled == true { throw JedError("57014", "canceling statement due to user request") }
        let token = StorageRevision(); owner = token; serviceOwner = service; return token
    }
    func release(_ token: StorageRevision) {
        condition.lock(); defer { condition.unlock() }
        if owner === token { owner = nil; condition.broadcast() }
    }
    func acquireLocalMaintenance() throws -> StorageRevision {
        condition.lock(); defer { condition.unlock() }
        guard localMaintenance == nil, owner == nil || serviceOwner else { throw JedError("55006", "database has an active local writer") }
        let token = StorageRevision(); localMaintenance = token; return token
    }
    func releaseLocalMaintenance(_ token: StorageRevision) {
        condition.lock(); defer { condition.unlock() }
        if localMaintenance === token { localMaintenance = nil; condition.broadcast() }
    }
    var hasLocalWriter: Bool { condition.lock(); defer { condition.unlock() }; return localMaintenance != nil || owner != nil && !serviceOwner }
    var hasWriter: Bool { condition.lock(); defer { condition.unlock() }; return owner != nil || localMaintenance != nil }
    func revokeCurrent() { condition.lock(); owner = nil; condition.broadcast(); condition.unlock() }
    func owns(_ token: StorageRevision) -> Bool {
        condition.lock(); defer { condition.unlock() }; return owner === token
    }
}

/// One independently releasable reference to a published version. A cursor
/// retains its own pin instead of sharing a session's close operation.
internal final class SnapshotPin: Sendable {
    let snapshot: DomainSnapshot
    private let owner: DatabaseCore
    private let role: (any SnapshotRole)?
    private let epoch: StorageRevision
    private let active = Mutex(true)
    fileprivate init(snapshot: DomainSnapshot, owner: DatabaseCore, epoch: StorageRevision, role: (any SnapshotRole)? = nil) {
        self.snapshot = snapshot; self.owner = owner; self.role = role; self.epoch = epoch
    }
    func checkRole() throws { try owner.checkEpoch(epoch); try role?.checkRole() }
    func retain() throws -> SnapshotPin {
        try active.withLock { active in
            guard active else { throw JedError("55000", "snapshot pin is closed") }
            try checkRole(); try role?.retainRead(version: snapshot.txid)
            do { return try owner.pinRetained(snapshot, epoch: epoch, role: role) }
            catch { role?.releaseRead(version: snapshot.txid); throw error }
        }
    }
    func close() {
        let release = active.withLock { value in let previous = value; value = false; return previous }
        if release { owner.releasePin(snapshot.txid, epoch: epoch); role?.releaseRead(version: snapshot.txid) }
    }
    deinit { close() }
}

/// Transaction-owned cursors borrow the existing writer pin. Their owner
/// closes every cursor before releasing that writer, so no begin_read RPC is
/// needed. The token and epoch keep an escaped borrowed pin from remaining
/// usable after its transaction ends or changes storage roles.
private final class BorrowedWriterRole: SnapshotRole {
    let core: DatabaseCore, token: StorageRevision, epoch: StorageRevision
    let remote: (any DatabaseRemote)?
    init(core: DatabaseCore, token: StorageRevision, epoch: StorageRevision, remote: (any DatabaseRemote)?) {
        self.core = core; self.token = token; self.epoch = epoch; self.remote = remote
    }
    func checkRole() throws { try core.checkWriter(token: token, epoch: epoch); try remote?.checkRole() }
    func retainRead(version: UInt64) throws { try checkRole() }
    func releaseRead(version: UInt64) {}
}

/// Storage-domain coordination shared by every session. Root publication and
/// pin registration use the same lock, including the persist→publish window.
/// SQL/session state and attachment routing stay above this storage identity.
internal final class DatabaseCore: Sendable {
    let planIdentity = StorageRevision()
    struct Registry: Sendable {
        var storage: StorageDomain
        var snapshot: DomainSnapshot
        var meta: ValidatedMeta
        var history: PageServiceHistory?
        var epoch = StorageRevision()
        var admitting = true
        var sqlReady = true
        var remote: (any DatabaseRemote)?
        var pins: [UInt64: Int] = [:]
    }
    var storage: StorageDomain { registry.withLock { $0.storage } }
    private let registry: Mutex<Registry>
    private let writer = WriterGate()
    private let environment: Environment?
    private let commitBoundary = NSLock()
    private let coordinator = WeakRoleCoordinator()
    private var remote: (any DatabaseRemote)? { registry.withLock { $0.remote } }

    init(storage: StorageDomain, environment: Environment? = nil, remote: (any DatabaseRemote)? = nil, sqlReady: Bool = true) {
        self.environment = environment
        var initial = Registry(storage: storage, snapshot: storage.committed, meta: storage.pager.selected, remote: remote)
        initial.sqlReady = sqlReady; registry = Mutex(initial)
    }
    var version: UInt64 { registry.withLock { $0.meta.meta.txid } }
    /// Resident facts for provisional name/capability routing only. This does
    /// not adopt a newer service catalog or protect on-disk pages. The caller
    /// must revalidate under a writer lease or read pin before execution.
    var routingSnapshot: DomainSnapshot { registry.withLock { $0.snapshot } }
    var oldestLiveVersion: UInt64 { registry.withLock { $0.pins.keys.min() ?? $0.meta.meta.txid } }
    var livePinCount: Int { registry.withLock { $0.pins.values.reduce(0, +) } }

    /// Logical read admission. Local owners only register a pin; remote roles
    /// add their protocol begin and adoption at this throwing boundary.
    func beginRead() throws -> SnapshotPin {
        for attempt in 0...1 {
            try coordinator.value?.awaitAdmission()
            do {
                if let remote { return try remote.beginRead(core: self) }
                return try registry.withLock { state in
                    guard state.admitting else { throw JedError("08006", "database role is draining") }
                    try adoptIfNeeded(&state)
                    state.pins[state.snapshot.txid, default: 0] += 1
                    return SnapshotPin(snapshot: state.snapshot, owner: self, epoch: state.epoch)
                }
            } catch let failure as JedError where failure.code == "08006" && attempt == 0 && coordinator.value != nil { continue }
        }
        throw JedError("08006", "database role changed during read admission")
    }
    /// Capture the exact metadata belonging to a normal logical read pin.
    /// Publication racing the two steps retries before the backup writes bytes;
    /// the resident copy remains valid even when relaxed commits replace its slot.
    func beginBackup() throws -> PinnedBackup {
        for _ in 0..<1000 {
            let pin = try beginRead()
            let selected = registry.withLock { state -> PinnedBackup? in
                guard state.meta.meta.txid == pin.snapshot.txid,
                      state.storage.pool === pin.snapshot.pool else { return nil }
                return PinnedBackup(pin: pin, selected: state.meta, kind: state.storage.kind)
            }
            if let selected {
                do { try pin.checkRole(); return selected }
                catch { pin.close(); throw error }
            }
            pin.close()
        }
        throw JedError("55006", "could not pin a stable backup generation")
    }
    private func adoptIfNeeded(_ state: inout Registry) throws {
        if !state.sqlReady || state.snapshot.txid != state.meta.meta.txid {
            state.snapshot = try state.storage.adoptServiceSnapshot(state.meta, environment: environment, force: !state.sqlReady)
            state.sqlReady = true
        }
    }

    func pinLatest() -> SnapshotPin {
        registry.withLock { state in
            state.pins[state.snapshot.txid, default: 0] += 1
            return SnapshotPin(snapshot: state.snapshot, owner: self, epoch: state.epoch)
        }
    }
    fileprivate func pinRetained(_ snapshot: DomainSnapshot, epoch: StorageRevision, role: (any SnapshotRole)? = nil) throws -> SnapshotPin {
        try registry.withLock { state in
            guard state.epoch === epoch else { throw JedError("08006", "database storage role changed") }
            state.pins[snapshot.txid, default: 0] += 1
            return SnapshotPin(snapshot: snapshot, owner: self, epoch: epoch, role: role)
        }
    }
    fileprivate func releasePin(_ version: UInt64, epoch: StorageRevision) {
        registry.withLock { state in
            guard state.epoch === epoch, let count = state.pins[version] else { return }
            if count == 1 { state.pins.removeValue(forKey: version) }
            else { state.pins[version] = count - 1 }
        }
    }
    func acquireWriter(cancellation: CancellationToken? = nil, wait: Bool = true,
                       relaxed: Bool = false, timeoutMilliseconds: UInt64 = 0) throws -> WriterLease {
        let deadline = timeoutMilliseconds == 0 ? nil : ContinuousClock.now.advanced(by: .milliseconds(Int64(clamping: timeoutMilliseconds)))
        for attempt in 0...1 {
            var remaining = timeoutMilliseconds
            if let deadline {
                let duration = ContinuousClock.now.duration(to: deadline)
                let components = duration.components
                guard duration > .zero else { throw JedError("55P03", "canceling statement due to lock timeout") }
                remaining = max(1, UInt64(clamping: components.seconds) * 1000 + UInt64(clamping: components.attoseconds / 1_000_000_000_000_000))
            }
            do { return try acquireWriterOnce(cancellation: cancellation, wait: wait, relaxed: relaxed, timeoutMilliseconds: remaining) }
            catch let error as JedError where error.code == "08006" && attempt == 0 && coordinator.value != nil { continue }
        }
        throw JedError("08006", "database role changed during write admission")
    }
    private func acquireWriterOnce(cancellation: CancellationToken?, wait: Bool,
                                   relaxed: Bool, timeoutMilliseconds: UInt64) throws -> WriterLease {
        try coordinator.value?.awaitWriteAdmission(timeoutMilliseconds: timeoutMilliseconds, cancellation: cancellation)
        guard !storage.readOnly else { throw JedError("25006", "cannot set transaction read-write mode on a read-only database") }
        let token = try writer.acquire(cancellation: cancellation, wait: wait, timeoutMilliseconds: timeoutMilliseconds)
        do {
            let role = try registry.withLock { state in
                guard state.admitting else { throw JedError("08006", "database role is draining") }
                return (state.remote, state.epoch)
            }
            if let remote = role.0 {
                let snapshot = try remote.beginWrite(core: self, relaxed: relaxed, timeoutMilliseconds: timeoutMilliseconds, cancellation: cancellation)
                do { try checkEpoch(role.1) } catch { remote.endWrite(); throw error }
                return WriterLease(core: self, token: token, snapshot: snapshot, epoch: role.1, remote: remote)
            }
            return try registry.withLock { state in
                guard state.admitting, state.epoch === role.1 else { throw JedError("08006", "database storage role changed") }
                try adoptIfNeeded(&state)
                return WriterLease(core: self, token: token, snapshot: state.snapshot, epoch: state.epoch)
            }
        } catch { writer.release(token); throw error }
    }
    /// Maintenance/close must fail immediately while a transaction holds the
    /// gate. A read-only handle may flush prior committed batches through this
    /// same exclusion seam; the operation itself checks its storage capability.
    func maintenance<T>(_ operation: () throws -> T) throws -> T {
        let token = try writer.acquire(wait: false); defer { writer.release(token) }
        return try operation()
    }
    /// Rewrapping excludes this handle's local writers, but an idle page-client
    /// writer may retain its grant: the data key and generation stay unchanged.
    /// The commit boundary serializes key writes with streamed page delivery.
    func credentialMaintenance<T>(_ operation: () throws -> T) throws -> T {
        let token = try writer.acquireLocalMaintenance()
        defer { writer.releaseLocalMaintenance(token) }
        commitBoundary.lock(); defer { commitBoundary.unlock() }
        let admitted = registry.withLock { $0.admitting && $0.remote == nil }
        guard admitted else { throw JedError("08006", "database storage role changed") }
        return try operation()
    }
    func idleBatchFlush(expected: StorageDomain) throws -> Int64? {
        let token = try writer.acquire(wait: false); defer { writer.release(token) }
        commitBoundary.lock(); defer { commitBoundary.unlock() }
        let current = registry.withLock { state in state.admitting && state.remote == nil && state.storage === expected }
        guard current else { return nil }
        return try expected.pager.idleFlush()
    }
    /// The watermark remains locked through streaming and replacement. A reader
    /// can pin either address space, never a root whose file has been replaced.
    func replaceStorage(_ operation: (StorageDomain, DomainSnapshot) throws -> StorageDomain) throws {
        let token = try writer.acquire(wait: false); defer { writer.release(token) }
        commitBoundary.lock(); defer { commitBoundary.unlock() }
        guard writer.owns(token) else { throw JedError("08006", "database storage role changed") }
        try registry.withLock { state in
            guard state.pins.isEmpty else { throw JedError("55006", "database has active readers") }
            try adoptIfNeeded(&state)
            let replacement = try operation(state.storage, state.snapshot)
            state.storage = replacement; state.snapshot = replacement.committed; state.meta = replacement.pager.selected
            state.history?.publish(txid: state.meta.meta.txid, digest: PageServiceHistory.digest(state.meta), indices: [], reset: true)
        }
    }
    fileprivate func check(_ lease: WriterLease) throws {
        try lease.checkRole()
        guard lease.core === self, writer.owns(lease.token) else { throw JedError("55000", "writer lease is closed") }
    }

    fileprivate func checkWriter(token: StorageRevision, epoch: StorageRevision) throws {
        try checkEpoch(epoch)
        guard writer.owns(token) else { throw JedError("08006", "borrowed transaction snapshot ended") }
    }
    fileprivate func policy(_ requested: DomainCommitPolicy) -> DomainCommitPolicy {
        registry.withLock { state in
            var result = requested
            if state.remote != nil { result.reclaimAllowed = false; result.canCompactCommitted = false }
            let next = state.snapshot.txid == UInt64.max ? UInt64.max : state.snapshot.txid + 1
            result.oldestPinnedVersion = min(result.oldestPinnedVersion, state.pins.keys.min() ?? next)
            result.canCompactCommitted = result.canCompactCommitted && (state.pins.keys.min() ?? state.snapshot.txid) >= state.snapshot.txid
            return result
        }
    }
    fileprivate func publish(_ snapshot: DomainSnapshot, epoch: StorageRevision, written: [UInt32] = []) {
        registry.withLock { state in
            guard state.epoch === epoch else { return }
            state.snapshot = snapshot; state.meta = state.storage.pager.selected
            state.history?.publish(txid: state.meta.meta.txid, digest: PageServiceHistory.digest(state.meta),
                indices: written)
        }
    }
    fileprivate func releaseWriter(_ token: StorageRevision) { writer.release(token) }
    func adoptRemote(_ selected: ValidatedMeta, remote role: any DatabaseRemote) throws -> DomainSnapshot {
        try registry.withLock { state in
            guard state.remote === role else { throw JedError("08006", "database storage role changed") }
            state.meta = selected
            try adoptIfNeeded(&state)
            return state.snapshot
        }
    }
    func registerRemoteRead(_ snapshot: DomainSnapshot, role: any DatabaseRemote) throws -> SnapshotPin {
        try registry.withLock { state in
            guard state.remote === role else { throw JedError("08006", "database storage role changed") }
            state.pins[snapshot.txid, default: 0] += 1
            return SnapshotPin(snapshot: snapshot, owner: self, epoch: state.epoch, role: role)
        }
    }
    func checkEpoch(_ epoch: StorageRevision) throws {
        try registry.withLock { state in
            guard state.epoch === epoch else { throw JedError("08006", "database storage role changed") }
        }
    }
    func coordinateRoles(with value: any DatabaseRoleCoordinator) { coordinator.install(value) }
    var hasLocalWriter: Bool { writer.hasLocalWriter }
    var hasWriter: Bool { writer.hasWriter }
    func pauseAdmissions(rejectLocalWriter: Bool = false) throws {
        try registry.withLock { state in
            if rejectLocalWriter, writer.hasLocalWriter { throw JedError("55006", "database has an active writer") }
            state.admitting = false
        }
    }
    func resumeAdmissions() { registry.withLock { $0.admitting = true } }
    /// Stop admitting old transactions before the host closes or replaces the
    /// file descriptor. Active physical commit has already left this boundary.
    func quiesceRole(flush: Bool) throws {
        commitBoundary.lock(); defer { commitBoundary.unlock() }
        try registry.withLock { state in
            state.epoch = StorageRevision(); state.pins.removeAll(); state.history = nil; state.admitting = false
            writer.revokeCurrent()
            defer { state.storage.pager.revokeRole() }
            if flush { try state.storage.pager.flush(shrink: false) }
        }
    }
    /// The host first stops its old service/connection. Role replacement never
    /// waits for idle transactions, but must serialize against physical commit.
    func installRole(storage: StorageDomain, remote: (any DatabaseRemote)?, sqlReady: Bool = true) {
        commitBoundary.lock(); defer { commitBoundary.unlock() }
        registry.withLock { state in
            state.storage.pager.revokeRole()
            state = Registry(storage: storage, snapshot: storage.committed, meta: storage.pager.selected, remote: remote)
            state.sqlReady = sqlReady
            writer.revokeCurrent()
        }
    }
    fileprivate func localCommit<T>(_ lease: WriterLease, _ body: () throws -> T) throws -> T {
        commitBoundary.lock(); defer { commitBoundary.unlock() }
        try check(lease); return try body()
    }

    fileprivate func memoryCommit<T>(_ lease: WriterLease, _ body: (MemoryCommitGuard) throws -> T) throws -> T {
        commitBoundary.lock(); defer { commitBoundary.unlock() }
        try check(lease)
        guard storage.kind == .memory else { throw JedError("55000", "memory commit guard requires an in-memory domain") }
        return try body(MemoryCommitGuard(lease: lease, storage: storage))
    }

    /// Start serving after recovery and before accepting clients. The caller
    /// supplies the other slot only after ordinary recovery has validated it.
    func startService(incarnation: [UInt8], predecessor: [UInt8]? = nil) throws {
        try registry.withLock { state in
            state.history = try PageServiceHistory(incarnation: incarnation, initial: state.meta, predecessor: predecessor)
        }
    }
    func stopService() { registry.withLock { $0.history = nil } }
    private func serviceAnswer(_ point: ServiceCachePoint, state: Registry) throws -> ServiceReadReply {
        guard let history = state.history else { throw JedError("08006", "page service is not active") }
        return history.answer(point, selected: state.meta)
    }
    func serviceBeginRead(_ point: ServiceCachePoint) throws -> ServiceReadPin {
        try registry.withLock { state in
            let reply = try serviceAnswer(point, state: state)
            state.pins[state.meta.meta.txid, default: 0] += 1
            return ServiceReadPin(core: self, selected: state.meta, reply: reply, epoch: state.epoch)
        }
    }
    func serviceBeginWrite(_ point: ServiceCachePoint, relaxed: Bool, timeoutMilliseconds: UInt64,
                           cancellation: CancellationToken) throws -> ServiceWriterLease {
        guard !storage.readOnly else { throw JedError("25006", "page-service owner is read only") }
        let token = try writer.acquire(cancellation: cancellation, timeoutMilliseconds: timeoutMilliseconds, service: true)
        do {
            return try registry.withLock { state in
                let pager = state.storage.pager
                if !relaxed { try pager.flush(shrink: false) }
                else { try pager.beforeClientTiming() }
                let version = state.meta.meta.txid
                let allowed = (state.pins.keys.min() ?? version) >= state.storage.freeGeneration
                let allocation = try pager.allocationSnapshot(reuseAllowed: allowed)
                let pending = relaxed ? allocation.pending : []
                let reuse = pending.isEmpty ? [] : Array(allocation.reusableFree.prefix(PageServiceProtocol.maxReusePages))
                let granted = !allowed ? [] : pending.isEmpty ? allocation.persistedFree : reuse
                let reply = ServiceWriteReply(read: try serviceAnswer(point, state: state),
                    reuseGrant: allowed, pending: pending, reuse: reuse)
                let base = ServiceDeltaBase(txid: version, pageCount: state.meta.meta.pageCount,
                    codec: pager.codec, granted: Set(granted),
                    pending: Dictionary(uniqueKeysWithValues: pending.map { ($0.pageIndex, $0.digest) }))
                state.pins[version, default: 0] += 1
                return ServiceWriterLease(core: self, token: token, selected: state.meta,
                    reply: reply, base: base, relaxed: relaxed, epoch: state.epoch)
            }
        } catch { writer.release(token); throw error }
    }
    func serviceStatus(txid: UInt64, digest: UInt64) throws -> UInt8 {
        // Include local commits' persist-to-publication window, not only the
        // registry-held remote commit barrier.
        commitBoundary.lock(); defer { commitBoundary.unlock() }
        return try registry.withLock { state in
            guard let history = state.history else { throw JedError("08006", "page service is not active") }
            return history.status(txid: txid, digest: digest, current: state.meta.meta.txid)
        }
    }
    func serviceAfterAcknowledgement(_ lease: ServiceWriterLease, entropy: KeyEntropy?,
                                     synchronous: Bool, storageLimit: Int64) throws {
        commitBoundary.lock(); defer { commitBoundary.unlock() }
        try serviceCheckWriter(lease)
        try storage.pager.flushServiceCap()
        // A reader may arrive while the plan is prepared. Recheck the watermark
        // and identity before storage writes; abandoning only maintenance never
        // changes the already acknowledged client commit.
        do {
            let work = try registry.withLock { state -> (StorageDomain, DomainSnapshot, Bool)? in
                guard state.epoch === lease.epoch, state.pins.isEmpty else { return nil }
                let trigger = try state.storage.serviceMaintenanceTrigger()
                guard trigger.needed else { return nil }
                state.snapshot = try state.storage.adoptServiceSnapshot(state.meta, environment: nil, force: true)
                state.sqlReady = false
                var snapshot = state.snapshot; snapshot.requireStorageMaintenance()
                return (state.storage, snapshot, trigger.reclaim)
            }
            guard let (domain, snapshot, reclaim) = work else { return }
            var policy = DomainCommitPolicy(); policy.oldestPinnedVersion = snapshot.txid + 1
            policy.storageLimit = storageLimit
            guard let plan = try domain.prepare(snapshot, policy: policy, entropy: entropy, forceReclaim: reclaim, synchronous: synchronous) else { return }
            try registry.withLock { state in
                guard state.epoch === lease.epoch, state.pins.isEmpty,
                      state.storage === domain, domain.committed.identity === plan.identity,
                      state.meta.meta.txid == plan.pages.baseTxid else { return }
                let committed = try domain.persist(plan, synchronous: synchronous)
                state.snapshot = committed; state.meta = domain.pager.selected; state.sqlReady = false
                let written = plan.pages.body.map(\.index) + plan.pages.manifest.pages.map(\.pageIndex)
                state.history?.publish(txid: state.meta.meta.txid,
                    digest: PageServiceHistory.digest(state.meta), indices: written)
            }
        } catch {
            // Serialization/adoption failure is local maintenance failure. Only
            // a failed physical commit revokes serving after the client reply.
            if storage.pager.isPoisoned { throw error }
        }
    }
    func serviceReleasePin(_ version: UInt64, epoch: StorageRevision) { releasePin(version, epoch: epoch) }
    func serviceReleaseWriter(_ token: StorageRevision) {
        commitBoundary.lock(); defer { commitBoundary.unlock() }
        if writer.owns(token) {
            storage.pager.abandonServiceDelta()
            writer.release(token)
        }
    }
    func serviceCheckWriter(_ lease: ServiceWriterLease) throws {
        try checkEpoch(lease.epoch)
        guard lease.core === self, writer.owns(lease.token) else { throw JedError("55000", "service writer is closed") }
    }
    func serviceBeginDelta(_ lease: ServiceWriterLease, delta: ServiceIncomingDelta) throws {
        commitBoundary.lock(); defer { commitBoundary.unlock() }
        try serviceCheckWriter(lease)
        let domain = storage
        domain.pool.invalidate(Set(delta.written))
        try domain.pager.beginServiceDelta(delta, baseTxid: lease.selected.meta.txid,
            syncDirectory: domain.serviceDirectorySync)
    }
    func serviceWritePage(_ lease: ServiceWriterLease, page: StoredPage) throws {
        commitBoundary.lock(); defer { commitBoundary.unlock() }
        try serviceCheckWriter(lease)
        try storage.pager.writeServicePage(page, baseTxid: lease.selected.meta.txid)
    }
    func serviceFinishDelta(_ lease: ServiceWriterLease, delta: ServiceIncomingDelta,
                            entropy: KeyEntropy?) throws -> UInt64 {
        commitBoundary.lock(); defer { commitBoundary.unlock() }
        try serviceCheckWriter(lease)
        try delta.complete()
        // From this point an EOF cannot cancel the commit. Keeping the registry
        // locked through storage and publication also orders commit_status.
        return try registry.withLock { state in
            let selected = try state.storage.pager.finishServiceDelta(delta, baseTxid: lease.selected.meta.txid,
                synchronous: !lease.relaxed, entropy: entropy)
            state.storage.noteServiceCommit(selected, written: Set(delta.written))
            state.meta = selected
            state.history?.publish(txid: selected.meta.txid,
                digest: PageServiceHistory.digest(selected), indices: delta.written)
            return selected.meta.txid
        }
    }

}

/// Borrowed only inside WriterLease.withMemoryCommitGuard. Group commit nests
/// these guards in deterministic domain order and performs all throwing work
/// before the one file persist. No callback escapes the guarded scope.
internal final class MemoryCommitGuard {
    private let lease: WriterLease
    private let storage: StorageDomain
    fileprivate init(lease: WriterLease, storage: StorageDomain) { self.lease = lease; self.storage = storage }
    func stage(_ plan: PreparedDomainCommit) throws -> AdmittedMemoryDomainCommit {
        try lease.core.check(lease)
        return try storage.admitMemory(storage.stageMemory(plan))
    }
    func publish(_ admitted: AdmittedMemoryDomainCommit) -> DomainSnapshot {
        let snapshot = storage.finishMemory(admitted)
        lease.core.publish(snapshot, epoch: lease.epoch)
        return snapshot
    }
}

/// The owner serializes calls on this lease. Prepared commits are immutable and
/// may be preflighted across every changed domain before any domain persists.
internal final class WriterLease {
    fileprivate let core: DatabaseCore
    fileprivate let token: StorageRevision
    let snapshot: DomainSnapshot
    private let remote: (any DatabaseRemote)?
    private var closed = false
    fileprivate let epoch: StorageRevision
    fileprivate init(core: DatabaseCore, token: StorageRevision, snapshot: DomainSnapshot, epoch: StorageRevision, remote: (any DatabaseRemote)? = nil) {
        self.core = core; self.token = token; self.snapshot = snapshot; self.remote = remote; self.epoch = epoch
    }
    var domainKind: StorageDomainKind { core.storage.kind }
    var domainIdentity: ObjectIdentifier { ObjectIdentifier(core) }
    func withMemoryCommitGuard<T>(_ body: (MemoryCommitGuard) throws -> T) throws -> T {
        try core.memoryCommit(self, body)
    }
    func checkRole() throws { try core.checkEpoch(epoch); try remote?.checkRole() }
    func pin(_ working: DomainSnapshot, borrowingWriter: Bool = false) throws -> SnapshotPin {
        try core.check(self)
        guard working.identity === snapshot.identity, working.txid == snapshot.txid else { throw JedError("55000", "snapshot does not belong to this transaction") }
        if borrowingWriter {
            let role = BorrowedWriterRole(core: core, token: token, epoch: epoch, remote: remote)
            return try core.pinRetained(working, epoch: epoch, role: role)
        }
        try remote?.retainRead(version: working.txid)
        do { return try core.pinRetained(working, epoch: epoch, role: remote) }
        catch { remote?.releaseRead(version: working.txid); throw error }
    }
    func precheck(_ working: DomainSnapshot, policy: DomainCommitPolicy = .init()) throws {
        try core.check(self); try core.storage.precheck(working, policy: core.policy(policy))
    }
    func prepare(_ working: DomainSnapshot, policy: DomainCommitPolicy = .init(), entropy: KeyEntropy? = nil, synchronous: Bool = true) throws -> PreparedDomainCommit? {
        try core.check(self)
        return try core.storage.prepare(working, policy: core.policy(policy), entropy: entropy, synchronous: synchronous)
    }
    @discardableResult func persist(_ prepared: PreparedDomainCommit, synchronous: Bool = true,
                                   syncDirectory: () throws -> Void = {}, afterPersist: () throws -> Void = {}) throws -> DomainSnapshot {
        try core.check(self)
        func persist() throws -> DomainSnapshot {
            let replacement: DomainSnapshot
            if let remote { replacement = try remote.commit(prepared, storage: core.storage) }
            else { replacement = try core.storage.persist(prepared, synchronous: synchronous, syncDirectory: syncDirectory) }
            // A known durable commit remains successful after role replacement;
            // publication then preserves the replacement role's recovered state.
            defer { core.publish(replacement, epoch: epoch, written: planWritten(prepared)) }
            try afterPersist(); return replacement
        }
        if remote != nil { return try persist() }
        return try core.localCommit(self, persist)
    }
    private func planWritten(_ prepared: PreparedDomainCommit) -> [UInt32] {
        Set(prepared.pages.body.map(\.index)).union(prepared.pages.manifest.pages.map(\.pageIndex)).sorted()
    }
    func stageMemory(_ prepared: PreparedDomainCommit) throws -> StagedDomainCommit {
        try core.check(self)
        return try core.storage.stageMemory(prepared)
    }
    @discardableResult func adoptMemory(_ staged: StagedDomainCommit) throws -> DomainSnapshot {
        try core.check(self)
        let replacement = try core.storage.adoptMemory(staged)
        core.publish(replacement, epoch: epoch)
        return replacement
    }
    func close() {
        guard !closed else { return }; closed = true
        remote?.endWrite(); core.releaseWriter(token)
    }
    deinit { close() }
}
