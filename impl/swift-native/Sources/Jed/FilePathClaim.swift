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
/// Counts forks of this process. A child's fork handler bumps the count, so a
/// handle can tell it was inherited by comparing against the count it was
/// born under, without a system call on every statement.
internal enum ForkWatch {
    private static let forks = Atomic<Int>(0)
    private static let installed: Bool = pthread_atfork(nil, nil, { ForkWatch.forked() }) == 0
    private static func forked() { forks.add(1, ordering: .relaxed) }
    /// Nil when no handler could be installed; callers then compare pids.
    static var generation: Int? { installed ? forks.load(ordering: .relaxed) : nil }
}

internal final class FilePathClaim: Sendable {
    private struct Registry { var pid = getpid(); var paths: Set<String> = [] }
    private static let registry = Mutex(Registry())
    private let closed = Mutex(false)
    private let opener: pid_t
    let path: String
    private let born = ForkWatch.generation
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
        let inherited = born.map { $0 != ForkWatch.generation } ?? (getpid() != opener)
        guard !closed.withLock({ $0 }), !inherited else {
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
