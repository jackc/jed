/// Role-specific callbacks stay outside DatabaseCore's registry lock. A pin
/// retains the role that created it, so reconnecting cannot release another
/// incarnation's pins or make an old transaction valid again.
internal protocol SnapshotRole: Sendable {
    func checkRole() throws
    func retainRead(version: UInt64) throws
    func releaseRead(version: UInt64)
}

internal protocol DatabaseRemote: AnyObject, SnapshotRole {
    func beginRead(core: DatabaseCore) throws -> SnapshotPin
    func beginWrite(core: DatabaseCore, relaxed: Bool, timeoutMilliseconds: UInt64,
                    cancellation: CancellationToken?) throws -> DomainSnapshot
    func commit(_ prepared: PreparedDomainCommit, storage: StorageDomain) throws -> DomainSnapshot
    func endWrite()
}
