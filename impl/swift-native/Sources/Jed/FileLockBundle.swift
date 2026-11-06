#if os(Linux) || canImport(Darwin)
import Foundation
import Synchronization
#if os(Linux)
import Glibc
#else
import Darwin
#endif

/// The kernel lock on the stable presence inode is the sole authority. Bundle
/// contents are diagnostics/protocol metadata; they never establish liveness.
internal final class FileLockBundle: Sendable {
    private struct State {
        var directory: Int32; var presence: Int32; var arrival: Int32
        var ownsPresence = false; var closed = false
    }
    private let state: Mutex<State>
    private let opener: pid_t
    private let ownedClaim: FilePathClaim?
    let path: String
    private init(path: String, directory: Int32, presence: Int32, arrival: Int32, ownedClaim: FilePathClaim?) {
        self.path = path; opener = getpid(); self.ownedClaim = ownedClaim
        state = Mutex(State(directory: directory, presence: presence, arrival: arrival))
    }
    private static func io(_ operation: String) -> JedError { JedError("58030", "\(operation) failed (errno \(errno))") }
    private static func closeFD(_ fd: Int32) {
        guard fd >= 0 else { return }
        #if os(Linux)
        _ = Glibc.close(fd)
        #else
        _ = Darwin.close(fd)
        #endif
    }
    static func normalize(_ path: String, creating: Bool) throws -> String {
        guard !path.isEmpty, !path.utf8.contains(0) else { throw JedError("58030", "invalid database path") }
        let url = URL(fileURLWithPath: path)
        let resolving = creating ? url.deletingLastPathComponent().path : url.path
        let resolved = resolving.withCString { realpath($0, nil) }
        guard let resolved else {
            if !creating && errno == ENOENT { throw JedError("58P01", "database file does not exist: \(path)") }
            throw io("resolve database path")
        }
        defer { free(resolved) }
        let parent = String(cString: resolved)
        return creating ? URL(fileURLWithPath: parent).appendingPathComponent(url.lastPathComponent).path : parent
    }
    static func checkHardLinks(_ path: String) throws {
        var info = stat()
        guard path.withCString({ stat($0, &info) }) == 0 else {
            if errno == ENOENT { throw JedError("58P01", "database file does not exist: \(path)") }
            throw io("inspect database identity")
        }
        guard info.st_nlink == 1 else { throw JedError("55006", "hard-linked database paths cannot use jed locking: \(path)") }
    }
    private static func regular(_ name: String, directory: Int32) throws -> Int32 {
        let made = name.withCString { openat(directory, $0, O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC | O_NOFOLLOW, mode_t(0o644)) }
        if made >= 0 { closeFD(made) }
        else if errno != EEXIST { throw io("create coordination entry") }
        let fd = name.withCString { openat(directory, $0, O_RDONLY | O_CLOEXEC | O_NOFOLLOW | O_NONBLOCK) }
        guard fd >= 0 else { throw io("open coordination entry") }
        var info = stat()
        guard fstat(fd, &info) == 0, info.st_mode & mode_t(S_IFMT) == mode_t(S_IFREG) else {
            closeFD(fd); throw JedError("58030", "coordination entry is not a regular file")
        }
        return fd
    }
    private static func validateMarker(_ path: String, directory: Int32) throws {
        let entries: [String]
        do { entries = try FileManager.default.contentsOfDirectory(atPath: path + ".lock") }
        catch { throw JedError("58030", "cannot read coordination directory") }
        for entry in entries.sorted() where entry.hasPrefix("protocol-v") && entry != "protocol-v1" && entry != "protocol-v2" {
            throw JedError("0A000", "unsupported coordination protocol marker \(entry); supported: protocol-v2")
        }
        closeFD(try regular("protocol-v2", directory: directory))
    }
    /// Record bytes locate the endpoint and improve diagnostics; only presence
    /// decides ownership. Refuse non-regular entries without blocking on a FIFO.
    static func readOwner(_ path: String) -> ServiceOwnerRecord? {
        let name = path + ".lock/owner"
        let fd = name.withCString { open($0, O_RDONLY | O_CLOEXEC | O_NOFOLLOW | O_NONBLOCK) }
        guard fd >= 0 else { return nil }; defer { closeFD(fd) }
        var info = stat()
        guard fstat(fd, &info) == 0, info.st_mode & mode_t(S_IFMT) == mode_t(S_IFREG),
              info.st_size >= 43, info.st_size <= 65_536 else { return nil }
        var bytes = [UInt8](repeating: 0, count: Int(info.st_size))
        let success = bytes.withUnsafeMutableBufferPointer { bytes -> Bool in
            var position = 0
            while position < bytes.count {
                let count = read(fd, bytes.baseAddress!.advanced(by: position), bytes.count - position)
                if count < 0 && errno == EINTR { continue }
                if count <= 0 { return false }; position += count
            }
            return true
        }
        return success ? ServiceOwnerRecord.decode(bytes) : nil
    }
    static func ownerDetail(_ path: String) -> String {
        guard let record = readOwner(path) else { return "" }
        if record.uid != UInt32(getuid()) { return " (owned by another user's process)" }
        return record.serving ? "" : " (owned by a non-serving process)"
    }
    private static func lock(_ fd: Int32, exclusive: Bool, deadline: ContinuousClock.Instant, path: String) throws {
        while true {
            if flock(fd, (exclusive ? LOCK_EX : LOCK_SH) | LOCK_NB) == 0 { return }
            let failure = errno
            if failure == EINTR { continue }
            if failure == ENOSYS || failure == EOPNOTSUPP { throw JedError("0A000", "OS file locks are unavailable") }
            guard failure == EWOULDBLOCK || failure == EAGAIN else { throw io("acquire file lock") }
            guard ContinuousClock.now < deadline else { throw JedError("55006", "database file is in use: \(path)\(ownerDetail(path))") }
            Thread.sleep(forTimeInterval: 0.002)
        }
    }
    /// Prepare once for an election. Arrival SH remains held until ownership
    /// is won or the client closes these handles after completing hello.
    static func prepare(path: String, timeoutMilliseconds: Int64,
                        claim external: FilePathClaim? = nil) throws -> FileLockBundle {
        guard timeoutMilliseconds >= 0 else { throw JedError("22023", "negative file lock timeout") }
        let claim = try external ?? FilePathClaim.acquire(path)
        try claim.checkOpen()
        guard claim.path == path else { throw JedError("55006", "path claim does not match database") }
        var directory: Int32 = -1, presence: Int32 = -1, arrival: Int32 = -1
        do {
            let bundle = path + ".lock"
            if bundle.withCString({ mkdir($0, mode_t(0o755)) }) != 0 && errno != EEXIST { throw io("create coordination directory") }
            directory = bundle.withCString { open($0, O_RDONLY | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW) }
            guard directory >= 0 else { throw io("open coordination directory") }
            try validateMarker(path, directory: directory)
            presence = try regular("presence", directory: directory)
            arrival = try regular("arrival", directory: directory)
            let deadline = ContinuousClock.now.advanced(by: .milliseconds(timeoutMilliseconds))
            try lock(arrival, exclusive: false, deadline: deadline, path: path)
            try validateMarker(path, directory: directory)
            return FileLockBundle(path: path, directory: directory, presence: presence, arrival: arrival,
                ownedClaim: external == nil ? claim : nil)
        } catch {
            closeFD(arrival); closeFD(presence); closeFD(directory)
            if external == nil { claim.close() }; throw error
        }
    }
    func tryOwnership() throws -> Bool {
        try state.withLock { state in
            guard !state.closed, getpid() == opener else { throw JedError("55006", "coordination handle is closed or inherited") }
            if state.ownsPresence { return true }
            while true {
                if flock(state.presence, LOCK_EX | LOCK_NB) == 0 {
                    state.ownsPresence = true; _ = flock(state.arrival, LOCK_UN); return true
                }
                if errno == EINTR { continue }
                if errno == EWOULDBLOCK || errno == EAGAIN { return false }
                if errno == ENOSYS || errno == EOPNOTSUPP { throw JedError("0A000", "OS file locks are unavailable") }
                throw Self.io("acquire file ownership")
            }
        }
    }
    static func acquireExclusive(path: String, timeoutMilliseconds: Int64,
                                 random: any RandomSource = OSRandomSource(),
                                 claim: FilePathClaim? = nil) throws -> FileLockBundle {
        let deadline = ContinuousClock.now.advanced(by: .milliseconds(timeoutMilliseconds))
        let bundle = try prepare(path: path, timeoutMilliseconds: timeoutMilliseconds, claim: claim)
        do {
            while try !bundle.tryOwnership() {
                guard ContinuousClock.now < deadline else { throw JedError("55006", "database file is in use: \(path)\(ownerDetail(path))") }
                Thread.sleep(forTimeInterval: 0.002)
            }
            var incarnation: [UInt8]
            repeat {
                incarnation = try random.bytes(count: 16)
                guard incarnation.count == 16 else { throw JedError("58030", "invalid owner incarnation entropy") }
            } while incarnation.allSatisfy({ $0 == 0 })
            try bundle.writeOwner(ServiceOwnerRecord(serving: false, uid: UInt32(getuid()), incarnation: incarnation,
                pid: UInt32(getpid()), endpoint: ""))
            return bundle
        } catch { bundle.close(); throw error }
    }
    func writeOwner(_ record: ServiceOwnerRecord) throws {
        let bytes = try record.encode(), digits = Array("0123456789abcdef".utf8)
        let suffix = String(decoding: record.incarnation.flatMap { [digits[Int($0 >> 4)], digits[Int($0 & 15)]] }, as: UTF8.self)
        let name = "owner.tmp-" + suffix
        try state.withLock { state in
            guard !state.closed, state.ownsPresence, getpid() == opener else { throw JedError("55006", "owner record requires file ownership") }
            let fd = name.withCString { openat(state.directory, $0, O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC | O_NOFOLLOW, mode_t(0o644)) }
            guard fd >= 0 else { throw Self.io("create owner record") }
            defer { Self.closeFD(fd); _ = name.withCString { unlinkat(state.directory, $0, 0) } }
            try bytes.withUnsafeBufferPointer { bytes in
                var written = 0
                while written < bytes.count {
                    let count = write(fd, bytes.baseAddress!.advanced(by: written), bytes.count - written)
                    if count < 0 && errno == EINTR { continue }
                    guard count > 0 else { throw Self.io("write owner record") }
                    written += count
                }
            }
            guard name.withCString({ name in "owner".withCString { renameat(state.directory, name, state.directory, $0) } }) == 0 else {
                throw Self.io("publish owner record")
            }
        }
    }
    func checkOpen() throws {
        try state.withLock { state in
            guard !state.closed else { throw JedError("55000", "database file ownership is closed") }
            guard getpid() == opener else { throw JedError("55006", "database handle was inherited across fork; reopen it") }
        }
    }
    /// Caller must stop service and close the database descriptor first. Never
    /// explicitly unlock an inherited flock: it shares the parent's description.
    func close() {
        state.withLock { state in
            if state.closed { return }; state.closed = true
            if getpid() == opener, state.ownsPresence { _ = "owner".withCString { unlinkat(state.directory, $0, 0) } }
            Self.closeFD(state.arrival); Self.closeFD(state.presence); Self.closeFD(state.directory)
            state.arrival = -1; state.presence = -1; state.directory = -1
            if getpid() == opener { ownedClaim?.close() }
        }
    }
    deinit { close() }
}
#endif
