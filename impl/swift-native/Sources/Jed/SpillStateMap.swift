/// Equality-keyed finite operator state. Output order is always carried by the
/// caller's separate first-occurrence spool, never by this map's bucket order.
internal final class SpillStateMap {
    private struct Entry { let keyBytes: Int64; var value: [SQLValue]; var valueBytes: Int64 }
    /// An encoded key hashed a word at a time. It is the same seeded hash as
    /// the array's own, fed eight bytes per step instead of one.
    fileprivate struct Key: Hashable {
        let bytes: [UInt8]
        static func == (a: Key, b: Key) -> Bool { a.bytes == b.bytes }
        func hash(into hasher: inout Hasher) {
            let count = bytes.count
            var index = 0
            while index + 8 <= count {
                var word: UInt64 = 0
                for offset in 0..<8 { word = word << 8 | UInt64(bytes[index + offset]) }
                hasher.combine(word); index += 8
            }
            var tail: UInt64 = 0
            while index < count { tail = tail << 8 | UInt64(bytes[index]); index += 1 }
            hasher.combine(tail); hasher.combine(count)
        }
    }
    /// One key's encoding and resident entry, read by `find` so that the
    /// `put` which follows repeats neither. It is valid until this map is
    /// next changed.
    struct Found {
        fileprivate let key: Key
        fileprivate let resident: (keyBytes: Int64, valueBytes: Int64)?
        fileprivate let spilled: Bool
        let value: [SQLValue]?
    }
    private let config: SpillConfig
    private let charge: SpillCharge
    private var resident: [Key: Entry] = [:]
    private var disk: SpillHashStore?
    private var closed = false, failed = false, sealed = false
    private(set) var residentBytes: Int64 = 0
    var isSpilled: Bool { disk != nil }
    init(config: SpillConfig) { self.config = config; charge = SpillCharge(meter: config.meter) }
    private func checkOpen() throws {
        guard !closed, !failed else { throw JedError("58030", "spill state map is closed or unusable") }
    }
    private func tupleBytes(_ values: [SQLValue]) -> Int64 {
        values.reduce(0) { saturatingAdd($0, MemorySizes.value($1)) }
    }
    func get(_ key: [SQLValue]) throws -> [SQLValue]? { try find(key).value }
    /// `get`, keeping what the matching `put(_:key:value:)` needs.
    func find(_ key: [SQLValue]) throws -> Found {
        try checkOpen()
        let bytes = try ValueEqualityKey.encode(key)
        if let disk {
            do {
                guard let encoded = try disk.get(bytes, hash: SpillHashStore.hash(bytes)) else {
                    return Found(key: Key(bytes: bytes), resident: nil, spilled: true, value: nil)
                }
                return Found(key: Key(bytes: bytes), resident: nil, spilled: true, value: try SpillRowCodec.decode(encoded))
            } catch { failed = true; throw error }
        }
        let wrapped = Key(bytes: bytes)
        guard let entry = resident[wrapped] else { return Found(key: wrapped, resident: nil, spilled: false, value: nil) }
        return Found(key: wrapped, resident: (entry.keyBytes, entry.valueBytes), spilled: false, value: entry.value)
    }
    func insert(_ key: [SQLValue]) throws -> Bool {
        if try get(key) != nil { return false }; try put(key, value: []); return true
    }
    func put(_ key: [SQLValue], value: [SQLValue]) throws {
        try checkOpen()
        guard !sealed else { throw JedError("58030", "spill state map charge already released") }
        let wrapped = Key(bytes: try ValueEqualityKey.encode(key))
        try store(wrapped, old: disk == nil ? resident[wrapped].map { ($0.keyBytes, $0.valueBytes) } : nil, key: key, value: value)
    }
    /// `put` for the key of a `find` made since this map last changed.
    func put(_ found: Found, key: [SQLValue], value: [SQLValue]) throws {
        try checkOpen()
        guard !sealed else { throw JedError("58030", "spill state map charge already released") }
        guard found.spilled == (disk != nil) else { throw JedError("58030", "spill state map changed under a held key") }
        try store(found.key, old: found.resident, key: key, value: value)
    }
    private func store(_ wrapped: Key, old: (keyBytes: Int64, valueBytes: Int64)?, key: [SQLValue], value: [SQLValue]) throws {
        let bytes = wrapped.bytes
        do {
            if let disk {
                try disk.put(bytes, value: SpillRowCodec.encode(value), hash: SpillHashStore.hash(bytes)); return
            }
            let keyBytes = old?.keyBytes ?? tupleBytes(key), valueBytes = tupleBytes(value)
            let added: Int64, removed: Int64
            if let old {
                added = max(0, valueBytes - old.valueBytes); removed = max(0, old.valueBytes - valueBytes)
            } else {
                added = saturatingAdd(GeneratedSpec.memoryEntry, saturatingAdd(keyBytes, valueBytes)); removed = 0
            }
            let admitted: Bool
            if config.canSpill { admitted = charge.tryReserve(added) }
            else { try charge.reserve(added); admitted = true }
            let nextBytes = saturatingAdd(residentBytes, added) - removed
            if config.canSpill && (!admitted || nextBytes > config.workMemoryBytes) {
                let target = try SpillHashStore(host: config.host!)
                do {
                    for (key, entry) in resident {
                        try target.put(key.bytes, value: SpillRowCodec.encode(entry.value), hash: SpillHashStore.hash(key.bytes))
                    }
                    try target.put(bytes, value: SpillRowCodec.encode(value), hash: SpillHashStore.hash(bytes))
                } catch { target.close(); throw error }
                disk = target; resident = [:]; residentBytes = 0; charge.releaseAll()
            } else {
                if removed > 0 { charge.release(removed) }
                resident[wrapped] = Entry(keyBytes: keyBytes, value: value, valueBytes: valueBytes)
                residentBytes = nextBytes
            }
        } catch { failed = true; throw error }
    }
    func releaseCharge() { sealed = true; charge.releaseAll() }
    func close() {
        guard !closed else { return }; closed = true
        disk?.close(); disk = nil; resident = [:]; residentBytes = 0; charge.releaseAll()
    }
    deinit { close() }
}
