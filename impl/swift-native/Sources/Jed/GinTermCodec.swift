/// Integrity/rebuild walks know the column opclass and can recover a run's term
/// prefix without decoding its storage key. Gather already knows this prefix.
internal extension GinOpclass {
    func termLength(in bytes: [UInt8], start: Int = 0) throws -> Int {
        guard start >= 0, start < bytes.count else { throw JedError.corruption("truncated GIN term") }
        func terminated(from offset: Int, truncated: Bool? = nil) throws -> Int {
            var offset = offset, content = 0
            while offset < bytes.count {
                let byte = bytes[offset]; offset += 1
                if byte == 0 {
                    guard offset < bytes.count else { throw JedError.corruption("truncated GIN escape") }
                    let escape = bytes[offset]; offset += 1
                    if escape == 1 {
                        guard content <= 128, truncated != true || content == 128 else {
                            throw JedError.corruption("invalid truncated GIN term")
                        }
                        return offset - start
                    }
                    guard escape == 255 else { throw JedError.corruption("invalid GIN escape") }
                }
                content += 1
                guard content <= 128 else { throw JedError.corruption("oversized GIN term") }
            }
            throw JedError.corruption("unterminated GIN term")
        }
        guard let element else {
            let flag = bytes[start], kind = flag & ~0x10
            guard [UInt8(1), 2, 3, 4, 5].contains(kind), flag & ~0x1f == 0 else {
                throw JedError.corruption("unknown JSON GIN term flag")
            }
            if kind == 2 {
                guard flag == 2 else { throw JedError.corruption("truncated JSON null term") }
                return 1
            }
            if kind == 3 {
                guard flag == 3, bytes.count - start >= 2, bytes[start + 1] <= 1 else {
                    throw JedError.corruption("invalid JSON boolean term")
                }
                return 2
            }
            return try terminated(from: start + 1, truncated: flag & 0x10 != 0)
        }
        let width: Int
        switch element {
        case .text, .bytea: return try terminated(from: start)
        case .decimal:
            let sign = bytes[start]
            if sign == 4 { return 1 }
            guard sign == 3 || sign == 5, bytes.count - start >= 6 else { throw JedError.corruption("invalid numeric GIN term") }
            let terminal: UInt8 = sign == 3 ? 255 : 0
            var offset = start + 5
            while offset - start < 128, offset < bytes.count {
                let byte = bytes[offset]; offset += 1
                if byte == terminal { return offset - start }
                guard sign == 3 ? (155...254).contains(byte) : (1...100).contains(byte) else {
                    throw JedError.corruption("invalid numeric GIN digit")
                }
            }
            guard offset - start == 128 else { throw JedError.corruption("truncated numeric GIN term") }
            return 128
        case .i16: width = 2
        case .i32, .f32, .date: width = 4
        case .i64, .f64, .timestamp, .timestamptz: width = 8
        case .boolean: width = 1
        case .uuid, .interval: width = 16
        default: throw JedError.corruption("invalid GIN scalar opclass")
        }
        guard bytes.count - start >= width,
              element != .boolean || bytes[start] <= 1 else { throw JedError.corruption("invalid fixed-width GIN term") }
        return width
    }
}

internal extension GinIndex {
    static func decodeRun(_ record: [UInt8], opclasses: [GinOpclass]) throws -> (term: [UInt8], keys: [[UInt8]]) {
        guard (1...32).contains(opclasses.count), !record.isEmpty else { throw JedError.corruption("invalid GIN tuple width") }
        let prefix = opclasses.count == 1 ? 0 : 1
        let column = prefix == 0 ? 0 : Int(record[0])
        guard column < opclasses.count else { throw JedError.corruption("invalid GIN column tag") }
        let length = try opclasses[column].termLength(in: record, start: prefix) + prefix
        return (Array(record.prefix(length)), try GinPostingRun.decode(termLength: length, bytes: record))
    }
}
