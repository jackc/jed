#if os(Linux)
import Foundation

/// One engine process's remote storage role. admission orders snapshot refresh;
/// pins protects registration and frame transmission, independently of response
/// waiting, so an awaited writer does not stall already running readers.
internal final class PageServiceClient: DatabaseRemote, @unchecked Sendable {
    let connection: PageServiceConnection
    private let admission = NSLock()
    private let pins = NSRecursiveLock()
    private var readers: [UInt64: Int] = [:]
    private var writePin: UInt64?
    private var cache = ServiceCachePoint.cold
    private var servedMeta: [UInt8] = []
    private var heldCodec: (any PageCodec)?
    private var revoked = false
    private var openingDeadline: ContinuousClock.Instant?
    private let resolve: @Sendable (UInt64, UInt64) throws -> Bool

    private init(connection: PageServiceConnection,
                 resolve: @escaping @Sendable (UInt64, UInt64) throws -> Bool) {
        self.connection = connection; self.resolve = resolve
    }
    static func open(connection: PageServiceConnection, credential: Credential?, environment: Environment,
                     unlockedCodec: EncryptedPageCodec? = nil, deadline: ContinuousClock.Instant? = nil,
                     catalogID: UInt64, cacheBytes: Int,
                     resolve: @escaping @Sendable (UInt64, UInt64) throws -> Bool = { _, _ in
                         throw JedError("08007", "commit outcome is unknown after losing the page-service connection")
                     }) throws -> (core: DatabaseCore, client: PageServiceClient) {
        let client = PageServiceClient(connection: connection, resolve: resolve)
        client.openingDeadline = deadline
        defer { client.pins.lock(); client.openingDeadline = nil; client.pins.unlock() }
        do {
            guard case .readBegun(let reply) = try connection.request(.beginRead(.cold)).wait(deadline: deadline, onCancel: { connection.close() }) else {
                throw PageServiceProtocol.violation("begin_read received another reply kind")
            }
            let meta = try client.validate(reply)
            client.pins.lock(); client.readers[meta.txid] = 1; client.servedMeta = reply.meta; client.pins.unlock()
            defer { client.releaseRead(version: meta.txid) }
            if unlockedCodec == nil { try EncryptionPolicy.checkCredential(encrypted: meta.encrypted, credential: credential) }
            else if !meta.encrypted || unlockedCodec!.pageSize != meta.pageSize {
                throw JedError("28P04", "database encryption key changed")
            }
            let codec: any PageCodec
            var keyBlock: KeyBlockSelection?
            if meta.encrypted {
                let pages = try client.readPages([2, 3])
                let selected = try KeyBlock.select(page2: pages[0], page3: pages[1], pageSize: meta.pageSize)
                codec = try unlockedCodec ?? EncryptedPageCodec(pageSize: meta.pageSize, dataKey: selected.block.unlock(credential), opening: true)
                keyBlock = selected
            } else { codec = try PlainPageCodec(pageSize: meta.pageSize, opening: true) }
            client.heldCodec = codec
            let selected = try client.authenticate(reply, codec: codec, staleKey: unlockedCodec != nil)
            let store = PageServiceReadStore(client: client, pageSize: meta.pageSize)
            let pager = try Pager.remote(store: store, selected: selected, codec: codec, keyBlock: keyBlock)
            let pool = try NodePool(pager: pager, cacheBytes: cacheBytes)
            let storage = try StorageDomain.remote(pager: pager, pool: pool, catalogID: catalogID,
                readOnly: connection.hello.flags & 1 != 0, environment: environment)
            client.cache = reply.point
            let core = DatabaseCore(storage: storage, environment: environment, remote: client)
            return (core, client)
        } catch { client.close(); throw error }
    }
    func checkRole() throws {
        pins.lock(); defer { pins.unlock() }
        guard !revoked, !connection.isClosed else { throw JedError("08006", "page-service storage role ended") }
    }
    private func validate(_ reply: ServiceReadReply) throws -> MetaPage {
        let meta = try MetaPage.decode(reply.meta)
        guard meta.pageSize == Int(connection.hello.pageSize), meta.txid == reply.point.txid,
              reply.point.incarnation.count == 16, reply.point.incarnation.contains(where: { $0 != 0 }),
              reply.invalidate <= 1, reply.invalidate == 0 || reply.indices.isEmpty,
              zip(reply.indices, reply.indices.dropFirst()).allSatisfy({ $0 < $1 }) else {
            throw PageServiceProtocol.violation("invalid snapshot reply")
        }
        return meta
    }
    private func authenticate(_ reply: ServiceReadReply, codec: any PageCodec, staleKey: Bool) throws -> ValidatedMeta {
        let meta = try validate(reply)
        guard meta.encrypted == (codec.reservedBytes != 0) else {
            throw JedError(codec.reservedBytes != 0 ? "28P04" : "28P02", "database encryption key changed")
        }
        var entries: [ManifestEntry] = [], chain: [UInt32] = [], slot: UInt32 = 0
        if let encrypted = codec as? EncryptedPageCodec {
            var pages: [ManifestPageBytes] = []
            let parsed = try MetaRecovery.validateManifest(meta, bytes: reply.meta, verifyBodies: false) { index in
                let bytes = try readPages([index])[0]
                pages.append(ManifestPageBytes(pageIndex: index, bytes: bytes)); return bytes
            }
            entries = parsed.entries; chain = parsed.chain
            if (try? encrypted.verifyMeta(reply.meta, slot: 0, manifest: pages)) == nil {
                guard (try? encrypted.verifyMeta(reply.meta, slot: 1, manifest: pages)) != nil else {
                    throw JedError(staleKey ? "28P04" : "XX001", "served metadata does not authenticate under the held key")
                }
                slot = 1
            }
        }
        return ValidatedMeta(meta: meta, slot: slot, floorSlot: slot, bytes: reply.meta, entries: entries, chain: chain)
    }
    private func point() -> ServiceCachePoint { pins.lock(); defer { pins.unlock() }; return cache }
    private func adopt(_ reply: ServiceReadReply, core: DatabaseCore) throws -> DomainSnapshot {
        let storage = core.storage
        let selected = try authenticate(reply, codec: storage.pager.codec, staleKey: true)
        if reply.invalidate == 1 { storage.pool.invalidateAll() }
        else { storage.pool.invalidate(Set(reply.indices)) }
        if selected.meta.txid != storage.committed.txid || reply.invalidate == 1 {
            try storage.pager.adoptRemoteMeta(selected)
        }
        let snapshot = try core.adoptRemote(selected, remote: self)
        pins.lock(); cache = reply.point; servedMeta = reply.meta; pins.unlock()
        return snapshot
    }
    func beginRead(core: DatabaseCore) throws -> SnapshotPin {
        admission.lock(); defer { admission.unlock() }
        try checkRole()
        guard case .readBegun(let reply) = try connection.request(.beginRead(point())).wait() else {
            throw PageServiceProtocol.violation("begin_read received another reply kind")
        }
        let meta: MetaPage
        do { meta = try validate(reply) }
        catch { try? connection.notify(.endRead(reply.point.txid)); throw error }
        // The received owner pin is not released until local registration or
        // failed adoption has accounted for it, even if an older local pin ends.
        pins.lock()
        let duplicate = readers[meta.txid] != nil
        readers[meta.txid, default: 0] += 1
        let previousMeta = servedMeta; servedMeta = reply.meta
        if duplicate { try? connection.notify(.endRead(meta.txid)) }
        pins.unlock()
        do {
            let snapshot = try adopt(reply, core: core)
            return try core.registerRemoteRead(snapshot, role: self)
        } catch {
            pins.lock(); servedMeta = previousMeta; pins.unlock()
            releaseRead(version: meta.txid); throw error
        }
    }
    func retainRead(version: UInt64) throws {
        pins.lock(); defer { pins.unlock() }
        try checkRole()
        if let count = readers[version] { readers[version] = count + 1; return }
        // A cursor born in a write transaction needs an independent read pin
        // before commit/abort releases the write pin. The held gate fixes txid.
        guard writePin == version,
              case .readBegun(let reply) = try connection.request(.beginRead(cache)).wait(), reply.point.txid == version else {
            throw JedError("08006", "cannot retain an unpinned remote snapshot")
        }
        _ = try validate(reply); readers[version] = 1
    }
    func releaseRead(version: UInt64) {
        pins.lock(); defer { pins.unlock() }
        guard let count = readers[version] else { return }
        if count > 1 { readers[version] = count - 1 }
        else { readers.removeValue(forKey: version); try? connection.notify(.endRead(version)) }
    }
    func beginWrite(core: DatabaseCore, relaxed: Bool, timeoutMilliseconds: UInt64,
                    cancellation: CancellationToken?) throws -> DomainSnapshot {
        try checkRole()
        if cancellation?.isCancelled == true { throw JedError("57014", "canceling statement due to user request") }
        // Waiting for the owner's gate holds neither admission nor pins.
        guard case .writeBegun(let reply) = try connection.request(.beginWrite(point(), timeout: timeoutMilliseconds,
            flags: relaxed ? 1 : 0)).wait(cancellation: cancellation, onCancel: { self.connection.close() }) else {
            throw PageServiceProtocol.violation("begin_write received another reply kind")
        }
        admission.lock(); defer { admission.unlock() }
        var accepted = false
        defer {
            if !accepted {
                pins.lock(); writePin = nil; try? connection.notify(.abort); pins.unlock()
            }
        }
        let meta = try validate(reply.read)
        guard zip(reply.pending, reply.pending.dropFirst()).allSatisfy({ $0.pageIndex < $1.pageIndex }),
              reply.pending.allSatisfy({ $0.pageIndex >= meta.firstBodyPage && $0.pageIndex < meta.pageCount }),
              zip(reply.reuse, reply.reuse.dropFirst()).allSatisfy({ $0 < $1 }),
              reply.reuse.allSatisfy({ $0 >= meta.firstBodyPage && $0 < meta.pageCount }),
              (relaxed || reply.pending.isEmpty), (reply.reuseGrant && !reply.pending.isEmpty) || reply.reuse.isEmpty else {
            throw PageServiceProtocol.violation("invalid write grant")
        }
        pins.lock(); writePin = meta.txid; let previousMeta = servedMeta; servedMeta = reply.read.meta; pins.unlock()
        do {
            let snapshot = try adopt(reply.read, core: core)
            try core.storage.pager.grantRemoteWrite(reply)
            if cancellation?.isCancelled == true { throw JedError("57014", "canceling statement due to user request") }
            accepted = true; return snapshot
        } catch {
            pins.lock(); servedMeta = previousMeta; pins.unlock(); throw error
        }
    }
    func commit(_ prepared: PreparedDomainCommit, storage: StorageDomain) throws -> DomainSnapshot {
        admission.lock(); defer { admission.unlock() }
        try checkRole()
        let begin = ServiceCommitBegin(txid: prepared.pages.manifest.meta.txid, meta: prepared.pages.metaBytes,
            manifest: prepared.pages.manifest.pages.map { StoredPage(index: $0.pageIndex, bytes: $0.bytes) })
        let digest = try ServiceIncomingDelta.resolutionDigest(begin.meta)
        var endSent = false
        let response: ServiceReply
        pins.lock()
        do {
            guard writePin == prepared.pages.baseTxid else { throw JedError("08006", "remote writer no longer holds its base") }
            response = try connection.sendCommit(begin, body: prepared.pages.body, endSent: &endSent)
            writePin = nil; pins.unlock()
        } catch { pins.unlock(); throw error }
        do {
            guard case .committed(let txid) = try response.wait(), txid == begin.txid else {
                throw PageServiceProtocol.violation("commit acknowledgement has the wrong generation")
            }
        } catch {
            let error = error as? JedError ?? JedError("08006", "page-service commit connection failed")
            if !endSent || error.code != "08006" { throw error }
            guard try resolve(begin.txid, digest) else { throw JedError("08006", "remote commit was not published") }
        }
        let snapshot = try storage.persistRemote(prepared)
        pins.lock(); cache.txid = begin.txid; servedMeta = begin.meta; pins.unlock()
        return snapshot
    }
    func endWrite() {
        pins.lock(); defer { pins.unlock() }
        guard writePin != nil else { return }; writePin = nil
        try? connection.notify(.abort)
    }
    fileprivate func readPages(_ indices: [UInt32]) throws -> [[UInt8]] {
        pins.lock()
        let request: ServiceReply
        let deadline = openingDeadline
        do {
            try checkRole()
            guard let newest = (Array(readers.keys) + [writePin].compactMap { $0 }).max() else {
                throw JedError("08006", "remote page read has no live snapshot pin")
            }
            request = try connection.request(.readPages(txid: newest, indices: indices))
            pins.unlock()
        } catch { pins.unlock(); throw error }
        guard case .pages(let pages) = try request.wait(deadline: deadline, onCancel: { self.connection.close() }), pages.count == indices.count else {
            throw PageServiceProtocol.violation("read_pages returned the wrong page count")
        }
        return pages
    }
    fileprivate func metadata() throws -> [UInt8] {
        pins.lock(); defer { pins.unlock() }; try checkRole(); return servedMeta
    }
    var unlockedCodec: EncryptedPageCodec? {
        pins.lock(); defer { pins.unlock() }; return heldCodec as? EncryptedPageCodec
    }
    func close() { pins.lock(); revoked = true; pins.unlock(); connection.close() }
}

/// This device can only read the owner-pinned page space. Commit writes bypass
/// it through PageServiceClient's delta boundary, never as arbitrary byte RPC.
private final class PageServiceReadStore: BlockStore {
    let client: PageServiceClient
    let pageSize: Int
    init(client: PageServiceClient, pageSize: Int) { self.client = client; self.pageSize = pageSize }
    func read(at offset: UInt64, count: Int) throws -> [UInt8] {
        guard offset % UInt64(pageSize) == 0, count == pageSize,
              offset / UInt64(pageSize) <= UInt32.max else { throw JedError("58030", "unaligned remote page read") }
        let index = UInt32(offset / UInt64(pageSize))
        return try index < 2 ? client.metadata() : client.readPages([index])[0]
    }
    func size() throws -> UInt64 { UInt64(try MetaPage.decode(client.metadata()).pageCount) * UInt64(pageSize) }
    func write(at offset: UInt64, bytes: [UInt8]) throws { throw JedError("58030", "remote writes require a commit delta") }
    func setSize(_ bytes: UInt64) throws { throw JedError("58030", "remote resize requires a commit delta") }
    func sync() throws { throw JedError("58030", "remote durability requires a commit delta") }
}
#endif
