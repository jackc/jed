import Synchronization

internal final class StagedDomainCommit: Sendable {
    let plan: PreparedDomainCommit
    fileprivate let pages: StagedMemoryPages
    fileprivate init(plan: PreparedDomainCommit, pages: StagedMemoryPages) { self.plan = plan; self.pages = pages }
    var snapshot: DomainSnapshot { plan.snapshot }
}

internal final class AdmittedMemoryDomainCommit {
    let snapshot: DomainSnapshot
    fileprivate let accounting: DomainStorageState
    fileprivate let pages: AdmittedMemoryPages
    fileprivate init(plan: PreparedDomainCommit, pages: AdmittedMemoryPages) {
        snapshot = plan.snapshot; self.pages = pages
        accounting = DomainStorageState(snapshot: plan.snapshot, catalogPages: plan.catalogPages,
            gistPages: plan.gistPages, livePages: plan.livePages, liveAtCompaction: plan.liveAtCompaction,
            freeGeneration: plan.freeGeneration, lastWritten: plan.written, repairCatalogPages: plan.catalogPages.count)
    }
}

/// Owns one byte-device address space. The DatabaseCore writer gate surrounds
/// precheck/prepare/persist/publication; readers obtain snapshots through its
/// atomic pin registry, never by racing this object's storage bookkeeping.
internal final class StorageDomain: Sendable {
    let pager: Pager
    let pool: NodePool
    let kind: StorageDomainKind
    let readOnly: Bool
    private let state: Mutex<DomainStorageState>
    private let memoryStageHook = Mutex<(@Sendable (UInt32) throws -> Void)?>(nil)
    func installMemoryStageHook(_ hook: (@Sendable (UInt32) throws -> Void)?) {
        memoryStageHook.withLock { $0 = hook }
    }
    private let durableDirectorySync: @Sendable () throws -> Void

    private init(pager: Pager, pool: NodePool, kind: StorageDomainKind, readOnly: Bool, initial: DomainStorageState,
                 durableDirectorySync: @escaping @Sendable () throws -> Void = {}) {
        self.pager = pager; self.pool = pool; self.kind = kind; self.readOnly = readOnly; state = Mutex(initial)
        self.durableDirectorySync = durableDirectorySync
    }
    var committed: DomainSnapshot { state.withLock { $0.snapshot } }
    var livePages: UInt32 { state.withLock { $0.livePages } }
    var storageBytes: UInt64 {
        (kind == .file ? UInt64(livePages) : UInt64(pager.selected.meta.pageCount)) * UInt64(pager.pageSize)
    }

    static func open(store: any BlockStore, kind: StorageDomainKind = .file, catalogID: UInt64,
                     cacheBytes: Int? = 256 * 1024 * 1024, credential: Credential? = nil,
                     readOnly: Bool = false, unlockedCodec: EncryptedPageCodec? = nil, heldSessionKey: Bool = false, collatedKey: KeyEncoding.CollatedKey? = nil,
                     collationVerdict: ((CatalogCollation) throws -> Bool)? = nil,
                     validateCatalog: (CatalogImage) throws -> Void,
                     durableDirectorySync: @escaping @Sendable () throws -> Void = {},
                     syncDirectory: () throws -> Void = {}) throws -> StorageDomain {
        let pager = try Pager.open(store: store, credential: credential, writable: !readOnly,
            ephemeral: kind == .memory, unlockedCodec: unlockedCodec, heldSessionKey: heldSessionKey, syncDirectory: syncDirectory)
        let pool = try NodePool(pager: pager, cacheBytes: kind == .memory ? nil : cacheBytes)
        let initial = try loadSnapshot(pager: pager, pool: pool, selected: pager.selected,
            kind: kind, catalogID: catalogID, collatedKey: collatedKey,
            collationVerdict: collationVerdict, validateCatalog: validateCatalog)
        return StorageDomain(pager: pager, pool: pool, kind: kind, readOnly: readOnly,
            initial: initial, durableDirectorySync: durableDirectorySync)
    }

    static func remote(pager: Pager, pool: NodePool, catalogID: UInt64, readOnly: Bool,
                       environment: Environment, structuralOnly: Bool = false) throws -> StorageDomain {
        let initial = try loadSnapshot(pager: pager, pool: pool, selected: pager.selected, kind: .file,
            catalogID: catalogID, collatedKey: { try environment.unicode.key($0, collation: $1) },
            collationVerdict: { if structuralOnly { return false }; return try CatalogEnvironment.verdict($0, environment: environment) },
            validateCatalog: { if !structuralOnly { try CatalogEnvironment.validate($0, environment: environment) } })
        return StorageDomain(pager: pager, pool: pool, kind: .file, readOnly: readOnly, initial: initial)
    }

