/// A writer leaf's records in key order, held in short chunks.
///
/// Every edit of a persistent leaf makes a new leaf beside the old one. With
/// one flat array that copies a reference to every record of the leaf; here
/// it copies the chunk list and the one chunk that changed.
internal struct RecordList: Sendable {
    private static let chunkSize = 16
    private static let chunkLimit = 32

    /// Each chunk is non-empty and holds at most `chunkLimit` records.
    private var chunks: [[PackedRecord]]
    private(set) var count: Int

    init(_ records: [PackedRecord] = []) {
        var chunks: [[PackedRecord]] = []
        chunks.reserveCapacity((records.count + Self.chunkSize - 1) / Self.chunkSize)
        var start = 0
        while start < records.count {
            let end = min(start + Self.chunkSize, records.count)
            chunks.append(Array(records[start..<end]))
            start = end
        }
        self.chunks = chunks; count = records.count
    }

    var isEmpty: Bool { count == 0 }

    /// All records as one array, for whole-leaf work such as a split.
    var flat: [PackedRecord] {
        var result: [PackedRecord] = []
        result.reserveCapacity(count)
        for chunk in chunks { result.append(contentsOf: chunk) }
        return result
    }

    /// The chunk and offset of an existing record.
    @inline(__always) private func locate(_ index: Int) -> (chunk: Int, offset: Int) {
        precondition(index >= 0 && index < count, "record index out of range")
        var offset = index, chunk = 0
        while offset >= chunks[chunk].count { offset -= chunks[chunk].count; chunk += 1 }
        return (chunk, offset)
    }

    subscript(index: Int) -> PackedRecord {
        let place = locate(index)
        return chunks[place.chunk][place.offset]
    }

    mutating func replace(at index: Int, with record: PackedRecord) {
        let place = locate(index)
        chunks[place.chunk][place.offset] = record
    }

    /// Inserts before the record at `index`; `index == count` appends.
    mutating func insert(_ record: PackedRecord, at index: Int) {
        precondition(index >= 0 && index <= count, "record index out of range")
        count += 1
        guard !chunks.isEmpty else { chunks = [[record]]; return }
        var chunk: Int, offset: Int
        if index == count - 1 { chunk = chunks.count - 1; offset = chunks[chunk].count }
        else {
            offset = index; chunk = 0
            while offset >= chunks[chunk].count { offset -= chunks[chunk].count; chunk += 1 }
        }
        chunks[chunk].insert(record, at: offset)
        if chunks[chunk].count > Self.chunkLimit {
            let half = chunks[chunk].count / 2
            let tail = Array(chunks[chunk][half...])
            chunks[chunk].removeSubrange(half...)
            chunks.insert(tail, at: chunk + 1)
        }
    }

    @discardableResult mutating func remove(at index: Int) -> PackedRecord {
        let place = locate(index)
        let old = chunks[place.chunk].remove(at: place.offset)
        if chunks[place.chunk].isEmpty { chunks.remove(at: place.chunk) }
        count -= 1
        return old
    }
}
