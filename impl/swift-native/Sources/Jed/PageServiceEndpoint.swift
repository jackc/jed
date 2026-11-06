#if os(Linux)
import Foundation
import Glibc

/// Endpoint policy is host configuration, never SQL input. Permission checks
/// use lstat so a symlink cannot redirect the private directory boundary.
internal enum PageServiceEndpoint {
    private static func privateDirectory(_ path: String, create: Bool) -> Bool {
        guard !path.isEmpty, !path.utf8.contains(0) else { return false }
        if create, path.withCString({ mkdir($0, mode_t(0o700)) }) != 0, errno != EEXIST { return false }
        var info = stat()
        guard path.withCString({ lstat($0, &info) }) == 0,
              info.st_mode & mode_t(S_IFMT) == mode_t(S_IFDIR),
              info.st_uid == geteuid(), info.st_mode & 0o7777 == 0o700 else { return false }
        return true
    }
    static func choose(database: String, incarnation: [UInt8]) -> String? {
        guard incarnation.count == 16 else { return nil }
        let bundled = database + ".lock/uid-\(geteuid())", socket = bundled + "/endpoint"
        if socket.utf8.count <= PageServiceProtocol.endpointMaxBytes, privateDirectory(bundled, create: true) { return socket }
        let digits = Array("0123456789abcdef".utf8)
        let suffix = String(decoding: incarnation.prefix(8).flatMap { [digits[Int($0 >> 4)], digits[Int($0 & 15)]] }, as: UTF8.self)
        let environment = ProcessInfo.processInfo.environment
        var directories: [(String, Bool)] = []
        if let runtime = environment["XDG_RUNTIME_DIR"] { directories.append((runtime, false)) }
        directories.append(("/tmp/jed-\(geteuid())", true))
        for (directory, create) in directories {
            guard privateDirectory(directory, create: create) else { continue }
            let endpoint = directory + "/jed-" + suffix + ".sock"
            // The first acceptable private directory owns the fallback choice;
            // a too-long result makes this owner non-serving.
            return endpoint.utf8.count <= PageServiceProtocol.endpointMaxBytes ? endpoint : nil
        }
        return nil
    }
    static func remove(_ path: String) { _ = path.withCString { unlink($0) } }
}
#endif