    private static func loadSnapshot(pager: Pager, pool: NodePool, selected: ValidatedMeta,
                                     kind: StorageDomainKind, catalogID: UInt64,
                                     collatedKey: KeyEncoding.CollatedKey?,
                                     collationVerdict: ((CatalogCollation) throws -> Bool)?,
                                     validateCatalog: (CatalogImage) throws -> Void) throws -> DomainStorageState {
        let meta = selected.meta
        let chain = try CatalogChain.read(root: meta.root, pageCount: meta.pageCount, codec: pager.codec, read: pager.readBody)
        let catalog = try CatalogImage.collect(chain.entries, catalogID: catalogID, collatedKey: collatedKey, collationVerdict: collationVerdict)
        try validateCatalog(catalog)
        let types = try catalog.resolvedTypes(catalogID: catalogID)
        var tables: [SQLText: TableStore] = [:], indexes: [SQLText: TableStore] = [:], gistTrees: [SQLText: GistTree] = [:]
        var gistPages: [UInt32] = [], relationNames = Set(catalog.tables.map(\.name) + catalog.sequences.map(\.name) + catalog.views.map(\.name))
        for table in catalog.tables.sorted(by: { $0.name < $1.name }) {
            guard table.rowCount >= 0, UInt64(table.rowCount) <= UInt64(meta.pageCount) * UInt64(pager.codec.capacity / 4) else {
                throw JedError.corruption("table row count exceeds the file")
            }
            let rowTypes = try table.columns.map { try $0.type.resolve(types) }
            let tree = try BPlusTree.open(root: table.root, rowCount: table.rowCount, pageCount: meta.pageCount, types: rowTypes, pool: pool)
            var nextRowID: Int64 = 0
            if table.primaryKey.isEmpty, !tree.isEmpty {
                var cursor = TreeCursor(tree, descending: true)
                guard let record = try cursor.next(), record.key.count == 8 else { throw JedError.corruption("invalid synthetic row key") }
                var key = ByteReader(record.key)
                let last = Int64(bitPattern: try key.u64() ^ (UInt64(1) << 63))
                guard last >= 0, last < Int64.max else { throw JedError.corruption("synthetic row key out of range") }
                nextRowID = last + 1
            }
            tables[table.name] = TableStore(tree: tree, nextRowID: nextRowID)
            for index in table.indexes.sorted(by: { $0.name < $1.name }) {
                guard relationNames.insert(index.name).inserted else { throw JedError.corruption("duplicate index relation name") }
                if index.kind == 2 {
                    let ops = try index.keys.map { key -> GistOpclass in
                        guard case .column(let ordinal) = key, rowTypes.indices.contains(ordinal) else {
                            throw JedError.corruption("invalid GiST key column")
                        }
                        return try GistOpclass(type: rowTypes[ordinal])
                    }
                    let opened = try GistTree.open(root: index.root, pageCount: meta.pageCount, opclasses: ops, codec: pager.codec, read: pager.readBody)
                    var leafKeys = BPlusTree(codec: pager.codec, types: [])
                    for key in try opened.tree.leafKeys() { _ = try leafKeys.insert(PackedRecord(key: key, cells: [])) }
                    indexes[index.name] = TableStore(tree: leafKeys); gistTrees[index.name] = opened.tree
                    gistPages.append(contentsOf: opened.pages)
                } else {
                    indexes[index.name] = TableStore(tree: try BPlusTree.open(root: index.root,
                        pageCount: meta.pageCount, types: [], pool: pool))
                }
            }
        }
        let snapshot = try DomainSnapshot(catalogID: catalogID, txid: meta.txid, catalog: catalog,
            pool: pool, tables: tables, indexes: indexes, gistTrees: gistTrees)
        let free = try pager.allocationSnapshot(reuseAllowed: true)
        return DomainStorageState(snapshot: snapshot, catalogPages: chain.pages, gistPages: gistPages,
            livePages: meta.livePages, liveAtCompaction: meta.pageCount - UInt32(free.persistedFree.count),
            freeGeneration: meta.txid, lastWritten: [], repairCatalogPages: kind == .file ? chain.pages.count : 0)
    }

    var freeGeneration: UInt64 { state.withLock { $0.freeGeneration } }
    var lastWritten: Set<UInt32> { state.withLock { $0.lastWritten } }
    func serviceDirectorySync() throws { try durableDirectorySync() }

