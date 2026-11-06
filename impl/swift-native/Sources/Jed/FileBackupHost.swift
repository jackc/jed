#if os(Linux) || canImport(Darwin)
import Foundation
import CJedHost
#if os(Linux)
import Glibc
#else
import Darwin
#endif

/// Destination publication policy. The engine supplies pages and retains its
/// source pin; this host owns the temporary inode and atomic publication.
internal enum FileBackupHost {
    static func write(destination original: String, skipFsync: Bool,
                      operations: any FileOperations = NativeFileOperations(),
                      checkRole: () throws -> Void,
                      body: (any FileBlockStore) throws -> Void) throws {
        let path = try operations.resolve(original, creating: true)
        if try operations.exists(path) {
            throw JedError("58P02", "database file already exists: \(path)")
        }
        let temporary = path + ".jedtmp"
        // O_EXCL prevents two copies to one destination from sharing an inode.
        // An existing temp file belongs to its creator and is not removed here.
        let store = try operations.create(temporary, skipFsync: skipFsync)
        var published = false, complete = false
        defer {
            try? store.close()
            if !complete {
                try? operations.remove(published ? path : temporary)
                if published { try? operations.syncDirectory(path, skipFsync: skipFsync) }
            }
        }
        try body(store)
        try store.sync()
        try store.close()
        try checkRole()
        try operations.rename(temporary, to: path, replace: false)
        published = true
        try operations.syncDirectory(path, skipFsync: skipFsync)
        try checkRole()
        complete = true
    }
}
#endif
