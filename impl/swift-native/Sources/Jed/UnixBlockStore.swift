#if os(Linux) || canImport(Darwin)
#if os(Linux)
import Glibc
#else
import Darwin
#endif
import Synchronization

/// Narrow POSIX adapter. This byte device does not acquire database locks:
/// the owner elected by the lock-bundle protocol is its only database caller.
/// Every pointer is a synchronous borrow of a Swift-owned buffer; POSIX retains
/// none. No descriptor or pointer escapes this type.
internal final class UnixBlockStore: BlockStore {
    enum Mode { case readOnly, readWrite, createExclusive }
    private let descriptor: Mutex<Int32>

    init(path: String, mode: Mode) throws {
        guard !path.utf8.contains(0) else { throw JedError("58030", "file path contains NUL") }
        let flags: Int32
        switch mode {
        case .readOnly: flags = O_RDONLY | O_CLOEXEC
        case .readWrite: flags = O_RDWR | O_CLOEXEC
        case .createExclusive: flags = O_RDWR | O_CREAT | O_EXCL | O_CLOEXEC
        }
        let fd = path.withCString { open($0, flags, mode_t(0o600)) }
        guard fd >= 0 else { throw Self.ioError("open") }
        descriptor = Mutex(fd)
    }

    deinit {
        descriptor.withLock { fd in
            if fd >= 0 { _ = close(fd); fd = -1 }
        }
    }

    private static func ioError(_ operation: String) -> JedError {
        let code = errno
        return JedError("58030", "\(operation) failed (errno \(code))")
    }

    private static func valid(_ fd: Int32) throws {
        guard fd >= 0 else { throw JedError("58030", "byte device is closed") }
    }

    private static func length(_ fd: Int32) throws -> UInt64 {
        var info = stat()
        guard fstat(fd, &info) == 0 else { throw ioError("fstat") }
        guard info.st_size >= 0 else { throw JedError("58030", "negative file size") }
        return UInt64(info.st_size)
    }

    private static func write(_ fd: Int32, at offset: UInt64, bytes: [UInt8]) throws {
        guard offset <= UInt64(Int64.max), UInt64(bytes.count) <= UInt64(Int64.max) - offset else {
            throw JedError("58030", "write offset exceeds supported file size")
        }
        try bytes.withUnsafeBufferPointer { buffer in
            var written = 0
            while written < buffer.count {
                let count = pwrite(fd, buffer.baseAddress!.advanced(by: written), buffer.count - written, off_t(offset + UInt64(written)))
                if count < 0 && errno == EINTR { continue }
                guard count > 0 else { throw ioError("pwrite") }
                written += count
            }
        }
    }

    public func read(at offset: UInt64, count: Int) throws -> [UInt8] {
        try descriptor.withLock { fd in
            try Self.valid(fd)
            guard count >= 0, offset <= UInt64(Int64.max), UInt64(count) <= UInt64(Int64.max) - offset else {
                throw JedError("58030", "read offset exceeds supported file size")
            }
            let size = try Self.length(fd)
            guard offset <= size, UInt64(count) <= size - offset else { throw JedError("58030", "short file read") }
            var result = [UInt8](repeating: 0, count: count)
            try result.withUnsafeMutableBufferPointer { buffer in
                var received = 0
                while received < count {
                    let length = pread(fd, buffer.baseAddress!.advanced(by: received), count - received, off_t(offset + UInt64(received)))
                    if length < 0 && errno == EINTR { continue }
                    guard length > 0 else { throw Self.ioError("pread") }
                    received += length
                }
            }
            return result
        }
    }

    public func write(at offset: UInt64, bytes: [UInt8]) throws {
        try descriptor.withLock { fd in
            try Self.valid(fd)
            try Self.write(fd, at: offset, bytes: bytes)
        }
    }

    public func size() throws -> UInt64 {
        try descriptor.withLock { fd in try Self.valid(fd); return try Self.length(fd) }
    }

    public func sync() throws {
        try descriptor.withLock { fd in
            try Self.valid(fd)
            while true {
                #if os(Linux)
                let result = fdatasync(fd)
                #else
                let result = fsync(fd)
                #endif
                if result == 0 { return }
                if errno != EINTR { throw Self.ioError("data barrier") }
            }
        }
    }

    public func setSize(_ bytes: UInt64) throws {
        try descriptor.withLock { fd in
            try Self.valid(fd)
            guard bytes <= UInt64(Int64.max) else { throw JedError("58030", "file size exceeds supported file size") }
            let previous = try Self.length(fd)
            if bytes > previous {
                // Real zero writes allocate blocks before the metadata barrier;
                // preallocation chunk policy belongs to the pager.
                let zeros = [UInt8](repeating: 0, count: 65_536)
                var position = previous
                while position < bytes {
                    let count = Int(min(UInt64(zeros.count), bytes - position))
                    try Self.write(fd, at: position, bytes: count == zeros.count ? zeros : Array(zeros[..<count]))
                    position += UInt64(count)
                }
                while fsync(fd) != 0 { if errno != EINTR { throw Self.ioError("metadata barrier") } }
            } else if bytes < previous {
                while ftruncate(fd, off_t(bytes)) != 0 { if errno != EINTR { throw Self.ioError("truncate") } }
            }
        }
    }
}
#endif
