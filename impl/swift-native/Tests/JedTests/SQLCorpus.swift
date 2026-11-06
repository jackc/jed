import Foundation
import Synchronization
@testable import Jed

enum SQLCorpusTrace {
    static func write(_ line: String) {
        guard ProcessInfo.processInfo.environment["JED_SWIFT_TRACE_SQL"] == "1" else { return }
        let bytes = Data((line + "\n").utf8)
        // SwiftPM may buffer its child's pipe. A direct progress file keeps a
        // stopped record identifiable even when that pipe has not been drained.
        if let path = ProcessInfo.processInfo.environment["JED_SWIFT_SQL_TRACE_PATH"],
           let file = FileHandle(forWritingAtPath: path) {
            defer { try? file.close() }
            do { try file.seekToEnd(); try file.write(contentsOf: bytes); return }
            catch { FileHandle.standardError.write(Data("SQL progress write failed: \(error)\n".utf8)) }
        }
        FileHandle.standardError.write(bytes)
    }
}

/// Format adapter only: it does not inspect SQL syntax or decide which records
/// the engine supports. A caller selects whole files before constructing it.
struct SQLCorpusFile {
    enum Sort: String { case nosort, rowsort, valuesort }
    enum Kind { case statement(error: String?); case query(tags: [UInt8], sort: Sort) }
    struct Directive { let name: String; let value: String; let line: Int }
    struct HostAction { let beforeRecord: Int; let directive: Directive }
    struct Record {
        let line: Int
        let kind: Kind
        let sql: String
        let expected: [String]
        let directives: [Directive]
    }
    let requirements: [String]
    let environment: [Directive]
    let records: [Record]
    let hostActions: [HostAction]

    static let recordDirectives: Set<String> = [
        "names", "types", "cost", "params", "seed", "clock", "clock_advance", "timezone", "set",
        "max_cost", "lifetime_max_cost", "max_sql_length", "max_scalar_bytes", "max_query_memory_bytes",
        "max_storage_bytes", "max_temp_storage_bytes", "work_mem", "default_privileges", "grant", "revoke",
        "allow_ddl", "allow_temp_ddl",
    ]
    static let fileDirectives: Set<String> = ["requires", "load-collation", "load-timezone", "fixture", "skip", "format", "attach"]
    static let actions: Set<String> = ["upgrade-collations"]

    func skips(_ mode: String) -> Bool {
        environment.contains { $0.name == "skip" && $0.value.split(whereSeparator: \.isWhitespace).first == Substring(mode) }
    }

