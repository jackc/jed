/// Byte-oriented string primitives and the PostgreSQL hex/base64/escape codecs.
/// No Foundation codecs, host Unicode normalization, or platform libraries enter
/// these formats.
internal enum ByteaFunctions {
    static func call(name: String, args: [SQLValue], constant: Bool, meter: CostMeter) throws -> SQLValue {
        if args.contains(where: \.isNull) { return .null }
        func integer(_ i: Int) throws -> Int64 { guard case .integer(let n) = args[i] else { throw JedError("42804", "integer argument required") }; return n }
        func bytes(_ i: Int) throws -> [UInt8] { guard case .bytea(let b) = args[i] else { throw JedError("42804", "bytea argument required") }; return b }
        if name == "encode" || name == "decode" {
            guard case .text(let format) = args[1] else { throw JedError("42804", "encoding name must be text") }
            let mode = format.asciiLowercased.description
            guard ["hex", "base64", "escape"].contains(mode) else { throw JedError("22023", "unrecognized encoding: \"\(format)\"") }
            if name == "encode" {
                let result = encode(try bytes(0), mode: mode)
                if !constant { meter.charge(Int64(result.utf8.count)); try meter.guardCost() }
                return .text(result)
            }
            guard case .text(let input) = args[0] else { throw JedError("42804", "encoded input must be text") }
            switch mode {
            case "hex": return .bytea(try ScalarCasts.decodeHex(input.utf8))
            case "escape": return .bytea(try ScalarCasts.decodeEscape(input.utf8, code: "22P02"))
            default: return .bytea(try decodeBase64(input))
            }
        }
        let input = try bytes(0)
        switch name {
        case "substr", "substring":
            return .bytea(Array(input[try window(length: input.count, start: integer(1), count: args.count > 2 ? integer(2) : nil)]))
        case "overlay":
            let replacement = try bytes(1), start = try integer(2), count = try args.count > 3 ? integer(3) : Int64(replacement.count)
            guard start > 0 else { throw JedError("22011", "negative substring length not allowed") }
            let end = Int128(start) + Int128(count)
            guard end >= Int32.min, end <= Int32.max else { throw JedError("22003", "integer out of range").withDataType("i32") }
            let head = try window(length: input.count, start: 1, count: start - 1), tail = try window(length: input.count, start: Int64(end), count: nil)
            let result = Array(input[head]) + replacement + Array(input[tail])
            if !constant { meter.charge(Int64(result.count)); try meter.guardCost() }
            return .bytea(result)
        case "position", "strpos": return .integer(Int64(BytePattern(try bytes(1)).first(in: input).map { $0 + 1 } ?? 0))
        case "btrim", "ltrim", "rtrim":
            let set = Set(try bytes(1)); var lo = 0, hi = input.count
            if name != "rtrim" { while lo < hi && set.contains(input[lo]) { lo += 1 } }
            if name != "ltrim" { while hi > lo && set.contains(input[hi - 1]) { hi -= 1 } }
            return .bytea(Array(input[lo..<hi]))
        case "get_byte", "set_byte", "get_bit", "set_bit":
            let index = try integer(1), bit = name.hasSuffix("bit"), length = Int64(input.count) * (bit ? 8 : 1)
            guard index >= 0, index < length else { throw JedError("2202E", "index \(index) out of valid range, 0..\(length - 1)") }
            let position = Int(bit ? index / 8 : index), shift = UInt8(index % 8)
            if name == "get_byte" { return .integer(Int64(input[position])) }
            if name == "get_bit" { return .integer(Int64((input[position] >> shift) & 1)) }
            let value = try integer(2); var output = input
            if bit {
                guard value == 0 || value == 1 else { throw JedError("22023", "new bit must be 0 or 1") }
                output[position] = (output[position] & ~(1 << shift)) | UInt8(value) << shift
            } else { output[position] = UInt8(truncatingIfNeeded: value) }
            return .bytea(output)
        default: throw JedError("0A000", "bytea kernel is not connected: \(name)")
        }
    }
    static func window(length: Int, start: Int64, count: Int64?) throws -> Range<Int> {
        if let count, count < 0 { throw JedError("22011", "negative substring length not allowed") }
        let lower = Int(max(0, min(Int128(length), Int128(start) - 1)))
        let upper = count.map { Int(max(0, min(Int128(length), Int128(start) - 1 + Int128($0)))) } ?? length
        return lower..<max(lower, upper)
    }
    static func encode(_ bytes: [UInt8], mode: String) -> SQLText {
        let hex = Array("0123456789abcdef".utf8), base64 = Array("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/".utf8)
        var result: [UInt8] = []
        switch mode {
        case "hex": for b in bytes { result += [hex[Int(b >> 4)], hex[Int(b & 15)]] }
        case "escape":
            for b in bytes {
                if b == 92 { result += [92, 92] }
                else if b == 0 || b >= 128 { result += [92, 48 + b / 64, 48 + b / 8 % 8, 48 + b % 8] }
                else { result.append(b) }
            }
        default:
            var p = 0, line = 0
            while p + 3 <= bytes.count {
                let n = UInt32(bytes[p]) << 16 | UInt32(bytes[p + 1]) << 8 | UInt32(bytes[p + 2])
                result += [base64[Int(n >> 18)], base64[Int(n >> 12 & 63)], base64[Int(n >> 6 & 63)], base64[Int(n & 63)]]
                p += 3; line += 4
                if line == 76 { result.append(10); line = 0 }
            }
            if p < bytes.count {
                let a = bytes[p], b = p + 1 < bytes.count ? bytes[p + 1] : 0
                result += [base64[Int(a >> 2)], base64[Int((a & 3) << 4 | b >> 4)], p + 1 < bytes.count ? base64[Int((b & 15) << 2)] : 61, 61]
            }
        }
        return SQLText(validatedUTF8: result)
    }
    static func decodeBase64(_ text: SQLText) throws -> [UInt8] {
        var output: [UInt8] = [], position = 0, buffer: UInt32 = 0, padding = 0
        for scalar in text.description.unicodeScalars {
            let c = scalar.value
            if c == 32 || c == 9 || c == 10 || c == 13 { continue }
            let value: UInt32
            if c == 61 {
                if padding == 0 {
                    guard position == 2 || position == 3 else { throw JedError("22023", "unexpected \"=\" while decoding base64 sequence") }
                    padding = position == 2 ? 2 : 1
                }
                value = 0
            } else {
                let decoded: UInt32
                if (65...90).contains(c) { decoded = c - 65 }
                else if (97...122).contains(c) { decoded = c - 97 + 26 }
                else if (48...57).contains(c) { decoded = c - 48 + 52 }
                else if c == 43 { decoded = 62 }
                else if c == 47 { decoded = 63 }
                else { throw JedError("22023", "invalid symbol \"\(scalar)\" found while decoding base64 sequence") }
                value = padding == 0 ? decoded : 0
            }
            buffer = buffer << 6 | value; position += 1
            if position == 4 {
                output.append(UInt8(buffer >> 16))
                if padding < 2 { output.append(UInt8(truncatingIfNeeded: buffer >> 8)) }
                if padding == 0 { output.append(UInt8(truncatingIfNeeded: buffer)) }
                position = 0; buffer = 0; padding = 0
            }
        }
        guard position == 0 else { throw JedError("22023", "invalid base64 end sequence") }
        return output
    }
}

