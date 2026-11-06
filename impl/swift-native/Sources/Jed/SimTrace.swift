import Foundation

/// Trace parsing accepts the shared operation vocabulary, never SQL outcomes.
/// The SQL source remains byte-for-byte text after the session-name separator.
internal struct SimTrace: Sendable {
    struct Step: Sendable {
        let line: Int
        let kind: String
        var process = 0
        var session = ""
        var mode = ""
        var sql = ""
        var parameters: [String: String] = [:]
        func fault() throws -> SimFault {
            func integer(_ key: String, default fallback: Int = 0, probability: Bool = false) throws -> Int {
                guard let value = parameters[key] else { return fallback }
                guard let number = Int(value), number >= 0, !probability || number <= 1000 else {
                    throw JedError("22023", "simulation line \(line): invalid \(key)")
                }; return number
            }
            switch mode {
            case "power_loss": return SimFault(kind: .powerLoss, survive: try integer("survive", probability: true),
                tear: try integer("tear", probability: true), nth: try integer("nth", default: 1))
            case "process_crash": return SimFault(kind: .processCrash, issue: try integer("issue", probability: true), nth: try integer("nth", default: 1))
            case "io_error" where parameters["op"] == "write": return SimFault(kind: .write, probability: try integer("p", probability: true))
            case "io_error" where parameters["op"] == "sync": return SimFault(kind: .sync, nth: try integer("nth", default: 1))
            case "read_error": return SimFault(kind: .read, probability: try integer("p", probability: true))
            case "enospc": return SimFault(kind: .noSpace)
            case "spill_error": return SimFault(kind: .spill, probability: try integer("p", probability: true))
            case "disconnect": return SimFault(kind: .disconnect, nth: try integer("nth", default: 1))
            default: throw JedError("22023", "simulation line \(line): unknown fault \(mode)")
            }
        }
    }
    let header: [String: String]
    let steps: [Step]
    init(_ source: String) throws {
        var header: [String: String] = [:], steps: [Step] = []
        for (index, raw) in source.components(separatedBy: "\n").enumerated() {
            let line = raw.trimmingCharacters(in: .whitespacesAndNewlines)
            if line.isEmpty { continue }
            if line.first == "#" {
                let pair = line.dropFirst().split(separator: ":", maxSplits: 1)
                if pair.count == 2 { header[pair[0].trimmingCharacters(in: .whitespaces)] = pair[1].trimmingCharacters(in: .whitespaces) }
                continue
            }
            let fields = line.split(whereSeparator: { $0 == " " || $0 == "\t" }).map(String.init)
            var step = Step(line: index + 1, kind: fields[0])
            func invalid(_ message: String) -> JedError { JedError("22023", "simulation line \(index + 1): \(message)") }
            func require(_ condition: Bool, _ message: String) throws { if !condition { throw invalid(message) } }
            func process(_ text: String) throws -> Int {
                guard text.first == "p", let number = Int(text.dropFirst()), number >= 0 else { throw invalid("expected p<N>") }; return number
            }
            func parameters(_ start: Int) throws -> [String: String] {
                var output: [String: String] = [:]
                for field in fields.dropFirst(start) {
                    let pair = field.split(separator: "=", maxSplits: 1, omittingEmptySubsequences: false)
                    guard pair.count == 2 else { throw invalid("expected key=value") }; output[String(pair[0])] = String(pair[1])
                }; return output
            }
            switch step.kind {
            case "open":
                try require(fields.count == 3 || fields.count == 4, "open <sid> read|write [p<N>]")
                step.session = fields[1]; step.mode = fields[2]
                try require(step.mode == "read" || step.mode == "write", "invalid session mode")
                if fields.count == 4 { step.process = try process(fields[3]) }
            case "spawn", "exit", "kill", "disconnect":
                try require(fields.count == 2, "\(step.kind) p<N>"); step.process = try process(fields[1])
            case "on":
                try require(fields.count >= 3, "on <sid> <sql>"); step.session = fields[1]
                let rest = line.dropFirst(2).drop(while: { $0 == " " || $0 == "\t" }).dropFirst(step.session.count)
                step.sql = String(rest.drop(while: { $0 == " " || $0 == "\t" }))
            case "commit", "rollback", "close":
                try require(fields.count == 2, "\(step.kind) <sid>"); step.session = fields[1]
            case "fault", "crash":
                try require(fields.count >= 2, "\(step.kind) <kind> [key=value]")
                step.mode = fields[1]; step.parameters = try parameters(2)
                if step.kind == "fault" { _ = try step.fault() }
                else { try require(step.mode == "process" || step.mode == "power", "unknown crash kind") }
            case "clock":
                try require(fields.count == 2, "clock +<micros>")
                try require(fields[1].first == "+" && UInt64(fields[1].dropFirst()) != nil, "invalid clock delta")
                step.parameters["delta"] = String(fields[1].dropFirst())
            case "attach":
                try require(fields.count == 2, "attach <name>")
                try require(fields[1] != "main" && fields[1] != "temp", "invalid attachment name"); step.session = fields[1]
            case "compact":
                try require(fields.count <= 2, "compact [<database>]"); step.session = fields.count == 2 ? fields[1] : "main"
            case "change_key", "rekey":
                try require(fields.count == 2 || fields.count == 3, "\(step.kind) <key> [<database>]")
                try require(["1", "2", "3"].contains(fields[1]) || step.kind == "rekey" && fields[1] == "plain", "invalid key")
                step.mode = fields[1]; step.session = fields.count == 3 ? fields[2] : "main"
                try require(step.session != "temp", "temporary databases have no credentials")
            case "replay":
                try require(fields.count == 2, "replay <kind>"); step.mode = fields[1]
            case "restore":
                try require(fields.count == 2, "restore <j>")
                try require((Int(fields[1]) ?? 0) >= 1, "invalid backup ordinal"); step.parameters["j"] = fields[1]
            case "corrupt": step.parameters = try parameters(1)
            case "backup":
                try require(fields.count == 2 || fields.count == 3, "backup <form> [p<N>]")
                try require(["page_identical", "compacted"].contains(fields[1]), "invalid backup form")
                step.mode = fields[1]; step.process = fields.count == 3 ? try process(fields[2]) : -1
            case "check", "reopen", "tick": try require(fields.count == 1, "unexpected step argument")
            default: throw invalid("unknown operation \(step.kind)")
            }
            steps.append(step)
        }
        guard header["format"] == "simulation" else { throw JedError("22023", "missing simulation format header") }
        self.header = header; self.steps = steps
    }
}
