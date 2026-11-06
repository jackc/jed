import Foundation

internal enum BackupForm: Sendable { case pageIdentical, compacted }
internal struct PinnedBackup: Sendable {
    let pin: SnapshotPin
    let selected: ValidatedMeta
    let kind: StorageDomainKind
}

/// Tooling storage operation: no SQL access path or application API promise.
/// A backup owns an ordinary reader pin and at most one page payload at a time.
internal enum BackupImage {
    static func write(_ source: PinnedBackup, form: BackupForm, entropy: KeyEntropy?,
                      sink: (StoredPage) throws -> Void) throws -> DomainImageSummary {
        let snapshot = source.pin.snapshot
        guard let pager = snapshot.pool.pager else { throw JedError("58030", "backup source has no page device") }
        let codec = pager.codec, meta = source.selected.meta
        try source.pin.checkRole()
        if form == .pageIdentical, source.kind == .memory {
            throw JedError("0A000", "page-identical backup requires a file database")
        }
        let keyBlock: KeyBlock?
        if meta.encrypted {
            let page2 = try pager.readStored(2), page3 = try pager.readStored(3)
            keyBlock = try KeyBlock.select(page2: page2, page3: page3, pageSize: codec.pageSize).block
        } else { keyBlock = nil }
        func emit(_ page: StoredPage) throws { try source.pin.checkRole(); try sink(page) }
        if form == .compacted {
            guard snapshot.txid < UInt64.max else { throw JedError.corruption("transaction id exhausted") }
            return try DomainImage.write(snapshot, txid: snapshot.txid + 1, keyBlock: keyBlock, entropy: entropy, sink: emit)
        }
        var copied: Set<UInt32> = []
        func copy(_ reference: PageReference) throws -> [UInt8] {
            guard reference.index >= codec.firstBodyPage, reference.index < meta.pageCount,
                  copied.insert(reference.index).inserted else { throw JedError.corruption("invalid or multiply referenced backup page") }
            try source.pin.checkRole()
            let stored = try pager.readStored(reference.index)
            let clear = try codec.openBody(pageIndex: reference.index, sealed: stored, expectedTag: reference.tag)
            try emit(StoredPage(index: reference.index, bytes: stored)); return clear
        }
        let catalog = try CatalogChain.read(root: meta.root, pageCount: meta.pageCount, codec: codec, read: copy)
        var gistPages: [UInt32] = []
        func gist(_ reference: PageReference, depth: Int) throws {
            if reference.index == 0 { return }
            guard depth <= GistTree.maximumDepth else { throw JedError.corruption("backup GiST nesting exceeds limit") }
            let bytes = try copy(reference)
            guard bytes[0] == 5 || bytes[0] == 6 else { throw JedError.corruption("invalid backup GiST page") }
            let count = Int(try PageEnvelope.readU32(bytes, at: 4))
            guard count > 0, count <= GistTree.fanout else { throw JedError.corruption("invalid backup GiST fanout") }
            gistPages.append(reference.index)
            if bytes[0] == 5 { return }
            var reader = ByteReader(bytes[16...]), children: [PageReference] = []
            for _ in 0..<count {
                let boundSize = Int(try reader.u16())
                _ = try reader.take(boundSize)
                let index = try reader.u32(), tag = Array(try reader.take(codec.reservedBytes == 0 ? 0 : 16))
                guard index != 0 else { throw JedError.corruption("null backup GiST child") }
                children.append(PageReference(index, tag: tag))
            }
            guard try reader.take(reader.remaining).allSatisfy({ $0 == 0 }) else { throw JedError.corruption("nonzero backup GiST padding") }
            for child in children { try gist(child, depth: depth + 1) }
        }
        for table in snapshot.catalog.tables.sorted(by: { $0.name < $1.name }) {
            for index in table.indexes.sorted(by: { $0.name < $1.name }) where index.kind == 2 { try gist(index.root, depth: 1) }
        }
        let reached = try PageReachability.collect(snapshot, catalogPages: catalog.pages, gistPages: gistPages,
            pageCount: meta.pageCount, readAllPages: true, read: copy)
        guard reached.count == Int(meta.livePages) else { throw JedError.corruption("backup live-page count differs from metadata") }
        let free = try FreeList.load(meta: meta, protected: reached.union(source.selected.protectedPages), read: copy)
        guard free.chain.allSatisfy({ !reached.contains($0) }) else { throw JedError.corruption("backup free-list chain overlaps live pages") }
        // Dead pages and the source commit manifest are deliberately absent
        // from a bootstrap copy, including holes below its original high water.
        let zero = [UInt8](repeating: 0, count: codec.pageSize)
        for index in codec.firstBodyPage..<meta.pageCount where !copied.contains(index) {
            try emit(StoredPage(index: index, bytes: zero))
        }
        if let keyBlock {
            let bytes = try keyBlock.encode(pageSize: codec.pageSize)
            try emit(StoredPage(index: 2, bytes: bytes)); try emit(StoredPage(index: 3, bytes: bytes))
        }
        let manifest = try CommitManifest.build(pageSize: meta.pageSize, encrypted: meta.encrypted,
            txid: meta.txid, root: meta.root, pageCount: meta.pageCount, freeList: meta.freeList,
            livePages: meta.livePages, entries: [], chain: [])
        let clear = try manifest.meta.encode(inline: [])
        for slot: UInt32 in 0...1 {
            let bytes: [UInt8]
            if let encrypted = codec as? EncryptedPageCodec {
                guard let entropy else { throw JedError("58030", "encrypted backup requires key entropy") }
                bytes = try encrypted.sealMeta(clear, slot: slot, nonce: EncryptionPolicy.entropy(24, from: entropy))
            } else { bytes = clear }
            try emit(StoredPage(index: slot, bytes: bytes))
        }
        return DomainImageSummary(root: meta.root, pageCount: meta.pageCount, livePages: meta.livePages)
    }
}

extension Database {
    internal func backup(_ destination: String, form: BackupForm) throws {
        try checkOpen()
        let source = try core.beginBackup(); defer { source.pin.close() }
        #if os(Linux) || canImport(Darwin)
        var skip = (host as? FileDatabaseHost)?.skipsFsync ?? false
        #if os(Linux)
        if let service = host as? ServiceDatabaseHost { skip = service.skipsFsync }
        #endif
        try FileBackupHost.write(destination: destination, skipFsync: skip,
            operations: (host as? any DatabaseFilesystemProvider)?.fileOperations ?? localFileHost?.fileOperations ?? NativeFileOperations(),
            checkRole: { try source.pin.checkRole() }) { store in
            let image = try BackupImage.write(source, form: form, entropy: keyEntropy) { page in
                try store.write(at: UInt64(page.index) * UInt64(page.bytes.count), bytes: page.bytes)
            }
            try store.setSize(UInt64(image.pageCount) * UInt64(source.selected.meta.pageSize))
        }
        #else
        throw JedError("0A000", "native backup files are unavailable on this host")
        #endif
    }
}
