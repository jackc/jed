import Synchronization

internal struct StoredPage: Sendable {
    let index: UInt32
    let bytes: [UInt8]
}

/// Completed serialization, including encryption and manifest reservation, is
/// separate from the storage phase. Constructing this value performs no writes.
internal struct PreparedPageCommit: Sendable {
    let baseTxid: UInt64
    let body: [StoredPage]
    let manifest: CommitManifest
    let metaBytes: [UInt8]
    let slot: UInt32
    let freePages: [UInt32]
    let entries: [ManifestEntry]
}

internal enum CommitFault: Sendable {
    case body(ordinal: UInt32, tearBytes: Int)
    case meta(tearBytes: Int)
    case sync(ordinal: UInt32)
}

/// Host-independent page I/O, recovery and commit publication. A database's
/// single writer supplies immutable prepared pages; readers retain their roots.
/// This mutex serializes physical reads against the short storage commit. Tree
/// decoding and expression execution happen outside it.
internal final class Pager: Sendable {
    private struct State {
        var store: any BlockStore
        var allocatedPages: UInt32
        var selected: ValidatedMeta
        var freePages: [UInt32]
        var floorFree: Set<UInt32> = []
        var pending: [UInt32: UInt64] = [:]
        var floorSlot: UInt32
        var recoverySync = true
        var directorySync = true
        var poisoned = false
        var revoked = false
        var shrinkPending = false
        var shrinkPages: UInt32 = 0
        var fault: CommitFault?
        var bodyWrites: UInt32 = 0
        var syncs: UInt32 = 0
        var keyBlock: KeyBlockSelection?
    }
    let pageSize: Int
    let codec: any PageCodec
    private let state: Mutex<State>

    private init(store: any BlockStore, selected: ValidatedMeta, codec: any PageCodec,
                 freePages: [UInt32], keyBlock: KeyBlockSelection?, allocatedPages: UInt32) {
        pageSize = selected.meta.pageSize
        self.codec = codec
        state = Mutex(State(store: store, allocatedPages: allocatedPages, selected: selected,
            freePages: freePages, floorSlot: selected.floorSlot, keyBlock: keyBlock))
    }

    static func open(store: any BlockStore, credential: Credential? = nil,
                     writable: Bool = false, syncDirectory: () throws -> Void = {}) throws -> Pager {
        let selected = try MetaRecovery.open(store)
        try EncryptionPolicy.checkCredential(encrypted: selected.meta.encrypted, credential: credential)
        let size = selected.meta.pageSize
        let codec: any PageCodec
        var keyBlock: KeyBlockSelection?
        if selected.meta.encrypted {
            let copy2 = try store.read(at: UInt64(size) * 2, count: size)
            let copy3 = try store.read(at: UInt64(size) * 3, count: size)
            let block = try KeyBlock.select(page2: copy2, page3: copy3, pageSize: size)
            let encrypted = try EncryptedPageCodec(pageSize: size, dataKey: block.block.unlock(credential), opening: true)
            // Structural fallback is finished. Authentication errors propagate.
            var chain: [ManifestPageBytes] = []
            for index in selected.chain {
                chain.append(ManifestPageBytes(pageIndex: index,
                    bytes: try store.read(at: UInt64(index) * UInt64(size), count: size)))
            }
            try encrypted.verifyMeta(selected.bytes, slot: selected.slot, manifest: chain)
            codec = encrypted; keyBlock = block
        } else { codec = try PlainPageCodec(pageSize: size, opening: true) }
        // Logical free-list corruption cannot trigger selection of an older root.
        let free = try FreeList.load(meta: selected.meta, protected: selected.protectedPages) { ref in
            try codec.openBody(pageIndex: ref.index,
                sealed: store.read(at: UInt64(ref.index) * UInt64(size), count: size), expectedTag: ref.tag)
        }
        let allocated = try store.size() / UInt64(size)
        guard allocated <= UInt32.max else { throw JedError.corruption("file exceeds the page-address range") }
        let pager = Pager(store: store, selected: selected, codec: codec, freePages: free.entries,
            keyBlock: keyBlock, allocatedPages: UInt32(allocated))
        if writable, keyBlock?.needsRepair == true { try pager.repairKeyBlock(syncDirectory: syncDirectory) }
        return pager
    }

    var selected: ValidatedMeta { state.withLock { $0.selected } }
    var pendingEntries: [ManifestEntry] {
        state.withLock { state in state.pending.keys.sorted().map { ManifestEntry(pageIndex: $0, digest: state.pending[$0]!) } }
    }
    var isPoisoned: Bool { state.withLock { $0.poisoned } }

    func readStored(_ index: UInt32) throws -> [UInt8] {
        try state.withLock { state in
            guard !state.revoked else { throw JedError("58030", "storage was revoked after a failed flush") }
            guard index < state.allocatedPages else { throw JedError.corruption("page address exceeds file bounds") }
            return try state.store.read(at: UInt64(index) * UInt64(pageSize), count: pageSize)
        }
    }

