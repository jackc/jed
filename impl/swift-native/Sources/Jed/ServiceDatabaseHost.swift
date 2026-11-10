#if os(Linux)
import Foundation
import Glibc
import Synchronization

/// One pathname claim spans every owner/client incarnation of a public handle.
/// The condition guards role changes; it is entirely resident on the ordinary
/// owner's foreground path. Only election workers touch the lock bundle.
internal final class ServiceDatabaseHost: CoordinatedDatabaseClose, DatabaseRoleCoordinator, DatabaseScratchPolicy, DatabaseScratchProvider, DatabaseFilesystemProvider, @unchecked Sendable {
    private enum Role {
        case owner(FileDatabaseHost, PageServiceOwner?, String?)
        case client(PageServiceClient)
    }
    let path: String
    private let claim: any ServicePathClaim
    private let platform: any ServiceHostPlatform
    private let options: OpenOptions
    private let structuralOnly: Bool
    var skipsFsync: Bool { options.skipFsync }
    var forceSealedScratch: Bool { options.scratchEncryption == .on }
    var fileOperations: any FileOperations { platform.operations }
    var scratchFileHost: any ScratchFileHost {
        platform.scratch(sealed: forceSealedScratch || core?.storage.pager.codec.reservedBytes != 0, random: random)
    }
    private let catalogID: UInt64
    private let random: any RandomSource
    private let condition = NSCondition()
    private let coreState = Mutex<DatabaseCore?>(nil)
    private var core: DatabaseCore? {
        get { coreState.withLock { $0 } }
        set { coreState.withLock { $0 = newValue } }
    }
    private var role: Role?
    private weak var database: Database?
    private var closed = false
    private var closing = false
    private var electing = false
    private var electionCompletion: UInt64 = 0
    private var electionFailure: JedError?
    private var failure: JedError?
    private var requester: UInt64?
    private var transferring = false
    private var noticeRequester: UInt64?
    private var pauseUntil: Int64?
    private var heldCodec: EncryptedPageCodec?