    /// Evaluate the specified owner-only trigger without decoding table rows.
    func serviceMaintenanceTrigger() throws -> (needed: Bool, reclaim: Bool) {
        guard kind == .file, !readOnly, pager.selected.meta.txid < UInt64.max else { return (false, false) }
        let current = state.withLock { $0 }, free = try pager.allocationSnapshot(reuseAllowed: true)
        let reclaim = current.liveAtCompaction == 0 || DomainCommitPlanner.reclaimDue(
            pageCount: free.highWater, free: free.persistedFree.count, live: current.liveAtCompaction)
        let tail = free.highWater > pager.codec.firstBodyPage && free.persistedFree.last == free.highWater - 1
        return (reclaim || tail, reclaim)
    }

    /// Publish structural accounting immediately, without parsing stored SQL or
    /// consulting the local Environment. The coordinator publishes its meta and
    /// invalidation window in the same critical section as reader pins.
    func noteServiceCommit(_ selected: ValidatedMeta, written: Set<UInt32>) {
        state.withLock { current in
            current.livePages = selected.meta.livePages; current.lastWritten = written
        }
    }

    /// Called by the coordinator only when a foreign commit advanced its meta.
    /// Failure affects that logical begin alone; the already published commit
    /// remains available to clients with a suitable environment.
    func adoptServiceSnapshot(_ selected: ValidatedMeta, environment: Environment?, force: Bool = false) throws -> DomainSnapshot {
        let current = state.withLock { $0 }
        if !force, current.snapshot.txid == selected.meta.txid { return current.snapshot }
        let loaded: DomainStorageState
        if let environment {
            loaded = try Self.loadSnapshot(pager: pager, pool: pool, selected: selected, kind: kind,
                catalogID: current.snapshot.catalogID,
                collatedKey: { try environment.unicode.key($0, collation: $1) },
                collationVerdict: { try CatalogEnvironment.verdict($0, environment: environment) },
                validateCatalog: { try CatalogEnvironment.validate($0, environment: environment) })
        } else {
            loaded = try Self.loadSnapshot(pager: pager, pool: pool, selected: selected, kind: kind,
                catalogID: current.snapshot.catalogID, collatedKey: nil,
                collationVerdict: { _ in false }, validateCatalog: { _ in })
        }
        var adopted = loaded
        adopted.freeGeneration = current.freeGeneration
        adopted.liveAtCompaction = current.liveAtCompaction
        adopted.lastWritten = current.lastWritten
        state.withLock { $0 = adopted }
        return adopted.snapshot
    }

    /// The caller owns creation of an empty destination and any atomic path
    /// replacement/lock-bundle protocol. This method only writes the byte device.
    static func create(store: any BlockStore, kind: StorageDomainKind = .memory, catalogID: UInt64,
                       pageSize: Int = 4096, cacheBytes: Int? = 256 * 1024 * 1024,
                       credential: Credential? = nil, parameters: Argon2Parameters = .default,
                       entropy: KeyEntropy? = nil, syncDirectory: () throws -> Void = {}) throws -> StorageDomain {
        guard try store.size() == 0 else { throw JedError("55006", "database creation requires an empty byte device") }
        try PageEnvelope.validateSize(pageSize, encrypted: credential != nil, opening: false)
        try EncryptionPolicy.checkCredential(encrypted: credential != nil, inMemory: kind == .memory, credential: credential)
        let codec: any PageCodec
        var keyBlock: KeyBlock?
        if let credential {
            guard let entropy else { throw JedError("58030", "encrypted creation requires key entropy") }
            let created = try KeyBlock.create(credential: credential, parameters: parameters, entropy: entropy)
            keyBlock = created.block; codec = try EncryptedPageCodec(pageSize: pageSize, dataKey: created.dataKey)
        } else { codec = try PlainPageCodec(pageSize: pageSize) }
        var allocation = try PageAllocator(highWater: codec.firstBodyPage, free: [], firstBody: codec.firstBodyPage)
        let catalog = try CatalogChain.write([], allocator: &allocation, codec: codec, entropy: entropy)
        let manifest = try CommitManifest.build(pageSize: pageSize, encrypted: credential != nil,
            txid: kind == .memory ? 0 : 1, root: catalog.root, pageCount: allocation.highWater,
            freeList: PageSealer.nullReference(codec), livePages: UInt32(catalog.pages.count), entries: [], chain: [])
        let clear = try manifest.meta.encode(inline: [])
        var metas: [[UInt8]] = []
        for slot: UInt32 in 0...1 {
            if let encrypted = codec as? EncryptedPageCodec {
                metas.append(try encrypted.sealMeta(clear, slot: slot, manifest: [], nonce: EncryptionPolicy.entropy(24, from: entropy!)))
            } else { metas.append(clear) }
        }
        try store.setSize(UInt64(allocation.highWater) * UInt64(pageSize))
        for page in catalog.pages { try store.write(at: UInt64(page.index) * UInt64(pageSize), bytes: page.bytes) }
        if let keyBlock {
            let bytes = try keyBlock.encode(pageSize: pageSize)
            try store.write(at: UInt64(2 * pageSize), bytes: bytes); try store.write(at: UInt64(3 * pageSize), bytes: bytes)
        }
        try store.write(at: 0, bytes: metas[0]); try store.write(at: UInt64(pageSize), bytes: metas[1]); try store.sync()
        try syncDirectory()
        let domain = try open(store: store, kind: kind, catalogID: catalogID, cacheBytes: cacheBytes,
            credential: credential, validateCatalog: { _ in })
        domain.pager.acknowledgeFreshDurableImage()
        // Creation has no prior reclamation baseline. The supported file
        // allocators rebuild on the first unpinned commit, retaining even a
        // lone bootstrap-page residue for the next allocation. An ordinary
        // reopen instead seeds its baseline from the loaded high-water/list.
        if kind == .file { domain.state.withLock { $0.liveAtCompaction = 0 } }
        return domain
    }

