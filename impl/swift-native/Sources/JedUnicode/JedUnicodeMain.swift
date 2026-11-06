import Foundation
import JedUnicodeCompiler
#if canImport(Glibc)
import Glibc
#elseif canImport(Darwin)
import Darwin
#endif

@main
struct JedUnicodeCommand {
    static let usage = """
    Usage:
      jed-unicode compile --name NAME --out PATH DEFINITION...
      jed-unicode casing --out PATH SOURCE
      jed-unicode bundle --preset casing-only|non-cjk|everything --spec-dir PATH --out PATH

    Definitions are explicit DUCET/LDML files in order. The casing command writes
    a property-only JUCD bundle. --spec-dir names the repository's spec directory.
    Every command requires an explicit output; no source download is performed.
    """
    static func main() {
        do { try run(Array(CommandLine.arguments.dropFirst())) }
        catch {
            FileHandle.standardError.write(Data(("jed-unicode: \(error)\n").utf8))
            exit(1)
        }
    }
    private static func run(_ arguments: [String]) throws {
        guard let command = arguments.first else { print(usage); return }
        if command == "--help" || command == "help" { print(usage); return }
        var options: [String: String] = [:], files: [String] = [], cursor = 1, positional = false
        let allowed: Set<String>
        switch command {
        case "compile": allowed = ["--name", "--out"]
        case "casing": allowed = ["--out"]
        case "bundle": allowed = ["--preset", "--spec-dir", "--out"]
        default: throw ToolError("unknown command: " + command)
        }
        while cursor < arguments.count {
            let argument = arguments[cursor]; cursor += 1
            if !positional && argument == "--" { positional = true; continue }
            if !positional && argument.hasPrefix("--") {
                guard allowed.contains(argument), options[argument] == nil, cursor < arguments.count else {
                    throw ToolError("invalid, repeated, or incomplete option: " + argument)
                }
                options[argument] = arguments[cursor]; cursor += 1
            } else { files.append(argument) }
        }
        guard let output = options["--out"], !output.isEmpty else { throw ToolError("--out PATH is required") }
        let bytes: [UInt8]
        switch command {
        case "compile":
            guard let name = options["--name"], !files.isEmpty else { throw ToolError("compile requires --name and at least one definition") }
            bytes = try UnicodeCompiler.compile(name: name, definitions: files.map(readText))
        case "casing":
            guard files.count == 1 else { throw ToolError("casing requires exactly one source file") }
            bytes = try UnicodeCompiler.bundle(root: nil, casing: UnicodeCompiler.compileCasing(readText(files[0])))
        default:
            guard files.isEmpty, let directory = options["--spec-dir"] else { throw ToolError("bundle requires --spec-dir and accepts no positional files") }
            let preset = options["--preset"] ?? "non-cjk"
            guard ["casing-only", "non-cjk", "everything"].contains(preset) else { throw ToolError("unknown bundle preset: " + preset) }
            let base = URL(fileURLWithPath: directory).appendingPathComponent("collation")
            let casing = try UnicodeCompiler.compileCasing(readText(base.appendingPathComponent("17.0.0/casing.txt").path))
            if preset == "casing-only" { bytes = try UnicodeCompiler.bundle(root: nil, casing: casing) }
            else {
                // These are the complete authored baseline presets. No implicit
                // weights or future Han/tailoring data are synthesized.
                let root = try readBytes(base.appendingPathComponent("fixtures/unicode.coll").path)
                let spanish = try readBytes(base.appendingPathComponent("fixtures/es.coll").path)
                bytes = try UnicodeCompiler.bundle(root: root, tailorings: [spanish], casing: casing)
            }
        }
        try Data(bytes).write(to: URL(fileURLWithPath: output), options: .atomic)
        print("wrote \(output) (\(bytes.count) bytes)")
    }
    private static func readBytes(_ path: String) throws -> [UInt8] { Array(try Data(contentsOf: URL(fileURLWithPath: path))) }
    private static func readText(_ path: String) throws -> String {
        let bytes = try readBytes(path)
        guard let text = String(bytes: bytes, encoding: .utf8) else { throw ToolError("source is not valid UTF-8: " + path) }
        return text
    }
    private struct ToolError: Error, CustomStringConvertible {
        let description: String
        init(_ description: String) { self.description = description }
    }
}
