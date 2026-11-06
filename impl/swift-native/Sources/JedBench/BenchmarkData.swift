import Foundation
import Jed

struct BenchmarkError: Error, CustomStringConvertible, Sendable {
    let description: String
    init(_ description: String) { self.description = description }
}

/// Mechanical JSON projection of the pinned shared TOML. SQL remains data.
struct BenchmarkInput: Decodable, Sendable {
    let schema: Int
    let fingerprint: String
    let benchmarks: [BenchmarkCase]
    let tableRows: [String: [String: Int64]]
}

struct BenchmarkCase: Decodable, Sendable {
    let name: String
    let dataset: String
    let kind: String
    let sql: String
    let setupSQL: [String]
    let warmup: Int
    let iterations: Int
    let seed: UInt64
    let expectRowsPerIter: Int
    let batch: Int
    let readers: Int
    let relaxed: Bool
    let hostFunctions: Bool
    let parameters: [BenchmarkParameter]

    func validate() throws {
        guard !name.isEmpty, !sql.isEmpty, warmup >= 0, iterations > 0,
              ["query", "write_rollback", "write_durable", "concurrent_read", "cold_open"].contains(kind),
              ["small", "large", "gin", "text", "scratch"].contains(dataset),
              kind != "write_rollback" || batch > 0,
              kind != "concurrent_read" || readers > 0,
              kind != "cold_open" || dataset != "scratch" && setupSQL.isEmpty,
              !relaxed || kind == "write_durable" && dataset == "scratch" else {
            throw BenchmarkError("invalid shared workload \(name)")
        }
        for (index, parameter) in parameters.enumerated() { try parameter.validate(index: index, earlier: parameters) }
    }
    func checkRows(_ count: Int) throws {
        if expectRowsPerIter > 0, count != expectRowsPerIter {
            throw BenchmarkError("\(name): expected \(expectRowsPerIter) rows, got \(count)")
        }
    }
    var writeTable: String {
        get throws {
            let words = sql.split(whereSeparator: { $0.isWhitespace })
            for index in words.indices.dropLast() where ["INTO", "UPDATE", "FROM"].contains(words[index].uppercased()) {
                return String(words[index + 1].split(separator: "(")[0])
            }
            throw BenchmarkError("write workload has no target table: \(name)")
        }
    }
}

struct BenchmarkParameter: Decodable, Sendable {
    let gen: String
    let min: Int64
    let max: Int64
    let start: Int64
    let minLen: Int
    let maxLen: Int
    let base: Int
    let offMin: Int64
    let offMax: Int64
    let elemMin: Int64
    let elemMax: Int64

    func validate(index: Int, earlier: [BenchmarkParameter]) throws {
        guard ["serial", "int_uniform", "int_window", "text", "int_array"].contains(gen) else {
            throw BenchmarkError("unknown parameter generator \(gen)")
        }
        if gen == "int_uniform" { try Self.span(min, max) }
        if gen == "int_window" {
            guard base >= 0, base < index, ["serial", "int_uniform", "int_window"].contains(earlier[base].gen) else {
                throw BenchmarkError("window parameter requires an earlier integer")
            }
            try Self.span(offMin, offMax)
        }
        if gen == "text" || gen == "int_array" {
            guard minLen >= 0, maxLen >= minLen else { throw BenchmarkError("invalid parameter length") }
            if gen == "int_array" { try Self.span(elemMin, elemMax) }
        }
    }
    private static func span(_ low: Int64, _ high: Int64) throws {
        let width = high.subtractingReportingOverflow(low)
        guard high >= low, !width.overflow, width.partialValue < Int64.max else {
            throw BenchmarkError("parameter range is out of bounds")
        }
    }
}

struct BenchmarkRandom {
    var state: UInt64
    mutating func next() -> UInt64 {
        state &+= 0x9e3779b97f4a7c15
        var value = state
        value = (value ^ (value >> 30)) &* 0xbf58476d1ce4e5b9
        value = (value ^ (value >> 27)) &* 0x94d049bb133111eb
        return value ^ (value >> 31)
    }
    mutating func integer(_ low: Int64, _ high: Int64) -> Int64 { low + Int64(next() % UInt64(high - low + 1)) }
}