    private func context(_ working: DomainSnapshot, policy: DomainCommitPolicy) throws -> (DomainStorageState, PagerAllocationState, DomainBodyLayout, UInt32)? {
        let previous = state.withLock { $0 }
        guard working.identity === previous.snapshot.identity, working.txid == previous.snapshot.txid,
              previous.snapshot.txid < UInt64.max else { throw JedError("55006", "working snapshot belongs to an obsolete generation") }
        guard working.hasChanges(comparedTo: previous.snapshot) else { return nil }
        guard !readOnly else { throw JedError("25006", "database is read only") }
        let reuse = policy.oldestPinnedVersion >= previous.freeGeneration
        var free = try pager.allocationSnapshot(reuseAllowed: reuse)
        var layout = try DomainCommitPlanner.layout(working, highWater: free.highWater, free: free.reusableFree)
        var live = try DomainCommitPlanner.live(after: layout, previous: previous, pageCount: free.highWater)
        var current = previous
        if try !DomainCommitPlanner.admit(layout: layout, live: live, previous: current, pageCount: free.highWater, kind: kind, policy: policy),
           kind == .memory, policy.canCompactCommitted, policy.oldestPinnedVersion >= previous.snapshot.txid {
            var reached = try PageReachability.collect(previous.snapshot, catalogPages: previous.catalogPages,
                gistPages: previous.gistPages, pageCount: free.highWater, read: pager.readBody)
            reached.formUnion(previous.lastWritten)
            let rebuilt = (pager.codec.firstBodyPage..<free.highWater).filter { !reached.contains($0) }
            if rebuilt.count > free.persistedFree.count {
                // This is a reader-safe allocation proof for the prospective
                // commit. Do not change published accounting during preflight:
                // another domain may still refuse or fail its durable commit.
                current.liveAtCompaction = UInt32(reached.count); current.freeGeneration = previous.snapshot.txid
                free = PagerAllocationState(highWater: free.highWater, persistedFree: rebuilt,
                    reusableFree: rebuilt, pending: free.pending, protected: free.protected)
                layout = try DomainCommitPlanner.layout(working, highWater: free.highWater, free: free.reusableFree)
                live = try DomainCommitPlanner.live(after: layout, previous: current, pageCount: free.highWater)
            }
        }
        if try !DomainCommitPlanner.admit(layout: layout, live: live, previous: current, pageCount: free.highWater, kind: kind, policy: policy),
           !(working.pendingBytes == 0 && layout.catalogPages.count <= current.repairCatalogPages) {
            throw DomainCommitPlanner.refusal(policy)
        }
        return (current, free, layout, live)
    }

    /// Call for every limited changed domain, in attachment-name then main order,
    /// before preparing/writing any domain in a multi-root transaction.
    func precheck(_ working: DomainSnapshot, policy: DomainCommitPolicy) throws { _ = try context(working, policy: policy) }

