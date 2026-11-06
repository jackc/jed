import Synchronization

/// These service leases pin only validated structural metadata. They never
/// require the owner to resolve catalog SQL in its own Environment.
internal final class ServiceReadPin: Sendable {
    let epoch: StorageRevision
    let core: DatabaseCore
    let selected: ValidatedMeta
    let reply: ServiceReadReply
    private let active = Mutex(true)
    init(core: DatabaseCore, selected: ValidatedMeta, reply: ServiceReadReply, epoch: StorageRevision) {
        self.core = core; self.selected = selected; self.reply = reply; self.epoch = epoch
    }
    func close() {
        let release = active.withLock { value in let old = value; value = false; return old }
        if release { core.serviceReleasePin(selected.meta.txid, epoch: epoch) }
    }
    deinit { close() }
}

internal final class ServiceWriterLease: Sendable {
    let epoch: StorageRevision
    let core: DatabaseCore
    let token: StorageRevision
    let selected: ValidatedMeta
    let reply: ServiceWriteReply
    let base: ServiceDeltaBase
    let relaxed: Bool
    private struct State { var active = true; var pinned = true; var acknowledged = false }
    private let state = Mutex(State())
    init(core: DatabaseCore, token: StorageRevision, selected: ValidatedMeta,
         reply: ServiceWriteReply, base: ServiceDeltaBase, relaxed: Bool, epoch: StorageRevision) {
        self.core = core; self.token = token; self.selected = selected
        self.reply = reply; self.base = base; self.relaxed = relaxed; self.epoch = epoch
    }
    func begin(_ begin: ServiceCommitBegin) throws -> ServiceIncomingDelta {
        let delta = try ServiceIncomingDelta(base: base, begin: begin)
        try core.serviceBeginDelta(self, delta: delta)
        return delta
    }
    func write(_ page: StoredPage) throws { try core.serviceWritePage(self, page: page) }
    func finish(_ delta: ServiceIncomingDelta, entropy: KeyEntropy?) throws -> UInt64 {
        try core.serviceFinishDelta(self, delta: delta, entropy: entropy)
    }
    /// Release only the writer's read pin after acknowledgement; retain its
    /// exclusive token through cap flushing and at most one maintenance commit.
    func afterAcknowledgement(entropy: KeyEntropy? = nil, synchronous: Bool = true, storageLimit: Int64 = 0) throws {
        let first = state.withLock { state in
            guard state.active, !state.acknowledged else { return false }
            state.acknowledged = true; return true
        }
        guard first else { throw JedError("55000", "service writer acknowledgement is already complete") }
        releasePin()
        try core.serviceAfterAcknowledgement(self, entropy: entropy, synchronous: synchronous, storageLimit: storageLimit)
    }
    private func releasePin() {
        let release = state.withLock { state in let old = state.pinned; state.pinned = false; return old }
        if release { core.serviceReleasePin(selected.meta.txid, epoch: epoch) }
    }
    func close() {
        releasePin()
        let release = state.withLock { state in let old = state.active; state.active = false; return old }
        if release { core.serviceReleaseWriter(token) }
    }
    deinit { close() }
}
