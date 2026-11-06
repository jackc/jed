internal protocol DatabaseScratchProvider: Sendable { var scratchFileHost: any ScratchFileHost { get } }

internal protocol DatabaseScratchPolicy: Sendable {
    var forceSealedScratch: Bool { get }
}

extension Database {
    /// Native file owners, local page clients and stream clients share the same
    /// private scratch policy. The optional seam keeps memory-only hosts from
    /// acquiring filesystem access merely because a temp table grows.
    internal var scratchFileHost: (any ScratchFileHost)? {
        #if os(Linux) || canImport(Darwin)
        if let local = localFileHost { return local.scratch }
        if let provider = host as? any DatabaseScratchProvider { return provider.scratchFileHost }
        if let policy = host as? any DatabaseScratchPolicy {
            return NativeScratchFileHost(sealed: policy.forceSealedScratch || core.storage.pager.codec.reservedBytes != 0)
        }
        #endif
        return nil
    }
}
