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
internal final class FileDatabaseHost: DatabaseHostLifetime, DatabaseFilesystemProvider {
    let path: String
    private struct State { var store: any FileBlockStore; var closed = false; var poisoned = false }
    private let state: Mutex<State>
    var store: any FileBlockStore { state.withLock { $0.store } }
    let scratch: any ScratchFileHost
    let fileOperations: any FileOperations
    private let bundle: FileLockBundle?
    private let opener: pid_t
    private let skipFsync: Bool
    var skipsFsync: Bool { skipFsync }
    private init(path: String, store: any FileBlockStore, bundle: FileLockBundle?, skipFsync: Bool,
                 operations: any FileOperations, scratch: any ScratchFileHost) {
        self.path = path; self.state = Mutex(State(store: store)); self.bundle = bundle; self.skipFsync = skipFsync; opener = getpid()
        self.fileOperations = operations; self.scratch = scratch
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
    func syncDirectory() throws { try fileOperations.syncDirectory(path, skipFsync: skipFsync) }

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
            try fileOperations.remove(temporary)
            let target = try fileOperations.create(temporary, skipFsync: skipFsync)
            var renamed = false
            defer { try? target.close(); if !renamed { try? fileOperations.remove(temporary) } }
            try fileOperations.copyPermissions(from: state.store, to: target)
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
            do { try fileOperations.rename(temporary, to: path, replace: true) }
            catch {
                let failure = error
                do { try state.store.reopen(path: path) } catch { state.poisoned = true; source.pager.revoke() }
                throw failure
            }
            renamed = true; source.pager.revoke()
            var opened: (any FileBlockStore)?
            do {
                try fileOperations.syncDirectory(path, skipFsync: skipFsync)
                let device = try fileOperations.open(path, readOnly: false, skipFsync: skipFsync)
                opened = device
                let capacity = source.pool.capacity.map { $0 * codec.pageSize }
                let replacement = try StorageDomain.open(store: device, kind: .file, catalogID: snapshot.catalogID,
                    cacheBytes: capacity, unlockedCodec: codec as? EncryptedPageCodec,
                    collatedKey: { try environment.unicode.key($0, collation: $1) },
                    collationVerdict: { try CatalogEnvironment.verdict($0, environment: environment) },
                    validateCatalog: { try CatalogEnvironment.validate($0, environment: environment) },
                    durableDirectorySync: { [fileOperations, path, skipFsync] in try fileOperations.syncDirectory(path, skipFsync: skipFsync) })
                replacement.pager.acknowledgeFreshDurableImage()
                state.store = device
                return replacement
            } catch { try? opened?.close(); state.poisoned = true; throw error }
        }
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
        try NativeFileOperations().syncDirectory(path, skipFsync: skip)
    }
    static func create(path original: String, options: CreateOptions, catalogID: UInt64, random: any RandomSource,
                       bundle supplied: FileLockBundle? = nil) throws -> Database {
        try PageEnvelope.validateSize(options.pageSize, encrypted: options.credential != nil, opening: false)
        let path = try FileLockBundle.normalize(original, creating: true)
        let bundle = try supplied ?? lock(path: path, mode: options.locking, timeout: options.fileLockTimeoutMilliseconds)
        return try createResolved(path: path, options: options, catalogID: catalogID, random: random,
            operations: NativeFileOperations(), bundle: bundle)
    }
    /// Resolved pathname and ownership are supplied by the host. Both native and
    /// simulated files execute this exact creation/publication sequence.
    static func createResolved(path: String, options: CreateOptions, catalogID: UInt64, random: any RandomSource,
                               operations: any FileOperations, scratch: (any ScratchFileHost)? = nil,
                               bundle: FileLockBundle? = nil) throws -> Database {
        var host: FileDatabaseHost?
        var temporary: String?
        do {
            try PageEnvelope.validateSize(options.pageSize, encrypted: options.credential != nil, opening: false)
            guard try !operations.exists(path) else { throw JedError("58P02", "database file already exists: \(path)") }
            let temporaryPath = try operations.temporaryCreatePath(path)
            let store = try operations.create(temporaryPath, skipFsync: options.skipFsync)
            temporary = temporaryPath
            let owner = FileDatabaseHost(path: path, store: store, bundle: bundle, skipFsync: options.skipFsync,
                operations: operations, scratch: scratch ?? NativeScratchFileHost(sealed: options.credential != nil || options.scratchEncryption == .on))
            host = owner
            let storage = try StorageDomain.create(store: store, kind: .file, catalogID: catalogID,
                pageSize: options.pageSize, cacheBytes: options.cacheBytes, credential: options.credential,
                parameters: options.kdf, entropy: { try random.bytes(count: $0) })
            try operations.rename(temporaryPath, to: path, replace: false); temporary = nil
            try operations.syncDirectory(path, skipFsync: options.skipFsync)
            return Database(core: DatabaseCore(storage: storage, environment: options.environment), environment: options.environment,
                maxStorageBytes: options.maxStorageBytes, keySource: random, host: owner)
        } catch {
            if let host { try? host.close() } else { bundle?.close() }
            if let temporary { try? operations.remove(temporary) }
            throw error
        }
    }
    static func open(path original: String, options: OpenOptions, catalogID: UInt64,
                     bundle supplied: FileLockBundle? = nil, unlockedCodec: EncryptedPageCodec? = nil,
                     structural: Bool = false) throws -> Database {
        let path = try FileLockBundle.normalize(original, creating: false)
        if options.locking != .none { try FileLockBundle.checkHardLinks(path) }
        let bundle = try supplied ?? lock(path: path, mode: options.locking, timeout: options.fileLockTimeoutMilliseconds)
        do {
            if options.locking != .none { try FileLockBundle.checkHardLinks(path) }
            return try openResolved(path: path, options: options, catalogID: catalogID, operations: NativeFileOperations(),
                bundle: bundle, unlockedCodec: unlockedCodec, structural: structural)
        } catch { bundle?.close(); throw error }
    }
    static func openResolved(path: String, options: OpenOptions, catalogID: UInt64, operations: any FileOperations,
                             scratch: (any ScratchFileHost)? = nil, bundle: FileLockBundle? = nil,
                             unlockedCodec: EncryptedPageCodec? = nil, structural: Bool = false) throws -> Database {
        var host: FileDatabaseHost?
        var store: (any FileBlockStore)?
        do {
            let device = try operations.open(path, readOnly: options.readOnly, skipFsync: options.skipFsync)
            store = device
            let storage = try StorageDomain.open(store: device, kind: .file, catalogID: catalogID,
                cacheBytes: options.cacheBytes, credential: options.credential, readOnly: options.readOnly, unlockedCodec: unlockedCodec,
                heldSessionKey: structural && unlockedCodec != nil,
                collatedKey: { try options.environment.unicode.key($0, collation: $1) },
                collationVerdict: { if structural { return false }; return try CatalogEnvironment.verdict($0, environment: options.environment) },
                validateCatalog: { if !structural { try CatalogEnvironment.validate($0, environment: options.environment) } },
                durableDirectorySync: { try operations.syncDirectory(path, skipFsync: options.skipFsync) },
                syncDirectory: { try operations.syncDirectory(path, skipFsync: options.skipFsync) })
            let owner = FileDatabaseHost(path: path, store: device, bundle: bundle, skipFsync: options.skipFsync,
                operations: operations, scratch: scratch ?? NativeScratchFileHost(sealed: storage.pager.codec.reservedBytes != 0 || options.scratchEncryption == .on))
            host = owner
            return Database(core: DatabaseCore(storage: storage, environment: options.environment), environment: options.environment,
                maxStorageBytes: options.maxStorageBytes, host: owner)
        } catch {
            if let host { try? host.close() } else { try? store?.close(); bundle?.close() }
            throw error
        }
    }
}
#endif
