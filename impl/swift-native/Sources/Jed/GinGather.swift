/// Statement-owned candidate collector. The frozen Q1/Q2 schedule does not
/// charge GIN keys before relation-row admission; this owner makes the lifetime
/// explicit without adding an experimental-only memory limit trip point.
internal final class GinCandidates {
    private var keys: [[UInt8]]
    private var position = 0
    private(set) var isClosed = false
    var remainingCount: Int { keys.count - position }
    init(_ keys: [[UInt8]]) { self.keys = keys }
    func next() -> [UInt8]? {
        guard !isClosed, position < keys.count else { close(); return nil }
        let key = keys[position]; position += 1
        if position == keys.count { close() }
        return key
    }
    func close() { keys.removeAll(keepingCapacity: false); position = 0; isClosed = true }
}

internal struct GinGatherResult {
    let candidates: GinCandidates
    let pageReads: Int
    let ginEntries: Int
    let requiresFullScan: Bool
}

internal struct GinGather {
    private let index: GinIndex
    private let meter: CostMeter?
    private var pageReads = 0
    private var ginEntries = 0

    /// Source OR arms are gathered independently and in source order; a full
    /// scan arm wins before any posting read, including over an empty arm.
    static func gather(_ arms: [GinQuery], index: GinIndex, meter: CostMeter? = nil) throws -> GinGatherResult {
        for arm in arms { if case .fullScan = arm {
            return GinGatherResult(candidates: GinCandidates([]), pageReads: 0, ginEntries: 0, requiresFullScan: true)
        } }
        var gatherer = GinGather(index: index, meter: meter), result: [[UInt8]] = []
        for arm in arms {
            if case .gather(let tree) = arm, let normalized = tree.normalized() {
                result = union(result, try gatherer.evaluate(normalized))
            }
        }
        // The baseline charges this block after gather; the caller adds logical
        // page_reads and table point-fetches to its scan block. Preserve that
        // guard order while polling cancellation inside the traversal.
        if let meter {
            let weight = GeneratedSpec.costs.first { $0.id == "gin_entry" }!.weight
            let product = Int64(gatherer.ginEntries).multipliedReportingOverflow(by: weight)
            meter.charge(product.overflow ? Int64.max : product.partialValue)
        }
        return GinGatherResult(candidates: GinCandidates(result), pageReads: gatherer.pageReads,
            ginEntries: gatherer.ginEntries, requiresFullScan: false)
    }

