import Foundation
import Jed

/// Opt-in file interchange transport. Every operation uses the public host API;
/// the Ruby driver supplies SQL and owns all comparisons and applicability rules.
@main struct JedInteropMain {
    static func hex(_ bytes: [UInt8]) -> String { bytes.map { String(format: "%02x", $0) }.joined() }
    static func bytes(_ text: String) throws -> [UInt8] {
        let source = Array(text.utf8)
        guard source.count % 2 == 0 else { throw JedError("22023", "odd hex input") }
        var output: [UInt8] = []
        for at in stride(from: 0, to: source.count, by: 2) {
            guard let byte = UInt8(String(decoding: source[at..<(at+2)], as: UTF8.self), radix: 16) else { throw JedError("22023", "invalid hex input") }
            output.append(byte)
        }
        return output
    }
    static func text(_ hex: String) throws -> String {
        guard let text = String(bytes: try bytes(hex), encoding: .utf8) else { throw JedError("22021", "invalid UTF-8 input") }
        return text
    }
    static func credential(_ kind: String, _ encoded: String) throws -> Credential? {
        switch kind {
        case "plain": return nil
        case "key": return try .hostKey(bytes(encoded))
        case "pass": return .passphrase(try text(encoded))
        default: throw JedError("22023", "invalid credential kind")
        }
    }
    static func reply(_ line: String) { FileHandle.standardOutput.write(Data((line + "\n").utf8)) }
    static func fail(_ error: Error) {
        reply("ERR\t" + ((error as? JedError)?.code ?? "XXXXX") + "\t" + hex(Array(String(describing: error).utf8)))
    }
    static func main() {
        do { try run() } catch { fail(error); exit(1) }
    }
    static func run() throws {
        let args = Array(CommandLine.arguments.dropFirst())
        guard args.count == 6 else { throw JedError("22023", "expected action path fixtures credential-kind credential-hex environment") }
        var builder = Environment.builder()
        if args[5] != "empty" {
            try builder.loadUnicodeData(Array(Data(contentsOf: URL(fileURLWithPath: args[2] + "/unicode.jucd"))))
            try builder.loadTimeZoneData(Array(Data(contentsOf: URL(fileURLWithPath: args[2] + "/tzdata.jtz"))))
        }
        if args[5] == "full" {
            try builder.register(HostFunction(name: SQLText("geo_hash"), arguments: [.i64], result: .i64,
                volatility: .immutable, crossCore: true, componentID: SQLText("com.example/geo_hash"), semanticVersion: 1,
                kernel: { values in
                    guard case .integer(let value) = values[0] else { throw JedError("42804", "expected host integer") }
                    let result = value.multipliedReportingOverflow(by: 10)
                    guard !result.overflow else { throw JedError("22003", "host integer overflow") }
                    return .integer(result.partialValue)
                }))
        }
        builder.useInsecureKeyEntropy(seed: 0x4a454442)
        let environment = builder.build(), cred = try credential(args[3], args[4])
        let kdf = try Argon2Parameters(memoryKiB: 32, iterations: 1, parallelism: 1)
        let database: Database
        if args[0] == "create" {
            database = try Database.create(options: CreateOptions(environment: environment, path: args[1], pageSize: 1024,
                credential: cred, kdf: kdf, locking: .none, scratchEncryption: .on))
        } else if args[0] == "open" {
            database = try Database.open(args[1], options: OpenOptions(environment: environment, credential: cred, locking: .none, scratchEncryption: .on))
        } else { throw JedError("22023", "invalid action") }
        defer { try? database.close() }
        reply("READY")
        while let line = readLine() {
            guard line.utf8.count <= 16 * 1024 * 1024 else { throw JedError("54000", "interop command too large") }
            let fields = line.split(separator: "\t", omittingEmptySubsequences: false).map(String.init)
            do {
                let arity = ["EXEC": 2, "QUERY": 2, "ATTACH": 4, "REWRAP": 3, "UPGRADE": 1, "CLOSE": 1]
                guard arity[fields[0]] == fields.count else { throw JedError("22023", "invalid command arity") }
                var result = ""
                switch fields[0] {
                case "EXEC": _ = try database.execute(text(fields[1]))
                case "QUERY":
                    let rows = try database.query(text(fields[1])); defer { rows.close() }
                    var output: [String] = []
                    while let row = try rows.next() {
                        output.append(try row.map { value in
                            switch value {
                            case .null: return "-"
                            case .text(let text): return hex(text.utf8)
                            default: throw JedError("42804", "interop QUERY requires text fields")
                            }
                        }.joined(separator: ":"))
                    }
                    result = String(output.count) + ";" + output.joined(separator: ",")
                case "ATTACH":
                    guard fields.count == 4 else { throw JedError("22023", "invalid attach") }
                    try database.attach(SQLText("aux"), source: .file(text(fields[1]), OpenOptions(environment: environment,
                        credential: credential(fields[2], fields[3]), locking: .none, scratchEncryption: .on)), readOnly: false)
                case "REWRAP":
                    guard let target = try credential(fields[1], fields[2]) else { throw JedError("22023", "missing rewrap credential") }
                    try database.changeCredential(credential: target, kdf: kdf)
                case "UPGRADE": try database.upgradeCollations()
                case "CLOSE": try database.close(); reply("OK\t"); return
                default: throw JedError("22023", "unknown interop command")
                }
                reply("OK\t" + result)
            } catch { fail(error) }
        }
    }
}
