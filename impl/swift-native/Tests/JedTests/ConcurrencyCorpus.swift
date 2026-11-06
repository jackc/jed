import Foundation
@testable import Jed

/// The shared schedule grammar adds session control around ordinary corpus
/// records. It never rewrites SQL or changes a schedule's expected values.
struct ConcurrencyCorpusFile {
    enum Operation {
        case open(String, writable: Bool, blocks: Bool)
        case record(String, SQLCorpusFile.Record)
        case end(String, String)
        case expect(String, UInt64)
    }
    struct Step { let line: Int; let operation: Operation }
    let setup: [SQLCorpusFile.Directive]
    let requirements: [String]
    let steps: [Step]

    static func format(_ source: String) -> String? {
        for line in source.components(separatedBy: "\n") {
            let text = line.trimmingCharacters(in: .whitespacesAndNewlines)
            guard text.hasPrefix("#") else { continue }
            let comment = text.dropFirst().trimmingCharacters(in: .whitespaces)
            if comment.hasPrefix("format:") { return comment.dropFirst(7).trimmingCharacters(in: .whitespaces) }
        }
        return nil
    }

    init(_ source: String) throws {
        guard Self.format(source) == "concurrency" else { throw SQLCorpusError("missing concurrency format header") }
        let lines = source.components(separatedBy: "\n").map { $0.hasSuffix("\r") ? String($0.dropLast()) : $0 }
        let controls: Set<String> = ["open", "on", "commit", "rollback", "close", "expect"]
        func boundary(_ line: String) -> Bool {
            let text = line.trimmingCharacters(in: .whitespaces)
            return text.isEmpty || text.hasPrefix("#") || text.split(whereSeparator: \.isWhitespace).first.map { controls.contains(String($0)) } == true
        }
        var setup: [SQLCorpusFile.Directive] = [], steps: [Step] = [], requirements: [String]?
        var index = 0
        while index < lines.count {
            let line = index + 1, text = lines[index].trimmingCharacters(in: .whitespaces)
            index += 1
            if text.isEmpty { continue }
            if text.hasPrefix("#") {
                let comment = text.dropFirst().trimmingCharacters(in: .whitespaces)
                guard let colon = comment.firstIndex(of: ":") else { continue }
                let name = String(comment[..<colon]), value = comment[comment.index(after: colon)...].trimmingCharacters(in: .whitespaces)
                if name == "requires" {
                    guard requirements == nil else { throw SQLCorpusError("line \(line): duplicate requires header") }
                    requirements = value.split(separator: ",").map { $0.trimmingCharacters(in: .whitespaces) }
                } else if name == "attach" || name == "max_storage_bytes" {
                    setup.append(.init(name: name, value: value, line: line))
                } else if name == "format" {
                    guard value == "concurrency" else { throw SQLCorpusError("line \(line): conflicting format header") }
                } else if SQLCorpusFile.fileDirectives.contains(name) || SQLCorpusFile.recordDirectives.contains(name) || SQLCorpusFile.actions.contains(name) {
                    throw SQLCorpusError("line \(line): unsupported concurrency directive \(name)")
                }
                continue
            }
            // Inline comments belong to schedule controls, never SQL or result cells.
            let control = text.split(separator: "#", maxSplits: 1, omittingEmptySubsequences: false)[0]
            let words = control.split(whereSeparator: \.isWhitespace).map(String.init)
            guard let first = words.first else { continue }
            let operation: Operation
            switch first {
            case "open":
                guard (3...4).contains(words.count), words[2] == "read" || words[2] == "write",
                      words.count == 3 || words[3] == "blocks" else {
                    throw SQLCorpusError("line \(line): expected open <session> read|write [blocks]")
                }
                let blocks = words.count == 4
                guard !blocks || words[2] == "write" else { throw SQLCorpusError("line \(line): only writers can block") }
                operation = .open(words[1], writable: words[2] == "write", blocks: blocks)
            case "commit", "rollback", "close":
                guard words.count == 2 else { throw SQLCorpusError("line \(line): expected \(first) <session>") }
                operation = .end(words[1], first)
            case "expect":
                guard words.count == 3, words[1] == "version" || words[1] == "oldest_live", let expected = UInt64(words[2]) else {
                    throw SQLCorpusError("line \(line): expected version or oldest_live with an unsigned value")
                }
                operation = .expect(words[1], expected)
            case "on":
                guard words.count >= 4, words[2] == "statement" || words[2] == "query" else {
                    throw SQLCorpusError("line \(line): expected on <session> <record>")
                }
                var body: [String] = []
                if words[2] == "query" {
                    while index < lines.count, lines[index].trimmingCharacters(in: .whitespaces) != "----", !boundary(lines[index]) {
                        body.append(lines[index]); index += 1
                    }
                    guard index < lines.count, lines[index].trimmingCharacters(in: .whitespaces) == "----" else {
                        throw SQLCorpusError("line \(line): query is missing ----")
                    }
                    body.append("----"); index += 1
                }
                while index < lines.count, !boundary(lines[index]) { body.append(lines[index]); index += 1 }
                let recordSource = "# requires: txn.shared\n" + words.dropFirst(2).joined(separator: " ") + "\n" + body.joined(separator: "\n") + "\n\n"
                let parsed = try SQLCorpusFile(recordSource)
                guard parsed.records.count == 1, let record = parsed.records.first else { throw SQLCorpusError("line \(line): invalid scheduled record") }
                operation = .record(words[1], .init(line: line, kind: record.kind, sql: record.sql,
                    expected: record.expected, directives: []))
            default: throw SQLCorpusError("line \(line): unknown concurrency operation \(first)")
            }
            steps.append(.init(line: line, operation: operation))
        }
        guard let requirements, !requirements.isEmpty else { throw SQLCorpusError("missing requires header") }
        self.setup = setup; self.requirements = requirements; self.steps = steps
    }
}