    private static func union(_ a: [[UInt8]], _ b: [[UInt8]]) -> [[UInt8]] {
        var left = 0, right = 0, result: [[UInt8]] = []
        while left < a.count || right < b.count {
            if left == a.count { result.append(b[right]); right += 1 }
            else if right == b.count { result.append(a[left]); left += 1 }
            else if a[left] == b[right] { result.append(a[left]); left += 1; right += 1 }
            else if a[left].lexicographicallyPrecedes(b[right]) { result.append(a[left]); left += 1 }
            else { result.append(b[right]); right += 1 }
        }
        return result
    }
    private static func intersection(_ a: [[UInt8]], _ b: [[UInt8]]) -> [[UInt8]] {
        var left = 0, right = 0, result: [[UInt8]] = []
        while left < a.count && right < b.count {
            if a[left] == b[right] { result.append(a[left]); left += 1; right += 1 }
            else if a[left].lexicographicallyPrecedes(b[right]) { left += 1 }
            else { right += 1 }
        }
        return result
    }
    private mutating func evaluate(_ query: GinQueryTree) throws -> [[UInt8]] {
        try meter?.pollStep()
        switch query {
        case .term(let term): return try postingList(term)
        case .any(let children):
            var result: [[UInt8]] = []
            for child in children { result = Self.union(result, try evaluate(child)) }
            return result
        case .all(let children):
            var leaves: [[UInt8]] = [], rest: [GinQueryTree] = []
            for child in children {
                if case .term(let term) = child { leaves.append(term) } else { rest.append(child) }
            }
            var result: [[UInt8]]
            if !leaves.isEmpty { result = try intersectTerms(leaves) }
            else if !rest.isEmpty { result = try evaluate(rest.removeFirst()) }
            else { return [] }
            for child in rest {
                if result.isEmpty { break }
                result = Self.intersection(result, try evaluate(child))
            }
            return result
        }
    }
    private func bounds(_ term: [UInt8]) -> (TreeBound, TreeBound?) {
        (TreeBound(term), GinTerms.successor(term).map { TreeBound($0, inclusive: false) })
    }
    private func span(_ term: [UInt8]) -> Int {
        let (lower, upper) = bounds(term)
        return index.tree.overlapNodeCount(lower: lower, upper: upper)
    }
    private mutating func runs(_ term: [UInt8]) throws -> [[UInt8]] {
        let (lower, upper) = bounds(term)
        var cursor = TreeCursor(index.tree, lower: lower, upper: upper), records: [[UInt8]] = []
        while let record = try cursor.next() { try meter?.pollStep(); records.append(record.key) }
        pageReads += span(term)
        return records
    }
    private mutating func decode(_ term: [UInt8], runs: [[UInt8]]) throws -> [[UInt8]] {
        var result: [[UInt8]] = []
        for run in runs {
            try meter?.pollStep()
            let keys = try GinPostingRun.decode(termLength: term.count, bytes: run)
            guard result.last.map({ $0.lexicographicallyPrecedes(keys[0]) }) ?? true else {
                throw JedError.corruption("GIN posting runs overlap")
            }
            result.append(contentsOf: keys); ginEntries += keys.count
        }
        return result
    }
    private mutating func postingList(_ term: [UInt8]) throws -> [[UInt8]] {
        let (lower, upper) = bounds(term)
        var cursor = TreeCursor(index.tree, lower: lower, upper: upper), result: [[UInt8]] = []
        while let record = try cursor.next() {
            try meter?.pollStep()
            let keys = try GinPostingRun.decode(termLength: term.count, bytes: record.key)
            guard result.last.map({ $0.lexicographicallyPrecedes(keys[0]) }) ?? true else {
                throw JedError.corruption("GIN posting runs overlap")
            }
            result.append(contentsOf: keys); ginEntries += keys.count
        }
        pageReads += span(term)
        return result
    }
    private struct Spanned {
        let term: [UInt8]
        let span: Int
        let runCount: Int
        let runBytes: Int
        let peeked: [[UInt8]]?
    }
    private mutating func intersectTerms(_ terms: [[UInt8]]) throws -> [[UInt8]] {
        if terms.count == 1 { return try postingList(terms[0]) }
        guard !terms.isEmpty else { return [] }
        let height = index.tree.height
        var ordered: [Spanned] = []
        for term in terms {
            try meter?.pollStep()
            let overlap = span(term)
            if overlap <= height {
                let peeked = try runs(term)
                ordered.append(Spanned(term: term, span: overlap, runCount: peeked.count,
                    runBytes: peeked.reduce(0) { $0 + $1.count }, peeked: peeked))
            } else {
                ordered.append(Spanned(term: term, span: overlap, runCount: Int.max, runBytes: Int.max, peeked: nil))
            }
        }
        ordered.sort {
            if $0.span != $1.span { return $0.span < $1.span }
            if $0.runCount != $1.runCount { return $0.runCount < $1.runCount }
            if $0.runBytes != $1.runBytes { return $0.runBytes < $1.runBytes }
            return $0.term.lexicographicallyPrecedes($1.term)
        }
        let driver = ordered[0]
        var candidates = try driver.peeked.map { try decode(driver.term, runs: $0) } ?? postingList(driver.term)
        // Peek records are needed only for selecting/reading the driver. Later
        // probes follow their own leaf groups even when that term was peeked.
        let following = ordered.dropFirst().map(\.term)
        ordered.removeAll(keepingCapacity: false)
        for term in following {
            if candidates.isEmpty { break }
            var survivors: [[UInt8]] = [], start = 0
            while start < candidates.count {
                let first = term + candidates[start]
                var end = start
                while end + 1 < candidates.count,
                      index.tree.overlapNodeCount(lower: TreeBound(first), upper: TreeBound(term + candidates[end + 1])) == height { end += 1 }
                let last = term + candidates[end]
                let found = try probe(term: term, group: candidates[start...end], first: first, last: last)
                survivors.append(contentsOf: found); start = end + 1
            }
            candidates = survivors
        }
        return candidates
    }
    private mutating func probe(term: [UInt8], group: ArraySlice<[UInt8]>, first: [UInt8], last: [UInt8]) throws -> [[UInt8]] {
        // At most one logical leaf group plus the first following run. Stream
        // through runs and candidates together, keeping only the current run's
        // bounded decoded keys; no posting-list collector is used here.
        let upper = GinTerms.successor(term).map { TreeBound($0, inclusive: false) }
        var cursor = TreeCursor(index.tree, lower: TreeBound(first), upper: upper)
        var candidate = group.startIndex, survivors: [[UInt8]] = [], stop = last
        while let record = try cursor.next() {
            try meter?.pollStep()
            let keys = try GinPostingRun.decode(termLength: term.count, bytes: record.key)
            var keyIndex = 0
            while candidate < group.endIndex, !(record.key.lexicographicallyPrecedes(term + group[candidate])) {
                while keyIndex < keys.count, keys[keyIndex].lexicographicallyPrecedes(group[candidate]) { keyIndex += 1 }
                if keyIndex < keys.count, keys[keyIndex] == group[candidate] { survivors.append(group[candidate]) }
                candidate += 1
            }
            if !record.key.lexicographicallyPrecedes(last) { stop = record.key; break }
        }
        pageReads += index.tree.overlapNodeCount(lower: TreeBound(first), upper: TreeBound(stop))
        ginEntries += group.count
        return survivors
    }
}
