import Foundation

/// Syntax coverage only. SQLSTATEs produced by binding/evaluation are allowed to
/// survive parsing; this executable never resolves or executes a statement.
@main
struct ParserAudit {
    struct Record: Decodable {
        let file: String
        let line: Int
        let expected: String
        let sql: String
    }

    static func main() throws {
        var total = 0, parsed = 0, expectedRejections = 0
        var deferredSyntaxErrors = 0, unexpectedRejections = 0
        let decoder = JSONDecoder()
        while let line = readLine() {
            let record = try decoder.decode(Record.self, from: Data(line.utf8))
            total += 1
            do {
                _ = try SQLParser(record.sql).parse()
                parsed += 1
                if record.expected == "42601" { deferredSyntaxErrors += 1 }
            } catch let error as JedError {
                if record.expected == error.code {
                    expectedRejections += 1
                } else {
                    unexpectedRejections += 1
                    let diagnostic: [String: Any] = [
                        "file": record.file, "line": record.line, "expected": record.expected,
                        "actual": error.code, "message": error.message, "sql": record.sql
                    ]
                    let bytes = try JSONSerialization.data(withJSONObject: diagnostic, options: [.sortedKeys])
                    FileHandle.standardError.write(bytes + Data([10]))
                }
            }
        }
        let summary = ["records": total, "parsed": parsed,
                       "expected_rejections": expectedRejections,
                       "accepted_expected_42601": deferredSyntaxErrors,
                       "unexpected_rejections": unexpectedRejections]
        let bytes = try JSONSerialization.data(withJSONObject: summary, options: [.sortedKeys])
        FileHandle.standardOutput.write(bytes + Data([10]))
        if unexpectedRejections != 0 { exit(1) }
    }
}
