/// The byte-pinned LZ4 block algorithm in spec/fileformat/lz4.md.
internal enum LZ4 {
    private static func word(_ bytes: [UInt8], _ position: Int) -> UInt32 {
        UInt32(bytes[position]) | (UInt32(bytes[position + 1]) << 8)
            | (UInt32(bytes[position + 2]) << 16) | (UInt32(bytes[position + 3]) << 24)
    }

    private static func extend(_ length: Int, into output: inout [UInt8]) {
        var remaining = length
        while remaining >= 255 { output.append(255); remaining -= 255 }
        output.append(UInt8(remaining))
    }

    static func compress(_ input: [UInt8]) -> [UInt8] {
        var output: [UInt8] = []
        var table = [Int](repeating: -1, count: 4096)
        var position = 0
        var anchor = 0
        let limit = input.count - 12
        while position <= limit {
            let current = word(input, position)
            let hash = Int((current &* 2_654_435_761) >> 20)
            let candidate = table[hash]
            table[hash] = position
            if candidate >= 0 && position - candidate <= 65_535 && word(input, candidate) == current {
                var length = 4
                while position + length < input.count - 5 && input[candidate + length] == input[position + length] {
                    length += 1
                }
                let literals = position - anchor
                output.append(UInt8(min(literals, 15) << 4 | min(length - 4, 15)))
                if literals >= 15 { extend(literals - 15, into: &output) }
                output.append(contentsOf: input[anchor..<position])
                let distance = position - candidate
                output.append(UInt8(distance & 255))
                output.append(UInt8(distance >> 8))
                if length >= 19 { extend(length - 19, into: &output) }
                position += length
                anchor = position
            } else {
                position += 1
            }
        }
        let literals = input.count - anchor
        output.append(UInt8(min(literals, 15) << 4))
        if literals >= 15 { extend(literals - 15, into: &output) }
        output.append(contentsOf: input[anchor...])
        return output
    }

    static func decompress(_ input: [UInt8], rawLength: Int) throws -> [UInt8] {
        guard rawLength >= 0 && !input.isEmpty else {
            throw JedError.corruption("invalid LZ4 block")
        }
        var position = 0
        var output: [UInt8] = []
        // Do not reserve rawLength: a hostile length field must not allocate
        // storage before the block has demonstrated the bytes it expands to.
        func length(_ initial: Int) throws -> Int {
            var result = initial
            if initial == 15 {
                while true {
                    guard position < input.count else { throw JedError.corruption("truncated LZ4 length") }
                    let byte = Int(input[position])
                    position += 1
                    guard result <= rawLength && byte <= rawLength - result else {
                        throw JedError.corruption("LZ4 length exceeds declared output")
                    }
                    result += byte
                    if byte < 255 { break }
                }
            }
            return result
        }
        while position < input.count {
            let token = input[position]
            position += 1
            let literals = try length(Int(token >> 4))
            guard literals <= input.count - position && literals <= rawLength - output.count else {
                throw JedError.corruption("invalid LZ4 literals")
            }
            output.append(contentsOf: input[position..<(position + literals)])
            position += literals
            if position == input.count {
                guard output.count == rawLength else { throw JedError.corruption("LZ4 output length mismatch") }
                return output
            }
            guard input.count - position >= 2 else { throw JedError.corruption("truncated LZ4 offset") }
            let distance = Int(input[position]) | (Int(input[position + 1]) << 8)
            position += 2
            guard distance > 0 && distance <= output.count else { throw JedError.corruption("invalid LZ4 offset") }
            let extensionLength = try length(Int(token & 15))
            guard rawLength - output.count >= 4 && extensionLength <= rawLength - output.count - 4 else {
                throw JedError.corruption("LZ4 match exceeds declared output")
            }
            for _ in 0..<(extensionLength + 4) { output.append(output[output.count - distance]) }
        }
        throw JedError.corruption("LZ4 block has no final literals")
    }
}
