#if os(Linux) || canImport(Darwin)
import Foundation
import CJedHost
import Synchronization
#if os(Linux)
import Glibc
#else
import Darwin
#endif

/// Native file ownership is retained independently of query snapshots. Closing
/// the byte descriptor precedes releasing presence, including failure cleanup.
internal final class FileDatabaseHost: DatabaseHostLifetime {
    let path: String
    private struct State { var store: UnixBlockStore; var closed = false; var poisoned = false }
    private let state: Mutex<State>
    var store: UnixBlockStore { state.withLock { $0.store } }
    let scratch: NativeScratchFileHost
    private let bundle: FileLockBundle?
    private let opener: pid_t
    private let skipFsync: Bool
    private init(path: String, store: UnixBlockStore, bundle: FileLockBundle?, skipFsync: Bool, sealedScratch: Bool) {
        self.path = path; self.state = Mutex(State(store: store)); self.bundle = bundle; self.skipFsync = skipFsync; opener = getpid()
        scratch = NativeScratchFileHost(sealed: sealedScratch)
    }
    func checkOpen() throws {
        try state.withLock { state in
            guard !state.closed else { throw JedError("55000", "database is closed") }
            guard !state.poisoned else { throw JedError("58030", "database requires reopen after failed maintenance") }
        }
        guard getpid() == opener else { throw JedError("55006", "database handle was inherited across fork; reopen it") }
        try bundle?.checkOpen()
    }
    func close() throws {
        try state.withLock { state in
            if state.closed { return }
            // close(2) consumes the descriptor even when a late I/O error is
            // reported. UnixBlockStore always invalidates it before returning.
            defer { state.closed = true; bundle?.close() }
            try state.store.close()
        }
    }
    deinit { try? close() }

    func poison() { state.withLock { $0.poisoned = true } }
    func syncDirectory() throws { try Self.syncDirectory(path, skip: skipFsync) }

    /// All preparation writes go to the private replacement. The caller holds
    /// the writer gate and pin registry until the new storage is published.
    func replace(source: StorageDomain, snapshot: DomainSnapshot, txid: UInt64,
                 codec: any PageCodec, keyBlock: KeyBlock?, entropy: KeyEntropy?,
                 environment: Environment, limit: Int64 = 0) throws -> StorageDomain {
        try state.withLock { state in
            guard !state.closed, !state.poisoned, getpid() == opener else {
                throw JedError("58030", "file host is unavailable for replacement")
            }
            try source.pager.checkWritable()
            let temporary = path + ".jedtmp"
            // A previous interrupted maintenance may leave this disposable
            // sibling. Unlinking first also avoids following a stale symlink.
            if temporary.withCString({ unlink($0) }) != 0, errno != ENOENT { throw Self.io("remove stale maintenance image") }
            let target = try UnixBlockStore(path: temporary, mode: .createExclusive,
                durability: skipFsync ? .unsynced : .durable)
            var renamed = false
            defer { try? target.close(); if !renamed { _ = temporary.withCString { unlink($0) } } }
            try target.setPermissions(state.store.permissions())
            let image = try DomainImage.write(snapshot, txid: txid, codec: codec, keyBlock: keyBlock, entropy: entropy) { page in
                try target.write(at: UInt64(page.index) * UInt64(codec.pageSize), bytes: page.bytes)
            }
            try target.setSize(UInt64(image.pageCount) * UInt64(codec.pageSize))
            if limit > 0, UInt64(image.livePages) * UInt64(codec.pageSize) > UInt64(limit) {
                throw JedError("54P06", "database main exceeds max_storage_bytes (\(limit))")
            }
            try target.sync(); try target.close()
            do { try state.store.close() }
            catch {
                do { try state.store.reopen(path: path) } catch { state.poisoned = true; source.pager.revoke() }
                throw error
            }
            if temporary.withCString({ from in path.withCString { to in rename(from, to) } }) != 0 {
                let failure = Self.io("replace database image")
                do { try state.store.reopen(path: path) } catch { state.poisoned = true; source.pager.revoke() }
                throw failure
            }
            renamed = true; source.pager.revoke()
            var opened: UnixBlockStore?
            do {
                try Self.syncDirectory(path, skip: skipFsync)
                let device = try UnixBlockStore(path: path, mode: .readWrite, durability: skipFsync ? .unsynced : .durable)
                opened = device
                let capacity = source.pool.capacity.map { $0 * codec.pageSize }
                let replacement = try StorageDomain.open(store: device, kind: .file, catalogID: snapshot.catalogID,
                    cacheBytes: capacity, unlockedCodec: codec as? EncryptedPageCodec,
                    collatedKey: { try environment.unicode.key($0, collation: $1) },
                    collationVerdict: { try CatalogEnvironment.verdict($0, environment: environment) },
                    validateCatalog: { try CatalogEnvironment.validate($0, environment: environment) },
                    durableDirectorySync: { [path, skipFsync] in try Self.syncDirectory(path, skip: skipFsync) })
                replacement.pager.acknowledgeFreshDurableImage()
                state.store = device
                return replacement
            } catch { try? opened?.close(); state.poisoned = true; throw error }
        }
    }