/// Canonical stepped-sequential execution. A blocked open is a pending schedule
/// action, admitted immediately after its holder releases the real writer gate.
final class ConcurrencyCorpusRunner {
    struct Report {
        var sql = SQLCorpusRunner.Report()
        var steps = 0, assertions = 0, handoffs = 0
    }
    private struct OpenSession { let session: Session; let writable: Bool }
    private let database: Database
    private var sessions: [String: OpenSession] = [:]
    private var holder: String?, blocked: String?

    init(file: ConcurrencyCorpusFile) throws {
        let database = try Database()
        do {
            for directive in file.setup {
                if directive.name == "attach" {
                    try database.attach(SQLText(directive.value), source: .memory, readOnly: false)
                } else {
                    let words = directive.value.split(whereSeparator: \.isWhitespace)
                    guard (1...2).contains(words.count), let limit = Int64(words[0]), limit >= 0 else {
                        throw SQLCorpusError("line \(directive.line): invalid max_storage_bytes directive")
                    }
                    let name = try words.count == 2 ? SQLText(String(words[1])) : DomainNames.main
                    try database.setMaxStorageBytes(name, limit)
                }
            }
        } catch { try? database.close(); throw error }
        self.database = database
    }
    func close() throws {
        // close() releases an unfinished writer without admitting a pending open.
        for name in sessions.keys.sorted() { sessions[name]?.session.close() }
        sessions.removeAll(); holder = nil; blocked = nil
        try database.close()
    }
    func run(_ file: ConcurrencyCorpusFile) -> Report {
        var report = Report()
        for step in file.steps {
            SQLCorpusTrace.write("Concurrency step begin: line \(step.line)")
            report.steps += 1
            do {
                switch step.operation {
                case .open(let name, let writable, let blocks):
                    guard sessions[name] == nil, blocked != name else { throw SQLCorpusError("session \(name) is already open") }
                    if blocks {
                        guard holder != nil else { throw SQLCorpusError("blocked writer \(name) has no gate holder") }
                        guard blocked == nil else { throw SQLCorpusError("a writer is already queued") }
                        blocked = name
                    } else if writable {
                        guard holder == nil else { throw SQLCorpusError("writer gate is held; schedule must use blocks") }
                        sessions[name] = OpenSession(session: try database.writeSession(), writable: true)
                        holder = name
                    } else { sessions[name] = OpenSession(session: try database.readSession(), writable: false) }
                case .record(let name, let record):
                    guard blocked != name, let open = sessions[name] else { throw SQLCorpusError("on unavailable session \(name)") }
                    SQLCorpusRunner.runRecord(record, session: open.session, report: &report.sql)
                case .end(let name, let command):
                    guard blocked != name, let open = sessions[name] else { throw SQLCorpusError("\(command) of unavailable session \(name)") }
                    guard open.writable ? command != "close" : command == "close" else { throw SQLCorpusError("invalid \(command) for session mode") }
                    if command == "commit" { try open.session.commit() }
                    else if command == "rollback" { try open.session.rollback() }
                    open.session.close(); sessions.removeValue(forKey: name)
                    if holder == name {
                        holder = nil
                        if let pending = blocked {
                            sessions[pending] = OpenSession(session: try database.writeSession(), writable: true)
                            holder = pending; blocked = nil; report.handoffs += 1
                        }
                    }
                case .expect(let kind, let expected):
                    report.assertions += 1
                    let actual = kind == "version" ? database.version : database.core.oldestLiveVersion
                    if actual != expected { report.sql.failures.append("line \(step.line): expected \(kind) \(expected), got \(actual)") }
                }
            } catch {
                report.sql.failures.append("line \(step.line): schedule \(error)")
                return report
            }
        }
        let remaining = (Array(sessions.keys) + (blocked.map { [$0] } ?? [])).sorted()
        guard remaining.isEmpty else {
            report.sql.failures.append("schedule ended with sessions still open: \(remaining.joined(separator: ", "))")
            return report
        }
        report.sql.completed = true
        return report
    }
}