    func prepare(_ working: DomainSnapshot, policy: DomainCommitPolicy = DomainCommitPolicy(),
                 entropy: KeyEntropy? = nil, verifyLiveCount: Bool = false, forceReclaim: Bool = false) throws -> PreparedDomainCommit? {
        guard let (previous, free, layout, live) = try context(working, policy: policy) else { return nil }
        let codec = pager.codec
        let body = try DomainCommitPlanner.write(working, highWater: free.highWater, free: free.reusableFree, entropy: entropy)
        let written = Set(body.pages.map(\.index))
        guard written == layout.written, body.allocation.highWater == layout.allocation.highWater,
              body.kept == layout.kept, body.keptChains == layout.keptChains else {
            throw JedError.corruption("commit layout disagrees with serialized pages")
        }
        let bytes = Dictionary(uniqueKeysWithValues: body.pages.map { ($0.index, $0.bytes) })
        func read(_ reference: PageReference) throws -> [UInt8] {
            if let stored = bytes[reference.index] { return try codec.openBody(pageIndex: reference.index, sealed: stored, expectedTag: reference.tag) }
            return try pager.readBody(reference)
        }
        let consumed = Set(free.reusableFree).subtracting(body.allocation.remaining)
        let remaining = free.persistedFree.filter { !consumed.contains($0) }
        let reclaim = policy.reclaimAllowed && policy.oldestPinnedVersion >= body.snapshot.txid &&
            (forceReclaim || previous.liveAtCompaction == 0 || DomainCommitPlanner.reclaimDue(pageCount: body.allocation.highWater, free: remaining.count, live: previous.liveAtCompaction))
        var reached: Set<UInt32>?
        if reclaim || verifyLiveCount {
            reached = try PageReachability.collect(body.snapshot, catalogPages: body.catalogPages, gistPages: body.gistPages,
                pageCount: body.allocation.highWater, read: read)
            if verifyLiveCount, reached!.count != Int(live) { throw JedError.corruption("live-page delta disagrees with reachability") }
        }
        var persist = remaining, liveAtCompaction = previous.liveAtCompaction, generation = previous.freeGeneration
        if reclaim {
            var protected = reached!; protected.formUnion(written)
            liveAtCompaction = UInt32(protected.count)
            protected.formUnion(free.pending.map(\.pageIndex))
            persist = (codec.firstBodyPage..<body.allocation.highWater).filter { !protected.contains($0) }
            generation = body.snapshot.txid
        }
        let prepared: PreparedPageCommit
        if kind == .file {
            let allocation = try CommitAllocation.plan(persist: persist, safe: Array(body.allocation.remaining),
                highWater: body.allocation.highWater, dirtyCount: body.pages.count, pendingCount: free.pending.count,
                truncate: free.pending.isEmpty && policy.reclaimAllowed, codec: codec, entropy: entropy)
            prepared = try pager.prepare(body: body.pages + allocation.freeList.pages, root: body.root,
                pageCount: allocation.freeList.highWater, freeList: allocation.freeList.head, livePages: live,
                freePages: allocation.freeList.entries, manifestPages: allocation.manifestIDs,
                reuseAllowed: policy.oldestPinnedVersion >= previous.freeGeneration, entropy: entropy)
        } else {
            prepared = try pager.prepareMemory(body: body.pages, root: body.root, pageCount: body.allocation.highWater,
                freePages: persist, reuseAllowed: policy.oldestPinnedVersion >= previous.freeGeneration,
                reusablePages: free.reusableFree)
        }
        return PreparedDomainCommit(identity: working.identity, pages: prepared, snapshot: body.snapshot,
            catalogPages: body.catalogPages, gistPages: body.gistPages, livePages: live,
            freeGeneration: generation, liveAtCompaction: liveAtCompaction, written: written)
    }

    /// The writer lease stays held across staging, main persistence and root
    /// publication. Dropping this result abandons bytes in otherwise free slots.
    /// A staged snapshot must not become reader-visible before adoption.
    func stageMemory(_ plan: PreparedDomainCommit) throws -> StagedDomainCommit {
        try state.withLock { current in
            guard kind == .memory, plan.identity === current.snapshot.identity,
                  plan.pages.baseTxid == current.snapshot.txid else {
                throw JedError("55006", "invalid or obsolete memory domain stage")
            }
        }
        // The temporary host may spill its current bytes before staging. This
        // happens after every group precheck and before the durable file phase.
        // No storage mutex is held across the host copy or its cache resize.
        let hook = memoryStageHook.withLock { $0 }
        try hook?(plan.pages.manifest.meta.pageCount)
        return try state.withLock { current in
            guard kind == .memory, plan.identity === current.snapshot.identity,
                  plan.pages.baseTxid == current.snapshot.txid else {
                throw JedError("55006", "invalid or obsolete memory domain stage")
            }
            pool.invalidate(Set(plan.pages.body.map(\.index)))
            return try StagedDomainCommit(plan: plan, pages: pager.stageMemory(plan.pages))
        }
    }

