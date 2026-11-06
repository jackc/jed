/// Equality-keyed finite operator state. Output order is always carried by the
/// caller's separate first-occurrence spool, never by this map's bucket order.
internal final class SpillStateMap {
    private struct Entry { let keyBytes: Int64; var value: [SQLValue]; var valueBytes: Int64 }
    private let config: SpillConfig
    private let charge: SpillCharge
    private var resident: [[UInt8]: Entry] = [:]
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
    func get(_ key: [SQLValue]) throws -> [SQLValue]? {
        try checkOpen()
        let bytes = try ValueEqualityKey.encode(key)
        if let disk {
            do {
                guard let encoded = try disk.get(bytes, hash: SpillHashStore.hash(bytes)) else { return nil }
                return try SpillRowCodec.decode(encoded)
            } catch { failed = true; throw error }
        }
        return resident[bytes]?.value
    }
    func insert(_ key: [SQLValue]) throws -> Bool {
        if try get(key) != nil { return false }; try put(key, value: []); return true
    }
    func put(_ key: [SQLValue], value: [SQLValue]) throws {
        try checkOpen()
        guard !sealed else { throw JedError("58030", "spill state map charge already released") }
        let bytes = try ValueEqualityKey.encode(key)
        do {
            if let disk {
                try disk.put(bytes, value: SpillRowCodec.encode(value), hash: SpillHashStore.hash(bytes)); return
            }
            let old = resident[bytes], keyBytes = old?.keyBytes ?? tupleBytes(key), valueBytes = tupleBytes(value)
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
                        try target.put(key, value: SpillRowCodec.encode(entry.value), hash: SpillHashStore.hash(key))
                    }
                    try target.put(bytes, value: SpillRowCodec.encode(value), hash: SpillHashStore.hash(bytes))
                } catch { target.close(); throw error }
                disk = target; resident = [:]; residentBytes = 0; charge.releaseAll()
            } else {
                if removed > 0 { charge.release(removed) }
                resident[bytes] = Entry(keyBytes: keyBytes, value: value, valueBytes: valueBytes)
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