    private init(path: String, claim: any ServicePathClaim, options: OpenOptions, catalogID: UInt64, random: any RandomSource, structuralOnly: Bool = false, platform: any ServiceHostPlatform = NativeServiceHostPlatform()) {
        self.structuralOnly = structuralOnly
        self.platform = platform; self.path = path; self.claim = claim; self.options = options; self.catalogID = catalogID; self.random = random
    }
    static func open(path original: String, options: OpenOptions, catalogID: UInt64, structuralOnly: Bool = false) throws -> Database {
        try options.pageService.validate()
        let path = try FileLockBundle.normalize(original, creating: false)
        try FileLockBundle.checkHardLinks(path)
        return try openResolved(path: path, options: options, catalogID: catalogID, structuralOnly: structuralOnly,
            platform: NativeServiceHostPlatform())
    }
    static func openResolved(path: String, options: OpenOptions, catalogID: UInt64, structuralOnly: Bool = false,
                             platform: any ServiceHostPlatform) throws -> Database {
        try options.pageService.validate()
        let claim = try platform.claim(path)
        let host = ServiceDatabaseHost(path: path, claim: claim, options: options, catalogID: catalogID,
            random: options.environment.newKeySource(), structuralOnly: structuralOnly, platform: platform)
        host.electing = true
        do { try host.elect(initial: true); return host.wrap() }
        catch { host.closeFailedOpen(); throw error }
    }
    static func create(path original: String, options: CreateOptions, catalogID: UInt64, random: any RandomSource) throws -> Database {
        try options.pageService.validate()
        let path = try FileLockBundle.normalize(original, creating: true)
        let claim = try FilePathClaim.acquire(path)
        let opening = OpenOptions(environment: options.environment, cacheBytes: options.cacheBytes,
            maxStorageBytes: options.maxStorageBytes, credential: options.credential, locking: .automatic,
            fileLockTimeoutMilliseconds: options.fileLockTimeoutMilliseconds, skipFsync: options.skipFsync,
            synchronous: options.synchronous, flushIntervalMilliseconds: options.flushIntervalMilliseconds, scratchEncryption: options.scratchEncryption, pageService: options.pageService)
        let host = ServiceDatabaseHost(path: path, claim: claim, options: opening, catalogID: catalogID, random: random)
        host.electing = true
        do {
            let bundle = try FileLockBundle.acquireExclusive(path: path,
                timeoutMilliseconds: options.fileLockTimeoutMilliseconds, claim: claim)
            let local = try FileDatabaseHost.create(path: path, options: options, catalogID: catalogID, random: random, bundle: bundle)
            try host.becomeOwner(local, bundle: bundle, structural: false)
            return host.wrap()
        } catch { host.closeFailedOpen(); throw error }
    }
    private func wrap() -> Database {
        let result = Database(core: core!, environment: options.environment,
            maxStorageBytes: options.maxStorageBytes, keySource: random, host: self, flushIntervalMilliseconds: options.flushIntervalMilliseconds)
        result.synchronous = options.synchronous
        core!.coordinateRoles(with: self)
        condition.lock(); database = result; electing = false; condition.broadcast()
        let retry: Bool
        if case .client(let client) = role { retry = client.connection.isClosed } else { retry = false }
        condition.unlock()
        if retry && platform.backgroundElection { Thread.detachNewThread { try? self.awaitAdmission() } }
        return result
    }
    var isClosed: Bool { condition.lock(); defer { condition.unlock() }; return closed }
    var serviceOwner: PageServiceOwner? {
        condition.lock(); defer { condition.unlock() }
        if case .owner(_, let server, _) = role { return server }; return nil
    }
    var serviceClient: PageServiceClient? {
        condition.lock(); defer { condition.unlock() }
        if case .client(let client) = role { return client }; return nil
    }
    var toolingRole: String {
        condition.lock(); defer { condition.unlock() }
        switch role {
        case .owner: return "owner"
        case .client: return "client"
        case nil: return "none"
        }
    }
    var localHost: FileDatabaseHost? {
        condition.lock(); defer { condition.unlock() }
        if case .owner(let host, _, _) = role { return host }; return nil
    }
    func checkOpen() throws {
        try claim.checkOpen()
        condition.lock(); defer { condition.unlock() }
        guard !closed, !closing else { throw JedError("55000", "database is closed") }
        if let failure { throw failure }
    }
    func checkAdmitted() throws {
        try claim.checkAdmitted()
        condition.lock(); defer { condition.unlock() }
        guard !closed, !closing else { throw JedError("55000", "database is closed") }
        if let failure { throw failure }
    }
    private func entropy(_ count: Int) throws -> [UInt8] {
        condition.lock(); let source = database?.currentKeySource ?? random; condition.unlock()
        return try source.bytes(count: count)
    }
    private func maintenanceSettings() -> (Bool, Int64) {
        condition.lock(); defer { condition.unlock() }
        return (database?.synchronous ?? options.synchronous, database?.maxStorageBytes ?? options.maxStorageBytes)
    }
    private func closeFailedOpen() {
        condition.lock(); let old = role; role = nil; closed = true; condition.broadcast(); condition.unlock()
        dispose(old); claim.close()
    }
    private func dispose(_ role: Role?) {
        switch role {
        case .owner(let file, let server, let endpoint):
            server?.stop(); try? file.close(); if let endpoint { platform.remove(endpoint) }
        case .client(let client): client.close()
        case nil: break
        }
    }
    private func adopt(_ storage: StorageDomain, remote: PageServiceClient?, structural: Bool) {
        if let core { core.installRole(storage: storage, remote: remote, sqlReady: !structural) }
        else { core = DatabaseCore(storage: storage, environment: options.environment, remote: remote, sqlReady: !structural) }
        heldCodec = storage.pager.codec as? EncryptedPageCodec
    }
    private func becomeOwner(_ local: Database, bundle: any ServiceOwnership, structural: Bool) throws {
        guard let file = local.host as? FileDatabaseHost else { throw JedError("58030", "file owner host is unavailable") }
        var incarnation = try platform.incarnation()
        while !incarnation.contains(where: { $0 != 0 }) { incarnation = try platform.incarnation() }
        let endpoint = options.pageService.serve ? platform.endpoint(path: path, incarnation: incarnation) : nil
        do {
            try bundle.writeOwner(ServiceOwnerRecord(serving: endpoint != nil, uid: platform.uid,
                incarnation: incarnation, pid: platform.pid, endpoint: endpoint ?? ""))
            let listener = try endpoint.map { try platform.listen($0) }
            adopt(local.core.storage, remote: nil, structural: structural)
            let current = core!
            let selected = current.storage.pager.selected
            let predecessor = try? file.store.read(at: UInt64(1 - selected.slot) * UInt64(selected.meta.pageSize), count: selected.meta.pageSize)
            try current.startService(incarnation: incarnation, predecessor: predecessor)
            let server = listener.map { socket in
                PageServiceOwner(core: current, listener: socket, limits: options.pageService.ownerLimits,
                    yields: options.pageService.ownership == .yield,
                    entropy: { [weak self] count in
                        guard let self else { throw JedError("08006", "database owner ended") }; return try self.entropy(count)
                    }, ownership: { [weak self] in self?.requestOwnership($0) ?? false },
                    ended: { [weak self] in self?.connectionEnded($0) },
                    poisoned: { [weak self] in self?.ownerFailed() },
                    synchronous: { [weak self] in self?.maintenanceSettings().0 ?? true },
                    storageLimit: { [weak self] in self?.maintenanceSettings().1 ?? 0 }, timers: platform.ownerTimers,
                    dispatchWork: { [platform] in platform.dispatch($0) })
            }
            condition.lock(); let old = role; role = .owner(file, server, endpoint); condition.unlock()
            file.bindIdleFlusher(to: current, failed: { [weak self] in self?.ownerFailed() })
            dispose(old); server?.start()
        } catch { try? file.close(); if let endpoint { platform.remove(endpoint) }; throw error }
    }
    private func elect(initial: Bool) throws {
        let deadline = ContinuousClock.now.advanced(by: .milliseconds(max(0, options.fileLockTimeoutMilliseconds)))
        let expires = ServiceHostTime.deadline(platform.now(), milliseconds: UInt64(max(0, options.fileLockTimeoutMilliseconds)))
        let bundle = try platform.prepare(path: path, timeout: options.fileLockTimeoutMilliseconds, claim: claim)
        var transferred = false
        defer { if !transferred { bundle.close() } }
        while true {
            condition.lock(); let stop = closed || closing; let pause = pauseUntil; condition.unlock()
            if stop { throw JedError("55000", "database is closed") }
            if options.pageService.ownership != .never, pause == nil || platform.now() >= pause!, try bundle.tryOwnership() {
                let local = try platform.openFile(path: path, options: options, catalogID: catalogID, bundle: bundle,
                    unlockedCodec: heldCodec, structural: structuralOnly || !initial, random: random)
                transferred = true
                try becomeOwner(local, bundle: bundle, structural: structuralOnly || !initial)
                return
            }
            if let owner = platform.owner(path), owner.serving, owner.uid == platform.uid, !owner.endpoint.isEmpty {
                do {
                    let socket = try platform.connect(owner.endpoint, deadline: deadline)
                    let connection = try PageServiceConnection.open(socket: socket,
                        hello: ServiceHello(flags: options.readOnly ? 1 : 0), deadline: deadline,
                        notice: { [weak self] kind, requester in self?.receiveNotice(kind, requester: requester) },
                        disconnected: { [weak self] in self?.disconnected() }, synchronousDisconnect: !platform.backgroundElection)
                    let opened = try PageServiceClient.open(connection: connection, credential: options.credential,
                        environment: options.environment, unlockedCodec: heldCodec, deadline: deadline, catalogID: catalogID, cacheBytes: options.cacheBytes, structuralOnly: structuralOnly,
                        resolve: { [weak self] txid, digest in
                            guard let self else { throw JedError("08007", "database handle ended during commit resolution") }
                            return try self.resolve(txid: txid, digest: digest)
                        })
                    adopt(opened.core.storage, remote: opened.client, structural: structuralOnly)
                    condition.lock(); let old = role; role = .client(opened.client); condition.unlock()
                    dispose(old)
                    if options.pageService.ownership == .prefer, connection.hello.flags & 4 != 0 {
                        _ = try connection.request(.requestOwnership).wait(deadline: deadline, onCancel: { connection.close() })
                    }
                    return
                } catch let error as JedError where error.code == "08006" {
                    // A published endpoint may still be binding, or its owner
                    // may have exited between reading the record and hello.
                }
            }
            if platform.now() >= expires {
                let code = initial ? "55006" : "08006"
                var error = JedError(code, "database file is in use: \(path)\(ownerDetail())")
                error.detail = path; throw error
            }
            platform.pause()
        }
    }
    private func ownerDetail() -> String {
        guard let owner = platform.owner(path) else { return "" }
        if owner.uid != platform.uid { return " (owned by another user's process)" }
        return owner.serving ? "" : " (owned by a non-serving process)"
    }
    private func finishElection() {
        let result = Result { try elect(initial: false) }
        condition.lock(); electing = false; electionCompletion &+= 1
        if case .failure(let error) = result { electionFailure = error as? JedError ?? JedError("08006", "page-service election failed") }
        condition.broadcast(); condition.unlock()
    }
    func awaitAdmission() throws { try awaitAdmission(deadline: nil, cancellation: nil) }
    private func awaitAdmission(deadline: ContinuousClock.Instant?, cancellation: CancellationToken?) throws {
        condition.lock()
        let observed = electionCompletion
        while true {
            if closed || closing { condition.unlock(); throw JedError("55000", "database is closed") }
            if let failure { condition.unlock(); throw failure }
            if cancellation?.isCancelled == true { condition.unlock(); throw JedError("57014", "canceling statement due to user request") }
            if electionCompletion != observed, let electionFailure { condition.unlock(); throw electionFailure }
            if !electing && requester == nil && !transferring {
                switch role {
                case .owner: condition.unlock(); return
                case .client(let client) where !client.connection.isClosed: condition.unlock(); return
                default: break
                }
                electing = true; electionFailure = nil
                // Election belongs to the handle. A canceled or timed-out
                // writer leaves this bounded worker available to other callers.
                if platform.backgroundElection {
                    Thread.detachNewThread { self.finishElection() }
                } else {
                    condition.unlock(); finishElection(); condition.lock(); continue
                }
            }
            if let deadline, ContinuousClock.now >= deadline {
                condition.unlock(); throw JedError("55P03", "canceling statement due to lock timeout")
            }
            if deadline == nil && cancellation == nil { condition.wait() }
            else { _ = condition.wait(until: Date(timeIntervalSinceNow: 0.001)) }
        }
    }
    func awaitWriteAdmission(timeoutMilliseconds: UInt64, cancellation: CancellationToken?) throws {
        let deadline = timeoutMilliseconds == 0 ? nil : ContinuousClock.now.advanced(by: .milliseconds(Int64(clamping: timeoutMilliseconds)))
        while true {
            try awaitAdmission(deadline: deadline, cancellation: cancellation)
            condition.lock(); let selected = role; condition.unlock()
            guard case .client(let client) = selected, client.connection.hello.flags & 2 != 0, !options.readOnly else { return }
            guard options.pageService.ownership != .never else { throw JedError("25006", "page-service owner is read only") }
            // Probe write permission without touching any page; the read-only
            // owner never needs to open its own descriptor with write access.
            try platform.probeWritable(path)
            _ = try client.connection.request(.requestOwnership).wait(cancellation: cancellation, deadline: deadline, timeoutCode: "55P03")
            while !client.connection.isClosed {
                if cancellation?.isCancelled == true { throw JedError("57014", "canceling statement due to user request") }
                if let deadline, ContinuousClock.now >= deadline { throw JedError("55P03", "canceling statement due to lock timeout") }
                platform.pause()
            }
        }
    }
    private func receiveNotice(_ kind: UInt8, requester: UInt64) {
        condition.lock(); defer { condition.unlock() }
        if kind == 1 { noticeRequester = requester }
        else if kind == 2 { noticeRequester = nil; pauseUntil = nil }
        condition.broadcast()
    }
    private func disconnected() {
        condition.lock()
        if let requested = noticeRequester, case .client(let client) = role, requested != client.connection.hello.connection {
            pauseUntil = ServiceHostTime.deadline(platform.now(), milliseconds: 2000)
        }
        noticeRequester = nil
        let restart = core != nil && !closed && !closing && !electing
        condition.broadcast(); condition.unlock()
        if restart && platform.backgroundElection { Thread.detachNewThread { try? self.awaitAdmission() } }
    }
    private func requestOwnership(_ id: UInt64) -> Bool {
        condition.lock()
        guard !closed, !closing, !transferring,
              options.pageService.ownership == .yield || options.readOnly,
              case .owner(_, let server?, _) = role else { condition.unlock(); return false }
        if let requester { condition.unlock(); return requester == id }
        // The accepted reply may reach a fast client before the worker runs.
        // Publish the service admission state now; broadcasting here would
        // recursively take the requesting connection's operation lock.
        guard server.markDraining(requester: id) else { condition.unlock(); return false }
        // Also stop local callers that passed the coordinator and are still
        // waiting for the writer token before their registry admission check.
        try? core?.pauseAdmissions()
        requester = id; condition.unlock()
        platform.dispatch { self.yieldRole(server: server, requester: id) }
        return true
    }
    private func connectionEnded(_ id: UInt64) {
        condition.lock()
        guard requester == id, !transferring, !closing else { condition.unlock(); return }
        let server: PageServiceOwner?
        if case .owner(_, let current, _) = role { server = current } else { server = nil }
        // Clear both resident gates before another requester can be admitted;
        // recipient notifications must stay outside the host condition.
        let cleared = server?.clearDrainReservation(requester: id) ?? false
        requester = nil; core?.resumeAdmissions()
        condition.broadcast(); condition.unlock()
        if cleared { server?.announceDrainCancellation() }
    }
    private func yieldRole(server: PageServiceOwner, requester id: UInt64) {
        guard let core else { return }
        condition.lock()
        guard requester == id, !closing, !closed else { condition.unlock(); return }
        condition.unlock()
        server.beginDrain(kind: 1, requester: id)
        condition.lock()
        let cancelledBeforeDrain = requester != id || closing || closed
        let resume = requester == nil && !closing && !closed
        if resume { core.resumeAdmissions() }
        condition.unlock()
        if cancelledBeforeDrain { if resume { server.cancelDrain(requester: id) }; return }
        let deadline = ServiceHostTime.deadline(platform.now(), milliseconds: options.pageService.yieldGraceMilliseconds)
        while core.livePinCount != 0 || core.hasWriter {
            condition.lock(); let cancelled = requester != id || closing || closed; condition.unlock()
            if cancelled { return }
            if platform.now() >= deadline { break }; platform.pause()
        }
        condition.lock()
        guard requester == id, !closing, !closed else { condition.unlock(); return }
        transferring = true; heldCodec = core.storage.pager.codec as? EncryptedPageCodec
        let old = role; role = nil; condition.unlock()
        server.stop()
        do { try core.quiesceRole(flush: true) }
        catch { condition.lock(); failure = error as? JedError ?? JedError("58030", "owner handoff flush failed"); condition.unlock() }
        dispose(old)
        condition.lock(); requester = nil; transferring = false
        pauseUntil = ServiceHostTime.deadline(platform.now(), milliseconds: 2000); condition.broadcast(); condition.unlock()
        if platform.backgroundElection { try? awaitAdmission() }
    }
    private func ownerFailed() {
        condition.lock()
        if closed || closing { condition.unlock(); return }
        failure = JedError("58030", "page-service owner storage failed; reopen the database")
        let current = role; condition.broadcast(); condition.unlock()
        // Poisoning stops service but does not end the file owner's lifetime.
        // Keep presence and the descriptor until the host closes or dies, so a
        // client cannot promote during resolution of an in-flight failed commit.
        try? core?.quiesceRole(flush: false)
        if case .owner(_, let server, _) = current { server?.stop() }
    }
    private func resolve(txid: UInt64, digest: UInt64) throws -> Bool {
        do {
            try awaitAdmission()
            condition.lock(); let selected = role; condition.unlock()
            let status: UInt8
            switch selected {
            case .owner: status = try core!.serviceStatus(txid: txid, digest: digest)
            case .client(let client):
                guard case .commitStatusReply(let reply) = try client.connection.request(.commitStatus(txid: txid, digest: digest)).wait() else {
                    throw PageServiceProtocol.violation("commit_status received another reply kind")
                }; status = reply
            case nil: throw JedError("08007", "commit outcome is unknown")
            }
            if status == 0 { return true }; if status == 1 { return false }
            throw JedError("08007", "commit is outside the owner's resolution window")
        } catch let error as JedError where error.code != "08007" {
            throw JedError("08007", "commit outcome could not be resolved after reconnecting")
        }
    }
    func close() throws { if let core { try closeDatabase(core) } else { closeFailedOpen() } }
    func closeDatabase(_ core: DatabaseCore) throws {
        condition.lock()
        if closed { condition.unlock(); return }
        if closing { while !closed { condition.wait() }; condition.unlock(); return }
        do { try core.pauseAdmissions(rejectLocalWriter: true) }
        catch { condition.unlock(); throw error }
        // Close cancels future admission/election and waits for an in-flight
        // role installation to finish before releasing its file/path lifetime.
        // Owner EOF can start election just before an otherwise idle close.
        closing = true; requester = nil; condition.broadcast()
        while electing || transferring { condition.wait() }
        let old = role; condition.unlock()
        if case .owner(_, let server?, _) = old {
            server.beginDrain(kind: 3)
            let deadline = ServiceHostTime.deadline(platform.now(), milliseconds: options.pageService.shutdownGraceMilliseconds)
            while server.hasClientTransactions && platform.now() < deadline { platform.pause() }
            server.stop()
        }
        var problem: (any Error)?
        do { try core.quiesceRole(flush: localHost != nil) } catch { problem = error }
        dispose(old)
        condition.lock(); role = nil; closed = true; closing = false; condition.broadcast(); condition.unlock()
        claim.close()
        if let problem { throw problem }
    }
    deinit { closeFailedOpen() }
}
#endif
