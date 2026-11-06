import Foundation
@_spi(ExperimentalTools) import Jed
#if os(Linux)
import Glibc
#elseif canImport(Darwin)
import Darwin
#endif

/// The canonical process corpus's line protocol. SQL is sent unchanged through
/// the native public engine; this executable contains no evaluator or planner.
@main struct JedProcessMain {
    static func main() {
        do { try run() }
        catch {
            reply(error)
            FileHandle.standardError.write(Data((String(describing: error) + "\n").utf8))
            exit(1)
        }
    }
    private static func reply(_ value: String = "") {
        FileHandle.standardOutput.write(Data(("OK\t" + value + "\n").utf8))
    }
    private static func reply(_ error: any Error) {
        let engine = error as? JedError
        let detail = Array((engine?.message ?? String(describing: error)).utf8)
        FileHandle.standardOutput.write(Data(("ERR\t" + (engine?.code ?? "XXXXX") + "\t" + hex(detail) + "\n").utf8))
    }
    private static func ready() { FileHandle.standardOutput.write(Data("READY\n".utf8)) }
    private static func hex(_ bytes: [UInt8]) -> String {
        let digits = Array("0123456789abcdef".utf8)
        return String(decoding: bytes.flatMap { [digits[Int($0 >> 4)], digits[Int($0 & 15)]] }, as: UTF8.self)
    }
    private static func bytes(_ text: String) throws -> [UInt8] {
        let source = Array(text.utf8)
        guard source.count & 1 == 0 else { throw JedError("22023", "actor hex has odd length") }
        func nibble(_ c: UInt8) throws -> UInt8 {
            if c >= 48 && c <= 57 { return c - 48 }
            if c >= 65 && c <= 70 { return c - 55 }
            if c >= 97 && c <= 102 { return c - 87 }
            throw JedError("22023", "actor hex contains invalid digit")
        }
        var result: [UInt8] = []; result.reserveCapacity(source.count / 2)
        for index in stride(from: 0, to: source.count, by: 2) {
            result.append(try nibble(source[index]) * 16 + nibble(source[index + 1]))
        }
        return result
    }
    private static func sql(_ text: String) throws -> String {
        let decoded = try bytes(text)
        guard let value = String(bytes: decoded, encoding: .utf8) else { throw JedError("22021", "actor SQL is not UTF-8") }
        return value
    }
    private static func number(_ text: String) throws -> UInt64 {
        guard let value = UInt64(text) else { throw JedError("22023", "actor argument is not an unsigned integer") }
        return value
    }
    private static func query(_ rows: Rows) throws -> String {
        defer { rows.close() }
        var result: [String] = []
        while let row = try rows.next() {
            result.append(try row.map { value in
                guard case .integer(let number) = value else { throw JedError("42804", "QUERY_I64 expected integer fields") }
                return String(number)
            }.joined(separator: ":"))
        }
        return result.joined(separator: ",")
    }
    private static func run() throws {
        let arguments = Array(CommandLine.arguments.dropFirst())
        guard arguments.count >= 2 else { throw JedError("22023", "usage: jed-process ACTION PATH [OPTIONS]") }
        let action = arguments[0], path = arguments[1]
        if action == "owner-idle" { try ownerIdle(root: path); return }
        if action == "pages" || action == "pages-read-only" {
            try DatabaseTooling.servePageStream(path: path, readOnly: action == "pages-read-only"); return
        }
        var options: [String: String] = [:], positional: [String] = []
        for argument in arguments.dropFirst(2) {
            if let at = argument.firstIndex(of: "=") { options[String(argument[..<at])] = String(argument[argument.index(after: at)...]) }
            else { positional.append(argument) }
        }
        let timeout = try number(positional.first ?? "5000")
        guard timeout <= UInt64(Int64.max) else { throw JedError("22023", "actor timeout exceeds Int64") }
        let idle = try number(positional.count > 1 ? positional[1] : "60000")
        let ownership: OwnershipPreference
        switch options["ownership"] ?? "prefer" {
        case "prefer": ownership = .prefer
        case "yield": ownership = .yield
        case "never": ownership = .never
        default: throw JedError("22023", "unknown ownership preference")
        }
        let limit = try number(options["max_connections"] ?? "0")
        guard limit <= UInt64(Int.max) else { throw JedError("22023", "connection limit exceeds Int") }
        let service = try PageServiceOptions(ownership: ownership,
            yieldGraceMilliseconds: number(options["yield_grace"] ?? "5000"),
            shutdownGraceMilliseconds: number(options["shutdown_grace"] ?? "30000"),
            idleSessionMilliseconds: number(options["idle_session"] ?? "0"),
            idleWriteMilliseconds: idle, maxConnections: Int(limit))
        let credential = try options["key"].map { try Credential.hostKey(bytes($0)) }
        let locking: FileLocking = action == "open-exclusive" ? .exclusive : .automatic
        let database: Database
        switch action {
        case "create": database = try Database.create(options: CreateOptions(path: path, credential: credential,
            locking: locking, fileLockTimeoutMilliseconds: Int64(timeout), pageService: service))
        case "open", "open-exclusive", "open-read-only":
            database = try Database.open(path, options: OpenOptions(credential: credential, readOnly: action == "open-read-only",
                locking: locking, fileLockTimeoutMilliseconds: Int64(timeout), pageService: service))
        case "open-remote", "open-remote-read-only":
            guard let actor = ProcessInfo.processInfo.environment["JED_PAGES_ACTOR"] else {
                throw JedError("22023", "stream process actor is not configured")
            }
            database = try Database.openRemote(command: [actor, action == "open-remote-read-only" ? "pages-read-only" : "pages", path],
                options: OpenOptions(credential: credential, fileLockTimeoutMilliseconds: Int64(timeout)))
        default: throw JedError("22023", "unknown actor action \(action)")
        }
        database.synchronous = options["synchronous"] != "off"
        defer { try? database.close() }
        var reader: Session?, writer: Session?
        defer { reader?.close(); writer?.close() }
        ready()
        var input = CommandInput()
        while let line = try input.next() {
            let pair = line.split(separator: "\t", maxSplits: 1, omittingEmptySubsequences: false)
            let command = String(pair[0]), argument = pair.count > 1 ? String(pair[1]) : ""
            do {
                var value = ""
                switch command {
                case "EXEC": _ = try database.execute(sql(argument))
                case "QUERY_I64": value = try query(database.query(sql(argument)))
                case "READ_OPEN": reader?.close(); reader = try database.readSession()
                case "READ_QUERY_I64":
                    guard let reader else { throw JedError("25000", "no actor reader") }
                    value = try query(reader.query(sql(argument)))
                case "READ_CLOSE": reader?.close(); reader = nil
                case "WRITE_OPEN":
                    writer?.close(); var session = SessionOptions(); session.lockTimeoutMilliseconds = try number(argument.isEmpty ? "0" : argument)
                    writer = try database.session(options: session); try writer!.begin()
                case "WRITE_EXEC":
                    guard let writer else { throw JedError("25000", "no actor writer") }; _ = try writer.execute(sql(argument))
                case "WRITE_COMMIT":
                    guard let writer else { throw JedError("25000", "no actor writer") }; try writer.commit()
                case "WRITE_ROLLBACK":
                    guard let writer else { throw JedError("25000", "no actor writer") }; try writer.rollback()
                case "ATTACH":
                    let fields = argument.split(separator: "\t", maxSplits: 2, omittingEmptySubsequences: false)
                    guard fields.count == 3, fields[1] == "0" || fields[1] == "1" else {
                        throw JedError("22023", "invalid attachment actor arguments")
                    }
                    try database.attach(SQLText(String(fields[0])),
                        source: .file(String(fields[2]), OpenOptions(fileLockTimeoutMilliseconds: Int64(timeout))), readOnly: fields[1] == "1")
                case "DETACH": try database.detach(SQLText(argument))
                case "COMPACT": try database.compact(argument.isEmpty ? "main" : argument)
                case "BACKUP":
                    let fields = argument.split(separator: "\t", maxSplits: 1, omittingEmptySubsequences: false)
                    guard fields.count == 2, fields[0] == "page_identical" || fields[0] == "compacted" else { throw JedError("22023", "invalid backup actor arguments") }
                    try DatabaseTooling.backup(database, destination: String(fields[1]), pageIdentical: fields[0] == "page_identical")
                case "CHANGE_KEY": try database.changeCredential(credential: .hostKey(bytes(argument)))
                case "REKEY": try database.rekey(credential: argument.isEmpty ? nil : .hostKey(bytes(argument)))
                case "MAX_STORAGE_BYTES":
                    guard let limit = Int64(argument), limit >= 0 else { throw JedError("22023", "invalid storage limit") }
                    try database.setMaxStorageBytes(SQLText("main"), limit)
                case "STORAGE_BYTES": value = String(database.storageBytes)
                case "TXID": value = String(database.version)
                case "PAGE_COUNT": value = String(try DatabaseTooling.pageCount(database))
                case "ROLE": value = try DatabaseTooling.role(database, attachment: argument)
                case "HANGUP_NEXT_ACK": try DatabaseTooling.hangUpNextAcknowledgement(database)
                case "CRASH_NEXT_ACK": try DatabaseTooling.beforeNextAcknowledgement(database) { _exit(86) }
                case "TIME_LOOKUPS", "TIME_SCAN", "TIME_COMMITS": value = try timed(database, command: command, argument: argument)
                case "CLOSE":
                    reader?.close(); writer?.close(); reader = nil; writer = nil
                    reply(); try database.close(); return
                default: throw JedError("22023", "unknown actor command \(command)")
                }
                reply(value)
            } catch {
                if command == "CLOSE" { throw error }
                reply(error)
            }
        }
    }
    private static func ownerIdle(root: String) throws {
        var builder = Environment.builder()
        try builder.loadUnicodeData(Array(Data(contentsOf: URL(fileURLWithPath: root + "/spec/collation/fixtures/unicode.jucd"))))
        try builder.loadTimeZoneData(Array(Data(contentsOf: URL(fileURLWithPath: root + "/spec/tz/fixtures/tzdata.jtz"))))
        let environment = builder.build()
        var database: Database?, input = CommandInput()
        defer { try? database?.close() }
        ready()
        while let line = try input.next() {
            let fields = line.split(separator: "\t", maxSplits: 1, omittingEmptySubsequences: false)
            do {
                switch fields[0] {
                case "OWNER_OPEN":
                    guard fields.count == 2 else { throw JedError("22023", "missing owner path") }
                    try database?.close()
                    database = try Database.open(sql(String(fields[1])), options: OpenOptions(environment: environment, skipFsync: true))
                case "OWNER_CLOSE": try database?.close(); database = nil
                case "CLOSE": try database?.close(); database = nil; reply(); return
                default: throw JedError("22023", "unknown owner-idle command")
                }
                reply()
            } catch { reply(error) }
        }
    }
    private static func timed(_ database: Database, command: String, argument: String) throws -> String {
        let fields = argument.split(separator: "\t", omittingEmptySubsequences: false)
        let count = try number(String(fields.first ?? "")), rows = try number(fields.count > 1 ? String(fields[1]) : "1")
        guard rows > 0, count <= UInt64(Int64.max) / 7919, rows <= UInt64(Int64.max) else { throw JedError("22023", "invalid benchmark count") }
        let start = DispatchTime.now().uptimeNanoseconds
        var checksum: Int64 = 0
        for index in 0..<count {
            let key = Int64((index * 7919) % rows + 1)
            switch command {
            case "TIME_LOOKUPS":
                let values = try database.query("SELECT v FROM bench_ps WHERE id = $1", parameters: [.integer(key)])
                defer { values.close() }
                guard let row = try values.next(), let first = row.first, case .integer(let value) = first else { throw JedError("42804", "benchmark expected integer") }
                checksum &+= value
            case "TIME_SCAN":
                let values = try database.query("SELECT sum(v)::i64 FROM bench_ps")
                defer { values.close() }
                guard let row = try values.next(), let first = row.first, case .integer(let value) = first else { throw JedError("42804", "benchmark expected integer") }
                checksum = value
            default: _ = try database.execute("UPDATE bench_ps SET v = v + 1 WHERE id = $1", parameters: [.integer(key)])
            }
        }
        return "\(DispatchTime.now().uptimeNanoseconds - start):\(checksum)"
    }
}

private struct CommandInput {
    private var buffer: [UInt8] = []
    private var eof = false
    private var scanned = 0
    mutating func next() throws -> String? {
        while true {
            if let end = buffer[scanned...].firstIndex(of: 10) {
                let bytes = Array(buffer[..<end]); buffer.removeFirst(end + 1); scanned = 0
                guard let text = String(bytes: bytes, encoding: .utf8) else { throw JedError("22021", "actor line is not UTF-8") }
                return text
            }
            scanned = buffer.count
            if eof {
                if buffer.isEmpty { return nil }
                throw JedError("22023", "unterminated actor command")
            }
            guard buffer.count < 16 * 1024 * 1024 else { throw JedError("54000", "actor command exceeds 16 MiB") }
            var bytes = [UInt8](repeating: 0, count: min(4096, 16 * 1024 * 1024 - buffer.count))
            let count = bytes.withUnsafeMutableBytes { read(STDIN_FILENO, $0.baseAddress, $0.count) }
            if count < 0 && errno == EINTR { continue }
            guard count >= 0 else { throw JedError("58030", "read actor command failed") }
            if count == 0 { eof = true } else { buffer.append(contentsOf: bytes.prefix(count)) }
        }
    }
}
