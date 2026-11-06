#if os(Linux) || canImport(Darwin)
import Foundation
import CJedHost
#if os(Linux)
import Glibc
#else
import Darwin
#endif

/// Destination-only OS adapter. The engine supplies pages and retains its
/// source pin; this host owns the temporary inode and atomic publication.
internal enum FileBackupHost {
    static func write(destination original: String, skipFsync: Bool,
                      checkRole: () throws -> Void,
                      body: (UnixBlockStore) throws -> Void) throws {
        let path = try FileLockBundle.normalize(original, creating: true)
        var info = stat()
        if path.withCString({ lstat($0, &info) }) == 0 {
            throw JedError("58P02", "database file already exists: \(path)")
        }
        guard errno == ENOENT else { throw io("inspect backup destination") }
        let temporary = path + ".jedtmp"
        // O_EXCL prevents two copies to one destination from sharing an inode.
        // An existing temp file belongs to its creator and is not removed here.
        let store = try UnixBlockStore(path: temporary, mode: .createExclusive,
                                      durability: skipFsync ? .unsynced : .durable)
        var published = false, complete = false
        defer {
            try? store.close()
            if !complete {
                _ = (published ? path : temporary).withCString { unlink($0) }
                if published { try? FileDatabaseHost.syncDirectory(path, skip: skipFsync) }
            }
        }
        try body(store)
        try store.sync()
        try store.close()
        try checkRole()
        let result = temporary.withCString { source in path.withCString { target in
            jed_host_rename_noreplace(source, target)
        } }
        guard result == 0 else {
            if errno == EEXIST { throw JedError("58P02", "database file already exists: \(path)") }
            if errno == ENOSYS || errno == EOPNOTSUPP || errno == EINVAL {
                throw JedError("0A000", "atomic no-replace file publication is unavailable")
            }
            throw io("publish backup file")
        }
        published = true
        try FileDatabaseHost.syncDirectory(path, skip: skipFsync)
        try checkRole()
        complete = true
    }
    private static func io(_ operation: String) -> JedError {
        JedError("58030", "\(operation) failed (errno \(errno))")
    }
}
#endif
