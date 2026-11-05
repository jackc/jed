/// Deterministic lowest-free-first allocation. Snapshot reader-watermark and
/// batch-floor filtering happen before constructing the allocator.
internal struct PageAllocator {
    private let free: [UInt32]
    private var offset = 0
    private(set) var highWater: UInt32

    init(highWater: UInt32, free: [UInt32], firstBody: UInt32) throws {
        var previous: UInt32 = 0
        for page in free {
            guard page >= firstBody, page < highWater, page > previous else {
                throw JedError.corruption("invalid allocator free-page order")
            }
            previous = page
        }
        self.highWater = highWater; self.free = free
    }

    mutating func allocate() throws -> UInt32 {
        if offset < free.count { defer { offset += 1 }; return free[offset] }
        guard highWater < UInt32.max else { throw JedError("54000", "database page limit exceeded") }
        defer { highWater += 1 }
        return highWater
    }

    var remaining: ArraySlice<UInt32> { free[offset...] }

    static func trimTrailing(safe: [UInt32], highWater: UInt32) -> (safe: [UInt32], highWater: UInt32) {
        var count = safe.count, high = highWater
        while count > 0, high > 0, safe[count - 1] == high - 1 { count -= 1; high -= 1 }
        return (Array(safe.prefix(count)), high)
    }
}

internal enum PageSealer {
    static func nullReference(_ codec: any PageCodec) -> PageReference {
        PageReference(0, tag: codec.reservedBytes == 0 ? [] : Array(repeating: 0, count: 16))
    }

    /// Chain successor tags precede payload; parent callers receive the sealed
    /// tag so they can bind it without decrypting or rereading the child.
    static func seal(codec: any PageCodec, index: UInt32, type: UInt8, flags: UInt8 = 0,
                     count: UInt32, next: PageReference? = nil, payload: [UInt8],
                     entropy: KeyEntropy? = nil) throws -> (page: StoredPage, reference: PageReference) {
        let successor = next ?? nullReference(codec)
        let encrypted = codec.reservedBytes != 0
        let chain = type == 1 || type == 4 || type == 7
        guard encrypted ? successor.tag.count == 16 : successor.tag.isEmpty,
              successor.index != 0 || successor.tag.allSatisfy({ $0 == 0 }),
              chain || successor.index == 0,
              payload.count <= codec.capacity - (chain && encrypted ? 16 : 0) else {
            throw JedError.corruption("invalid logical page payload or successor")
        }
        var writer = ByteWriter()
        writer.u8(type); writer.u8(flags); writer.u16(0); writer.u32(count)
        writer.u32(successor.index); writer.u32(0)
        if chain && encrypted { writer.append(successor.tag) }
        writer.append(payload)
        writer.append(repeatElement(UInt8(0), count: codec.pageSize - codec.reservedBytes - writer.bytes.count))
        let nonce: [UInt8]
        if encrypted {
            guard let entropy else { throw JedError("58030", "encrypted page requires key entropy") }
            nonce = try EncryptionPolicy.entropy(24, from: entropy)
        } else { nonce = [] }
        let sealed = try codec.sealBody(pageIndex: index, plaintext: writer.bytes, salt: nonce)
        return (StoredPage(index: index, bytes: sealed.bytes), PageReference(index, tag: sealed.tag))
    }
}

internal struct FreeListWrite {
    let pages: [StoredPage]
    let head: PageReference
    let entries: [UInt32]
    let highWater: UInt32
}

internal extension FreeList {
    /// `persist` may include newly reclaimed pages; `safe` includes only pages
    /// reusable at the fallback and reader watermark and is a subset of it.
    static func serialize(persist: [UInt32], safe: [UInt32], highWater: UInt32,
                          codec: any PageCodec, entropy: KeyEntropy? = nil) throws -> FreeListWrite {
        let candidates = Set(persist)
        guard candidates.count == persist.count, zip(persist, persist.dropFirst()).allSatisfy({ $0 < $1 }),
              persist.allSatisfy({ $0 >= codec.firstBodyPage && $0 < highWater }),
              safe.allSatisfy({ candidates.contains($0) }) else {
            throw JedError.corruption("invalid free-list serialization candidates")
        }
        var allocator = try PageAllocator(highWater: highWater, free: safe, firstBody: codec.firstBodyPage)
        // The established format allocator keeps a lone residue in memory; a
        // chain would consume the same space it records. Reclamation recovers it.
        guard persist.count >= 2 else {
            return FreeListWrite(pages: [], head: PageSealer.nullReference(codec), entries: persist, highWater: highWater)
        }
        let perPage = (codec.capacity - (codec.reservedBytes == 0 ? 0 : 16)) / 4
        var ids: [UInt32] = [], drawn = 0
        while (persist.count - drawn + perPage - 1) / perPage > ids.count {
            let index = try allocator.allocate()
            ids.append(index)
            if index < highWater { drawn += 1 }
        }
        let used = Set(ids.prefix(drawn))
        let entries = persist.filter { !used.contains($0) }
        var reverse: [StoredPage] = []
        var next = PageSealer.nullReference(codec)
        for ordinal in ids.indices.reversed() {
            let lower = min(entries.count, ordinal * perPage), upper = min(entries.count, (ordinal + 1) * perPage)
            var payload = ByteWriter()
            for index in entries[lower..<upper] { payload.u32(index) }
            let sealed = try PageSealer.seal(codec: codec, index: ids[ordinal], type: 7,
                count: UInt32(upper - lower), next: next, payload: payload.bytes, entropy: entropy)
            reverse.append(sealed.page); next = sealed.reference
        }
        return FreeListWrite(pages: reverse.reversed(), head: next, entries: entries, highWater: allocator.highWater)
    }
}

internal struct CommitAllocation {
    let freeList: FreeListWrite
    let manifestIDs: [UInt32]

    /// Monotone reservation from validated-cow.md §2. Reclamation is computed
    /// once by the tree owner before entry; this loop only plans persisted lists.
    static func plan(persist: [UInt32], safe: [UInt32], highWater: UInt32,
                     dirtyCount: Int, pendingCount: Int, truncate: Bool,
                     codec: any PageCodec, entropy: KeyEntropy? = nil) throws -> CommitAllocation {
        guard dirtyCount >= 0, pendingCount >= 0, dirtyCount <= UInt32.max, pendingCount <= UInt32.max else {
            throw JedError("54000", "database page limit exceeded")
        }
        let trimmed = truncate ? PageAllocator.trimTrailing(safe: safe, highWater: highWater) : (safe, highWater)
        let candidates = persist.filter { $0 < trimmed.1 }
        let inline = (codec.pageSize - (codec.reservedBytes == 0 ? 64 : 136)) / 12
        let overflow = (codec.pageSize - 32) / 12
        var reserved = 0
        while true {
            var allocator = try PageAllocator(highWater: trimmed.1, free: trimmed.0, firstBody: codec.firstBodyPage)
            var ids: [UInt32] = []
            for _ in 0..<reserved { ids.append(try allocator.allocate()) }
            let held = Set(ids)
            let list = try FreeList.serialize(persist: candidates.filter { !held.contains($0) },
                safe: Array(allocator.remaining), highWater: allocator.highWater, codec: codec, entropy: entropy)
            let count = Int64(dirtyCount) + Int64(pendingCount) + Int64(list.pages.count)
            guard count <= UInt32.max else { throw JedError("54000", "database page limit exceeded") }
            let needed = max(0, (Int(count) - inline + overflow - 1) / overflow)
            if needed > reserved { reserved = needed; continue }
            return CommitAllocation(freeList: list, manifestIDs: ids)
        }
    }
}
