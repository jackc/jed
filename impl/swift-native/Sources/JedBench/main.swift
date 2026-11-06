import Foundation
import Jed

try benchmarkSelfCheck()
let arguments = Array(CommandLine.arguments.dropFirst())
if arguments == ["--check-adapter"] {
    print("benchmark PRNG and checksum vectors: PASS")
} else {
    guard arguments.count == 4 else {
        throw BenchmarkError("usage: jed-bench <mechanical-input.json> <data-dir> <results.jsonl> <samples.jsonl>")
    }
    let input = try JSONDecoder().decode(BenchmarkInput.self, from: Data(contentsOf: URL(fileURLWithPath: arguments[0])))
    guard input.schema == 1, input.fingerprint.count == 64, !input.benchmarks.isEmpty,
          Set(input.benchmarks.map(\.name)).count == input.benchmarks.count else { throw BenchmarkError("invalid benchmark input") }
    for benchmark in input.benchmarks { try benchmark.validate() }
    let runner = BenchmarkRunner(input: input, data: URL(fileURLWithPath: arguments[1], isDirectory: true))
    for path in arguments[2...] {
        guard !FileManager.default.fileExists(atPath: path), FileManager.default.createFile(atPath: path, contents: nil) else {
            throw BenchmarkError("benchmark output already exists or cannot be created: \(path)")
        }
    }
    let output = try FileHandle(forWritingTo: URL(fileURLWithPath: arguments[2]))
    let samples = try FileHandle(forWritingTo: URL(fileURLWithPath: arguments[3]))
    defer { try? output.close(); try? samples.close() }
    for benchmark in input.benchmarks {
        try FileHandle.standardError.write(contentsOf: Data("jed/swift/experimental-native: \(benchmark.name) (\(benchmark.dataset)) ...\n".utf8))
        let result = try runner.run(benchmark)
        try output.write(contentsOf: Data((try result.json + "\n").utf8))
        try samples.write(contentsOf: result.sampleJSON + Data([10]))
    }
}
