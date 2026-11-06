import Foundation

public enum FileLocking: Sendable { case automatic, exclusive, none }
public enum ScratchEncryption: Sendable { case automatic, on }

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
    public var scratchEncryption: ScratchEncryption
    public init(environment: Environment = Environment.builder().build(), path: String? = nil,
                pageSize: Int = 8192, cacheBytes: Int = 256 * 1024 * 1024, maxStorageBytes: Int64 = 0,
                credential: Credential? = nil, kdf: Argon2Parameters = .default,
                locking: FileLocking = .automatic, fileLockTimeoutMilliseconds: Int64 = 5000,
                skipFsync: Bool = false, scratchEncryption: ScratchEncryption = .automatic) {
        self.environment = environment; self.path = path; self.pageSize = pageSize; self.cacheBytes = cacheBytes
        self.maxStorageBytes = maxStorageBytes; self.credential = credential; self.kdf = kdf; self.locking = locking
        self.fileLockTimeoutMilliseconds = fileLockTimeoutMilliseconds; self.skipFsync = skipFsync; self.scratchEncryption = scratchEncryption
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
    public var scratchEncryption: ScratchEncryption
    public init(environment: Environment = Environment.builder().build(), cacheBytes: Int = 256 * 1024 * 1024,
                maxStorageBytes: Int64 = 0, credential: Credential? = nil, readOnly: Bool = false,
                locking: FileLocking = .automatic, fileLockTimeoutMilliseconds: Int64 = 5000,
                skipFsync: Bool = false, scratchEncryption: ScratchEncryption = .automatic) {
        self.environment = environment; self.cacheBytes = cacheBytes; self.maxStorageBytes = maxStorageBytes
        self.credential = credential; self.readOnly = readOnly; self.locking = locking
        self.fileLockTimeoutMilliseconds = fileLockTimeoutMilliseconds; self.skipFsync = skipFsync; self.scratchEncryption = scratchEncryption
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

    public static func create(options: CreateOptions = CreateOptions()) throws -> Database {
        let catalogID = allocateCatalogID(), random = options.environment.newKeySource()
        guard let path = options.path else {
            let storage = try StorageDomain.create(store: MemoryBlockStore(), catalogID: catalogID, pageSize: options.pageSize,
                credential: options.credential, parameters: options.kdf, entropy: { try random.bytes(count: $0) })
            return Database(core: DatabaseCore(storage: storage), environment: options.environment,
                maxStorageBytes: options.maxStorageBytes, keySource: random)
        }
        #if os(Linux) || canImport(Darwin)
        return try FileDatabaseHost.create(path: path, options: options, catalogID: catalogID, random: random)
        #else
        throw JedError("0A000", "native file databases are unavailable on this host")
        #endif
    }

    public static func open(_ path: String, options: OpenOptions = OpenOptions()) throws -> Database {
        #if os(Linux) || canImport(Darwin)
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
        guard name == "main" else { throw JedError("42704", "database \"\(name)\" is not attached") }
        guard core.storage.pager.codec.reservedBytes == 0 else { throw JedError("0A000", "serialization of an encrypted database is not supported") }
        let pin = core.pinLatest(); defer { pin.close() }
        var bytes: [UInt8] = []
        _ = try DomainImage.write(pin.snapshot, txid: pin.snapshot.txid) { page in
            let offset = UInt64(page.index) * UInt64(page.bytes.count), end = offset + UInt64(page.bytes.count)
            guard end <= UInt64(Int.max) else { throw JedError("54000", "serialized database exceeds host buffer size") }
            if bytes.count < Int(end) { bytes.append(contentsOf: repeatElement(0, count: Int(end) - bytes.count)) }
            bytes.replaceSubrange(Int(offset)..<Int(end), with: page.bytes)
        }
        return bytes
    }
}