    func readBody(_ reference: PageReference) throws -> [UInt8] {
        try codec.openBody(pageIndex: reference.index, sealed: readStored(reference.index), expectedTag: reference.tag)
    }

    func checkWritable() throws { try state.withLock { try Self.checkWritable($0) } }
    private static func checkWritable(_ state: State) throws {
        guard !state.poisoned else { throw JedError("58030", "database writer requires reopen after failed commit") }
    }

    /// Reader-watermark policy supplies reuseAllowed. During a relaxed batch,
    /// only pages already free at its durable floor can be reused.
    func allocationState(reuseAllowed: Bool) throws -> (highWater: UInt32, free: [UInt32], slot: UInt32) {
        try state.withLock { state in
            try Self.checkWritable(state)
            let free = !reuseAllowed ? [] : state.freePages.filter { state.pending.isEmpty || state.floorFree.contains($0) }
            return (state.selected.meta.pageCount, free, 1 - state.floorSlot)
        }
    }

    func prepare(body: [StoredPage], root: PageReference, pageCount: UInt32, freeList: PageReference,
                 livePages: UInt32, freePages: [UInt32], manifestPages: [UInt32], reuseAllowed: Bool,
                 entropy: KeyEntropy? = nil) throws -> PreparedPageCommit {
        let snapshot = try state.withLock { state -> State in try Self.checkWritable(state); return state }
        guard snapshot.selected.meta.txid < UInt64.max else { throw JedError.corruption("transaction id exhausted") }
        let txid = snapshot.selected.meta.txid + 1
        let safe = Set(reuseAllowed ? snapshot.freePages.filter { snapshot.pending.isEmpty || snapshot.floorFree.contains($0) } : [])
        var identities = snapshot.pending
        let oldProtected = snapshot.selected.protectedPages
        var written: Set<UInt32> = []
        for page in body {
            guard page.index >= codec.firstBodyPage, page.index < pageCount, page.bytes.count == pageSize,
                  written.insert(page.index).inserted, identities[page.index] == nil,
                  page.index >= snapshot.selected.meta.pageCount || safe.contains(page.index),
                  !oldProtected.contains(page.index) else {
                throw JedError.corruption("unsafe or malformed prepared body page")
            }
            identities[page.index] = Checksums.crc64(page.bytes)
        }
        for index in manifestPages {
            guard index >= snapshot.selected.meta.pageCount || safe.contains(index),
                  !oldProtected.contains(index), snapshot.pending[index] == nil else {
                throw JedError.corruption("unsafe manifest page reuse")
            }
        }
        let entries = identities.keys.sorted().map { ManifestEntry(pageIndex: $0, digest: identities[$0]!) }
        let manifest = try CommitManifest.build(pageSize: pageSize, encrypted: codec.reservedBytes != 0,
            txid: txid, root: root, pageCount: pageCount, freeList: freeList, livePages: livePages,
            entries: entries, chain: manifestPages)
        let protected = Set(identities.keys).union(manifestPages)
        var previous: UInt32 = 0
        for index in freePages {
            guard index > previous, index >= codec.firstBodyPage, index < pageCount, !protected.contains(index) else {
                throw JedError.corruption("invalid prepared free list")
            }
            previous = index
        }
        guard snapshot.pending.isEmpty || pageCount >= snapshot.selected.meta.pageCount else {
            throw JedError.corruption("cannot lower high-water inside a pending batch")
        }
        let slot = 1 - snapshot.floorSlot
        var metaBytes = try manifest.meta.encode(inline: manifest.inline)
        if let encrypted = codec as? EncryptedPageCodec {
            guard let entropy else { throw JedError("58030", "encrypted commit requires key entropy") }
            metaBytes = try encrypted.sealMeta(metaBytes, slot: slot, manifest: manifest.pages,
                nonce: EncryptionPolicy.entropy(24, from: entropy))
        }
        return PreparedPageCommit(baseTxid: snapshot.selected.meta.txid, body: body,
            manifest: manifest, metaBytes: metaBytes, slot: slot, freePages: freePages, entries: entries)
    }

