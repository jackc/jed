/// Host-only maintenance. SQL transactions never invoke these entry points;
/// each call either acquires the writer gate immediately or returns 55006.
extension Database {
    private func maintenanceStorage(_ name: String) throws -> StorageDomain {
        try checkOpen()
        guard name == "main" else { throw JedError("42704", "database is not attached: \(name)") }
        let storage = core.storage
        guard !storage.readOnly else { throw JedError("25006", "database is read-only") }
        return storage
    }

    public func compact(_ name: String = "main") throws {
        let expected = try maintenanceStorage(name)
        #if os(Linux) || canImport(Darwin)
        if expected.kind == .file, !(host is FileDatabaseHost) {
            throw JedError("0A000", "file host cannot atomically replace this database")
        }
        #else
        guard expected.kind == .memory else { throw JedError("0A000", "file replacement is unavailable") }
        #endif
        try core.replaceStorage { storage, snapshot in
            try checkOpen()
            guard storage === expected else { throw JedError("55006", "database changed concurrently") }
            try storage.pager.checkWritable()
            guard snapshot.txid < UInt64.max else { throw JedError.corruption("transaction id exhausted") }
            if storage.kind == .memory {
                let target = MemoryBlockStore()
                let image = try DomainImage.write(snapshot, txid: snapshot.txid + 1) { page in
                    try target.write(at: UInt64(page.index) * UInt64(storage.pager.pageSize), bytes: page.bytes)
                }
                try target.setSize(UInt64(image.pageCount) * UInt64(storage.pager.pageSize))
                let replacement = try StorageDomain.open(store: target, kind: .memory, catalogID: snapshot.catalogID,
                    collatedKey: { try self.environment.unicode.key($0, collation: $1) },
                    collationVerdict: { try CatalogEnvironment.verdict($0, environment: self.environment) },
                    validateCatalog: { try CatalogEnvironment.validate($0, environment: self.environment) })
                replacement.pager.acknowledgeFreshDurableImage()
                storage.pager.revoke()
                return replacement
            }
            #if os(Linux) || canImport(Darwin)
            return try (host as! FileDatabaseHost).replace(source: storage, snapshot: snapshot, txid: snapshot.txid + 1,
                codec: storage.pager.codec, keyBlock: storage.pager.selectedKeyBlock,
                entropy: keyEntropy, environment: self.environment)
            #else
            throw JedError("0A000", "file replacement is unavailable")
            #endif
        }
    }

    public func changeCredential(_ name: String = "main", credential: Credential,
                                 kdf: Argon2Parameters = .default) throws {
        let expected = try maintenanceStorage(name)
        guard expected.kind == .file else { throw JedError("55000", "in-memory databases cannot change credentials") }
        guard let codec = expected.pager.codec as? EncryptedPageCodec,
              let selected = expected.pager.selectedKeyBlock else { throw JedError("28P03", "database is not encrypted") }
        #if os(Linux) || canImport(Darwin)
        guard let host = host as? FileDatabaseHost else { throw JedError("55000", "file host cannot change credentials") }
        // Expensive KDF work and key-stream draws deliberately precede the gate.
        let replacement = try codec.rewrap(selected, credential: credential, parameters: kdf, entropy: keyEntropy)
        try core.maintenance {
            try checkOpen()
            guard core.storage === expected else { throw JedError("55006", "database changed concurrently") }
            do { try expected.pager.changeKeyBlock(replacement, replacing: selected, syncDirectory: host.syncDirectory) }
            catch { if expected.pager.isPoisoned { host.poison() }; throw error }
        }
        #else
        throw JedError("55000", "file credential changes are unavailable")
        #endif
    }

    public func rekey(_ name: String = "main", credential: Credential?,
                      kdf: Argon2Parameters = .default) throws {
        let expected = try maintenanceStorage(name)
        guard expected.kind == .file else { throw JedError("55000", "in-memory databases cannot be rekeyed") }
        #if os(Linux) || canImport(Darwin)
        guard let host = host as? FileDatabaseHost else { throw JedError("55000", "file host cannot rekey this database") }
        if credential != nil {
            guard expected.pager.pageSize >= 1024 else { throw JedError("0A000", "encryption requires pages of at least 1024 bytes") }
            guard host.scratch.sealed else { throw JedError("55000", "encrypting requires sealed scratch storage") }
        }
        let random = expected.pager.codec.reservedBytes == 0 ? environment.newKeySource() : currentKeySource
        let entropy: KeyEntropy = { try random.bytes(count: $0) }
        let codec: any PageCodec
        let keyBlock: KeyBlock?
        if let credential {
            let created = try KeyBlock.create(credential: credential, parameters: kdf, entropy: entropy)
            codec = try EncryptedPageCodec(pageSize: expected.pager.pageSize, dataKey: created.dataKey)
            keyBlock = created.block
        } else { codec = try PlainPageCodec(pageSize: expected.pager.pageSize); keyBlock = nil }
        try core.replaceStorage { storage, snapshot in
            try checkOpen()
            guard storage === expected else { throw JedError("55006", "database changed concurrently") }
            try storage.pager.checkWritable()
            guard snapshot.txid < UInt64.max else { throw JedError.corruption("transaction id exhausted") }
            let capacity = storage.pool.capacity ?? (Int.max / codec.pageSize)
            let budget = max(1_048_576, capacity * codec.pageSize)
            let temporary = try SpillableScratchStore(budget: Int64(budget), host: host.scratch)
            defer { try? temporary.close() }
            let scratchCodec: any PageCodec
            let scratchEntropy: KeyEntropy?
            if keyBlock != nil {
                let source = OSRandomSource()
                scratchCodec = try EncryptedPageCodec(pageSize: codec.pageSize, dataKey: source.bytes(count: 32))
                scratchEntropy = { try source.bytes(count: $0) }
            } else { scratchCodec = try PlainPageCodec(pageSize: codec.pageSize); scratchEntropy = nil }
            var scratch = try ScratchDomain.create(store: temporary, codec: scratchCodec, cacheBytes: budget, entropy: scratchEntropy)
            let rebuilt = try DomainRebuilder.rebuild(snapshot, scratch: &scratch, stageBytes: Int64(budget / 4))
            let replacement = try host.replace(source: storage, snapshot: rebuilt, txid: snapshot.txid + 1,
                codec: codec, keyBlock: keyBlock, entropy: entropy, environment: environment, limit: maxStorageBytes)
            installKeySource(random)
            return replacement
        }
        #else
        throw JedError("55000", "file rekeying is unavailable")
        #endif
    }
}
