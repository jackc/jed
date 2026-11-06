/// Stable external sorting over query-local framed rows. Runs represent adjacent
/// input intervals; equal keys retain interval order through binary compaction.
internal final class ExternalRowSorter {
    typealias Compare = ([SQLValue], [SQLValue]) throws -> Int
    private struct Run { let file: SpillRowFile; let level: Int }
    private let budget: Int64
    private let host: (any ScratchFileHost)?
    private let meter: CostMeter
    private let compare: Compare
    private var buffer: [[SQLValue]] = []
    private var bufferBytes: Int64 = 0
    private var charged: Int64 = 0
    private var runs: [Run] = []
    private var merger: Merge?
    private var position = 0
    private var finished = false
    private var closed = false
    private(set) var count: Int64 = 0

    init(workMemoryBytes: Int64, host: (any ScratchFileHost)?, meter: CostMeter,
         compare: @escaping Compare) {
        budget = max(0, workMemoryBytes); self.host = host
        self.meter = meter; self.compare = compare
    }
    private var canSpill: Bool { host != nil && budget > 0 }

    func append(_ row: [SQLValue]) throws {
        guard !finished && !closed else { throw JedError("XX000", "sorter no longer accepts rows") }
        let bytes = MemorySizes.row(row)
        let admitted = meter.resources.query.tryReserve(bytes)
        if admitted { charged = saturatingAdd(charged, bytes) }
        else if !canSpill { try meter.guardCost(); throw JedError("54P05", "query memory exceeded its limit") }
        buffer.append(row); bufferBytes = saturatingAdd(bufferBytes, bytes)
        count = saturatingAdd(count, 1)
        if canSpill && (!admitted || bufferBytes > budget) { try spill() }
    }

    private func sortBuffer() throws {
        // Tie-breaking by input position makes stability independent of the
        // host sorting algorithm. Index permutations add no logical charge.
        let order = try buffer.indices.sorted { left, right in
            let result = try compare(buffer[left], buffer[right])
            return result == 0 ? left < right : result < 0
        }
        buffer = order.map { buffer[$0] }
    }

    private func spill() throws {
        guard let host else { throw JedError("XX000", "sort spill has no scratch host") }
        try sortBuffer()
        let file = try SpillRowFile(host: host, kind: "sort")
        do { for row in buffer { try file.append(row) } }
        catch { try? file.close(); throw error }
        buffer = []; bufferBytes = 0
        meter.resources.query.release(charged); charged = 0
        runs.append(Run(file: file, level: 0))
        while runs.count > 1 {
            let last = runs.count - 1
            guard runs[last - 1].level == runs[last].level else { break }
            let left = runs[last - 1], right = runs[last]
            let merged = try merge(left.file, right.file, host: host)
            runs.removeLast(2); runs.append(Run(file: merged, level: right.level + 1))
        }
    }

    private func merge(_ left: SpillRowFile, _ right: SpillRowFile,
                       host: any ScratchFileHost) throws -> SpillRowFile {
        let input = try Merge(files: [left, right], memory: [], meter: meter, compare: compare)
        defer { input.close() }
        let output = try SpillRowFile(host: host, kind: "sort-merge")
        do {
            while let row = try input.next() { try output.append(row) }
            return output
        } catch { try? output.close(); throw error }
    }

    func finish() throws {
        guard !closed else { throw JedError("XX000", "sorter is closed") }
        guard !finished else { return }
        try sortBuffer()
        if !runs.isEmpty {
            merger = try Merge(files: runs.map(\.file), memory: buffer, meter: meter, compare: compare)
            runs = []; buffer = []
        }
        finished = true
    }

    func next() throws -> [SQLValue]? {
        if closed { return nil }
        try finish()
        if let merger { return try merger.next() }
        guard position < buffer.count else { return nil }
        let row = buffer[position]; position += 1
        return row
    }

    /// Final resident output stays charged until the caller finishes its LIMIT
    /// window or closes the cursor, including after rows have left the sorter.
    func close() {
        guard !closed else { return }; closed = true
        merger?.close(); merger = nil
        for run in runs { try? run.file.close() }
        runs = []; buffer = []
        meter.resources.query.release(charged); charged = 0
    }
    deinit { close() }

    private final class Merge {
        private struct Source {
            let file: SpillRowFile?
            let cursor: SpillRowFile.Cursor?
            var memory: [[SQLValue]]
            var position = 0
            mutating func next() throws -> [SQLValue]? {
                if let cursor {
                    let row = try cursor.next()
                    if row == nil { try file?.close() }
                    return row
                }
                guard position < memory.count else { return nil }
                let row = memory[position]; position += 1
                return row
            }
        }
        private struct Head { let row: [SQLValue]; let source: Int; let charged: Int64 }
        private var sources: [Source]
        private var heap: [Head] = []
        private var held: Int64 = 0
        private let meter: CostMeter
        private let compare: Compare
        private var closed = false

        init(files: [SpillRowFile], memory: [[SQLValue]], meter: CostMeter,
             compare: @escaping Compare) throws {
            self.meter = meter; self.compare = compare
            sources = files.map { Source(file: $0, cursor: $0.cursor(), memory: []) }
            if !memory.isEmpty { sources.append(Source(file: nil, cursor: nil, memory: memory)) }
            do {
                for index in sources.indices {
                    if let row = try sources[index].next() { try push(head(row, from: index)) }
                }
            } catch { close(); throw error }
        }
        private func head(_ row: [SQLValue], from source: Int) -> Head {
            var bytes: Int64 = 0
            if sources[source].file != nil {
                let amount = MemorySizes.row(row)
                if meter.resources.query.tryReserve(amount) { bytes = amount; held = saturatingAdd(held, amount) }
            }
            return Head(row: row, source: source, charged: bytes)
        }
        private func before(_ left: Head, _ right: Head) throws -> Bool {
            let order = try compare(left.row, right.row)
            return order == 0 ? left.source < right.source : order < 0
        }
        private func push(_ item: Head) throws {
            heap.append(item)
            var index = heap.count - 1
            while index > 0 {
                let parent = (index - 1) / 2
                guard try before(heap[index], heap[parent]) else { break }
                heap.swapAt(index, parent); index = parent
            }
        }
        private func pop() throws -> Head? {
            guard !heap.isEmpty else { return nil }
            if heap.count == 1 { return heap.removeLast() }
            let first = heap[0]; heap[0] = heap.removeLast()
            var index = 0
            while index * 2 + 1 < heap.count {
                let left = index * 2 + 1, right = left + 1
                var child = left
                if right < heap.count, try before(heap[right], heap[left]) { child = right }
                guard try before(heap[child], heap[index]) else { break }
                heap.swapAt(child, index); index = child
            }
            return first
        }
        func next() throws -> [SQLValue]? {
            if closed { return nil }
            guard let item = try pop() else { return nil }
            defer { meter.resources.query.release(item.charged); held -= item.charged }
            // Replacement head enters before the emitted head leaves (§6.8).
            if let next = try sources[item.source].next() { try push(head(next, from: item.source)) }
            return item.row
        }
        func close() {
            guard !closed else { return }; closed = true
            for source in sources { try? source.file?.close() }
            sources = []; heap = []
            meter.resources.query.release(held); held = 0
        }
        deinit { close() }
    }
}
