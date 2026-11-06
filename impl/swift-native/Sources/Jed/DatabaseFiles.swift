import Foundation

public enum FileLocking: Sendable { case automatic, exclusive, none }
public enum ScratchEncryption: Sendable { case automatic, on }

public enum OwnershipPreference: Sendable { case prefer, yield, never }

/// Native local-file coordination. Limits govern host waits and connections;
/// they never contribute to deterministic SQL cost.
public struct PageServiceOptions: Sendable {
    public var serve: Bool
    public var ownership: OwnershipPreference
    public var yieldGraceMilliseconds: UInt64
    public var shutdownGraceMilliseconds: UInt64
    public var idleSessionMilliseconds: UInt64
    public var idleWriteMilliseconds: UInt64
    public var maxConnections: Int
    public init(serve: Bool = true, ownership: OwnershipPreference = .prefer,
                yieldGraceMilliseconds: UInt64 = 5000, shutdownGraceMilliseconds: UInt64 = 30_000,
                idleSessionMilliseconds: UInt64 = 0, idleWriteMilliseconds: UInt64 = 60_000,
                maxConnections: Int = 0) {
        self.serve = serve; self.ownership = ownership; self.yieldGraceMilliseconds = yieldGraceMilliseconds
        self.shutdownGraceMilliseconds = shutdownGraceMilliseconds; self.idleSessionMilliseconds = idleSessionMilliseconds
        self.idleWriteMilliseconds = idleWriteMilliseconds; self.maxConnections = maxConnections
    }
    internal func validate() throws {
        guard maxConnections >= 0 else { throw JedError("22023", "negative page-service connection limit") }
    }
    #if os(Linux)
    internal var ownerLimits: PageServiceOwnerLimits {
        PageServiceOwnerLimits(idleSessionMilliseconds: idleSessionMilliseconds,
            idleWriteMilliseconds: idleWriteMilliseconds, maxConnections: maxConnections)
    }
    #endif
}

public struct CreateOptions: Sendable {
    public var environment: Environment
    public var path: String?
    public var pageSize: Int
    public var cacheBytes: Int
    public var maxStorageBytes: Int64
    public var credential: Credential?
    public var kdf: Argon2Parameters
    public var locking: FileLocking
    public var fileLockTimeoutMilliseconds: Int64
    public var skipFsync: Bool
    public var synchronous: Bool
    public var flushIntervalMilliseconds: UInt64
    public var scratchEncryption: ScratchEncryption
    public var pageService: PageServiceOptions
    public init(environment: Environment = Environment.builder().build(), path: String? = nil,
                pageSize: Int = 8192, cacheBytes: Int = 256 * 1024 * 1024, maxStorageBytes: Int64 = 0,
                credential: Credential? = nil, kdf: Argon2Parameters = .default,
                locking: FileLocking = .automatic, fileLockTimeoutMilliseconds: Int64 = 5000,
                skipFsync: Bool = false, synchronous: Bool = true, flushIntervalMilliseconds: UInt64 = 1000, scratchEncryption: ScratchEncryption = .automatic,
                pageService: PageServiceOptions = PageServiceOptions()) {
        self.environment = environment; self.path = path; self.pageSize = pageSize; self.cacheBytes = cacheBytes
        self.maxStorageBytes = maxStorageBytes; self.credential = credential; self.kdf = kdf; self.locking = locking
        self.fileLockTimeoutMilliseconds = fileLockTimeoutMilliseconds; self.skipFsync = skipFsync
        self.synchronous = synchronous; self.flushIntervalMilliseconds = flushIntervalMilliseconds; self.scratchEncryption = scratchEncryption; self.pageService = pageService
    }
}

public struct OpenOptions: Sendable {
    public var environment: Environment
    public var cacheBytes: Int
    public var maxStorageBytes: Int64
    public var credential: Credential?
    public var readOnly: Bool
    public var locking: FileLocking
    public var fileLockTimeoutMilliseconds: Int64
    public var skipFsync: Bool
    public var synchronous: Bool
    public var flushIntervalMilliseconds: UInt64
    public var scratchEncryption: ScratchEncryption
    public var pageService: PageServiceOptions
    public init(environment: Environment = Environment.builder().build(), cacheBytes: Int = 256 * 1024 * 1024,
                maxStorageBytes: Int64 = 0, credential: Credential? = nil, readOnly: Bool = false,
                locking: FileLocking = .automatic, fileLockTimeoutMilliseconds: Int64 = 5000,
                skipFsync: Bool = false, synchronous: Bool = true, flushIntervalMilliseconds: UInt64 = 1000, scratchEncryption: ScratchEncryption = .automatic,
                pageService: PageServiceOptions = PageServiceOptions()) {
        self.environment = environment; self.cacheBytes = cacheBytes; self.maxStorageBytes = maxStorageBytes
        self.credential = credential; self.readOnly = readOnly; self.locking = locking
        self.fileLockTimeoutMilliseconds = fileLockTimeoutMilliseconds; self.skipFsync = skipFsync
        self.synchronous = synchronous; self.flushIntervalMilliseconds = flushIntervalMilliseconds; self.scratchEncryption = scratchEncryption; self.pageService = pageService
    }
}

