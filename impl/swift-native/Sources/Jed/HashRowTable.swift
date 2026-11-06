/// Build then replay matching rows in insertion order. The full SQL key is
/// compared by the consumer: equal hashes are only candidate membership.
internal final class HashRowTable {
    private let config: SpillConfig
    private let charge: SpillCharge
    private var resident: [UInt64: [[SQLValue]]] = [:]
    private var disk: SpillRowChains?
    private var closed = false, failed = false, sealed = false
    private(set) var residentBytes: Int64 = 0
    var isSpilled: Bool { disk != nil }
    init(config: SpillConfig) { self.config = config; charge = SpillCharge(meter: config.meter) }
    fileprivate func checkOpen() throws {
        guard !closed, !failed else { throw JedError("58030", "hash row table is closed or unusable") }
    }
    func append(hash: UInt64, row: [SQLValue]) throws {
        try checkOpen()
        guard !sealed else { throw JedError("58030", "hash row table build already finished") }
        do {
            if let disk { try disk.append(hash: hash, row: row); return }
            let bytes = saturatingAdd(GeneratedSpec.memoryEntry, MemorySizes.row(row))
            let admitted: Bool
            if config.canSpill { admitted = charge.tryReserve(bytes) }
            else { try charge.reserve(bytes); admitted = true }
            let nextBytes = saturatingAdd(residentBytes, bytes)
            if config.canSpill && (!admitted || nextBytes > config.workMemoryBytes) {
                let target = try SpillRowChains(host: config.host!)
                do {
                    for (hash, rows) in resident { for row in rows { try target.append(hash: hash, row: row) } }
                    try target.append(hash: hash, row: row)
                } catch { target.close(); throw error }
                disk = target; resident = [:]; residentBytes = 0; charge.releaseAll()
            } else { resident[hash, default: []].append(row); residentBytes = nextBytes }
        } catch { failed = true; throw error }
    }
    func cursor(hash: UInt64) throws -> HashRowCursor {
        try checkOpen(); sealed = true
        if let disk {
            do {
                let chain = try disk.head(hash: hash)
                return HashRowCursor(owner: self, hash: hash, offset: chain.offset, count: chain.count, disk: true)
            } catch { failed = true; throw error }
        }
        return HashRowCursor(owner: self, hash: hash, offset: 0, count: UInt64(resident[hash]?.count ?? 0), disk: false)
    }
    fileprivate func row(hash: UInt64, index: UInt64) throws -> [SQLValue] {
        try checkOpen()
        guard let rows = resident[hash], index < UInt64(rows.count) else { throw JedError("58030", "invalid resident hash cursor") }
        return rows[Int(index)]
    }
    fileprivate func row(at offset: UInt64) throws -> (row: [SQLValue], next: UInt64) {
        try checkOpen()
        guard let disk else { throw JedError("58030", "missing spilled hash rows") }
        do { return try disk.row(at: offset) } catch { failed = true; throw error }
    }
    func releaseCharge() { sealed = true; charge.releaseAll() }
    func close() {
        guard !closed else { return }; closed = true
        disk?.close(); disk = nil; resident = [:]; residentBytes = 0; charge.releaseAll()
    }
    deinit { close() }
}

internal final class HashRowCursor {
    private var owner: HashRowTable?
    private let hash: UInt64, count: UInt64
    private let disk: Bool
    private var offset: UInt64, position: UInt64 = 0
    fileprivate init(owner: HashRowTable, hash: UInt64, offset: UInt64, count: UInt64, disk: Bool) {
        self.owner = owner; self.hash = hash; self.offset = offset; self.count = count; self.disk = disk
    }
    func next() throws -> [SQLValue]? {
        guard let owner else { return nil }
        do {
            try owner.checkOpen()
            guard position < count else {
                if disk && offset != 0 { throw JedError("58030", "scratch row chain exceeds its count") }
                close(); return nil
            }
            let row: [SQLValue]
            if disk {
                let record = try owner.row(at: offset); row = record.row; offset = record.next
            } else { row = try owner.row(hash: hash, index: position) }
            position += 1; return row
        } catch { close(); throw error }
    }
    func close() { owner = nil }
}