struct BenchmarkParameters {
    let parameters: [BenchmarkParameter]
    var random: BenchmarkRandom
    var serials: [Int64]
    init(_ benchmark: BenchmarkCase) {
        parameters = benchmark.parameters; random = BenchmarkRandom(state: benchmark.seed)
        serials = parameters.map(\.start)
    }
    mutating func next() throws -> [SQLValue] {
        var values: [SQLValue] = []
        values.reserveCapacity(parameters.count)
        for (index, parameter) in parameters.enumerated() {
            switch parameter.gen {
            case "serial":
                let next = serials[index].addingReportingOverflow(1)
                guard !next.overflow else { throw BenchmarkError("serial parameter overflow") }
                values.append(.integer(serials[index])); serials[index] = next.partialValue
            case "int_uniform": values.append(.integer(random.integer(parameter.min, parameter.max)))
            case "int_window":
                guard case .integer(let base) = values[parameter.base] else { throw BenchmarkError("window base is not integer") }
                let value = base.addingReportingOverflow(random.integer(parameter.offMin, parameter.offMax))
                guard !value.overflow else { throw BenchmarkError("window parameter overflow") }
                values.append(.integer(value.partialValue))
            case "text":
                let count = Int(random.integer(Int64(parameter.minLen), Int64(parameter.maxLen)))
                let bytes = (0..<count).map { _ in UInt8(97 + random.next() % 26) }
                values.append(.text(try SQLText(utf8: bytes)))
            case "int_array":
                let count = Int(random.integer(Int64(parameter.minLen), Int64(parameter.maxLen)))
                let elements = (0..<count).map { _ in SQLValue.integer(random.integer(parameter.elemMin, parameter.elemMax)) }
                values.append(.array(try SQLArray(elements: elements)))
            default: throw BenchmarkError("unknown parameter generator")
            }
        }
        return values
    }
}

struct BenchmarkChecksum: Sendable {
    private var value: UInt64 = 0xcbf29ce484222325
    mutating func byte(_ byte: UInt8) { value = (value ^ UInt64(byte)) &* 0x100000001b3 }
    mutating func text(_ bytes: some Sequence<UInt8>) { for item in bytes { byte(item) }; byte(0x1f) }
    mutating func integer(_ number: Int64) { text(String(number).utf8) }
    mutating func row(_ values: [SQLValue]) throws {
        for value in values {
            switch value {
            case .null: text("NULL".utf8)
            case .integer(let number): integer(number)
            case .text(let string): text(string.utf8)
            default: throw BenchmarkError("benchmark result contains an unsupported checksum type")
            }
        }
        byte(0x1e)
    }
    var hex: String { String(repeating: "0", count: 16 - String(value, radix: 16).count) + String(value, radix: 16) }
}

func benchmarkSelfCheck() throws {
    let vectors: [(UInt64, [UInt64])] = [
        (1, [0x910a2dec89025cc1, 0xbeeb8da1658eec67, 0xf893a2eefb32555e, 0x71c18690ee42c90b, 0x71bb54d8d101b5b9]),
        (1234567, [0x599ed017fb08fc85, 0x2c73f08458540fa5, 0x883ebce5a3f27c77, 0x3fbef740e9177b3f, 0xe3b8346708cb5ecd]),
    ]
    for (seed, expected) in vectors {
        var random = BenchmarkRandom(state: seed)
        guard expected.allSatisfy({ $0 == random.next() }) else { throw BenchmarkError("shared PRNG vector mismatch") }
    }
    var checksum = BenchmarkChecksum()
    try checksum.row([.integer(1), .null, .text(SQLText("abc"))]); try checksum.row([.integer(-7)])
    guard checksum.hex == "dd6e60407d30d28b" else { throw BenchmarkError("shared checksum vector mismatch") }
}
