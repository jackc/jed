internal enum RecordInput: Sendable {
    case value(SQLValue)
    /// Only an unchanged column of the same record may retain an external
    /// reference. Row-copying operations materialize their source values first.
    case kept(StoredCell, pending: PendingOverflow? = nil)
}

/// The deterministic two-pass large-value plan. Split weights and serialization
/// use its exact stored cells; compression is charged only at the write site.
internal enum RecordDisposition {
    private struct Candidate {
        let type: TypeDescriptor
        var original: StoredCell?
        var pending: PendingOverflow?
        var raw: [UInt8]?
        var rawLength = 0
        var compressed: [UInt8]?
        var compressedLength: Int?
        var compressionRejected = false
        var null = false
        var fixed: Int?
        var form: UInt8 = 0
        var plainSize = 0
        var size = 0
    }

    static func recordMaximum(capacity: Int, columns: Int) -> Int {
        max(0, (capacity - 12 - 16 * columns) / 2)
    }

    private static func shortLength(_ type: SQLType) -> Bool {
        switch type {
        case .scalar(.text), .scalar(.bytea), .scalar(.json), .scalar(.jsonpath): true
        default: false
        }
    }

    static func plan(key: [UInt8], values: [RecordInput], types: [TypeDescriptor],
                     codec: any PageCodec, meter: CostMeter? = nil, pager: Pager? = nil,
                     pageCount: UInt32? = nil) throws -> PackedRecord {
        guard values.count == types.count else {
            throw JedError("0A000", "record value count differs from its type layout")
        }
        var candidates: [Candidate] = []
        var total = key.count
        for (column, input) in values.enumerated() {
            let type = types[column], width = ValueCodec.fixedWidth(type.type)
            var candidate = Candidate(type: type, fixed: width)
            switch input {
            case .value(let value):
                candidate.null = value.isNull
                if !value.isNull { candidate.raw = try ValueCodec.encodeRawBody(value, type: type) }
                candidate.rawLength = candidate.raw?.count ?? 0
            case .kept(let stored, let pending):
                guard stored.type == type else { throw JedError("42804", "kept value changed its static type") }
                candidate.original = stored; candidate.null = stored.isNull
                if let pending {
                    guard let facts = try stored.largeFacts(encrypted: codec.reservedBytes != 0),
                          let head = facts.external,
                          head.index >= codec.firstBodyPage || head.index == 0 && head.tag.allSatisfy({ $0 == 0 }),
                          !pending.bytes.isEmpty, pending.bytes.count == facts.stored,
                          pending.rawLength == facts.raw, pending.compressed == facts.compressed else {
                        throw JedError.corruption("retained kept value disagrees with its external cell")
                    }
                    // A newly staged value has a zero placeholder. A committed
                    // resident root may retain the same bytes beside its real
                    // sealed chain reference; both carry these exact facts.
                    candidate.pending = pending
                }
                if let bytes = stored.bytes {
                    if width != nil { candidate.raw = Array(bytes); candidate.rawLength = bytes.count }
                    else if let facts = try stored.largeFacts(encrypted: codec.reservedBytes != 0) {
                        guard facts.external?.index != 0 || pending != nil else {
                            throw JedError.corruption("kept placeholder is missing its pending value")
                        }
                        candidate.rawLength = facts.raw
                        if facts.compressed {
                            candidate.compressedLength = facts.stored
                            if facts.external == nil { candidate.compressed = Array(bytes.dropFirst(7)) }
                        } else { candidate.compressionRejected = true }
                    } else {
                        var reader = ByteReader(bytes)
                        guard try reader.u8() == 0 else { throw JedError.corruption("invalid kept value form") }
                        if shortLength(type.type) {
                            let count = Int(try reader.u16())
                            guard count == reader.remaining else { throw JedError.corruption("invalid inline value length") }
                        }
                        candidate.raw = Array(try reader.take(reader.remaining))
                        candidate.rawLength = candidate.raw!.count
                    }
                }
            }
            guard candidate.rawLength <= UInt32.max else { throw JedError("0A000", "value exceeds the byte-format limit") }
            candidate.plainSize = width ?? (candidate.null ? 0 : candidate.rawLength + (shortLength(type.type) ? 3 : 1))
            candidate.size = candidate.plainSize
            total += candidate.size; candidates.append(candidate)
        }
        let maximum = recordMaximum(capacity: codec.capacity, columns: types.count)
        let compressionOrder = candidates.indices.filter {
            candidates[$0].fixed == nil && !candidates[$0].null && candidates[$0].rawLength >= 32
        }.sorted {
            candidates[$0].plainSize == candidates[$1].plainSize ? $0 < $1 : candidates[$0].plainSize > candidates[$1].plainSize
        }
        // Compression work is quantized by logical capacity C. Encrypted
        // overflow chains reserve an additional successor tag, but that does
        // not change this cost quantum (cost.md, encrypted.md).
        let slab = codec.capacity
        for index in compressionOrder where total > maximum {
            if let meter {
                meter.charge(Int64((candidates[index].rawLength + slab - 1) / slab))
                try meter.guardCost()
            }
            if candidates[index].compressionRejected { continue }
            if candidates[index].compressedLength == nil {
                let raw = try rawBytes(candidates[index], codec: codec, pager: pager, pageCount: pageCount)
                let compressed = LZ4.compress(raw)
                candidates[index].compressed = compressed
                candidates[index].compressedLength = compressed.count
            }
            let encoded = 7 + candidates[index].compressedLength!
            if encoded < candidates[index].plainSize {
                total -= candidates[index].size - encoded
                candidates[index].size = encoded; candidates[index].form = 3
            }
        }
        let extraTag = codec.reservedBytes == 0 ? 0 : 16
        let externalOrder = candidates.indices.filter {
            let item = candidates[$0]
            return item.fixed == nil && !item.null && item.size > (item.form == 3 ? 13 : 9) + extraTag
        }.sorted {
            candidates[$0].size == candidates[$1].size ? $0 < $1 : candidates[$0].size > candidates[$1].size
        }
        for index in externalOrder where total > maximum {
            let encoded = (candidates[index].form == 3 ? 13 : 9) + extraTag
            total -= candidates[index].size - encoded
            candidates[index].size = encoded; candidates[index].form = candidates[index].form == 3 ? 4 : 2
        }
        // RECORD_MAX drives the two disposition passes; irreducible fixed
        // slots and pointers may remain above it. The tree retains the pending
        // record; commit preflight checks its exact leaf and separator fit.
        var cells: [StoredCell] = [], overflows: [Int: PendingOverflow] = [:]
        for (column, item) in candidates.enumerated() {
            if item.null { cells.append(StoredCell(bytes: nil, type: item.type)); continue }
            if let original = item.original,
               item.fixed != nil || original.bytes?.first == item.form {
                if let pending = item.pending { overflows[column] = pending }
                cells.append(original); continue
            }
            let encoded: [UInt8]
            if item.fixed != nil {
                encoded = try rawBytes(item, codec: codec, pager: pager, pageCount: pageCount)
            } else if item.form == 0 {
                let raw = try rawBytes(item, codec: codec, pager: pager, pageCount: pageCount)
                var writer = ByteWriter(); writer.u8(0)
                if shortLength(item.type.type) {
                    guard raw.count <= UInt16.max else { throw JedError("0A000", "inline value exceeds its length field") }
                    writer.u16(UInt16(raw.count))
                }
                writer.append(raw); encoded = writer.bytes
            } else if item.form == 3 {
                let compressed = try compressedBytes(item, codec: codec, pager: pager, pageCount: pageCount)
                guard compressed.count <= UInt16.max else { throw JedError("0A000", "inline compressed value exceeds its length field") }
                var writer = ByteWriter(); writer.u8(3); writer.u32(UInt32(item.rawLength))
                writer.u16(UInt16(compressed.count)); writer.append(compressed); encoded = writer.bytes
            } else {
                let compressed = item.form == 4
                let content = compressed ? try compressedBytes(item, codec: codec, pager: pager, pageCount: pageCount)
                    : try rawBytes(item, codec: codec, pager: pager, pageCount: pageCount)
                let pending = PendingOverflow(bytes: content, rawLength: item.rawLength, compressed: compressed)
                overflows[column] = pending
                encoded = try pending.pointer(PageSealer.nullReference(codec))
            }
            cells.append(StoredCell(bytes: encoded[...], type: item.type))
        }
        return PackedRecord(key: key, cells: cells, overflows: overflows)
    }