/// Three private files regardless of key/row count: one linked row stream and
/// the hash map's disk directory/data. Only the current decoded row is held.
private final class SpillRowChains {
    private var index: SpillHashStore?
    private var store: (any ScratchStore)?
    private var length: UInt64 = 8
    init(host: any ScratchFileHost) throws {
        do {
            index = try SpillHashStore(host: host); store = try host.create(kind: "hash-rows")
            try store!.write(at: 0, bytes: Array(repeating: 0, count: 8))
        } catch { close(); throw JedError("58030", "cannot create scratch row chains: \(error)") }
    }
    private func devices() throws -> (SpillHashStore, any ScratchStore) {
        guard let index, let store else { throw JedError("58030", "scratch row chains are closed") }; return (index, store)
    }
    private func key(_ hash: UInt64) -> [UInt8] { var writer = ByteWriter(); writer.u64(hash); return writer.bytes }
    private func info(_ hash: UInt64, _ index: SpillHashStore) throws -> (head: UInt64, tail: UInt64, count: UInt64) {
        guard let bytes = try index.get(key(hash), hash: hash) else { return (0, 0, 0) }
        var reader = ByteReader(bytes)
        let head = try reader.u64(), tail = try reader.u64(), count = try reader.u64(); try reader.requireEnd()
        guard head >= 8, head < length, tail >= head, tail < length, count > 0 else {
            throw JedError("58030", "invalid scratch row chain metadata")
        }
        return (head, tail, count)
    }
    func head(hash: UInt64) throws -> (offset: UInt64, count: UInt64) {
        do { let (index, _) = try devices(); let value = try info(hash, index); return (value.head, value.count) }
        catch { throw JedError("58030", "cannot read scratch row chain head: \(error)") }
    }
    func append(hash: UInt64, row: [SQLValue]) throws {
        do {
            let (index, store) = try devices(), previous = try info(hash, index)
            let encoded = try SpillRowCodec.encode(row)
            guard UInt64(encoded.count) <= UInt64(Int64.max) - 16,
                  length <= UInt64(Int64.max) - 16 - UInt64(encoded.count), previous.count < UInt64.max else {
                throw JedError("58030", "scratch row chain too large")
            }
            let offset = length
            var header = ByteWriter(); header.u64(0); header.u64(UInt64(encoded.count))
            try store.write(at: offset, bytes: header.bytes)
            var position = 0
            while position < encoded.count {
                let end = min(encoded.count, position + 65_536)
                try store.write(at: offset + 16 + UInt64(position), bytes: Array(encoded[position..<end])); position = end
            }
            length += 16 + UInt64(encoded.count)
            if previous.tail != 0 { var link = ByteWriter(); link.u64(offset); try store.write(at: previous.tail, bytes: link.bytes) }
            var value = ByteWriter(); value.u64(previous.head == 0 ? offset : previous.head)
            value.u64(offset); value.u64(previous.count + 1)
            try index.put(key(hash), value: value.bytes, hash: hash)
        } catch { throw JedError("58030", "cannot append scratch row chain: \(error)") }
    }
    func row(at offset: UInt64) throws -> (row: [SQLValue], next: UInt64) {
        do {
            let (_, store) = try devices()
            guard offset >= 8, offset <= length, length - offset >= 16 else { throw JedError("58030", "invalid scratch row chain offset") }
            let header = try store.read(at: offset, count: 16)
            guard header.count == 16 else { throw JedError("58030", "short scratch row chain header") }
            var reader = ByteReader(header); let next = try reader.u64(), size = try reader.u64()
            guard size <= UInt64(Int.max), size <= length - offset - 16,
                  next == 0 || (next >= offset + 16 + size && next < length) else {
                throw JedError("58030", "invalid scratch row chain frame")
            }
            var bytes: [UInt8] = []; bytes.reserveCapacity(Int(size))
            while UInt64(bytes.count) < size {
                let amount = Int(min(65_536, size - UInt64(bytes.count)))
                let part = try store.read(at: offset + 16 + UInt64(bytes.count), count: amount)
                guard part.count == amount else { throw JedError("58030", "short scratch row chain payload") }
                bytes += part
            }
            return (try SpillRowCodec.decode(bytes), next)
        } catch { throw JedError("58030", "cannot read scratch row chain: \(error)") }
    }
    func close() {
        let oldIndex = index, oldStore = store; index = nil; store = nil
        oldIndex?.close(); try? oldStore?.close()
    }
    deinit { close() }
}
