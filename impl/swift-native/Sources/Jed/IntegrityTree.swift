internal struct IntegrityTreeResult { var records: UInt64 = 0; var unreadable = false }
internal struct IntegrityInterior {
    let keys: [[UInt8]]
    let children: [PageReference]
    init(_ bytes: [UInt8], encrypted: Bool) throws {
        let count = Int(try PageEnvelope.readU32(bytes, at: 4)), width = encrypted ? 20 : 4
        guard count <= (bytes.count - 16 - width) / (width + 4) else { throw JedError.corruption("interior directory overruns page") }
        var reader = ByteReader(bytes[16...]), children: [PageReference] = []
        for _ in 0...count {
            children.append(PageReference(try reader.u32(), tag: encrypted ? Array(try reader.take(16)) : []))
        }
        var ends: [Int] = [], previous = 0
        for _ in 0..<count {
            let end = Int(try reader.u32())
            guard end >= previous else { throw JedError.corruption("invalid separator offsets") }
            ends.append(end); previous = end
        }
        let data = try reader.take(previous)
        guard try reader.take(reader.remaining).allSatisfy({ $0 == 0 }) else { throw JedError.corruption("nonzero interior padding") }
        var keys: [[UInt8]] = [], offset = data.startIndex
        for end in ends { keys.append(Array(data[offset..<(data.startIndex + end)])); offset = data.startIndex + end }
        self.keys = keys; self.children = children
    }
}