public struct DeserializeOptions: Sendable {
    public var environment: Environment
    public var maxStorageBytes: Int64
    public init(environment: Environment = Environment.builder().build(), maxStorageBytes: Int64 = 0) {
        self.environment = environment; self.maxStorageBytes = maxStorageBytes
    }
}

extension Database {
    public var path: String? { host?.path }
    public var residentLeaves: Int { core.storage.pool.residentLeaves }
    #if os(Linux) || canImport(Darwin)
    internal var localFileHost: FileDatabaseHost? {
        if let host = host as? FileDatabaseHost { return host }
        #if os(Linux)
        return (host as? ServiceDatabaseHost)?.localHost
        #else
        return nil
        #endif
    }
    #endif

    public static func create(options: CreateOptions = CreateOptions()) throws -> Database {
        let catalogID = allocateCatalogID(), random = options.environment.newKeySource()
        guard let path = options.path else {
            let storage = try StorageDomain.create(store: MemoryBlockStore(), catalogID: catalogID, pageSize: options.pageSize,
                credential: options.credential, parameters: options.kdf, entropy: { try random.bytes(count: $0) })
            let database = Database(core: DatabaseCore(storage: storage), environment: options.environment,
                maxStorageBytes: options.maxStorageBytes, keySource: random, flushIntervalMilliseconds: options.flushIntervalMilliseconds)
            database.synchronous = options.synchronous; return database
        }
        #if os(Linux) || canImport(Darwin)
        #if os(Linux)
        if options.locking == .automatic {
            return try ServiceDatabaseHost.create(path: path, options: options, catalogID: catalogID, random: random)
        }
        #endif
        return try FileDatabaseHost.create(path: path, options: options, catalogID: catalogID, random: random)
        #else
        throw JedError("0A000", "native file databases are unavailable on this host")
        #endif
    }

    public static func open(_ path: String, options: OpenOptions = OpenOptions()) throws -> Database {
        #if os(Linux) || canImport(Darwin)
        #if os(Linux)
        if options.locking == .automatic {
            return try ServiceDatabaseHost.open(path: path, options: options, catalogID: allocateCatalogID())
        }
        #endif
        return try FileDatabaseHost.open(path: path, options: options, catalogID: allocateCatalogID())
        #else
        throw JedError("0A000", "native file databases are unavailable on this host")
        #endif
    }

    public static func deserialize(_ bytes: [UInt8], options: DeserializeOptions = DeserializeOptions()) throws -> Database {
        let store = MemoryBlockStore(), catalogID = allocateCatalogID()
        try store.write(at: 0, bytes: bytes)
        let storage = try StorageDomain.open(store: store, kind: .memory, catalogID: catalogID,
            collatedKey: { try options.environment.unicode.key($0, collation: $1) },
                collationVerdict: { try CatalogEnvironment.verdict($0, environment: options.environment) },
            validateCatalog: { try CatalogEnvironment.validate($0, environment: options.environment) })
        return Database(core: DatabaseCore(storage: storage), environment: options.environment, maxStorageBytes: options.maxStorageBytes)
    }

    /// The buffer is an explicit host result. Internal maintenance uses the page
    /// sink directly and never collects a database-sized image.
    public func serialize(_ name: String = "main") throws -> [UInt8] {
        try checkOpen()
        let pin = try domains.pinCommitted(SQLText(name), main: self); defer { pin.close() }
        guard pin.snapshot.pool.codec.reservedBytes == 0 else { throw JedError("0A000", "serialization of an encrypted database is not supported") }
        try pin.checkRole()
        var bytes: [UInt8] = []
        _ = try DomainImage.write(pin.snapshot, txid: pin.snapshot.txid) { page in
            try pin.checkRole()
            let offset = UInt64(page.index) * UInt64(page.bytes.count), end = offset + UInt64(page.bytes.count)
            guard end <= UInt64(Int.max) else { throw JedError("54000", "serialized database exceeds host buffer size") }
            if bytes.count < Int(end) { bytes.append(contentsOf: repeatElement(0, count: Int(end) - bytes.count)) }
            bytes.replaceSubrange(Int(offset)..<Int(end), with: page.bytes)
        }
        try pin.checkRole()
        return bytes
    }
}
