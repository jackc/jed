import Synchronization

internal protocol ScratchStore: BlockStore {
    func close() throws
}
internal protocol ScratchFileHost: Sendable {
    var sealed: Bool { get }
    func create(kind: String) throws -> any ScratchStore
}

/// Storage migration changes only residency. The caller evaluates the threshold
/// at a completed private stage, using logical committed page bytes. Copying is
/// bounded and the original byte device stays installed until the copy succeeds.
internal final class SpillableScratchStore: ScratchStore {
    private struct State {
        var backing: any BlockStore = MemoryBlockStore()
        var scratch: (any ScratchStore)?
        var closed = false
    }
    private let state = Mutex(State())
    private let host: (any ScratchFileHost)?
    private let budget: UInt64
    init(budget: Int64, host: (any ScratchFileHost)?) throws {
        guard budget >= 0 else { throw JedError("22023", "negative scratch budget") }
        self.budget = UInt64(budget); self.host = host
    }
    var spilled: Bool { state.withLock { $0.scratch != nil } }
    private func check(_ state: State) throws {
        guard !state.closed else { throw JedError("58030", "scratch storage is closed") }
    }
    func spillIfNeeded(committedBytes: UInt64) throws {
        try state.withLock { state in
            try check(state)
            guard state.scratch == nil, budget > 0, committedBytes > budget, let host else { return }
            let target = try host.create(kind: "temp")
            do {
                let length = try state.backing.size()
                try target.setSize(length)
                var offset: UInt64 = 0
                while offset < length {
                    let count = Int(min(65_536, length - offset))
                    try target.write(at: offset, bytes: state.backing.read(at: offset, count: count))
                    offset += UInt64(count)
                }
                state.backing = target; state.scratch = target
            } catch { try? target.close(); throw error }
        }
    }
    func read(at offset: UInt64, count: Int) throws -> [UInt8] {
        try state.withLock { try check($0); return try $0.backing.read(at: offset, count: count) }
    }
    func write(at offset: UInt64, bytes: [UInt8]) throws {
        try state.withLock { try check($0); try $0.backing.write(at: offset, bytes: bytes) }
    }
    func size() throws -> UInt64 { try state.withLock { try check($0); return try $0.backing.size() } }
    func setSize(_ bytes: UInt64) throws { try state.withLock { try check($0); try $0.backing.setSize(bytes) } }
    func sync() throws { try state.withLock { try check($0) } }
    func close() throws {
        try state.withLock { state in
            if state.closed { return }; state.closed = true
            try state.scratch?.close()
            state.scratch = nil; state.backing = MemoryBlockStore()
        }
    }
    deinit { try? close() }
}
