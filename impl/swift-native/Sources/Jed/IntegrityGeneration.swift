internal extension IntegrityChecker {
    func checkGeneration() throws {
        guard let slots = generation.metaSlots else { return }
        let selected = generation.selected
        let slot = slots.firstIndex(of: selected.bytes).map(UInt32.init)
        if slot == nil { try add("meta_invalid", page: selected.slot) }
        var last: UInt32 = slot ?? selected.slot
        do {
            let validated = try MetaRecovery.validateManifest(meta, bytes: selected.bytes) { index in
                try self.poll(); last = index; return try self.pager.readStored(index)
            }
            protected = Set(validated.entries.map(\.pageIndex)).union(validated.chain)
        } catch let error as JedError where error.code == "XX001" {
            try add("manifest_page_mismatch", page: last, error.message)
        }
    }
    func checkCatalog() throws {
        var current = meta.root, last = current.index
        var stored: [(UInt32, [UInt8])] = []
        let start = meta.encrypted ? 32 : 16
        do {
            let decoded = try CatalogChain.read(root: current, pageCount: meta.pageCount, codec: codec) { reference in
                last = reference.index; try self.poll()
                guard !self.marked(reference.index) else { throw JedError.corruption("catalog page repeated") }
                self.mark(reference.index)
                let bytes = try self.body(reference, cached: false)
                stored.append((reference.index, bytes)); return bytes
            }
            for entry in decoded.entries {
                if case .table(let table) = entry {
                    for index in table.indexes where index.kind == 2 { gistRoots[index.name] = index.root }
                }
            }
        } catch let error as JedError where error.code == "XX001" {
            damaged = true
            // A link rejected before the callback still names the bad page.
            if let previous = stored.last {
                current = PageReference(try PageEnvelope.readU32(previous.1, at: 8))
                if current.index != 0 && (current.index < meta.firstBodyPage || current.index >= meta.pageCount || marked(current.index)) {
                    last = current.index
                }
            }
            try add("catalog_page_corrupt", page: last, error.message); return
        }
        do {
            let expected = try CatalogChain.layout(snapshot.catalog.entries(), codec: codec)
            for ordinal in expected.indices {
                guard ordinal < stored.count else { try add("catalog_mismatch", page: stored.last?.0 ?? meta.root.index); return }
                let (index, bytes) = stored[ordinal], wanted = expected[ordinal]
                let content = bytes[start...]
                let storedCount = try PageEnvelope.readU32(bytes, at: 4)
                if bytes[1] != (wanted.continued ? 1 : 0) || storedCount != wanted.count ||
                    content.count < wanted.bytes.count || !content.prefix(wanted.bytes.count).elementsEqual(wanted.bytes) ||
                    !content.dropFirst(wanted.bytes.count).allSatisfy({ $0 == 0 }) {
                    try add("catalog_mismatch", page: index); return
                }
            }
            if stored.count > expected.count { try add("catalog_mismatch", page: stored[expected.count].0) }
        } catch let error as JedError where error.code == "XX001" {
            try add("catalog_mismatch", page: meta.root.index, error.message)
        }
    }
    func checkFreeList() throws {
        var entries: [UInt32] = []
        if generation.memory { entries = generation.freePages }
        else {
            var current = meta.freeList, chain: Set<UInt32> = []
            let start = meta.encrypted ? 32 : 16
            while current.index != 0 {
                try poll()
                guard current.index >= meta.firstBodyPage, current.index < meta.pageCount,
                      chain.insert(current.index).inserted else {
                    try add("free_list_corrupt", page: current.index); break
                }
                if marked(current.index) { try add("free_page_live", page: current.index) }
                do {
                    let bytes = try body(current, cached: false)
                    guard bytes[0] == 7 else { throw JedError.corruption("wrong free-list page type") }
                    let count = Int(try PageEnvelope.readU32(bytes, at: 4))
                    guard count <= (bytes.count - start) / 4 else { throw JedError.corruption("invalid free-list extent") }
                    var reader = ByteReader(bytes[start...])
                    for _ in 0..<count { entries.append(try reader.u32()) }
                    let next = try PageEnvelope.readU32(bytes, at: 8), tag = meta.encrypted ? Array(bytes[16..<32]) : []
                    guard next != 0 || tag.allSatisfy({ $0 == 0 }) else { throw JedError.corruption("invalid free-list terminator") }
                    report.pages += 1
                    current = PageReference(next, tag: tag)
                } catch let error as JedError where error.code == "XX001" {
                    try add("free_list_corrupt", page: current.index, error.message); break
                }
            }
        }
        var seen = [UInt8](repeating: 0, count: reached.count)
        for page in entries {
            try poll()
            if page < meta.firstBodyPage || page >= meta.pageCount { try add("free_page_out_of_range", page: page); continue }
            let slot = Int(page / 8), bit: UInt8 = 1 << (page % 8)
            if seen[slot] & bit != 0 { try add("free_page_duplicate", page: page) }
            else if marked(page) { try add("free_page_live", page: page) }
            else if protected.contains(page) { try add("free_page_manifest_dependency", page: page) }
            seen[slot] |= bit
        }
    }
}