    init(_ source: String) throws {
        let lines = source.components(separatedBy: "\n").map { $0.hasSuffix("\r") ? String($0.dropLast()) : $0 }
        var index = 0, records: [Record] = [], environment: [Directive] = [], pending: [Directive] = []
        var requirements: [String]?, hostActions: [HostAction] = []
        func invalid(_ message: String, _ line: Int) -> SQLCorpusError { SQLCorpusError("line \(line): \(message)") }
        while index < lines.count {
            let line = index + 1, header = lines[index].trimmingCharacters(in: .whitespaces)
            index += 1
            if header.isEmpty { continue }
            if header.hasPrefix("#") {
                let comment = header.dropFirst().trimmingCharacters(in: .whitespaces)
                guard let colon = comment.firstIndex(of: ":") else { continue }
                let name = String(comment[..<colon]), value = comment[comment.index(after: colon)...].trimmingCharacters(in: .whitespaces)
                let directive = Directive(name: name, value: value, line: line)
                if name == "requires" {
                    guard requirements == nil else { throw invalid("duplicate requires header", line) }
                    requirements = value.split(separator: ",").map { $0.trimmingCharacters(in: .whitespaces) }
                } else if Self.fileDirectives.contains(name) { environment.append(directive) }
                else if Self.recordDirectives.contains(name) { pending.append(directive) }
                else if Self.actions.contains(name) { hostActions.append(HostAction(beforeRecord: records.count, directive: directive)) }
                // Ordinary prose comments are not directives.
                continue
            }
            let fields = header.split(whereSeparator: \.isWhitespace).map(String.init)
            let kind: Kind
            if fields.first == "statement", fields.count >= 2 {
                if fields[1] == "ok", fields.count == 2 { kind = .statement(error: nil) }
                else if fields[1] == "error", fields.count >= 3, fields[2].utf8.count == 5 {
                    kind = .statement(error: fields[2])
                } else { throw invalid("invalid statement header", line) }
            } else if fields.first == "query", fields.count >= 2, fields.count <= 4 {
                let tags = Array(fields[1].utf8)
                guard !tags.isEmpty, tags.allSatisfy({ [73, 66, 84, 68, 82].contains($0) }) else {
                    throw invalid("invalid query column tags", line)
                }
                let sort: Sort
                if fields.count >= 3 {
                    guard let parsed = Sort(rawValue: fields[2]) else { throw invalid("invalid query sort mode", line) }
                    sort = parsed
                } else { sort = .nosort }
                kind = .query(tags: tags, sort: sort)
            } else { throw invalid("unsupported corpus control \(header)", line) }
            var sql: [String] = [], expected: [String] = []
            while index < lines.count, !lines[index].isEmpty, lines[index] != "----" {
                sql.append(lines[index]); index += 1
            }
            guard !sql.isEmpty else { throw invalid("empty SQL record", line) }
            if case .query = kind {
                guard index < lines.count, lines[index] == "----" else { throw invalid("query is missing ----", line) }
                index += 1
                while index < lines.count, !lines[index].isEmpty { expected.append(lines[index]); index += 1 }
                if expected.count == 1, expected[0].contains(" values hashing to ") {
                    throw invalid("hashed query results are not connected", line)
                }
            } else if pending.contains(where: { $0.name == "names" || $0.name == "types" }) {
                throw invalid("names/types directive decorates a statement", line)
            }
            records.append(Record(line: line, kind: kind, sql: sql.joined(separator: "\n"), expected: expected, directives: pending))
            pending = []
        }
        guard let requirements, !requirements.isEmpty else { throw SQLCorpusError("missing requires header") }
        guard pending.isEmpty else { throw SQLCorpusError("unconsumed record directives") }
        self.requirements = requirements; self.environment = environment; self.records = records; self.hostActions = hostActions
    }
}

struct SQLCorpusError: Error, CustomStringConvertible {
    let description: String
    init(_ message: String) { description = message }
}

/// Memory files retain one database and Session; disk files reopen the actual
/// file before each record. Pulling Rows transfers output to this test host.
final class SQLCorpusRunner {
    enum Mode: String {
        case auto, memory, disk
        func resolved(for file: SQLCorpusFile) -> Mode {
            self == .auto ? (file.skips("memory") ? .disk : .memory) : self
        }
    }
    struct Report {
        var records = 0, queries = 0, statements = 0, cells = 0, costs = 0, names = 0, types = 0, hostActions = 0
        var failures: [String] = []
        var completed = false
        var serviceClientRecords = 0, serviceFixtureRecords = 0
    }
    private final class Random: RandomSource {
        let source = Mutex<any RandomSource>(OSRandomSource())
        func reset(_ seed: UInt64?) { source.withLock { $0 = seed.map { SeededRandomSource(seed: $0) as any RandomSource } ?? OSRandomSource() } }
        func bytes(count: Int) throws -> [UInt8] { try source.withLock { try $0.bytes(count: count) } }
    }
    private final class Clock: ClockSource {
        let source = Mutex<any ClockSource>(WallClock())
        func reset(_ start: Int64?, step: Int64?) {
            source.withLock {
                if let start, let step { $0 = AdvancingClock(start: start, step: step) }
                else if let start { $0 = ManualClock(microseconds: start) }
                else { $0 = WallClock() }
            }
        }
        func microseconds() throws -> Int64 { try source.withLock { try $0.microseconds() } }
    }
    private let random = Random(), clock = Clock()
    private var database: Database?
    private var session: Session?
    private let environment: Environment
    private let mode: Mode
    private let accounting: SQLCorpusAccounting
    private let variant: SQLCorpusVariant
    private let service: SQLCorpusServiceOwner?
    private let name: String?
    private var temporaryDirectory: URL?
    private var temporaryHost: SQLCorpusTemporaryHost?
    var temporarySpills: Int { temporaryHost?.tempFiles ?? 0 }
    private var diskPath: String?
    private var mainStorageLimit: Int64 = 0
    private var fixtureOpenAsserted = false
    private static let maxCells = 200_000, maxBytes = 16 * 1024 * 1024

