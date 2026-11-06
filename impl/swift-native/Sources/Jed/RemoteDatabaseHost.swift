#if os(Linux)
import Foundation

internal final class RemoteDatabaseHost: CoordinatedDatabaseClose, DatabaseRoleCoordinator, DatabaseScratchPolicy, @unchecked Sendable {
    let path = "<remote>"
    private let options: OpenOptions
    var forceSealedScratch: Bool { options.scratchEncryption == .on }
    private let factory: @Sendable () throws -> any PageTransport
    private let catalogID: UInt64
    private let condition = NSCondition()
    private var core: DatabaseCore?
    private var client: PageServiceClient?
    private var connecting = false, closing = false, closed = false
    private var completion: UInt64 = 0
    private var failure: JedError?

    private init(options: OpenOptions, catalogID: UInt64, factory: @escaping @Sendable () throws -> any PageTransport) {
        self.options = options; self.catalogID = catalogID; self.factory = factory
    }
    static func open(options: OpenOptions, catalogID: UInt64,
                     factory: @escaping @Sendable () throws -> any PageTransport) throws -> Database {
        guard options.fileLockTimeoutMilliseconds >= 0, options.cacheBytes >= 0, options.maxStorageBytes >= 0 else {
            throw JedError("22023", "negative remote database option")
        }
        let host = RemoteDatabaseHost(options: options, catalogID: catalogID, factory: factory)
        try host.connect()
        let core = host.core!
        core.coordinateRoles(with: host)
        return Database(core: core, environment: options.environment, maxStorageBytes: options.maxStorageBytes, host: host)
    }
    var isClosed: Bool { condition.lock(); defer { condition.unlock() }; return closed }
    func checkOpen() throws {
        condition.lock(); defer { condition.unlock() }
        guard !closed && !closing else { throw JedError("55000", "database is closed") }
    }
    private func connect() throws {
        let deadline = ContinuousClock.now.advanced(by: .milliseconds(options.fileLockTimeoutMilliseconds))
        condition.lock(); let codec = client?.unlockedCodec; condition.unlock()
        let transport = try factory(), framed = PageFrameStream(transport)
        let connection = try PageServiceConnection.open(socket: framed,
            hello: ServiceHello(flags: options.readOnly ? 1 : 0, credential: options.credential), deadline: deadline,
            disconnected: { [weak self] in self?.disconnected() })
        let opened = try PageServiceClient.open(connection: connection, credential: options.credential,
            environment: options.environment, unlockedCodec: codec, deadline: deadline, catalogID: catalogID,
            cacheBytes: options.cacheBytes, readAhead: true, resolve: { [weak self] txid, digest in
                guard let self else { throw JedError("08007", "remote handle ended during commit resolution") }
                return try self.resolve(txid: txid, digest: digest)
            })
        condition.lock()
        if closed || closing { condition.unlock(); opened.client.close(); throw JedError("55000", "database is closed") }
        let original = core
        condition.unlock()
        if let original { original.installRole(storage: opened.core.storage, remote: opened.client) }
        condition.lock(); let old = client
        if core == nil { core = opened.core }; client = opened.client
        condition.unlock(); old?.close()
    }
    private func disconnected() {
        condition.lock(); let restart = core != nil && !closed && !closing && !connecting
        condition.broadcast(); condition.unlock()
        if restart { Thread.detachNewThread { try? self.awaitAdmission() } }
    }
    func awaitAdmission() throws { try awaitAdmission(deadline: nil, cancellation: nil) }
    private func awaitAdmission(deadline: ContinuousClock.Instant?, cancellation: CancellationToken?) throws {
        condition.lock(); let observed = completion
        while true {
            if closed || closing { condition.unlock(); throw JedError("55000", "database is closed") }
            if cancellation?.isCancelled == true { condition.unlock(); throw JedError("57014", "canceling statement due to user request") }
            if completion != observed, let failure { condition.unlock(); throw failure }
            if !connecting, let client, !client.connection.isClosed { condition.unlock(); return }
            if !connecting {
                connecting = true; failure = nil
                Thread.detachNewThread {
                    let result = Result { try self.connect() }
                    self.condition.lock(); self.connecting = false; self.completion &+= 1
                    if case .failure(let error) = result { self.failure = error as? JedError ?? JedError("08006", "remote reconnect failed") }
                    self.condition.broadcast(); self.condition.unlock()
                }
            }
            if let deadline, ContinuousClock.now >= deadline { condition.unlock(); throw JedError("55P03", "canceling statement due to lock timeout") }
            if deadline == nil && cancellation == nil { condition.wait() }
            else { _ = condition.wait(until: Date(timeIntervalSinceNow: 0.001)) }
        }
    }
    func awaitWriteAdmission(timeoutMilliseconds: UInt64, cancellation: CancellationToken?) throws {
        let deadline = timeoutMilliseconds == 0 ? nil : ContinuousClock.now.advanced(by: .milliseconds(Int64(clamping: timeoutMilliseconds)))
        try awaitAdmission(deadline: deadline, cancellation: cancellation)
    }
    private func resolve(txid: UInt64, digest: UInt64) throws -> Bool {
        do {
            try awaitAdmission()
            condition.lock(); let current = client; condition.unlock()
            guard let current, case .commitStatusReply(let status) = try current.connection.request(.commitStatus(txid: txid, digest: digest)).wait() else {
                throw JedError("08007", "remote commit status unavailable")
            }
            if status == 0 { return true }; if status == 1 { return false }
            throw JedError("08007", "remote commit is outside the resolution window")
        } catch { throw JedError("08007", "remote commit outcome could not be resolved") }
    }
    func close() throws {
        condition.lock(); let current = core; condition.unlock()
        if let current { try closeDatabase(current) }
    }
    func closeDatabase(_ core: DatabaseCore) throws {
        condition.lock()
        if closed { condition.unlock(); return }
        if closing { while !closed { condition.wait() }; condition.unlock(); return }
        do { try core.pauseAdmissions(rejectLocalWriter: true) }
        catch { condition.unlock(); throw error }
        closing = true; condition.broadcast()
        while connecting { condition.wait() }
        let old = client; condition.unlock()
        try? core.quiesceRole(flush: false); old?.close()
        condition.lock(); client = nil; closed = true; closing = false; condition.broadcast(); condition.unlock()
    }
    deinit { client?.close() }
}

extension Database {
    /// Opens a page client over a host-owned stream factory. Reconnection calls
    /// the factory again. This handle never opens or owns a local database file.
    public static func openRemote(options: OpenOptions = OpenOptions(),
                                  transport: @escaping @Sendable () throws -> any PageTransport) throws -> Database {
        try RemoteDatabaseHost.open(options: options, catalogID: allocateCatalogID(), factory: transport)
    }
    public static func openRemote(command: [String], options: OpenOptions = OpenOptions()) throws -> Database {
        try openRemote(options: options) { try ProcessPageTransport(command: command) }
    }
}
#endif
