import Foundation
@testable import Jed

/// Optional conformance observations, never engine admission inputs. Record
/// directives override these whole-run defaults exactly as in the shared runners.
struct SQLCorpusAccounting {
    let queryMemory: Int64?
    let workMemory: Int64?
    let temporaryBuffers: Int64?
    let peakPath: String?

    init(environment: [String: String] = ProcessInfo.processInfo.environment) throws {
        func integer(_ name: String, nonnegative: Bool) throws -> Int64? {
            guard let text = environment[name] else { return nil }
            guard let value = Int64(text), !nonnegative || value >= 0 else {
                throw SQLCorpusError("\(name) must be \(nonnegative ? "nonnegative " : "")integer bytes")
            }
            return value
        }
        queryMemory = try integer("JED_CONFORMANCE_QUERY_MEMORY", nonnegative: false)
        workMemory = try integer("JED_CONFORMANCE_WORK_MEM", nonnegative: true)
        temporaryBuffers = try integer("JED_CONFORMANCE_TEMP_BUFFERS", nonnegative: true)
        peakPath = environment["JED_CONFORMANCE_QUERY_MEMORY_PEAKS"]
        if let peakPath, peakPath.isEmpty { throw SQLCorpusError("query-memory peak path must not be empty") }
    }

    func record(name: String, ordinal: Int, storage: UInt64) throws {
        guard let peakPath else { return }
        guard !name.isEmpty, !name.contains("\t"), !name.contains("\n"), ordinal > 0 else {
            throw SQLCorpusError("query-memory peak record has invalid identity")
        }
        if !FileManager.default.fileExists(atPath: peakPath) {
            guard FileManager.default.createFile(atPath: peakPath, contents: nil) else {
                throw SQLCorpusError("cannot create query-memory peak output")
            }
        }
        let output = try FileHandle(forWritingTo: URL(fileURLWithPath: peakPath))
        defer { try? output.close() }
        try output.seekToEnd()
        try output.write(contentsOf: Data("\(name)\t\(ordinal)\t\(MemoryDiagnostics.shared.peak)\t\(storage)\n".utf8))
    }
}
