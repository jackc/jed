import Dispatch
import Synchronization

/// Only clean packed leaves enter this cache. Physical hits and eviction never
/// charge SQL cost. A traversal owns its immutable leaf after cache eviction.
internal final class NodePool: Sendable {
    private final class Loading: Sendable {
        let done = DispatchGroup()
        let result = Mutex<Result<PackedLeaf, any Error>?>(nil)
        init() { done.enter() }
        func finish(_ value: Result<PackedLeaf, any Error>) {
            result.withLock { $0 = value }; done.leave()
        }
        func wait() throws -> PackedLeaf {
            done.wait()
            return try result.withLock { try $0!.get() }
        }
    }
    private struct Identity: Hashable {
        let index: UInt32
        let tag: [UInt8]
        init(_ reference: PageReference) { index = reference.index; tag = reference.tag }
    }
    private struct Entry {
        let leaf: PackedLeaf
        var referenced: Bool
        var slot: Int
    }
    private struct State {
        var entries: [Identity: Entry] = [:]
        var slots: [Identity] = []
        var hand = 0
        var loading: [Identity: Loading] = [:]
    }
    private enum Lookup {
        case hit(PackedLeaf)
        case wait(Loading)
        case load(Loading)
    }
    let codec: any PageCodec
    let pager: Pager?
    let capacity: Int?
    private let read: @Sendable (PageReference) throws -> [UInt8]
    private let state = Mutex(State())

    init(codec: any PageCodec, cacheBytes: Int?, pager: Pager? = nil,
         read: @escaping @Sendable (PageReference) throws -> [UInt8]) throws {
        guard cacheBytes.map({ $0 >= 0 }) ?? true else { throw JedError("22023", "negative cache size") }
        self.codec = codec; self.pager = pager; self.read = read
        capacity = cacheBytes.map { max(1, $0 / codec.pageSize) }
    }

    convenience init(pager: Pager, cacheBytes: Int?) throws {
        try self.init(codec: pager.codec, cacheBytes: cacheBytes, pager: pager, read: pager.readBody)
    }

    var residentLeaves: Int { state.withLock { $0.entries.count } }
    func readBody(_ reference: PageReference) throws -> [UInt8] { try read(reference) }

    func load(_ reference: PageReference, types: [TypeDescriptor]) throws -> PackedLeaf {
        let identity = Identity(reference)
        let lookup = try state.withLock { state -> Lookup in
            if var entry = state.entries[identity] {
                guard entry.leaf.types == types else { throw JedError.corruption("leaf reused under a different row type") }
                entry.referenced = true; state.entries[identity] = entry
                return .hit(entry.leaf)
            }
            if let loading = state.loading[identity] { return .wait(loading) }
            let loading = Loading(); state.loading[identity] = loading
            return .load(loading)
        }
        switch lookup {
        case .hit(let leaf): return leaf
        case .wait(let loading):
            let leaf = try loading.wait()
            guard leaf.types == types else { throw JedError.corruption("leaf reused under a different row type") }
            return leaf
        case .load(let loading):
            // No cache mutex across device I/O, authentication, or PAX parsing.
            let result = Result { try PackedLeaf(bytes: read(reference), types: types, codec: codec) }
            state.withLock { state in
                // Invalidation detaches old flights before page reuse. Existing
                // waiters still receive the old immutable result; it cannot reenter.
                guard state.loading[identity] === loading else { return }
                state.loading.removeValue(forKey: identity)
                if case .success(let leaf) = result {
                    insert(leaf, identity: identity, state: &state)
                }
            }
            loading.finish(result)
            return try result.get()
        }
    }

    /// The owner calls this before reusing page addresses. Reader-watermark
    /// safety is independently enforced by allocation; eviction is not a pin.
    func invalidate(_ pages: Set<UInt32>) {
        state.withLock { state in
            for identity in state.entries.keys.filter({ pages.contains($0.index) }) {
                remove(identity, state: &state)
            }
            for identity in state.loading.keys.filter({ pages.contains($0.index) }) {
                state.loading.removeValue(forKey: identity)
            }
        }
    }

    func invalidateAll() { state.withLock { $0 = State() } }

    private func insert(_ leaf: PackedLeaf, identity: Identity, state: inout State) {
        if let capacity, state.entries.count >= capacity {
            while !state.slots.isEmpty {
                state.hand %= state.slots.count
                let candidate = state.slots[state.hand]
                if state.entries[candidate]!.referenced {
                    state.entries[candidate]!.referenced = false; state.hand += 1
                } else { remove(candidate, state: &state); break }
            }
        }
        state.entries[identity] = Entry(leaf: leaf, referenced: true, slot: state.slots.count)
        state.slots.append(identity)
    }

    private func remove(_ identity: Identity, state: inout State) {
        guard let entry = state.entries.removeValue(forKey: identity) else { return }
        let last = state.slots.removeLast()
        if entry.slot < state.slots.count {
            state.slots[entry.slot] = last; state.entries[last]!.slot = entry.slot
        }
        state.hand = state.slots.isEmpty ? 0 : state.hand % state.slots.count
    }
}
