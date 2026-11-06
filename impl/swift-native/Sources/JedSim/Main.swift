import Foundation
@_spi(ExperimentalTools) import Jed
#if os(Linux)
import Glibc
#else
import Darwin
#endif

@main struct JedSimMain {
    static func main() {
        do {
            let environment = ProcessInfo.processInfo.environment
            guard let input = environment["JED_SIM_IN"], !input.isEmpty,
                  let output = environment["JED_SIM_OUT"], !output.isEmpty else {
                throw ToolError("JED_SIM_IN and JED_SIM_OUT are required")
            }
            var directory: ObjCBool = false
            guard FileManager.default.fileExists(atPath: input, isDirectory: &directory) else { throw ToolError("missing trace input: " + input) }
            let paths: [String]
            if directory.boolValue {
                paths = try FileManager.default.contentsOfDirectory(atPath: input).filter { $0.hasSuffix(".sim") }.map { input + "/" + $0 }
            } else if input.hasSuffix(".sim") {
                paths = [input]
            } else {
                paths = try String(contentsOfFile: input, encoding: .utf8).split(separator: "\n").map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }.filter { !$0.isEmpty }
            }
            guard !paths.isEmpty else { throw ToolError("no simulation traces selected") }
            let names = paths.map { URL(fileURLWithPath: $0).deletingPathExtension().lastPathComponent }
            guard Set(names).count == names.count else { throw ToolError("duplicate simulation trace names") }
            try FileManager.default.createDirectory(atPath: output, withIntermediateDirectories: true)
            let destination = environment["JED_SIM_RESULTS"] ?? (output + "/swift-native.results.jsonl")
            var failed = false
            let progressHandle: FileHandle?
            if let path = environment["JED_SIM_TRACEFILE"] {
                _ = FileManager.default.createFile(atPath: path, contents: nil)
                progressHandle = try FileHandle(forWritingTo: URL(fileURLWithPath: path))
            } else { progressHandle = nil }
            defer { try? progressHandle?.close() }
            for path in paths.sorted(by: { $0.utf8.lexicographicallyPrecedes($1.utf8) }) {
                let name = URL(fileURLWithPath: path).deletingPathExtension().lastPathComponent
                let source = try String(contentsOfFile: path, encoding: .utf8)
                let prefix = environment["JED_SIM_IMAGES"].flatMap { value in
                    value.isEmpty ? nil : (value == "1" ? output : value) + "/" + name + ".swift-native"
                }
                let result = try NativeSimulation.run(source: source, name: name, outputDirectory: output, imagePrefix: prefix,
                    progress: { progressHandle?.write(Data(($0 + "\n").utf8)) })
                if !FileManager.default.fileExists(atPath: destination) { _ = FileManager.default.createFile(atPath: destination, contents: nil) }
                let handle = try FileHandle(forWritingTo: URL(fileURLWithPath: destination)); defer { try? handle.close() }
                try handle.seekToEnd(); try handle.write(contentsOf: Data((result + "\n").utf8))
                print(result)
                if result.contains("\"status\":\"fail\"") { failed = true }
            }
            exit(failed ? 1 : 0)
        } catch { FileHandle.standardError.write(Data((String(describing: error) + "\n").utf8)); exit(1) }
    }
    struct ToolError: Error, CustomStringConvertible { let description: String; init(_ value: String) { description = value } }
}
