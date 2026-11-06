import Foundation
import Jed
#if os(Linux)
import Glibc
#elseif canImport(Darwin)
import Darwin
#endif

private struct Usage: Error, CustomStringConvertible { let description: String }
private func usage() -> Usage {
    Usage(description: "usage: jed-check [--quick] [--constraints] [--max-findings N] [--format text|json] [--key-file FILE | --passphrase-file FILE | --passphrase] DBFILE")
}
private func hexadecimal(_ text: String) throws -> [UInt8] {
    let bytes = Array(text.utf8)
    guard bytes.count == 64 else { throw usage() }
    func digit(_ byte: UInt8) throws -> UInt8 {
        switch byte { case 48...57: return byte - 48; case 65...70: return byte - 55; case 97...102: return byte - 87; default: throw usage() }
    }
    return try stride(from: 0, to: bytes.count, by: 2).map { try digit(bytes[$0]) * 16 + digit(bytes[$0 + 1]) }
}
private func output(_ report: IntegrityReport, format: String) throws {
    if format == "json" {
        let findings: [[String: Any]] = report.findings.map {
            ["kind": $0.kind, "database": $0.database, "object": $0.object as Any? ?? NSNull(),
             "page": $0.page as Any? ?? NSNull(), "key": $0.key.map { $0.map { String(format: "%02x", $0) }.joined() } as Any? ?? NSNull(), "detail": $0.detail]
        }
        let data = try JSONSerialization.data(withJSONObject: ["findings": findings, "truncated": report.truncated,
            "skipped": report.skipped.map { ["object": $0.object, "reason": $0.reason] }, "pages": report.pages, "rows": report.rows], options: [.sortedKeys])
        FileHandle.standardOutput.write(data); print("")
    } else {
        for finding in report.findings {
            var line = finding.kind
            if let object = finding.object { line += " " + object }
            if let page = finding.page { line += " page \(page)" }
            if let key = finding.key { line += " key " + key.map { String(format: "%02x", $0) }.joined() }
            if !finding.detail.isEmpty { line += ": " + finding.detail }
            print(line)
        }
        for skipped in report.skipped { print("skipped \(skipped.object): \(skipped.reason)") }
        let verdict = report.ok ? "ok" : "\(report.findings.count) finding\(report.findings.count == 1 ? "" : "s")" +
            (report.truncated ? " (stopped at --max-findings; more may exist)" : "")
        print("\(verdict): \(report.pages) pages, \(report.rows) rows checked")
    }
}
private func prompt() throws -> String {
    let terminal = try FileHandle(forUpdating: URL(fileURLWithPath: "/dev/tty"))
    defer { try? terminal.close() }
    var settings = termios()
    guard tcgetattr(terminal.fileDescriptor, &settings) == 0 else { throw Usage(description: "cannot read terminal settings") }
    var hidden = settings; hidden.c_lflag &= ~tcflag_t(ECHO)
    guard tcsetattr(terminal.fileDescriptor, TCSAFLUSH, &hidden) == 0 else { throw Usage(description: "cannot disable terminal echo") }
    defer { _ = tcsetattr(terminal.fileDescriptor, TCSAFLUSH, &settings); terminal.write(Data([10])) }
    terminal.write(Data("Passphrase: ".utf8))
    var bytes = Data()
    while let byte = try terminal.read(upToCount: 1), !byte.isEmpty {
        if byte.first == 10 { break }; bytes.append(byte)
    }
    if bytes.last == 13 { bytes.removeLast() }
    guard let text = String(data: bytes, encoding: .utf8), !text.isEmpty else { throw Usage(description: "empty or non-UTF8 passphrase") }
    return text
}
private func run() throws -> Int32 {
    var args = Array(CommandLine.arguments.dropFirst()), options = IntegrityCheckOptions(), format = "text", credential: Credential?, path: String?
    while !args.isEmpty {
        let argument = args.removeFirst()
        switch argument {
        case "--quick": options.level = .quick
        case "--constraints": options.constraints = true
        case "--max-findings":
            guard !args.isEmpty, let value = Int(args.removeFirst()), value >= 0 else { throw usage() }
            options.maxFindings = value
        case "--format":
            guard !args.isEmpty else { throw usage() }; format = args.removeFirst()
            guard format == "text" || format == "json" else { throw usage() }
        case "--key-file":
            guard credential == nil, !args.isEmpty else { throw usage() }
            let bytes = try Data(contentsOf: URL(fileURLWithPath: args.removeFirst()))
            if bytes.count == 32 { credential = try .hostKey(Array(bytes)) }
            else {
                guard let text = String(data: bytes, encoding: .utf8), !text.isEmpty else { throw usage() }
                credential = try .hostKey(hexadecimal(text.trimmingCharacters(in: .whitespacesAndNewlines)))
            }
        case "--passphrase":
            guard credential == nil else { throw usage() }
            credential = .passphrase(try prompt())
        case "--passphrase-file":
            guard credential == nil, !args.isEmpty else { throw usage() }
            var bytes = try Data(contentsOf: URL(fileURLWithPath: args.removeFirst()))
            if bytes.last == 10 { bytes.removeLast(); if bytes.last == 13 { bytes.removeLast() } }
            guard let text = String(data: bytes, encoding: .utf8), !text.isEmpty else { throw Usage(description: "passphrase file is not UTF-8") }
            credential = .passphrase(text)
        case "--help", "-h": print(usage().description); return 0
        default:
            guard !argument.hasPrefix("-"), path == nil else { throw usage() }; path = argument
        }
    }
    guard let path else { throw usage() }
    let database: Database
    do { database = try Database.open(path, options: OpenOptions(credential: credential, readOnly: true)) }
    catch let error as JedError where error.code == "XX001" {
        if format == "json" {
            let data = try JSONSerialization.data(withJSONObject: ["open_failed": ["code": "XX001", "message": error.message]], options: [.sortedKeys])
            FileHandle.standardOutput.write(data); print("")
        } else { print("open_failed: XX001 \(error.message)") }
        return 2
    }
    defer { try? database.close() }
    let report = try database.integrityCheck(options: options)
    try output(report, format: format)
    return report.ok ? 0 : 2
}
do { exit(try run()) }
catch {
    FileHandle.standardError.write(Data((String(describing: error) + "\n").utf8)); exit(1)
}