    init(file: SQLCorpusFile, mode: Mode = .auto, name: String? = nil,
         accounting: SQLCorpusAccounting? = nil, variant: SQLCorpusVariant? = nil,
         service: SQLCorpusServiceOwner? = nil, fixture: (String, String) throws -> [UInt8]) throws {
        self.mode = mode.resolved(for: file)
        self.accounting = try accounting ?? SQLCorpusAccounting()
        self.variant = try variant ?? SQLCorpusVariant()
        self.service = service
        if service != nil, self.mode != .disk { throw SQLCorpusError("service corpus requires disk mode") }
        if service != nil, self.variant.encrypted { throw SQLCorpusError("service corpus owner protocol has no encrypted credential command") }
        try self.variant.validate(mode: self.mode)
        self.name = name
        if self.variant.encrypted, name == nil { throw SQLCorpusError("encrypted transcript requires the corpus filename") }
        if self.accounting.peakPath != nil, name == nil {
            throw SQLCorpusError("query-memory peak output requires the corpus filename")
        }
        var builder = Environment.builder()
        if file.environment.contains(where: { $0.name == "load-collation" || $0.name == "fixture" }) {
            try builder.loadUnicodeData(fixture("unicode", "jucd"))
        }
        if file.environment.contains(where: { $0.name == "load-timezone" }) {
            try builder.loadTimeZoneData(fixture("tzdata", "jtz"))
        }
        builder.setRandomSource(random); builder.setClockSource(clock)
        for directive in file.environment {
            switch directive.name {
            case "load-collation", "load-timezone", "attach", "fixture": break
            case "skip":
                guard let name = directive.value.split(whereSeparator: \.isWhitespace).first,
                      ["memory", "disk", "encrypted"].contains(String(name)) else {
                    throw SQLCorpusError("line \(directive.line): unknown storage-mode exclusion")
                }
            default: throw SQLCorpusError("line \(directive.line): file directive \(directive.name) is not connected")
            }
        }
        environment = builder.build()
        let fixtures = file.environment.filter { $0.name == "fixture" }
        guard fixtures.count <= 1 else { throw SQLCorpusError("multiple fixture directives") }
        do {
            let opened: Database
            if let directive = fixtures.first {
                let words = directive.value.split(whereSeparator: \.isWhitespace)
                guard words.count == 1 || (words.count == 3 && words[1] == "error" && words[2].utf8.count == 5) else {
                    throw SQLCorpusError("line \(directive.line): invalid fixture directive")
                }
                let relative = words[0].split(separator: "/", omittingEmptySubsequences: false)
                guard relative.count == 3, relative[0] == "fileformat", relative[1] == "fixtures", relative[2].hasSuffix(".jed") else {
                    throw SQLCorpusError("line \(directive.line): unsupported frozen fixture path")
                }
                let bytes = try fixture(String(relative[2].dropLast(4)), "jed")
                var candidate: Database?, failure: Error?
                do { candidate = try Database.deserialize(bytes, options: DeserializeOptions(environment: environment)) }
                catch { failure = error }
                if words.count == 3 {
                    try candidate?.close()
                    guard file.records.isEmpty, file.hostActions.isEmpty else { throw SQLCorpusError("open-error fixture must not contain records or host actions") }
                    guard (failure as? JedError)?.code == String(words[2]) else {
                        throw SQLCorpusError("fixture expected open SQLSTATE \(words[2]), got \(failure.map(String.init(describing:)) ?? "success")")
                    }
                    fixtureOpenAsserted = true
                    return
                }
                if let failure { throw failure }
                guard let candidate else { throw SQLCorpusError("fixture open returned no database") }
                opened = candidate
            } else if self.mode == .disk {
                let path = try makeTemporaryPath(); diskPath = path
                opened = try Database.create(options: CreateOptions(environment: environment, path: path, credential: self.variant.credential, locking: service == nil ? .none : .automatic, skipFsync: true))
            } else if self.accounting.temporaryBuffers != nil {
                _ = try makeTemporaryPath()
                let host = try SQLCorpusTemporaryHost(directory: temporaryDirectory!.path)
                temporaryHost = host
                opened = try host.database(environment: environment)
            } else { opened = try Database(environment: environment) }
            database = opened
            for directive in file.environment where directive.name == "attach" {
                try opened.attach(SQLText(directive.value), source: .memory, readOnly: false)
            }
            session = try opened.session()
        } catch { try? close(); throw error }
    }
    private func makeTemporaryPath() throws -> String {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("jed-swift-corpus-" + UUID().uuidString, isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        temporaryDirectory = directory
        return directory.appendingPathComponent("database.jed").path
    }
    func close() throws {
        session?.close(); session = nil
        let opened = database; database = nil
        defer {
            if let directory = temporaryDirectory { try? FileManager.default.removeItem(at: directory) }
            temporaryDirectory = nil
        }
        var failure: Error?
        do { try opened?.close() } catch { failure = error }
        do { try service?.release() } catch { if failure == nil { failure = error } }
        if let failure { throw failure }
    }
    private func reopen() throws {
        guard let path = diskPath else { return }
        session?.close(); session = nil
        let prior = database; database = nil
        try prior?.close()
        try service?.open(path: path)
        let opened = try Database.open(path, options: OpenOptions(environment: environment,
            maxStorageBytes: mainStorageLimit, credential: variant.credential, locking: service == nil ? .none : .automatic, skipFsync: true))
        database = opened
        try service?.assertClient(opened)
        session = try opened.session()
    }

    func run(_ file: SQLCorpusFile) -> Report {
        var report = Report(), nextAction = 0
        if fixtureOpenAsserted {
            report.hostActions = 1; report.completed = true; return report
        }
        func performActions(beforeRecord: Int) -> Bool {
            while nextAction < file.hostActions.count, file.hostActions[nextAction].beforeRecord == beforeRecord {
                let directive = file.hostActions[nextAction].directive
                nextAction += 1; report.hostActions += 1
                do {
                    switch directive.name {
                    case "upgrade-collations":
                        guard let session else { throw SQLCorpusError("host action has no open session") }
                        _ = try session.upgradeCollations()
                    default: throw SQLCorpusError("unconnected host action \(directive.name)")
                    }
                } catch {
                    report.failures.append("line \(directive.line): harness \(directive.name): \(error)")
                    return false // Canonical runner ends the file on a failed host action.
                }
            }
            return true
        }
        for (index, record) in file.records.enumerated() {
            guard performActions(beforeRecord: index) else { return report }
            do { try reopen() }
            catch { report.failures.append("line \(record.line): harness disk reopen: \(error)"); return report }
            guard let session else { report.failures.append("line \(record.line): harness has no open session"); return report }
            Self.runRecord(record, session: session, report: &report, variant: variant, name: name) { try configure(record.directives) }
            do {
                if let service, let database {
                    if diskPath != nil {
                        try service.assertClient(database, countRecord: false)
                        report.serviceClientRecords += 1
                    } else { report.serviceFixtureRecords += 1 }
                }
                if let name, let database {
                    try accounting.record(name: name, ordinal: index + 1, storage: database.storageBytes)
                }
            } catch {
                report.failures.append("line \(record.line): harness peak output: \(error)")
                return report
            }
        }
        report.completed = performActions(beforeRecord: file.records.count)
        return report
    }

    /// One parser/render/comparison contract serves sequential and scheduled sessions.
    static func runRecord(_ record: SQLCorpusFile.Record, session: Session, report: inout Report,
                          variant: SQLCorpusVariant? = nil, name: String? = nil,
                          configure: () throws -> Void = {}) {
        SQLCorpusTrace.write("SQL record begin: line \(record.line)")
        report.records += 1
        switch record.kind { case .query: report.queries += 1; case .statement: report.statements += 1 }
        do {
            try configure()
            let underflows = MemoryDiagnostics.shared.underflows
            MemoryDiagnostics.shared.resetPeak()
            defer {
                if MemoryDiagnostics.shared.underflows != underflows {
                    report.failures.append("line \(record.line): query-memory accounting released more than it reserved")
                }
            }
            let params = try Self.last("params", in: record.directives).map(ParameterDirective.parse) ?? []
            let priorCost = session.lifetimeCost
            var statementCost: Int64?
            var actual: [String] = [], columnNames: [String] = [], columnTypes: [String] = [], failure: Error?
            do {
                let rows = try session.query(record.sql, parameters: params)
                defer { statementCost = rows.cost; rows.close() }
                columnNames = rows.columns.map { $0.name.description }
                columnTypes = rows.columns.map { $0.type?.type.name ?? "unknown" }
                var bytes = 0, cells = 0
                while let row = try rows.next() {
                    guard row.count == rows.columns.count else { throw SQLCorpusError("cursor row width differs from metadata") }
                    cells += row.count
                    guard cells <= maxCells else { throw SQLCorpusError("host corpus output exceeds \(maxCells) cells") }
                    if case .query = record.kind {
                        for (index, value) in row.enumerated() {
                            let text = value.isNull ? "NULL" : try session.render(value, type: rows.columns[index].type ?? TypeDescriptor(.scalar(.text))).description
                            bytes += text.utf8.count
                            guard bytes <= maxBytes else { throw SQLCorpusError("host corpus output exceeds \(maxBytes) bytes") }
                            actual.append(text)
                        }
                    }
                }
            } catch { failure = error }
            // Preparation meters may accrue separately to session lifetime.
            // A returned cursor carries the statement's own cost contract.
            let cost = statementCost ?? (session.lifetimeCost - priorCost)
            var problems: [String] = []
            switch record.kind {
            case .statement(let expected):
                if let expected {
                    if (failure as? JedError)?.code != expected {
                        problems.append("expected SQLSTATE \(expected), got \(failure.map(String.init(describing:)) ?? "success")")
                    }
                } else if let failure { problems.append("unexpected \(failure)") }
            case .query(let tags, let sort):
                report.cells += actual.count
                if let failure { problems.append("unexpected \(failure)") }
                else if sort != .nosort, columnNames.count != tags.count { problems.append("expected \(tags.count) columns, got \(columnNames.count)") }
                else if variant?.encrypted == true, SQLCorpusVariant.isExplain(record.sql) {
                    guard let name else { throw SQLCorpusError("encrypted transcript requires the corpus filename") }
                    try variant!.transcribe(name: name, kind: "rows", sql: record.sql,
                        actual: Self.ordered(actual, columns: tags.count, sort: sort).joined(separator: " "))
                } else if !Self.match(record.expected, actual, tags: tags, sort: sort) {
                    problems.append("expected \(Self.preview(record.expected)), got \(Self.preview(actual))")
                    if sort == .nosort, record.expected.count == actual.count,
                       let index = actual.indices.first(where: {
                           tags[$0 % tags.count] == 82
                               ? !Self.realEqual(record.expected[$0], actual[$0])
                               : !record.expected[$0].utf8.elementsEqual(actual[$0].utf8)
                       }) {
                        problems.append("first differing cell \(index + 1): expected \(Self.preview([record.expected[index]])), got \(Self.preview([actual[index]]))")
                    }
                }
            }
            if let expected = try Self.integer("cost", in: record.directives) {
                report.costs += 1
                if variant?.encrypted == true {
                    // Frozen runners assert/transcribe cost only for successful
                    // statements and queries, never expected-error statements.
                    let expectsSuccess: Bool
                    switch record.kind { case .query, .statement(error: nil): expectsSuccess = true; default: expectsSuccess = false }
                    if failure == nil, expectsSuccess {
                        guard let name else { throw SQLCorpusError("encrypted transcript requires the corpus filename") }
                        try variant!.transcribe(name: name, kind: "cost", sql: record.sql, actual: String(cost))
                    }
                } else if expected != cost { problems.append("expected cost \(expected), got \(cost)") }
            }
            if let expected = Self.last("names", in: record.directives) {
                report.names += 1
                let names = Self.list(expected)
                if names.count != columnNames.count || !zip(names, columnNames).allSatisfy({ $0.utf8.elementsEqual($1.utf8) }) { problems.append("expected names \(names), got \(columnNames)") }
            }
            if let expected = Self.last("types", in: record.directives) {
                report.types += 1
                let types = Self.list(expected)
                if types.count != columnTypes.count || !zip(types, columnTypes).allSatisfy({ $0.utf8.elementsEqual($1.utf8) }) { problems.append("expected types \(types), got \(columnTypes)") }
            }
            if !problems.isEmpty { report.failures.append("line \(record.line): \(problems.joined(separator: "; "))") }
        } catch { report.failures.append("line \(record.line): harness \(error)") }
    }

    private func configure(_ directives: [SQLCorpusFile.Directive]) throws {
        guard let database, let session else { throw SQLCorpusError("record configuration has no open handle") }
        var options = SessionOptions()
        options.maxCost = try Self.integer("max_cost", in: directives) ?? options.maxCost
        options.maxSQLLength = Int(try Self.integer("max_sql_length", in: directives) ?? Int64(options.maxSQLLength))
        options.maxScalarBytes = try Self.integer("max_scalar_bytes", in: directives) ?? options.maxScalarBytes
        if options.maxScalarBytes == 0 { options.maxScalarBytes = GeneratedSpec.defaultScalarBytes }
        options.maxQueryMemoryBytes = try Self.integer("max_query_memory_bytes", in: directives) ?? accounting.queryMemory ?? options.maxQueryMemoryBytes
        options.temporaryBufferBytes = accounting.temporaryBuffers ?? options.temporaryBufferBytes
        options.maxTemporaryStorageBytes = try Self.integer("max_temp_storage_bytes", in: directives) ?? 0
        options.workMemoryBytes = try Self.integer("work_mem", in: directives) ?? accounting.workMemory ?? options.workMemoryBytes
        if let value = Self.last("default_privileges", in: directives) { options.defaultPrivileges = try Self.privileges(value) }
        if let value = try Self.boolean("allow_ddl", in: directives) { options.allowDDL = value }
        options.allowTemporaryDDL = try Self.boolean("allow_temp_ddl", in: directives)
        session.settings.options = options
        session.settings.privileges = SessionPrivileges(defaults: options.defaultPrivileges)
        for directive in directives where directive.name == "grant" || directive.name == "revoke" {
            guard let split = directive.value.range(of: " ON ") else { throw SQLCorpusError("invalid privilege grant") }
            let privileges = try Self.privileges(String(directive.value[..<split.lowerBound]))
            let name = try SQLText(directive.value[split.upperBound...].trimmingCharacters(in: .whitespaces)).asciiLowercased
            if directive.name == "grant" { session.grant(privileges, on: name, function: privileges.contains(.execute)) }
            else { session.revoke(privileges, on: name, function: privileges.contains(.execute)) }
        }
        if let limit = try Self.integer("lifetime_max_cost", in: directives) { session.lifetimeMaxCost = limit }
        for directive in directives where directive.name == "max_storage_bytes" {
            let words = directive.value.split(whereSeparator: \.isWhitespace)
            guard (1...2).contains(words.count), let limit = Int64(words[0]), limit >= 0 else {
                throw SQLCorpusError("invalid max_storage_bytes directive")
            }
            let domain = try words.count == 2 ? SQLText(String(words[1])) : DomainNames.main
            try database.setMaxStorageBytes(domain, limit)
            if domain == DomainNames.main { mainStorageLimit = limit }
        }
        for name in Array(session.settings.variables.keys) { try session.resetVariable(name) }
        for directive in directives where directive.name == "set" {
            for entry in directive.value.split(separator: ",") {
                let pair = entry.split(separator: "=", maxSplits: 1, omittingEmptySubsequences: false)
                guard pair.count == 2 else { throw SQLCorpusError("invalid session variable directive") }
                try session.setVariable(SQLText(pair[0].trimmingCharacters(in: .whitespaces)), value: SQLText(pair[1].trimmingCharacters(in: .whitespaces)))
            }
        }
        try session.setTimeZone(SQLText(Self.last("timezone", in: directives) ?? "UTC"))
        var seed: UInt64?
        if let value = Self.last("seed", in: directives) {
            guard let parsed = UInt64(value) else { throw SQLCorpusError("invalid seed directive") }; seed = parsed
        }
        var start = try Self.integer("clock", in: directives, nonnegative: false), step: Int64?
        if let value = Self.last("clock_advance", in: directives) {
            let pair = value.split(separator: ",").map { $0.trimmingCharacters(in: .whitespaces) }
            guard pair.count == 2, let parsedStart = Int64(pair[0]), let parsedStep = Int64(pair[1]) else { throw SQLCorpusError("invalid advancing clock directive") }
            start = parsedStart; step = parsedStep
        }
        random.reset(seed); clock.reset(start, step: step)
    }

    private static func last(_ name: String, in directives: [SQLCorpusFile.Directive]) -> String? { directives.last(where: { $0.name == name })?.value }
    private static func integer(_ name: String, in directives: [SQLCorpusFile.Directive], nonnegative: Bool = true) throws -> Int64? {
        guard let text = Self.last(name, in: directives) else { return nil }
        guard let value = Int64(text), !nonnegative || value >= 0 else { throw SQLCorpusError("invalid \(name) directive") }
        return value
    }
    private static func boolean(_ name: String, in directives: [SQLCorpusFile.Directive]) throws -> Bool? {
        guard let text = Self.last(name, in: directives) else { return nil }
        guard text == "on" || text == "off" else { throw SQLCorpusError("invalid \(name) directive") }
        return text == "on"
    }
    private static func privileges(_ text: String) throws -> PrivilegeSet {
        var value = PrivilegeSet()
        for word in text.split(whereSeparator: { $0 == "," || $0.isWhitespace }) {
            switch word {
            case "ALL": value.formUnion(.allTables)
            case "NONE": break
            case "SELECT": value.insert(.select)
            case "INSERT": value.insert(.insert)
            case "UPDATE": value.insert(.update)
            case "DELETE": value.insert(.delete)
            case "EXECUTE": value.insert(.execute)
            default: throw SQLCorpusError("invalid privilege directive")
            }
        }
        return value
    }
    private static func list(_ text: String) -> [String] { text.split(separator: ",").map { $0.trimmingCharacters(in: .whitespaces) } }
    private static func preview(_ values: [String]) -> String {
        let items = values.prefix(12).map { String($0.prefix(100)) }
        return "\(items)\(values.count > 12 ? " (\(values.count) cells)" : "")"
    }
    static func match(_ expected: [String], _ actual: [String], tags: [UInt8], sort: SQLCorpusFile.Sort) -> Bool {
        guard !tags.isEmpty, expected.count == actual.count else { return false }
        // The canonical nosort harness compares flattened cells. Historical
        // headers may have fewer rendering tags; #names/#types assert metadata.
        // A rowsort still needs complete rows so no trailing cell can disappear.
        if sort == .rowsort, actual.count % tags.count != 0 { return false }
        guard let expected = try? ordered(expected, columns: tags.count, sort: sort),
              let actual = try? ordered(actual, columns: tags.count, sort: sort) else { return false }
        for index in actual.indices {
            if sort != .valuesort, tags[index % tags.count] == 82 {
                guard realEqual(expected[index], actual[index]) else { return false }
            } else if !expected[index].utf8.elementsEqual(actual[index].utf8) { return false }
        }
        return true
    }
    static func ordered(_ values: [String], columns: Int, sort: SQLCorpusFile.Sort) throws -> [String] {
        func less(_ a: String, _ b: String) -> Bool { a.utf8.lexicographicallyPrecedes(b.utf8) }
        switch sort {
        case .nosort: return values
        case .valuesort: return values.sorted(by: less)
        case .rowsort:
            guard columns > 0, values.count % columns == 0 else { throw SQLCorpusError("incomplete transcript row") }
            let rows = stride(from: 0, to: values.count, by: columns).map { Array(values[$0..<($0 + columns)]) }
            return rows.sorted { $0.lexicographicallyPrecedes($1, by: less) }.flatMap { $0 }
        }
    }
    private static func realEqual(_ lhs: String, _ rhs: String) -> Bool {
        guard let a = Double(lhs), let b = Double(rhs) else { return lhs.utf8.elementsEqual(rhs.utf8) }
        if a.isNaN || b.isNaN { return a.isNaN && b.isNaN }
        if a == b { return true }
        return a.isFinite && b.isFinite && abs(a - b) <= 1e-9 * max(abs(a), abs(b), 1)
    }
}
