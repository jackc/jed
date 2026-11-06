#if os(Linux) || canImport(Darwin)
import Synchronization
#if os(Linux)
import Glibc
#else
import Darwin
#endif

/// Process-local duplicate-open exclusion outlives role changes. The registry
/// is diagnostic coordination within one process; the kernel presence lock is
/// the only authority between processes.
internal final class FilePathClaim: Sendable {
    private struct Registry { var pid = getpid(); var paths: Set<String> = [] }
    private static let registry = Mutex(Registry())
    private let closed = Mutex(false)
    private let opener: pid_t
    let path: String
    private init(path: String) { self.path = path; opener = getpid() }
    static func acquire(_ path: String) throws -> FilePathClaim {
        try registry.withLock { state in
            if state.pid != getpid() { state = Registry() }
            guard state.paths.insert(path).inserted else {
                throw JedError("55006", "database is already open in this process: \(path) (share one Database handle)")
            }
        }
        return FilePathClaim(path: path)
    }
    func checkOpen() throws {
        guard !closed.withLock({ $0 }), getpid() == opener else {
            throw JedError("55006", "database path claim is closed or inherited across fork")
        }
    }
    func close() {
        let release = closed.withLock { value in let previous = value; value = true; return !previous }
        guard release, getpid() == opener else { return }
        Self.registry.withLock { _ = $0.paths.remove(path) }
    }
    deinit { close() }
}
#endif
