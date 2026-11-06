/// Append-only private row frames. Positioned readers are independent and see
/// the prefix that existed when they were opened. The outer operator owns file
/// cleanup; closing a reader never closes another reader's file.
internal final class SpillRowFile {
    private var store: (any ScratchStore)?
    private var length: UInt64 = 0
    private var failed = false
    private(set) var count: Int64 = 0
    init(host: any ScratchFileHost, kind: String) throws {
        do { store = try host.create(kind: kind) }
        catch { throw JedError("58030", "cannot create scratch rows: \(error)") }
    }
    private func device() throws -> any ScratchStore {
        guard let store, !failed else { throw JedError("58030", "scratch row file is closed or unusable") }
        return store
    }
    func append(_ row: [SQLValue]) throws {
        let store = try device()
        do {
            let encoded = try SpillRowCodec.encode(row)
            guard count < Int64.max, UInt64(encoded.count) <= UInt64(Int64.max) - 8,
                  length <= UInt64(Int64.max) - UInt64(encoded.count) - 8 else {
                throw JedError("58030", "scratch row file is too large")
            }
            var header = ByteWriter(); header.u64(UInt64(encoded.count))
            try store.write(at: length, bytes: header.bytes)
            var position = 0
            while position < encoded.count {
                let end = min(encoded.count, position + 65_536)
                try store.write(at: length + 8 + UInt64(position), bytes: Array(encoded[position..<end]))
                position = end
            }
            length += UInt64(encoded.count) + 8; count += 1
        } catch { failed = true; throw JedError("58030", "cannot write scratch row: \(error)") }
    }
    func cursor() -> Cursor { Cursor(owner: self, count: count, length: length) }
    private func read(at offset: UInt64, limit: UInt64) throws -> (row: [SQLValue], end: UInt64) {
        let store = try device()
        do {
            guard offset <= limit, limit - offset >= 8 else { throw JedError("58030", "truncated scratch row header") }
            let header = try store.read(at: offset, count: 8)
            guard header.count == 8 else { throw JedError("58030", "short scratch row header") }
            var reader = ByteReader(header); let size = try reader.u64()
            guard size <= UInt64(Int.max), size <= limit - offset - 8 else { throw JedError("58030", "scratch row exceeds its file") }
            var bytes: [UInt8] = []; bytes.reserveCapacity(Int(size))
            while UInt64(bytes.count) < size {
                let amount = Int(min(65_536, size - UInt64(bytes.count)))
                let part = try store.read(at: offset + 8 + UInt64(bytes.count), count: amount)
                guard part.count == amount else { throw JedError("58030", "short scratch row payload") }
                bytes.append(contentsOf: part)
            }
            return (try SpillRowCodec.decode(bytes), offset + 8 + size)
        } catch { failed = true; throw JedError("58030", "cannot read scratch row: \(error)") }
    }
    final class Cursor {
        private var owner: SpillRowFile?
        private let count: Int64, length: UInt64
        private var position: Int64 = 0
        private var offset: UInt64 = 0
        fileprivate init(owner: SpillRowFile, count: Int64, length: UInt64) {
            self.owner = owner; self.count = count; self.length = length
        }
        func next() throws -> [SQLValue]? {
            guard let owner else { return nil }
            if position == count {
                guard offset == length else { throw JedError("58030", "scratch row count disagrees with file length") }
                return nil
            }
            let result = try owner.read(at: offset, limit: length)
            position += 1; offset = result.end; return result.row
        }
        func close() { owner = nil }
    }
    func close() throws {
        guard let current = store else { return }
        store = nil
        do { try current.close() }
        catch { throw JedError("58030", "cannot close scratch rows: \(error)") }
    }
    deinit { try? close() }
}