/// Linear byte search preserves SQL's byte identity, including canonically
/// equivalent Unicode strings that Swift String considers equal.
internal struct BytePattern {
    let bytes: [UInt8]
    let failure: [Int]
    init(_ bytes: [UInt8]) {
        self.bytes = bytes
        var table = [Int](repeating: 0, count: bytes.count), length = 0
        if bytes.count > 1 {
            for index in 1..<bytes.count {
                while length > 0 && bytes[index] != bytes[length] { length = table[length - 1] }
                if bytes[index] == bytes[length] { length += 1 }; table[index] = length
            }
        }
        failure = table
    }
    func first(in input: [UInt8]) -> Int? {
        if bytes.isEmpty { return 0 }
        var matched = 0
        for (index, byte) in input.enumerated() {
            while matched > 0 && byte != bytes[matched] { matched = failure[matched - 1] }
            if byte == bytes[matched] { matched += 1 }
            if matched == bytes.count { return index + 1 - matched }
        }
        return nil
    }
    /// Walk non-overlapping matches without retaining an input-sized index
    /// array. Amplifiers count first, admit their output, then walk again.
    func forEachPosition(in input: [UInt8], _ visit: (Int) -> Void) {
        guard !bytes.isEmpty else { return }
        var matched = 0
        for (index, byte) in input.enumerated() {
            while matched > 0 && byte != bytes[matched] { matched = failure[matched - 1] }
            if byte == bytes[matched] { matched += 1 }
            if matched == bytes.count { visit(index + 1 - matched); matched = 0 }
        }
    }
    func count(in input: [UInt8]) -> Int {
        var count = 0
        forEachPosition(in: input) { _ in count += 1 }
        return count
    }
}
