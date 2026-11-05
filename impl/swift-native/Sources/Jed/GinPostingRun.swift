/// Byte-exact v47 posting-run codec. A record contains a term, its largest row
/// key, front-coded earlier keys, and the two-byte front-coded body length.
internal enum GinPostingRun {
    static let maximumBytes = 64
    private static func invalid() -> JedError { JedError.corruption("a GIN posting run is malformed") }
    private static func shared(_ a: [UInt8], _ b: [UInt8]) -> Int {
        var index = 0
        while index < min(a.count, b.count), a[index] == b[index] { index += 1 }
        return index
    }
    private static func varint(_ value: Int, into writer: inout ByteWriter) throws {
        guard value >= 0, value < 1 << 21 else { throw invalid() }
        var rest = value
        repeat {
            let byte = UInt8(rest & 127); rest >>= 7
            writer.u8(byte | (rest == 0 ? 0 : 128))
        } while rest != 0
    }
    private static func varint(_ reader: inout ByteReader) throws -> Int {
        var value = 0
        for shift in stride(from: 0, to: 21, by: 7) {
            let byte = try reader.u8(); value |= Int(byte & 127) << shift
            if byte & 128 == 0 { return value }
        }
        throw invalid()
    }
    static func encode(term: [UInt8], keys: [[UInt8]]) throws -> [UInt8] {
        guard let last = keys.last, !last.isEmpty, keys.allSatisfy({ !$0.isEmpty }),
              zip(keys, keys.dropFirst()).allSatisfy({ $0.lexicographicallyPrecedes($1) }) else { throw invalid() }
        var body = ByteWriter(), previous = last
        for key in keys.dropLast() {
            let prefix = shared(previous, key)
            try varint(prefix, into: &body); try varint(key.count - prefix, into: &body)
            body.append(key.dropFirst(prefix)); previous = key
        }
        guard body.bytes.count <= UInt16.max else { throw invalid() }
        var writer = ByteWriter(); writer.append(term); writer.append(last); writer.append(body.bytes)
        writer.u16(UInt16(body.bytes.count)); return writer.bytes
    }
    static func decode(termLength: Int, bytes: [UInt8]) throws -> [[UInt8]] {
        guard termLength >= 0, termLength <= bytes.count, bytes.count - termLength >= 3 else { throw invalid() }
        var lengthReader = ByteReader(bytes.suffix(2))
        let bodyLength = Int(try lengthReader.u16()), bodyEnd = bytes.count - 2, lastEnd = bodyEnd - bodyLength
        guard lastEnd > termLength else { throw invalid() }
        // A persisted multi-key run is at most 64 bytes; only a singleton may
        // exceed that limit. This also bounds front-code reconstruction before
        // allocating from untrusted page bytes.
        guard bodyLength == 0 || bytes.count <= maximumBytes else { throw invalid() }
        let last = Array(bytes[termLength..<lastEnd])
        var reader = ByteReader(bytes[lastEnd..<bodyEnd]), result: [[UInt8]] = [], previous = last
        while reader.remaining > 0 {
            let prefix = try varint(&reader), suffix = try varint(&reader)
            guard prefix <= previous.count, suffix <= reader.remaining, prefix + suffix > 0 else { throw invalid() }
            let key = Array(previous.prefix(prefix)) + (try reader.take(suffix))
            guard result.last.map({ $0.lexicographicallyPrecedes(key) }) ?? true else { throw invalid() }
            result.append(key); previous = key
        }
        guard result.last.map({ $0.lexicographicallyPrecedes(last) }) ?? true else { throw invalid() }
        result.append(last); return result
    }
    static func pack(term: [UInt8], keys: [[UInt8]]) throws -> [[UInt8]] {
        var runs: [[UInt8]] = [], start = 0
        while start < keys.count {
            var end = start + 1
            while end < keys.count, try encode(term: term, keys: Array(keys[start...end])).count <= maximumBytes { end += 1 }
            runs.append(try encode(term: term, keys: Array(keys[start..<end]))); start = end
        }
        return runs
    }
    static func split(term: [UInt8], keys: [[UInt8]]) throws -> [[UInt8]] {
        guard !keys.isEmpty else { return [] }
        let run = try encode(term: term, keys: keys)
        if keys.count == 1 || run.count <= maximumBytes { return [run] }
        let middle = (keys.count + 1) / 2
        return try split(term: term, keys: Array(keys[..<middle])) + split(term: term, keys: Array(keys[middle...]))
    }
}
