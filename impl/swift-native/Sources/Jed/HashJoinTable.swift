/// Hash buckets are lookup-only; entries keep build order and probes compare
/// complete framed key bytes. Host dictionary iteration never emits rows.
internal final class HashJoinTable {
    private struct Entry { let key: [UInt8]; let position: Int }
    private var buckets: [UInt64: [Entry]] = [:]
    private let keys: [(column: Int, type: TypeDescriptor)]
    private let context: ExpressionContext
    private var reserved: Int64 = 0

    init(rows: [[SQLValue]], keys: [(column: Int, type: TypeDescriptor)], context: ExpressionContext) throws {
        self.keys = keys; self.context = context
        do {
            for (position, row) in rows.enumerated() {
                guard let key = try Self.encode(row, keys: keys, context: context) else { continue }
                let bytes = keys.reduce(GeneratedSpec.memoryEntry) { saturatingAdd($0, MemorySizes.value(row[$1.column])) }
                try context.meter.reserveQuery(bytes); reserved = saturatingAdd(reserved, bytes)
                buckets[Self.hash(key), default: []].append(Entry(key: key, position: position))
            }
        } catch { close(); throw error }
    }
    func probe(_ row: [SQLValue], keys: [(column: Int, type: TypeDescriptor)], visit: (Int) throws -> Void) throws {
        guard let key = try Self.encode(row, keys: keys, context: context) else { return }
        for entry in buckets[Self.hash(key)] ?? [] {
            context.meter.charge(Int64(max(1, min(key.count, entry.key.count)))); try context.meter.step()
            if entry.key == key { try visit(entry.position) }
        }
    }
    private static func encode(_ row: [SQLValue], keys: [(column: Int, type: TypeDescriptor)], context: ExpressionContext) throws -> [UInt8]? {
        var writer = ByteWriter(), present = true
        for key in keys {
            let value = row[key.column]
            if value.isNull { context.meter.charge(1); try context.meter.step(); present = false; continue }
            var type = key.type; type.collation = nil
            let bytes = try KeyEncoding.encode(value, type: type)
            context.meter.charge(Int64(max(1, bytes.count))); try context.meter.step()
            guard bytes.count <= UInt32.max else { throw JedError("54000", "hash key is too large") }
            writer.u32(UInt32(bytes.count)); writer.append(bytes)
        }
        return present ? writer.bytes : nil
    }
    private static func hash(_ bytes: [UInt8]) -> UInt64 {
        var value: UInt64 = 14_695_981_039_346_656_037
        for byte in bytes { value = (value ^ UInt64(byte)) &* 1_099_511_628_211 }
        return value
    }
    func close() { buckets = [:]; context.meter.resources.query.release(reserved); reserved = 0 }
    deinit { close() }
}
