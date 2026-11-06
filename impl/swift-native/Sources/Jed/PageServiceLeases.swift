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
    private let active = Mutex(true)
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
    /// The acknowledgement is sent before this cap flush. The writer stays held
    /// until it completes, even when the client disconnects after publication.
    func afterAcknowledgement() throws { try core.storage.pager.flushServiceCap() }
    func close() {
        let release = active.withLock { value in let old = value; value = false; return old }
        if release {
            core.serviceReleasePin(selected.meta.txid, epoch: epoch)
            core.serviceReleaseWriter(token)
        }
    }
    deinit { close() }
}
