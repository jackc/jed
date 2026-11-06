/// Private disk hash map. Directory heads live on scratch, linear splits move
/// one chain node at a time, and the allocator retains only 64 free-list heads.
/// Physical bucket placement is never an output-order or SQL-cost input.
internal final class SpillHashStore {
    private struct Node {
        let offset: UInt64
        var next: UInt64
        let hash: UInt64
        let keyLength: UInt64
        var valueLength: UInt64
        let capacity: UInt64
    }
    private var directory: (any ScratchStore)?
    private var data: (any ScratchStore)?
    private var base: UInt64 = 16, split: UInt64 = 0, length: UInt64 = 8
    private var freeHeads = Array(repeating: UInt64(0), count: 64)
    private var failed = false
    private(set) var count: UInt64 = 0
    var bucketCount: UInt64 { base + split }
    var allocatedBytes: UInt64 { length }
    init(host: any ScratchFileHost) throws {
        do {
            directory = try host.create(kind: "hash-heads")
            data = try host.create(kind: "hash-values")
            try directory!.write(at: 0, bytes: Array(repeating: 0, count: 128))
            try data!.write(at: 0, bytes: Array(repeating: 0, count: 8))
        } catch { close(); throw JedError("58030", "cannot create scratch hash map: \(error)") }
    }
    private func devices() throws -> (any ScratchStore, any ScratchStore) {
        guard !failed, let directory, let data else { throw JedError("58030", "scratch hash map is closed or unusable") }
        return (directory, data)
    }
    private func bucket(_ hash: UInt64) -> UInt64 {
        let low = hash & (base - 1); return low < split ? hash & (base * 2 - 1) : low
    }
    private func read(_ store: any ScratchStore, at offset: UInt64, count: Int) throws -> [UInt8] {
        var result: [UInt8] = []; result.reserveCapacity(count)
        while result.count < count {
            let amount = min(65_536, count - result.count)
            let part = try store.read(at: offset + UInt64(result.count), count: amount)
            guard part.count == amount else { throw JedError("58030", "short scratch hash read") }
            result.append(contentsOf: part)
        }
        return result
    }
    private func write(_ store: any ScratchStore, at offset: UInt64, bytes: [UInt8]) throws {
        var position = 0
        while position < bytes.count {
            let end = min(bytes.count, position + 65_536)
            try store.write(at: offset + UInt64(position), bytes: Array(bytes[position..<end])); position = end
        }
    }
    private func word(_ store: any ScratchStore, at offset: UInt64) throws -> UInt64 {
        var reader = ByteReader(try read(store, at: offset, count: 8)); return try reader.u64()
    }
    private func word(_ value: UInt64, _ store: any ScratchStore, at offset: UInt64) throws {
        var writer = ByteWriter(); writer.u64(value); try store.write(at: offset, bytes: writer.bytes)
    }
    private func node(_ offset: UInt64, _ data: any ScratchStore) throws -> Node {
        guard offset >= 8, offset <= length, length - offset >= 40 else { throw JedError("58030", "invalid scratch hash node") }
        var reader = ByteReader(try read(data, at: offset, count: 40))
        let next = try reader.u64(), hash = try reader.u64(), keyLength = try reader.u64()
        let valueLength = try reader.u64(), capacity = try reader.u64()
        guard capacity >= 64, capacity.nonzeroBitCount == 1, capacity <= length - offset,
              keyLength <= capacity - 40, valueLength <= capacity - 40 - keyLength,
              keyLength <= UInt64(Int.max), valueLength <= UInt64(Int.max),
              next == 0 || (next >= 8 && next < length) else { throw JedError("58030", "invalid scratch hash node bounds") }
        return Node(offset: offset, next: next, hash: hash, keyLength: keyLength, valueLength: valueLength, capacity: capacity)
    }
    private func find(_ key: [UInt8], hash: UInt64, directory: any ScratchStore, data: any ScratchStore) throws -> (Node?, UInt64) {
        var offset = try word(directory, at: bucket(hash) * 8), previous: UInt64 = 0, remaining = count
        while offset != 0 {
            guard remaining > 0 else { throw JedError("58030", "cyclic scratch hash chain") }; remaining -= 1
            let candidate = try node(offset, data)
            if candidate.hash == hash, candidate.keyLength == UInt64(key.count),
               try read(data, at: offset + 40, count: key.count) == key { return (candidate, previous) }
            previous = offset; offset = candidate.next
        }
        return (nil, previous)
    }
    func get(_ key: [UInt8], hash: UInt64) throws -> [UInt8]? {
        do {
            let (directory, data) = try devices()
            guard let found = try find(key, hash: hash, directory: directory, data: data).0 else { return nil }
            return try read(data, at: found.offset + 40 + found.keyLength, count: Int(found.valueLength))
        } catch { failed = true; throw JedError("58030", "cannot read scratch hash map: \(error)") }
    }
    private func allocate(_ needed: UInt64, _ data: any ScratchStore) throws -> (UInt64, UInt64) {
        var capacity: UInt64 = 64, slot = 6
        while capacity < needed {
            guard slot < 62 else { throw JedError("58030", "scratch hash value too large") }
            capacity *= 2; slot += 1
        }
        if freeHeads[slot] != 0 {
            let offset = freeHeads[slot]
            guard offset >= 8, offset <= length, capacity <= length - offset else {
                throw JedError("58030", "invalid scratch free-block offset")
            }
            let next = try word(data, at: offset)
            guard next == 0 || (next != offset && next >= 8 && next <= length && capacity <= length - next) else {
                throw JedError("58030", "invalid scratch free-block link")
            }
            freeHeads[slot] = next
            return (offset, capacity)
        }
        guard capacity <= UInt64(Int64.max) - length else { throw JedError("58030", "scratch hash map too large") }
        let offset = length; length += capacity; try data.setSize(length); return (offset, capacity)
    }
    private func free(_ node: Node, _ data: any ScratchStore) throws {
        let slot = node.capacity.trailingZeroBitCount
        try word(freeHeads[slot], data, at: node.offset); freeHeads[slot] = node.offset
    }
    private func writeNode(_ node: Node, key: [UInt8], value: [UInt8], data: any ScratchStore) throws {
        var header = ByteWriter(); header.u64(node.next); header.u64(node.hash)
        header.u64(node.keyLength); header.u64(node.valueLength); header.u64(node.capacity)
        try data.write(at: node.offset, bytes: header.bytes)
        try write(data, at: node.offset + 40, bytes: key)
        try write(data, at: node.offset + 40 + UInt64(key.count), bytes: value)
    }
    func put(_ key: [UInt8], value: [UInt8], hash: UInt64) throws {
        do {
            let (directory, data) = try devices()
            guard UInt64(key.count) <= UInt64(Int64.max) - 40,
                  UInt64(value.count) <= UInt64(Int64.max) - 40 - UInt64(key.count) else { throw JedError("58030", "scratch hash value too large") }
            let needed = UInt64(key.count) + UInt64(value.count) + 40
            let (found, previous) = try find(key, hash: hash, directory: directory, data: data)
            if var found, needed <= found.capacity {
                found.valueLength = UInt64(value.count)
                try writeNode(found, key: key, value: value, data: data); return
            }
            let (offset, capacity) = try allocate(needed, data)
            let next: UInt64
            if let found { next = found.next } else { next = try word(directory, at: bucket(hash) * 8) }
            let replacement = Node(offset: offset, next: next, hash: hash, keyLength: UInt64(key.count),
                valueLength: UInt64(value.count), capacity: capacity)
            try writeNode(replacement, key: key, value: value, data: data)
            if found != nil && previous != 0 { try word(offset, data, at: previous) }
            else { try word(offset, directory, at: bucket(hash) * 8) }
            if let found { try free(found, data) }
            else {
                count += 1
                if count > bucketCount * 4 { try splitBucket(directory: directory, data: data) }
            }
        } catch { failed = true; throw JedError("58030", "cannot update scratch hash map: \(error)") }
    }
    private func splitBucket(directory: any ScratchStore, data: any ScratchStore) throws {
        guard base <= UInt64(Int64.max) / 16 else { throw JedError("58030", "scratch hash directory too large") }
        let highBucket = base + split
        var offset = try word(directory, at: split * 8), lowTail: UInt64 = 0, highTail: UInt64 = 0
        try word(0, directory, at: split * 8); try word(0, directory, at: highBucket * 8)
        var remaining = count
        while offset != 0 {
            guard remaining > 0 else { throw JedError("58030", "cyclic scratch split chain") }; remaining -= 1
            let current = try node(offset, data), next = current.next
            try word(0, data, at: offset)
            if current.hash & (base * 2 - 1) == split {
                if lowTail == 0 { try word(offset, directory, at: split * 8) }
                else { try word(offset, data, at: lowTail) }
                lowTail = offset
            } else {
                if highTail == 0 { try word(offset, directory, at: highBucket * 8) }
                else { try word(offset, data, at: highTail) }
                highTail = offset
            }
            offset = next
        }
        split += 1
        if split == base { base *= 2; split = 0 }
    }
    static func hash(_ bytes: [UInt8]) -> UInt64 {
        bytes.reduce(14_695_981_039_346_656_037) { ($0 ^ UInt64($1)) &* 1_099_511_628_211 }
    }
    func close() {
        let oldDirectory = directory, oldData = data; directory = nil; data = nil
        try? oldDirectory?.close(); try? oldData?.close()
    }
    deinit { close() }
}