    /// Adoption installs accounting only, after the durable domain succeeds.
    /// Registry publication remains the coordinator's atomic operation.
    func adoptMemory(_ staged: StagedDomainCommit) throws -> DomainSnapshot {
        try state.withLock { current in
            let plan = staged.plan
            guard kind == .memory, plan.identity === current.snapshot.identity,
                  plan.pages.baseTxid == current.snapshot.txid else {
                throw JedError("55006", "invalid or obsolete memory domain adoption")
            }
            try pager.adoptMemory(staged.pages)
            current = DomainStorageState(snapshot: plan.snapshot, catalogPages: plan.catalogPages,
                gistPages: plan.gistPages, livePages: plan.livePages, liveAtCompaction: plan.liveAtCompaction,
                freeGeneration: plan.freeGeneration, lastWritten: plan.written, repairCatalogPages: plan.catalogPages.count)
            return current.snapshot
        }
    }

    /// Called while the owning memory core's commit boundary is held. All
    /// failure checks precede another domain's durable point.
    func admitMemory(_ staged: StagedDomainCommit) throws -> AdmittedMemoryDomainCommit {
        try state.withLock { current in
            let plan = staged.plan
            guard kind == .memory, plan.identity === current.snapshot.identity,
                  plan.pages.baseTxid == current.snapshot.txid else {
                throw JedError("55006", "invalid or obsolete staged memory domain")
            }
            return try AdmittedMemoryDomainCommit(plan: plan, pages: pager.admitMemory(staged.pages))
        }
    }
    /// Nonthrowing after pre-admission. The caller retains the same memory
    /// commit boundary and writer lease throughout durable commit/publication.
    func finishMemory(_ admitted: AdmittedMemoryDomainCommit) -> DomainSnapshot {
        state.withLock { current in
            pager.finishMemory(admitted.pages)
            current = admitted.accounting
            return current.snapshot
        }
    }

    func persistRemote(_ plan: PreparedDomainCommit) throws -> DomainSnapshot {
        try state.withLock { current in
            guard plan.identity === current.snapshot.identity, plan.pages.baseTxid == current.snapshot.txid else {
                throw JedError("08006", "remote domain commit is obsolete")
            }
            pool.invalidate(Set(plan.pages.body.map(\.index)).union(plan.pages.manifest.pages.map(\.pageIndex)))
            try pager.adoptRemoteCommit(plan.pages)
            current = DomainStorageState(snapshot: plan.snapshot, catalogPages: plan.catalogPages,
                gistPages: plan.gistPages, livePages: plan.livePages, liveAtCompaction: plan.liveAtCompaction,
                freeGeneration: plan.freeGeneration, lastWritten: plan.written, repairCatalogPages: plan.catalogPages.count)
            return current.snapshot
        }
    }

    /// Storage publication precedes DatabaseCore's registry-protected root swap.
    /// A fallback reader pinned in that interval is counted before the next reuse.
    func persist(_ plan: PreparedDomainCommit, synchronous: Bool = true,
                 syncDirectory: () throws -> Void = {}) throws -> DomainSnapshot {
        try state.withLock { current in
            guard plan.identity === current.snapshot.identity, plan.pages.baseTxid == current.snapshot.txid else {
                throw JedError("55006", "prepared domain commit is obsolete")
            }
            let invalidated = Set(plan.pages.body.map(\.index)).union(plan.pages.manifest.pages.map(\.pageIndex))
            pool.invalidate(invalidated)
            if kind == .file {
                try pager.commit(plan.pages, synchronous: synchronous) {
                    try durableDirectorySync(); try syncDirectory()
                }
            }
            else { try pager.commitMemory(plan.pages) }
            current = DomainStorageState(snapshot: plan.snapshot, catalogPages: plan.catalogPages,
                gistPages: plan.gistPages, livePages: plan.livePages, liveAtCompaction: plan.liveAtCompaction,
                freeGeneration: plan.freeGeneration, lastWritten: plan.written, repairCatalogPages: plan.catalogPages.count)
            return current.snapshot
        }
    }
}
