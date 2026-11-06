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

internal struct PagerAllocationState: Sendable {
    let highWater: UInt32
    let persistedFree: [UInt32]
    let reusableFree: [UInt32]
    let pending: [ManifestEntry]
    let protected: Set<UInt32>
}

internal enum CommitFault: Sendable {
    case body(ordinal: UInt32, tearBytes: Int)
    case meta(tearBytes: Int)
    case sync(ordinal: UInt32)
}

internal final class StagedMemoryPages: Sendable {
    let plan: PreparedPageCommit
    fileprivate let token = StorageRevision()
    fileprivate init(_ plan: PreparedPageCommit) { self.plan = plan }
}

internal final class AdmittedMemoryPages {
    fileprivate let staged: StagedMemoryPages
    fileprivate init(_ staged: StagedMemoryPages) { self.staged = staged }
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
        var batchFlush = BatchFlushState()
        var floorSlot: UInt32
        var recoverySync = true
        var directorySync = true
        var poisoned = false
        var revoked = false
        var roleRevoked = false
        var shrinkPending = false
        var shrinkPages: UInt32 = 0
        var fault: CommitFault?
        var bodyWrites: UInt32 = 0
        var syncs: UInt32 = 0
        var keyBlock: KeyBlockSelection?
        var ephemeral = false
        var memoryStage: StorageRevision?
        var remote = false
        var remoteReuse = true
        var serviceCommitInProgress = false
    }
    let pageSize: Int
    let codec: any PageCodec
    private let state: Mutex<State>

    private init(store: any BlockStore, selected: ValidatedMeta, codec: any PageCodec,
                 freePages: [UInt32], keyBlock: KeyBlockSelection?, allocatedPages: UInt32) {
        pageSize = selected.meta.pageSize
        self.codec = codec
        state = Mutex(State(store: store, allocatedPages: allocatedPages, selected: selected,
            freePages: freePages, floorSlot: selected.floorSlot,
            recoverySync: selected.meta.manifestEntries != 0, keyBlock: keyBlock))
    }

    static func open(store: any BlockStore, credential: Credential? = nil,
                     writable: Bool = false, ephemeral: Bool = false,
                     unlockedCodec: EncryptedPageCodec? = nil, heldSessionKey: Bool = false,
                     syncDirectory: () throws -> Void = {}) throws -> Pager {
        let selected = try MetaRecovery.open(store)
        guard !ephemeral || !selected.meta.encrypted else { throw JedError("55000", "in-memory databases cannot be encrypted") }
        if unlockedCodec == nil {
            try EncryptionPolicy.checkCredential(encrypted: selected.meta.encrypted, credential: credential)
        } else {
            if heldSessionKey && !selected.meta.encrypted { throw JedError("28P04", "database was decrypted since this client unlocked it") }
            guard selected.meta.encrypted, unlockedCodec!.pageSize == selected.meta.pageSize else {
                throw JedError.corruption("unlocked codec does not match replacement image")
            }
        }
        let size = selected.meta.pageSize
        let codec: any PageCodec
        var keyBlock: KeyBlockSelection?
        if selected.meta.encrypted {
            let copy2 = try store.read(at: UInt64(size) * 2, count: size)
            let copy3 = try store.read(at: UInt64(size) * 3, count: size)
            let block = try KeyBlock.select(page2: copy2, page3: copy3, pageSize: size)
            let encrypted = try unlockedCodec ?? EncryptedPageCodec(pageSize: size, dataKey: block.block.unlock(credential), opening: true)
            // Structural fallback is finished. Authentication errors propagate.
            var chain: [ManifestPageBytes] = []
            for index in selected.chain {
                chain.append(ManifestPageBytes(pageIndex: index,
                    bytes: try store.read(at: UInt64(index) * UInt64(size), count: size)))
            }
            do { try encrypted.verifyMeta(selected.bytes, slot: selected.slot, manifest: chain) }
            catch {
                if heldSessionKey, try heldKeyStale(encrypted, store: store) {
                    throw JedError("28P04", "database data key changed since this client unlocked it")
                }
                throw error
            }
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
        pager.state.withLock { $0.ephemeral = ephemeral }
        if writable, unlockedCodec == nil, keyBlock?.needsRepair == true { try pager.repairKeyBlock(syncDirectory: syncDirectory) }
        return pager
    }

    /// Only promotion uses stale-key classification. At least one structurally
    /// valid meta must exist, and neither valid slot may authenticate; an older
    /// slot authenticating the held key proves the selected-slot failure is
    /// ordinary damage. Physical I/O and unparseable metadata stay XX001/58030.
    private static func heldKeyStale(_ codec: EncryptedPageCodec, store: any BlockStore) throws -> Bool {
        var parsed = false
        for slot: UInt32 in 0...1 {
            do {
                let size = codec.pageSize
                let bytes = try store.read(at: UInt64(slot) * UInt64(size), count: size)
                let meta = try MetaPage.decode(bytes)
                guard meta.encrypted, meta.pageSize == size else { continue }
                let validated = try MetaRecovery.validateManifest(meta, bytes: bytes, verifyBodies: false) { index in
                    try store.read(at: UInt64(index) * UInt64(size), count: size)
                }
                var chain: [ManifestPageBytes] = []
                for index in validated.chain {
                    chain.append(ManifestPageBytes(pageIndex: index,
                        bytes: try store.read(at: UInt64(index) * UInt64(size), count: size)))
                }
                parsed = true
                if (try? codec.verifyMeta(bytes, slot: slot, manifest: chain)) != nil { return false }
            } catch let error as JedError where error.code == "XX001" { continue }
        }
        return parsed
    }

    /// The owner already validated recovery. Client metadata is trusted for
    /// selection, while encrypted clients authenticate it and every logical
    /// page still passes through the ordinary page codec.
    static func remote(store: any BlockStore, selected: ValidatedMeta, codec: any PageCodec,
                       keyBlock: KeyBlockSelection?) throws -> Pager {
        let pager = Pager(store: store, selected: selected, codec: codec, freePages: [],
            keyBlock: keyBlock, allocatedPages: selected.meta.pageCount)
        pager.state.withLock { $0.remote = true; $0.remoteReuse = false; $0.recoverySync = false; $0.directorySync = false }
        try pager.adoptRemoteMeta(selected)
        return pager
    }
    func adoptRemoteMeta(_ selected: ValidatedMeta) throws {
        let store = state.withLock { $0.store }
        let free = try FreeList.load(meta: selected.meta, protected: selected.protectedPages) { reference in
            try codec.openBody(pageIndex: reference.index,
                sealed: store.read(at: UInt64(reference.index) * UInt64(pageSize), count: pageSize), expectedTag: reference.tag)
        }
        try state.withLock { state in
            guard state.remote, selected.meta.pageSize == pageSize,
                  selected.meta.encrypted == (codec.reservedBytes != 0) else {
                throw JedError("08006", "remote storage identity changed")
            }
            state.selected = selected; state.allocatedPages = selected.meta.pageCount
            state.freePages = free.entries; state.pending.removeAll(); state.floorFree.removeAll()
            state.floorSlot = 0; state.remoteReuse = false
        }
    }
    func grantRemoteWrite(_ reply: ServiceWriteReply) throws {
        try state.withLock { state in
            guard state.remote, state.selected.meta.txid == reply.read.point.txid else {
                throw JedError("08006", "remote writer base changed")
            }
            state.remoteReuse = reply.reuseGrant
            state.pending = Dictionary(uniqueKeysWithValues: reply.pending.map { ($0.pageIndex, $0.digest) })
            state.floorFree = Set(reply.reuse)
        }
    }
    func adoptRemoteCommit(_ plan: PreparedPageCommit) throws {
        try state.withLock { state in
            guard state.remote, plan.baseTxid == state.selected.meta.txid else {
                throw JedError("08006", "remote prepared generation changed")
            }
            state.selected = ValidatedMeta(meta: plan.manifest.meta, slot: plan.slot, floorSlot: plan.slot,
                bytes: plan.metaBytes, entries: plan.entries, chain: plan.manifest.pages.map(\.pageIndex))
            state.allocatedPages = plan.manifest.meta.pageCount; state.freePages = plan.freePages
            state.pending.removeAll(); state.floorFree.removeAll(); state.remoteReuse = false
        }
    }

    /// A private scratch address space has synthetic metadata only. The host
    /// owns its cleanup and any sealed spill device; it can never be reopened
    /// as a database. Its throwaway page codec may use the encrypted layout.
    static func scratch(store: any BlockStore, codec: any PageCodec, root: PageReference,
                        pageCount: UInt32, body: [StoredPage]) throws -> Pager {
        guard try store.size() == 0 else { throw JedError("55006", "scratch storage must be empty") }
        let manifest = try CommitManifest.build(pageSize: codec.pageSize, encrypted: codec.reservedBytes != 0,
            txid: 0, root: root, pageCount: pageCount, freeList: PageSealer.nullReference(codec),
            livePages: 0, entries: [], chain: [])
        var written: Set<UInt32> = []
        for page in body {
            guard page.index >= codec.firstBodyPage, page.index < pageCount,
                  page.bytes.count == codec.pageSize, written.insert(page.index).inserted else {
                throw JedError.corruption("invalid scratch bootstrap page")
            }
        }
        guard written.contains(root.index) else { throw JedError.corruption("scratch bootstrap omits its root") }
        let selected = try ValidatedMeta(meta: manifest.meta, slot: 0, floorSlot: 0,
            bytes: manifest.meta.encode(inline: []), entries: [], chain: [])
        try store.setSize(UInt64(pageCount) * UInt64(codec.pageSize))
        for page in body { try store.write(at: UInt64(page.index) * UInt64(codec.pageSize), bytes: page.bytes) }
        let pager = Pager(store: store, selected: selected, codec: codec, freePages: [],
            keyBlock: nil, allocatedPages: pageCount)
        pager.state.withLock { $0.ephemeral = true; $0.recoverySync = false; $0.directorySync = false }
        return pager
    }

    var selected: ValidatedMeta { state.withLock { $0.selected } }
    var selectedKeyBlock: KeyBlock? { state.withLock { $0.keyBlock?.block } }
    var pendingEntries: [ManifestEntry] {
        state.withLock { state in state.pending.keys.sorted().map { ManifestEntry(pageIndex: $0, digest: state.pending[$0]!) } }
    }
    /// A durable claim excludes recovered foreign generations until their
    /// stabilization barrier and any required directory barrier have completed.
    var isDurable: Bool { state.withLock { $0.pending.isEmpty && !$0.recoverySync && !$0.directorySync && !$0.poisoned } }
    var isPoisoned: Bool { state.withLock { $0.poisoned } }

    /// Only a fresh image whose barrier and directory publication already
    /// succeeded may bypass recovered/foreign-generation stabilization.
    func acknowledgeFreshDurableImage() {
        state.withLock { $0.recoverySync = false; $0.directorySync = false }
    }

    func integrityGeneration(txid: UInt64, memory: Bool) throws -> IntegrityGeneration? {
        try state.withLock { state in
            if state.roleRevoked { throw JedError("08006", "page-service role was lost") }
            if state.revoked || state.poisoned { throw JedError("58030", "database storage is poisoned") }
            guard state.selected.meta.txid == txid else { return nil }
            let slots: [[UInt8]]?
            if memory || state.remote { slots = nil }
            else { slots = try [state.store.read(at: 0, count: pageSize),
                                state.store.read(at: UInt64(pageSize), count: pageSize)] }
            return IntegrityGeneration(selected: state.selected, freePages: state.freePages,
                metaSlots: slots, remote: state.remote, memory: memory)
        }
    }

    func readStored(_ index: UInt32) throws -> [UInt8] {
        try state.withLock { state in
            guard !state.roleRevoked else { throw JedError("08006", "database storage role changed") }
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
            let free = !reuseAllowed || !state.remoteReuse ? [] : state.freePages.filter { state.pending.isEmpty || state.floorFree.contains($0) }
            return (state.selected.meta.pageCount, free, 1 - state.floorSlot)
        }
    }

    func allocationSnapshot(reuseAllowed: Bool) throws -> PagerAllocationState {
        try state.withLock { state in
            try Self.checkWritable(state)
            return PagerAllocationState(highWater: state.selected.meta.pageCount,
                persistedFree: state.freePages,
                reusableFree: reuseAllowed && state.remoteReuse ? state.freePages.filter { state.pending.isEmpty || state.floorFree.contains($0) } : [],
                pending: state.pending.keys.sorted().map { ManifestEntry(pageIndex: $0, digest: state.pending[$0]!) },
                protected: state.selected.protectedPages)
        }
    }

    /// Ephemeral stores keep their free list in memory. The caller supplies a
    /// reader-safe reachability proof; no file meta or free-list page is written.
    func replaceMemoryFree(_ pages: [UInt32]) throws {
        try state.withLock { state in
            guard state.ephemeral, zip(pages, pages.dropFirst()).allSatisfy({ $0 < $1 }),
                  pages.allSatisfy({ $0 >= codec.firstBodyPage && $0 < state.selected.meta.pageCount }) else {
                throw JedError.corruption("invalid ephemeral free-list replacement")
            }
            state.freePages = pages
        }
    }

    func prepareMemory(body: [StoredPage], root: PageReference, pageCount: UInt32,
                       freePages: [UInt32], reuseAllowed: Bool,
                       reusablePages: [UInt32]? = nil) throws -> PreparedPageCommit {
        try state.withLock { state in
            try Self.checkWritable(state)
            guard state.ephemeral, state.selected.meta.txid < UInt64.max else {
                throw JedError.corruption("invalid ephemeral commit state")
            }
            let reusable = reusablePages ?? state.freePages
            guard zip(reusable, reusable.dropFirst()).allSatisfy({ $0 < $1 }),
                  reusable.allSatisfy({ $0 >= codec.firstBodyPage && $0 < state.selected.meta.pageCount }) else {
                throw JedError.corruption("invalid planned ephemeral reusable pages")
            }
            let safe = Set(reuseAllowed ? reusable : []), free = Set(freePages)
            var written: Set<UInt32> = []
            for page in body {
                guard page.index >= codec.firstBodyPage, page.index < pageCount, page.bytes.count == pageSize,
                      written.insert(page.index).inserted, !free.contains(page.index),
                      page.index >= state.selected.meta.pageCount || safe.contains(page.index) else {
                    throw JedError.corruption("unsafe ephemeral body page")
                }
            }
            guard free.count == freePages.count, zip(freePages, freePages.dropFirst()).allSatisfy({ $0 < $1 }),
                  freePages.allSatisfy({ $0 >= codec.firstBodyPage && $0 < pageCount }) else {
                throw JedError.corruption("invalid ephemeral free list")
            }
            let txid = state.selected.meta.txid + 1
            let manifest = try CommitManifest.build(pageSize: pageSize, encrypted: codec.reservedBytes != 0, txid: txid,
                root: root, pageCount: pageCount, freeList: PageSealer.nullReference(codec), livePages: 0, entries: [], chain: [])
            return PreparedPageCommit(baseTxid: state.selected.meta.txid, body: body, manifest: manifest,
                metaBytes: try manifest.meta.encode(inline: []), slot: UInt32(txid & 1), freePages: freePages, entries: [])
        }
    }

    /// An attachment writes only safe body slots before main persists. It
    /// changes neither the selected generation nor the free-page accounting.
    func stageMemory(_ plan: PreparedPageCommit) throws -> StagedMemoryPages {
        try state.withLock { state in
            try Self.checkWritable(state)
            guard state.ephemeral, plan.baseTxid == state.selected.meta.txid,
                  plan.entries.isEmpty, plan.manifest.pages.isEmpty else {
                throw JedError("55006", "obsolete or invalid staged memory commit")
            }
            let staged = StagedMemoryPages(plan)
            do {
                try reserve(plan.manifest.meta.pageCount, state: &state)
                for page in plan.body { try write(page.index, bytes: page.bytes, state: &state) }
                state.memoryStage = staged.token
                return staged
            } catch { state.memoryStage = nil; state.poisoned = true; throw error }
        }
    }

    /// No I/O after durable main succeeds. An abandoned or superseded stage
    /// cannot install accounting for bytes another plan has overwritten.
    func adoptMemory(_ staged: StagedMemoryPages) throws {
        try state.withLock { state in
            try Self.checkWritable(state)
            let plan = staged.plan
            guard state.ephemeral, state.memoryStage === staged.token,
                  plan.baseTxid == state.selected.meta.txid else {
                throw JedError("55006", "obsolete staged memory commit")
            }
            state.selected = ValidatedMeta(meta: plan.manifest.meta, slot: plan.slot, floorSlot: plan.slot,
                bytes: plan.metaBytes, entries: [], chain: [])
            state.floorSlot = plan.slot; state.freePages = plan.freePages
            state.recoverySync = false; state.directorySync = false; state.memoryStage = nil
        }
    }

    func admitMemory(_ staged: StagedMemoryPages) throws -> AdmittedMemoryPages {
        try state.withLock { state in
            try Self.checkWritable(state)
            guard state.ephemeral, state.memoryStage === staged.token,
                  staged.plan.baseTxid == state.selected.meta.txid else {
                throw JedError("55006", "obsolete staged memory admission")
            }
            return AdmittedMemoryPages(staged)
        }
    }
    /// The memory core commit guard excludes epoch replacement and the writer
    /// lease excludes another stage. This pointer/accounting update cannot fail
    /// after the group's file domain has crossed its durable point.
    func finishMemory(_ admitted: AdmittedMemoryPages) {
        state.withLock { state in
            let plan = admitted.staged.plan
            state.selected = ValidatedMeta(meta: plan.manifest.meta, slot: plan.slot, floorSlot: plan.slot,
                bytes: plan.metaBytes, entries: [], chain: [])
            state.floorSlot = plan.slot; state.freePages = plan.freePages
            state.recoverySync = false; state.directorySync = false; state.memoryStage = nil
        }
    }

    /// No persisted free-list or manifest pages: the memory high-water is the
    /// baseline Q4 measure. MemoryBlockStore's barriers have no host effect.
    func commitMemory(_ plan: PreparedPageCommit) throws {
        try state.withLock { state in
            try Self.checkWritable(state)
            guard state.ephemeral, plan.baseTxid == state.selected.meta.txid,
                  plan.slot == UInt32(plan.manifest.meta.txid & 1), plan.entries.isEmpty, plan.manifest.pages.isEmpty else {
                throw JedError("55006", "obsolete or invalid ephemeral commit")
            }
            do {
                try reserve(plan.manifest.meta.pageCount, state: &state)
                for page in plan.body { try write(page.index, bytes: page.bytes, state: &state) }
                try barrier(&state); try write(plan.slot, bytes: plan.metaBytes, state: &state); try barrier(&state)
                state.selected = ValidatedMeta(meta: plan.manifest.meta, slot: plan.slot, floorSlot: plan.slot,
                    bytes: plan.metaBytes, entries: [], chain: [])
                state.floorSlot = plan.slot; state.freePages = plan.freePages
                state.recoverySync = false; state.directorySync = false; state.memoryStage = nil
            } catch { state.poisoned = true; throw error }
        }
    }

    func prepare(body: [StoredPage], root: PageReference, pageCount: UInt32, freeList: PageReference,
                 livePages: UInt32, freePages: [UInt32], manifestPages: [UInt32], reuseAllowed: Bool,
                 entropy: KeyEntropy? = nil) throws -> PreparedPageCommit {
        let snapshot = try state.withLock { state -> State in try Self.checkWritable(state); return state }
        guard snapshot.selected.meta.txid < UInt64.max else { throw JedError.corruption("transaction id exhausted") }
        let txid = snapshot.selected.meta.txid + 1
        let safe = Set(reuseAllowed && snapshot.remoteReuse ? snapshot.freePages.filter { snapshot.pending.isEmpty || snapshot.floorFree.contains($0) } : [])
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
                    state.batchFlush.clear()
                    state.floorSlot = plan.slot
                    shrinkAfterBarrier(&state)
                } else {
                    for page in plan.body { state.pending[page.index] = Checksums.crc64(page.bytes) }
                    if state.pending.count >= meta.inlineCapacity {
                        do { try flushState(&state, shrink: true) }
                        catch { state.poisoned = true; state.revoked = true }
                    }
                }
                if !state.poisoned, !state.pending.isEmpty, state.batchFlush.preparedEmpty,
                   let reading = state.batchFlush.preparedReading { state.batchFlush.begin(at: reading) }
                state.batchFlush.preparedReading = nil; state.batchFlush.preparedEmpty = false
            } catch { state.poisoned = true; throw error }
        }
    }


    /// Check A is complete before entering this phase. A failed or abandoned
    /// delta leaves only unreachable pages; it never changes the selected meta.
    func beginServiceDelta(_ delta: ServiceIncomingDelta, baseTxid: UInt64,
                           syncDirectory: () throws -> Void) throws {
        try state.withLock { state in
            try Self.checkWritable(state)
            guard state.selected.meta.txid == baseTxid else { throw JedError("55006", "service writer generation changed") }
            guard !state.serviceCommitInProgress else { throw JedError("58030", "a service commit is already in progress") }
            state.serviceCommitInProgress = true
            do {
                try stabilize(&state, syncDirectory: syncDirectory)
                if state.pending.isEmpty { state.floorFree = Set(state.freePages) }
                try reserve(delta.meta.pageCount, state: &state)
                for page in delta.manifest { try write(page.pageIndex, bytes: page.bytes, state: &state) }
            } catch { state.poisoned = true; throw error }
        }
    }
    /// The streaming validator performs check B before passing each page here.
    func writeServicePage(_ page: StoredPage, baseTxid: UInt64) throws {
        try state.withLock { state in
            try Self.checkWritable(state)
            guard state.selected.meta.txid == baseTxid else { throw JedError("55006", "service writer generation changed") }
            do { try write(page.index, bytes: page.bytes, state: &state) }
            catch { state.poisoned = true; throw error }
        }
    }
    /// Check C passing precedes this call. Failures now close the connection;
    /// the client resolves whether the meta reached the durable floor.
    func finishServiceDelta(_ delta: ServiceIncomingDelta, baseTxid: UInt64,
                            synchronous: Bool, entropy: KeyEntropy?) throws -> ValidatedMeta {
        try state.withLock { state in
            try Self.checkWritable(state)
            guard state.selected.meta.txid == baseTxid else { throw JedError("55006", "service writer generation changed") }
            let beganEmpty = state.pending.isEmpty
            do {
                // Prior streaming body writes keep the poison guard armed. Date
                // a new batch before its meta write, so clock failure cannot be
                // reported after durable success or leave partial writes usable.
                let clientReading: Int64? = !synchronous && beganEmpty && state.batchFlush.enabled &&
                    delta.ownEntries.count < delta.meta.inlineCapacity ? try state.batchFlush.clock!.microseconds() : nil
                let slot = 1 - state.floorSlot
                var bytes = delta.bytes
                if let encrypted = codec as? EncryptedPageCodec {
                    guard let entropy else { throw JedError("58030", "encrypted service commit requires key entropy") }
                    bytes = try encrypted.sealMeta(bytes, slot: slot, manifest: delta.manifest,
                        nonce: EncryptionPolicy.entropy(24, from: entropy))
                }
                try write(slot, bytes: bytes, state: &state)
                if synchronous { try barrier(&state) }
                let selected = ValidatedMeta(meta: delta.meta, slot: slot,
                    floorSlot: synchronous ? slot : state.floorSlot, bytes: bytes,
                    entries: delta.entries, chain: delta.manifest.map(\.pageIndex))
                // This is structural adoption only; stored SQL and the owner's
                // environment do not participate in accepting a client commit.
                let free = try FreeList.load(meta: delta.meta, protected: delta.protected) { reference in
                    try codec.openBody(pageIndex: reference.index,
                        sealed: state.store.read(at: UInt64(reference.index) * UInt64(pageSize), count: pageSize),
                        expectedTag: reference.tag)
                }
                state.selected = selected; state.freePages = free.entries
                if synchronous {
                    state.floorSlot = slot
                    state.pending.removeAll(keepingCapacity: true); state.floorFree.removeAll(keepingCapacity: true)
                    state.batchFlush.clear()
                } else {
                    for entry in delta.ownEntries { state.pending[entry.pageIndex] = entry.digest }
                }
                // Clients cannot shrink. Any earlier owner shrink must not run
                // as a side effect of flushing a client's relaxed commit.
                state.shrinkPending = false
                if let clientReading, !state.pending.isEmpty {
                    state.batchFlush.begin(at: clientReading)
                } else if synchronous || state.pending.count >= state.selected.meta.inlineCapacity {
                    state.batchFlush.clear()
                }
                state.serviceCommitInProgress = false
                return selected
            } catch { state.poisoned = true; state.revoked = true; throw error }
        }
    }
    /// Abandoning an incomplete client delta only drops its commit guard; body
    /// pages remain unreachable and any physical I/O poison remains in force.
    func abandonServiceDelta() { state.withLock { $0.serviceCommitInProgress = false } }
    func configureBatchFlush(clock: any ClockSource, milliseconds: UInt64) {
        state.withLock { $0.batchFlush.configure(clock: clock, milliseconds: milliseconds) }
    }
    func copyBatchFlushSettings(from other: Pager) {
        let settings = other.state.withLock { ($0.batchFlush.clock, $0.batchFlush.interval) }
        guard let clock = settings.0 else { return }
        state.withLock { state in
            state.batchFlush.clock = clock; state.batchFlush.interval = settings.1; state.batchFlush.clear()
        }
    }
    /// The writer owns admission before any clock read. A remote planner never
    /// times the owner's batch; memory domains never consult this policy.
    @discardableResult func beforeCommitTiming(synchronous: Bool) throws -> Bool {
        try state.withLock { state in
            state.batchFlush.preparedReading = nil; state.batchFlush.preparedEmpty = false
            guard !state.remote, !state.ephemeral, state.batchFlush.enabled,
                  !synchronous || !state.pending.isEmpty else { return false }
            try Self.checkWritable(state)
            let now = try state.batchFlush.clock!.microseconds()
            var flushed = false
            if !state.pending.isEmpty, let deadline = state.batchFlush.deadline, now >= deadline {
                do { try flushState(&state, shrink: true); flushed = true }
                catch { state.poisoned = true; state.revoked = true; throw error }
            }
            state.batchFlush.preparedReading = now
            state.batchFlush.preparedEmpty = state.pending.isEmpty
            return flushed
        }
    }
    /// A relaxed client joins the batch it planned against. Expiry is admitted
    /// before write_begun, not again when its commit eventually arrives.
    func beforeClientTiming() throws {
        try state.withLock { state in
            guard state.batchFlush.enabled, !state.pending.isEmpty else { return }
            try Self.checkWritable(state)
            let now = try state.batchFlush.clock!.microseconds()
            if let deadline = state.batchFlush.deadline, now >= deadline {
                do { try flushState(&state, shrink: false) }
                catch { state.poisoned = true; state.revoked = true; throw error }
            }
        }
    }
    struct IdleFlushState: Sendable { let interval: Int64; let deadline: Int64?; let generation: UInt64 }
    var idleFlushState: IdleFlushState {
        state.withLock { state in IdleFlushState(interval: state.batchFlush.interval,
            deadline: state.remote || state.ephemeral || state.poisoned ? nil : state.batchFlush.deadline,
            generation: state.batchFlush.generation) }
    }
    /// Called only while the core's nonwaiting writer gate and physical commit
    /// boundary are held. Returns the remaining wait without any second read.
    func idleFlush() throws -> Int64? {
        try state.withLock { state in
            guard !state.remote, !state.ephemeral, !state.poisoned,
                  let deadline = state.batchFlush.deadline, !state.pending.isEmpty else { return nil }
            let now = try state.batchFlush.clock!.microseconds()
            if now < deadline {
                let (remaining, overflow) = deadline.subtractingReportingOverflow(now)
                return min(overflow ? Int64.max : remaining, state.batchFlush.interval)
            }
            do { try flushState(&state, shrink: true); return nil }
            catch { state.poisoned = true; state.revoked = true; throw error }
        }
    }
    func flushServiceCap() throws {
        try state.withLock { state in
            try Self.checkWritable(state)
            guard state.pending.count >= state.selected.meta.inlineCapacity else { return }
            do { try flushState(&state, shrink: false) }
            catch { state.poisoned = true; state.revoked = true; throw error }
        }
    }

    func flush(shrink: Bool = true) throws {
        try state.withLock { state in
            if state.remote { return }
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
        state.batchFlush.clear()
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

    /// No stale snapshot may use a descriptor after maintenance changes its
    /// address space. Normal commits retain their narrower writer poison rule.
    func poisonWriter() { state.withLock { $0.poisoned = true } }
    func revokeRole() { state.withLock { $0.roleRevoked = true; $0.poisoned = true } }
    func revoke() { state.withLock { $0.poisoned = true; $0.revoked = true } }

    /// Rewrapping changes only the redundant key pages. The KDF and encoding
    /// finish before this storage phase, and readers may keep their data roots.
    func changeKeyBlock(_ replacement: KeyBlock, replacing expected: KeyBlock,
                        syncDirectory: () throws -> Void) throws {
        let bytes = try replacement.encode(pageSize: pageSize)
        try state.withLock { state in
            try Self.checkWritable(state)
            // Flush an existing relaxed floor before checking the streamed
            // commit guard, matching ordinary key-block storage admission.
            do { try flushState(&state, shrink: true) }
            catch { state.poisoned = true; state.revoked = true; throw error }
            guard !state.serviceCommitInProgress else { throw JedError("58030", "database writer has a commit in progress") }
            guard let selected = state.keyBlock, selected.block == expected,
                  expected.generation < UInt64.max, replacement.generation == expected.generation + 1 else {
                throw JedError("55006", "the credential changed concurrently")
            }
            do {
                // The key operation always has its own stabilization barrier.
                if state.directorySync { try syncDirectory(); state.directorySync = false }
                // Key-block barriers preserve the pager's adopted-generation
                // commit state. The next data commit still performs its own
                // required stabilization, matching the frozen crash schedule.
                try barrier(&state)
                try write(selected.unselectedPage, bytes: bytes, state: &state)
                try barrier(&state)
                try write(selected.selectedPage, bytes: bytes, state: &state)
                try barrier(&state)
                state.keyBlock = KeyBlockSelection(block: replacement, selectedPage: 2, needsRepair: false)
            } catch { state.poisoned = true; state.revoked = true; throw error }
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
