import Synchronization

/// Opaque byte device, not a database or pager. Short reads/writes are errors.
/// `sync` makes in-region writes durable; growing `setSize` durably allocates
/// zeros. Page policy, locking, encryption and commit ordering live above this.
public protocol BlockStore: AnyObject, Sendable {
    func read(at offset: UInt64, count: Int) throws -> [UInt8]
    func write(at offset: UInt64, bytes: [UInt8]) throws
    func sync() throws
    func size() throws -> UInt64
    func setSize(_ bytes: UInt64) throws
}

/// Pure memory byte backing. Sparse internal chunks keep resizing independent
/// of any particular database page size; committed reads still use the pager.
public final class MemoryBlockStore: BlockStore {
    private struct State {
        var size: UInt64 = 0
        var chunks: [UInt64: [UInt8]] = [:]
    }
    private static let chunkSize = 8192
    private let state = Mutex(State())
    public init() {}

    public func size() throws -> UInt64 { state.withLock { $0.size } }
    public func sync() throws {}

    public func read(at offset: UInt64, count: Int) throws -> [UInt8] {
        try state.withLock { state in
            guard count >= 0, offset <= state.size, UInt64(count) <= state.size - offset else {
                throw JedError("58030", "short memory-store read")
            }
            var result = [UInt8](repeating: 0, count: count)
            var copied = 0
            while copied < count {
                let position = offset + UInt64(copied)
                let chunk = position / UInt64(Self.chunkSize)
                let within = Int(position % UInt64(Self.chunkSize))
                let length = min(count - copied, Self.chunkSize - within)
                if let bytes = state.chunks[chunk] {
                    result.replaceSubrange(copied..<(copied + length), with: bytes[within..<(within + length)])
                }
                copied += length
            }
            return result
        }
    }

    public func write(at offset: UInt64, bytes: [UInt8]) throws {
        try state.withLock { state in
            guard offset <= UInt64(Int64.max), UInt64(bytes.count) <= UInt64(Int64.max) - offset else {
                throw JedError("58030", "memory-store offset exceeds supported file size")
            }
            guard !bytes.isEmpty else { return }
            var copied = 0
            while copied < bytes.count {
                let position = offset + UInt64(copied)
                let chunk = position / UInt64(Self.chunkSize)
                let within = Int(position % UInt64(Self.chunkSize))
                let length = min(bytes.count - copied, Self.chunkSize - within)
                var body = state.chunks.removeValue(forKey: chunk) ?? [UInt8](repeating: 0, count: Self.chunkSize)
                body.replaceSubrange(within..<(within + length), with: bytes[copied..<(copied + length)])
                state.chunks[chunk] = body
                copied += length
            }
            state.size = max(state.size, offset + UInt64(bytes.count))
        }
    }

    public func setSize(_ bytes: UInt64) throws {
        guard bytes <= Int64.max else { throw JedError("58030", "memory-store size exceeds supported file size") }
        state.withLock { state in
            if bytes < state.size {
                let whole = bytes / UInt64(Self.chunkSize)
                let tail = Int(bytes % UInt64(Self.chunkSize))
                let discarded = state.chunks.keys.filter { $0 > whole || ($0 == whole && tail == 0) }
                for chunk in discarded { state.chunks.removeValue(forKey: chunk) }
                if tail > 0, var body = state.chunks.removeValue(forKey: whole) {
                    for index in tail..<Self.chunkSize { body[index] = 0 }
                    state.chunks[whole] = body
                }
            }
            state.size = bytes
        }
    }
}