internal extension IntegrityChecker {
    func walkTree(root: PageReference, object: SQLText, types: [TypeDescriptor],
                  table: CatalogTable? = nil, gin: Bool = false) throws -> IntegrityTreeResult {
        var result = IntegrityTreeResult(), leafDepth: Int?
        func visit(_ reference: PageReference, depth: Int, lower: [UInt8]?, upper: [UInt8]?) throws {
            guard try reach(reference, object: object) else { result.unreadable = true; return }
            guard depth < 256 else { result.unreadable = true; try add("page_corrupt", object, page: reference.index); return }
            let bytes: [UInt8]
            do { bytes = try body(reference) }
            catch let error as JedError where error.code == "XX001" {
                result.unreadable = true; try add("page_corrupt", object, page: reference.index, error.message); return
            }
            guard bytes[0] == 2 || bytes[0] == 3 else {
                result.unreadable = true; try add("page_type_wrong", object, page: reference.index); return
            }
            if bytes[0] == 2 {
                if let leafDepth, leafDepth != depth { try add("tree_depth_uneven", object, page: reference.index) }
                else if leafDepth == nil { leafDepth = depth }
            }
            let keys: [[UInt8]], leaf: PackedLeaf?, children: [PageReference]
            do {
                if bytes[0] == 2 {
                    let packed = try PackedLeaf(integrityBytes: bytes, types: types, codec: codec)
                    leaf = packed; keys = (0..<packed.count).map { Array(packed.key(at: $0)) }; children = []
                } else {
                    let interior = try IntegrityInterior(bytes, encrypted: meta.encrypted)
                    leaf = nil; keys = interior.keys; children = interior.children
                }
            } catch let error as JedError where error.code == "XX001" {
                result.unreadable = true; try add("page_corrupt", object, page: reference.index, error.message); return
            }
            if zip(keys, keys.dropFirst()).contains(where: { !$0.lexicographicallyPrecedes($1) }) {
                try add("keys_unordered", object, page: reference.index)
            }
            if keys.contains(where: { key in
                (lower.map { key.lexicographicallyPrecedes($0) } ?? false) ||
                (upper.map { !key.lexicographicallyPrecedes($0) } ?? false)
            }) { try add("separator_violation", object, page: reference.index) }
            if let leaf {
                if gin {
                    do {
                        var count: UInt64 = 0
                        for key in keys { count += try ginCount(key) }
                        result.records += count
                    } catch let error as JedError where error.code == "XX001" {
                        result.unreadable = true; try add("page_corrupt", object, page: reference.index, error.message); return
                    }
                }
                else { result.records += UInt64(leaf.count) }
                guard let table else { return }
                for row in 0..<leaf.count {
                    try poll(); report.rows += 1
                    let key = keys[row]
                    do {
                        let values = try decodeRow(leaf.record(at: row), object: object, page: reference.index)
                        if !table.primaryKey.isEmpty {
                            do {
                                let context = try freshContext()
                                var encoded: [UInt8] = []
                                for column in table.primaryKey {
                                    encoded += try KeyEncoding.encode(values[column], type: types[column], collatedKey: context.collatedKey)
                                }
                                if encoded != key { try add("row_key_not_primary_key", object, page: reference.index, key: key) }
                            } catch let error as JedError {
                                if error.code == "57014" || error.code == "08006" || error.code == "58030" { throw error }
                                try add("row_key_not_primary_key", object, page: reference.index, key: key, error.message)
                            }
                        }
                    } catch IntegrityStop.unreadable { result.unreadable = true }
                }
            } else {
                for ordinal in children.indices {
                    try visit(children[ordinal], depth: depth + 1,
                        lower: ordinal == 0 ? lower : keys[ordinal - 1], upper: ordinal == keys.count ? upper : keys[ordinal])
                }
            }
        }
        if root.index != 0 { try visit(root, depth: 0, lower: nil, upper: nil) }
        return result
    }
    /// Replays a structurally visited table without strict key ordering. A
    /// damaged key order is itself a finding, not permission to omit its rows.
    func scanRows(root: PageReference, types: [TypeDescriptor], body consume: ([UInt8], [SQLValue]) throws -> Void) throws {
        func visit(_ reference: PageReference, depth: Int) throws {
            try poll()
            guard depth < 256, reference.index >= meta.firstBodyPage, reference.index < meta.pageCount else {
                throw JedError.corruption("invalid replay tree")
            }
            let bytes = try body(reference)
            if bytes[0] == 2 {
                let leaf = try PackedLeaf(integrityBytes: bytes, types: types, codec: codec)
                for ordinal in 0..<leaf.count {
                    try poll()
                    let record = leaf.record(at: ordinal)
                    let values = try record.cells.map { try $0.materialize(pager: pager, pageCount: meta.pageCount) }
                    try consume(record.key, values)
                }
            } else if bytes[0] == 3 {
                for child in try IntegrityInterior(bytes, encrypted: meta.encrypted).children { try visit(child, depth: depth + 1) }
            } else { throw JedError.corruption("invalid replay node type") }
        }
        if root.index != 0 { try visit(root, depth: 0) }
    }
    func decodeRow(_ record: PackedRecord, object: SQLText, page: UInt32) throws -> [SQLValue] {
        var values: [SQLValue] = []
        for cell in record.cells {
            let external = cell.fixedWidth == nil && (cell.bytes?.first == 2 || cell.bytes?.first == 4)
            do {
                if !external { values.append(try cell.materialize()); continue }
                let bytes = cell.bytes!, compressed = bytes.first == 4
                var reader = ByteReader(bytes); _ = try reader.u8()
                let reference = try reader.u32(), length = Int(try reader.u32())
                let rawLength = compressed ? Int(try reader.u32()) : length
                let tag = meta.encrypted ? Array(try reader.take(16)) : []
                try reader.requireEnd()
                let slab = codec.capacity - (meta.encrypted ? 16 : 0), start = meta.encrypted ? 32 : 16
                guard length > 0, UInt64(length) <= UInt64(meta.pageCount - meta.firstBodyPage) * UInt64(slab) else {
                    throw JedError.corruption("invalid overflow length")
                }
                var output: [UInt8] = [], current = PageReference(reference, tag: tag)
                while output.count < length {
                    guard try reach(current, object: object, key: record.key) else { throw IntegrityStop.unreadable }
                    let bytes = try body(current, cached: false)
                    let count = Int(try PageEnvelope.readU32(bytes, at: 4)), next = try PageEnvelope.readU32(bytes, at: 8)
                    guard bytes[0] == 4, count == min(slab, length - output.count),
                          bytes[(start + count)...].allSatisfy({ $0 == 0 }) else { throw JedError.corruption("invalid overflow slab") }
                    output.append(contentsOf: bytes[start..<(start + count)])
                    let tag = meta.encrypted ? Array(bytes[16..<32]) : []
                    guard (output.count == length) == (next == 0), next != 0 || tag.allSatisfy({ $0 == 0 }) else {
                        throw JedError.corruption("invalid overflow terminator")
                    }
                    current = PageReference(next, tag: tag)
                }
                let raw = compressed ? try LZ4.decompress(output, rawLength: rawLength) : output
                values.append(try ValueCodec.decodeRawBody(raw, type: cell.type))
            } catch let error as JedError where error.code == "XX001" {
                try add(external ? "overflow_chain_corrupt" : "value_undecodable", object, page: page, key: record.key, error.message)
                throw IntegrityStop.unreadable
            }
        }
        return values
    }
    /// Posting counts use just the run's wire structure. Terms need not decode
    /// to a supported SQL value for the structural pass to remain meaningful.
    func ginCount(_ bytes: [UInt8]) throws -> UInt64 {
        guard bytes.count >= 4 else { throw JedError.corruption("truncated GIN run") }
        var tail = ByteReader(bytes.suffix(2))
        let size = Int(try tail.u16()), end = bytes.count - 2
        guard end - size >= 2 else { throw JedError.corruption("invalid GIN run body") }
        var reader = ByteReader(bytes[(end - size)..<end]), count: UInt64 = 1
        func variable(_ reader: inout ByteReader) throws -> Int {
            var value = 0
            for shift in stride(from: 0, to: 21, by: 7) {
                let byte = try reader.u8(); value |= Int(byte & 127) << shift
                if byte & 128 == 0 { return value }
            }
            throw JedError.corruption("invalid GIN run varint")
        }
        while reader.remaining > 0 {
            let shared = try variable(&reader), suffix = try variable(&reader)
            guard shared + suffix > 0 else { throw JedError.corruption("empty GIN posting") }
            _ = try reader.take(suffix); count += 1
        }
        return count
    }
}