    /// A false synchronous setting publishes before its deferred barrier. The
    /// cap flush runs inline here, as in deterministic simulation. Its failure
    /// revokes subsequent reads but does not undo an already published commit.
    func commit(_ plan: PreparedPageCommit, synchronous: Bool = true,
                syncDirectory: () throws -> Void = {}) throws {
        try state.withLock { state in
            try Self.checkWritable(state)
            guard plan.baseTxid == state.selected.meta.txid, plan.slot == 1 - state.floorSlot else {
                throw JedError("55006", "prepared commit is based on an obsolete generation")
            }
            do {
                try stabilize(&state, syncDirectory: syncDirectory)
                if state.pending.isEmpty { state.floorFree = Set(state.freePages) }
                try reserve(plan.manifest.meta.pageCount, state: &state)
                for page in plan.body { try write(page.index, bytes: page.bytes, state: &state) }
                for page in plan.manifest.pages { try write(page.pageIndex, bytes: page.bytes, state: &state) }
                try write(plan.slot, bytes: plan.metaBytes, state: &state)
                if synchronous { try barrier(&state) }
                let meta = plan.manifest.meta
                state.shrinkPending = state.shrinkPending || meta.pageCount < state.selected.meta.pageCount
                state.shrinkPages = meta.pageCount
                state.selected = ValidatedMeta(meta: meta, slot: plan.slot,
                    floorSlot: synchronous ? plan.slot : state.floorSlot, bytes: plan.metaBytes,
                    entries: plan.entries, chain: plan.manifest.pages.map(\.pageIndex))
                state.freePages = plan.freePages
                if synchronous {
                    state.pending.removeAll(keepingCapacity: true); state.floorFree.removeAll(keepingCapacity: true)
                    state.floorSlot = plan.slot
                    shrinkAfterBarrier(&state)
                } else {
                    for page in plan.body { state.pending[page.index] = Checksums.crc64(page.bytes) }
                    if state.pending.count >= meta.inlineCapacity {
                        do { try flushState(&state, shrink: true) }
                        catch { state.poisoned = true; state.revoked = true }
                    }
                }
            } catch { state.poisoned = true; throw error }
        }
    }

    func flush(shrink: Bool = true) throws {
        try state.withLock { state in
            try Self.checkWritable(state)
            do { try flushState(&state, shrink: shrink) }
            catch { state.poisoned = true; state.revoked = true; throw error }
        }
    }

    private func flushState(_ state: inout State, shrink: Bool) throws {
        guard !state.pending.isEmpty else { return }
        try barrier(&state)
        state.floorSlot = state.selected.slot
        state.selected.floorSlot = state.floorSlot
        state.pending.removeAll(keepingCapacity: true); state.floorFree.removeAll(keepingCapacity: true)
        if shrink { shrinkAfterBarrier(&state) }
    }

    private func stabilize(_ state: inout State, syncDirectory: () throws -> Void) throws {
        if state.directorySync { try syncDirectory(); state.directorySync = false }
        if state.recoverySync {
            try barrier(&state); state.recoverySync = false
        }
    }

    private func repairKeyBlock(syncDirectory: () throws -> Void) throws {
        try state.withLock { state in
            guard let selected = state.keyBlock, selected.needsRepair else { return }
            do {
                try stabilize(&state, syncDirectory: syncDirectory)
                try write(selected.unselectedPage, bytes: selected.block.encode(pageSize: pageSize), state: &state)
                try barrier(&state)
                state.keyBlock = KeyBlockSelection(block: selected.block, selectedPage: 2, needsRepair: false)
            } catch { state.poisoned = true; throw error }
        }
    }

    static func growthStep(pages: UInt32, pageSize: Int) -> UInt32 {
        min(max(pages, UInt32(max(1, 16_384 / pageSize))), UInt32(max(1, 1_048_576 / pageSize)))
    }

    private func reserve(_ minimum: UInt32, state: inout State) throws {
        guard minimum > state.allocatedPages else { return }
        var target = state.allocatedPages
        while target < minimum {
            let (sum, overflow) = target.addingReportingOverflow(Self.growthStep(pages: target, pageSize: pageSize))
            target = overflow ? .max : sum
        }
        try state.store.setSize(UInt64(target) * UInt64(pageSize))
        state.allocatedPages = target
    }

    private func shrinkAfterBarrier(_ state: inout State) {
        guard state.shrinkPending else { return }
        state.shrinkPending = false
        let (sum, overflow) = state.shrinkPages.addingReportingOverflow(Self.growthStep(pages: state.shrinkPages, pageSize: pageSize))
        let target: UInt32 = overflow ? .max : sum
        if state.allocatedPages > target {
            do { try state.store.setSize(UInt64(target) * UInt64(pageSize)); state.allocatedPages = target }
            catch { /* A durable root does not require its harmless slack to shrink. */ }
        }
    }

    private func write(_ index: UInt32, bytes: [UInt8], state: inout State) throws {
        var tear: Int?
        if let fault = state.fault {
            switch fault {
            case .body(let ordinal, let count):
                if index >= 2 { state.bodyWrites += 1; if state.bodyWrites == ordinal { tear = count } }
            case .meta(let count): if index < 2 { tear = count }
            case .sync: break
            }
        }
        if let tear {
            state.fault = nil
            if tear > 0 { try state.store.write(at: UInt64(index) * UInt64(pageSize), bytes: Array(bytes.prefix(tear))) }
            throw JedError("58030", "injected commit write failure")
        }
        try state.store.write(at: UInt64(index) * UInt64(pageSize), bytes: bytes)
    }

    private func barrier(_ state: inout State) throws {
        if case .sync(let ordinal) = state.fault {
            state.syncs += 1
            if state.syncs == ordinal { state.fault = nil; throw JedError("58030", "injected commit barrier failure") }
        }
        try state.store.sync()
    }

    func armFault(_ fault: CommitFault) {
        state.withLock { $0.fault = fault; $0.bodyWrites = 0; $0.syncs = 0 }
    }
}
