/// Ownership lifetime retained by a file host until its byte handle closes.
internal protocol FileOwnership: AnyObject, Sendable {
    func checkOpen() throws
    /// `checkOpen` without the opener-process check.
    func checkAdmitted() throws
    func close()
}
extension FileOwnership {
    func checkAdmitted() throws { try checkOpen() }
}

/// Byte handles may be closed independently of their inode. A failed native
/// rename reopens the old inode into this same object, preserving pager users.
internal protocol FileBlockStore: BlockStore {
    func close() throws
    func reopen(path: String) throws
}

/// Host pathname operations only. FileDatabaseHost retains common create,
/// replacement, poisoning and recovery policy; both OS and simulation supply
/// this seam. Neither adapter implements database commits or recovery.
internal protocol FileOperations: Sendable {
    var backgroundBatchFlush: Bool { get }
    func resolve(_ path: String, creating: Bool) throws -> String
    func exists(_ path: String) throws -> Bool
    func temporaryCreatePath(_ path: String) throws -> String
    func create(_ path: String, skipFsync: Bool) throws -> any FileBlockStore
    func createReplacementTemporary(_ path: String, skipFsync: Bool) throws -> any FileBlockStore
    func open(_ path: String, readOnly: Bool, skipFsync: Bool) throws -> any FileBlockStore
    func copyPermissions(from source: any FileBlockStore, to target: any FileBlockStore) throws
    func remove(_ path: String) throws
    func rename(_ source: String, to destination: String, replace: Bool) throws
    func syncDirectory(_ path: String, skipFsync: Bool) throws
}

internal protocol DatabaseFilesystemProvider: Sendable {
    var fileOperations: any FileOperations { get }
}

#if os(Linux) || canImport(Darwin)
import Foundation
import CJedHost
#if os(Linux)
import Glibc
#else
import Darwin
#endif

extension UnixBlockStore: FileBlockStore {}

extension FileOperations {
    var backgroundBatchFlush: Bool { true }
    /// Native replacement discards a stale sibling without following it. The
    /// simulator models this host operation as one canonical create/truncate.
    func createReplacementTemporary(_ path: String, skipFsync: Bool) throws -> any FileBlockStore {
        try remove(path)
        return try create(path, skipFsync: skipFsync)
    }
}

internal struct NativeFileOperations: FileOperations {
    func resolve(_ path: String, creating: Bool) throws -> String { try FileLockBundle.normalize(path, creating: creating) }
    func exists(_ path: String) throws -> Bool {
        var info = stat()
        if path.withCString({ lstat($0, &info) }) == 0 { return true }
        if errno == ENOENT { return false }; throw Self.io("inspect file destination")
    }
    func temporaryCreatePath(_ path: String) throws -> String {
        let bytes = try OSRandomSource().bytes(count: 16), digits = Array("0123456789abcdef".utf8)
        return path + ".create-" + String(decoding: bytes.flatMap { [digits[Int($0 >> 4)], digits[Int($0 & 15)]] }, as: UTF8.self)
    }
    func create(_ path: String, skipFsync: Bool) throws -> any FileBlockStore {
        try UnixBlockStore(path: path, mode: .createExclusive, durability: skipFsync ? .unsynced : .durable)
    }
    func open(_ path: String, readOnly: Bool, skipFsync: Bool) throws -> any FileBlockStore {
        try UnixBlockStore(path: path, mode: readOnly ? .readOnly : .readWrite, durability: skipFsync ? .unsynced : .durable)
    }
    func copyPermissions(from source: any FileBlockStore, to target: any FileBlockStore) throws {
        guard let source = source as? UnixBlockStore, let target = target as? UnixBlockStore else {
            throw JedError("58030", "native file operations received a foreign byte handle")
        }
        try target.setPermissions(source.permissions())
    }
    func remove(_ path: String) throws {
        if path.withCString({ unlink($0) }) != 0, errno != ENOENT { throw Self.io("remove file") }
    }
    func rename(_ source: String, to destination: String, replace: Bool) throws {
        let result = source.withCString { from in destination.withCString { to in
            if replace {
                #if os(Linux)
                return Glibc.rename(from, to)
                #else
                return Darwin.rename(from, to)
                #endif
            }
            return jed_host_rename_noreplace(from, to)
        } }
        guard result == 0 else {
            if !replace, errno == EEXIST { throw JedError("58P02", "database file already exists: \(destination)") }
            if !replace, errno == ENOSYS || errno == EOPNOTSUPP || errno == EINVAL {
                throw JedError("0A000", "atomic no-replace file publication is unavailable")
            }
            throw Self.io("publish database file")
        }
    }
    func syncDirectory(_ path: String, skipFsync: Bool) throws {
        if skipFsync { return }
        let directory = URL(fileURLWithPath: path).deletingLastPathComponent().path
        let descriptor = directory.withCString {
            #if os(Linux)
            Glibc.open($0, O_RDONLY | O_DIRECTORY | O_CLOEXEC)
            #else
            Darwin.open($0, O_RDONLY | O_DIRECTORY | O_CLOEXEC)
            #endif
        }
        guard descriptor >= 0 else { throw Self.io("open database directory") }
        defer {
            #if os(Linux)
            _ = Glibc.close(descriptor)
            #else
            _ = Darwin.close(descriptor)
            #endif
        }
        while fsync(descriptor) != 0 { if errno != EINTR { throw Self.io("sync database directory") } }
    }
    private static func io(_ operation: String) -> JedError { JedError("58030", "\(operation) failed (errno \(errno))") }
}
#endif
