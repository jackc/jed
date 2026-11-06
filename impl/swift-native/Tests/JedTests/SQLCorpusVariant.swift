import Foundation
@testable import Jed

/// Shared encrypted corpus mode: ordinary values/errors still use the corpus;
/// only cost-bearing outputs go to the mandatory cross-core transcript.
struct SQLCorpusVariant {
    let encrypted: Bool
    let transcriptPath: String?
    let credential: Credential?

    init(environment: [String: String] = ProcessInfo.processInfo.environment) throws {
        let value = environment["JED_CONFORMANCE_ENCRYPTED"]
        guard value == nil || value == "0" || value == "1" else {
            throw SQLCorpusError("JED_CONFORMANCE_ENCRYPTED must be 0 or 1")
        }
        encrypted = value == "1"
        credential = try encrypted ? .hostKey(Array(repeating: 0x4a, count: 32)) : nil
        transcriptPath = environment["JED_CONFORMANCE_TRANSCRIPT"]
        if encrypted, transcriptPath?.isEmpty != false {
            throw SQLCorpusError("encrypted corpus requires JED_CONFORMANCE_TRANSCRIPT")
        }
    }

    func validate(mode: SQLCorpusRunner.Mode) throws {
        guard !encrypted || mode == .disk else { throw SQLCorpusError("encrypted corpus requires disk mode") }
    }

    static func isExplain(_ sql: String) -> Bool {
        sql.drop(while: \.isWhitespace).prefix(7).lowercased() == "explain"
    }

    static func flat(_ value: String) -> String {
        value.replacingOccurrences(of: "\\", with: "\\\\")
            .replacingOccurrences(of: "\n", with: "\\n")
            .replacingOccurrences(of: "\t", with: "\\t")
    }

    func transcribe(name: String, kind: String, sql: String, actual: String) throws {
        guard encrypted, let transcriptPath else { throw SQLCorpusError("encrypted transcript is not configured") }
        guard !name.isEmpty, !name.contains("\t"), !name.contains("\n"), kind == "rows" || kind == "cost" else {
            throw SQLCorpusError("invalid encrypted transcript identity")
        }
        if !FileManager.default.fileExists(atPath: transcriptPath) {
            guard FileManager.default.createFile(atPath: transcriptPath, contents: nil) else {
                throw SQLCorpusError("cannot create encrypted corpus transcript")
            }
        }
        let output = try FileHandle(forWritingTo: URL(fileURLWithPath: transcriptPath))
        defer { try? output.close() }
        try output.seekToEnd()
        try output.write(contentsOf: Data("\(name)\t\(kind)\t\(Self.flat(sql))\t\(Self.flat(actual))\n".utf8))
    }
}