    private static func io(_ operation: String) -> JedError { JedError("58030", "\(operation) failed (errno \(errno))") }
    private static func closeFD(_ fd: Int32) {
        #if os(Linux)
        _ = Glibc.close(fd)
        #else
        _ = Darwin.close(fd)
        #endif
    }
    private static func lock(path: String, mode: FileLocking, timeout: Int64) throws -> FileLockBundle? {
        guard timeout >= 0 else { throw JedError("22023", "negative file lock timeout") }
        switch mode {
        case .none: return nil
        case .exclusive: return try FileLockBundle.acquireExclusive(path: path, timeoutMilliseconds: timeout)
        case .automatic: throw JedError("0A000", "automatic page-service ownership is not yet implemented by the Swift experiment")
        }
    }
    static func syncDirectory(_ path: String, skip: Bool) throws {
        if skip { return }
        let directory = URL(fileURLWithPath: path).deletingLastPathComponent().path
        let fd = directory.withCString {
            #if os(Linux)
            Glibc.open($0, O_RDONLY | O_DIRECTORY | O_CLOEXEC)
            #else
            Darwin.open($0, O_RDONLY | O_DIRECTORY | O_CLOEXEC)
            #endif
        }
        guard fd >= 0 else { throw io("open database directory") }
        defer { closeFD(fd) }
        while fsync(fd) != 0 { if errno != EINTR { throw io("sync database directory") } }
    }
    private static func exists(_ path: String) throws -> Bool {
        var info = stat()
        if path.withCString({ lstat($0, &info) }) == 0 { return true }
        if errno == ENOENT { return false }; throw io("inspect create destination")
    }
    private static func temporaryPath(_ path: String) throws -> String {
        let bytes = try OSRandomSource().bytes(count: 16), digits = Array("0123456789abcdef".utf8)
        return path + ".create-" + String(decoding: bytes.flatMap { [digits[Int($0 >> 4)], digits[Int($0 & 15)]] }, as: UTF8.self)
    }
    /// Native no-replace rename avoids a check/publish race even when the caller
    /// deliberately selected external coordination with locking=none.
    private static func publish(_ temporary: String, to path: String) throws {
        let result = temporary.withCString { source in path.withCString { target in
            jed_host_rename_noreplace(source, target)
        } }
        guard result == 0 else {
            if errno == EEXIST { throw JedError("58P02", "database file already exists: \(path)") }
            if errno == ENOSYS || errno == EOPNOTSUPP || errno == EINVAL { throw JedError("0A000", "atomic no-replace file publication is unavailable") }
            throw io("publish database file")
        }
    }
    static func create(path original: String, options: CreateOptions, catalogID: UInt64, random: any RandomSource) throws -> Database {
        try PageEnvelope.validateSize(options.pageSize, encrypted: options.credential != nil, opening: false)
        let path = try FileLockBundle.normalize(original, creating: true)
        let bundle = try lock(path: path, mode: options.locking, timeout: options.fileLockTimeoutMilliseconds)
        var host: FileDatabaseHost?
        var temporary: String?
        do {
            guard try !exists(path) else { throw JedError("58P02", "database file already exists: \(path)") }
            let temporaryPath = try self.temporaryPath(path)
            let store = try UnixBlockStore(path: temporaryPath, mode: .createExclusive, durability: options.skipFsync ? .unsynced : .durable)
            temporary = temporaryPath
            let owner = FileDatabaseHost(path: path, store: store, bundle: bundle, skipFsync: options.skipFsync,
                sealedScratch: options.credential != nil || options.scratchEncryption == .on)
            host = owner
            let storage = try StorageDomain.create(store: store, kind: .file, catalogID: catalogID,
                pageSize: options.pageSize, cacheBytes: options.cacheBytes, credential: options.credential,
                parameters: options.kdf, entropy: { try random.bytes(count: $0) })
            try publish(temporaryPath, to: path); temporary = nil
            try syncDirectory(path, skip: options.skipFsync)
            return Database(core: DatabaseCore(storage: storage), environment: options.environment,
                maxStorageBytes: options.maxStorageBytes, keySource: random, host: owner)
        } catch {
            if let host { try? host.close() } else { bundle?.close() }
            if let temporary { _ = temporary.withCString { unlink($0) } }
            throw error
        }
    }
    static func open(path original: String, options: OpenOptions, catalogID: UInt64) throws -> Database {
        let path = try FileLockBundle.normalize(original, creating: false)
        if options.locking != .none { try FileLockBundle.checkHardLinks(path) }
        let bundle = try lock(path: path, mode: options.locking, timeout: options.fileLockTimeoutMilliseconds)
        var host: FileDatabaseHost?
        var store: UnixBlockStore?
        do {
            if options.locking != .none { try FileLockBundle.checkHardLinks(path) }
            let device = try UnixBlockStore(path: path, mode: options.readOnly ? .readOnly : .readWrite,
                durability: options.skipFsync ? .unsynced : .durable)
            store = device
            let storage = try StorageDomain.open(store: device, kind: .file, catalogID: catalogID,
                cacheBytes: options.cacheBytes, credential: options.credential, readOnly: options.readOnly,
                collatedKey: { try options.environment.unicode.key($0, collation: $1) },
                collationVerdict: { try CatalogEnvironment.verdict($0, environment: options.environment) },
                validateCatalog: { try CatalogEnvironment.validate($0, environment: options.environment) },
                durableDirectorySync: { try syncDirectory(path, skip: options.skipFsync) },
                syncDirectory: { try syncDirectory(path, skip: options.skipFsync) })
            let owner = FileDatabaseHost(path: path, store: device, bundle: bundle, skipFsync: options.skipFsync,
                sealedScratch: storage.pager.codec.reservedBytes != 0 || options.scratchEncryption == .on)
            host = owner
            return Database(core: DatabaseCore(storage: storage), environment: options.environment,
                maxStorageBytes: options.maxStorageBytes, host: owner)
        } catch {
            if let host { try? host.close() } else { try? store?.close(); bundle?.close() }
            throw error
        }
    }
}
#endif