    private static func storedContent(_ item: Candidate, codec: any PageCodec, pager: Pager?,
                                      pageCount: UInt32?) throws -> [UInt8] {
        if let pending = item.pending { return pending.bytes }
        guard let original = item.original, let facts = try original.largeFacts(encrypted: codec.reservedBytes != 0),
              let reference = facts.external, let pager else { throw JedError.corruption("kept chain has no owning pager") }
        return try OverflowChain.read(reference, length: facts.stored,
            pageCount: pageCount ?? pager.selected.meta.pageCount, codec: codec, read: pager.readBody)
    }

    private static func compressedBytes(_ item: Candidate, codec: any PageCodec, pager: Pager?,
                                        pageCount: UInt32?) throws -> [UInt8] {
        if let compressed = item.compressed { return compressed }
        if item.compressedLength != nil { return try storedContent(item, codec: codec, pager: pager, pageCount: pageCount) }
        return LZ4.compress(try rawBytes(item, codec: codec, pager: pager, pageCount: pageCount))
    }

    private static func rawBytes(_ item: Candidate, codec: any PageCodec, pager: Pager?,
                                 pageCount: UInt32?) throws -> [UInt8] {
        if let raw = item.raw { return raw }
        if item.compressedLength != nil {
            return try LZ4.decompress(compressedBytes(item, codec: codec, pager: pager, pageCount: pageCount), rawLength: item.rawLength)
        }
        return try storedContent(item, codec: codec, pager: pager, pageCount: pageCount)
    }
}
