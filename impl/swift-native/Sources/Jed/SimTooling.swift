import Foundation

/// Opt-in simulation entry point. Inputs/results are host tooling; the runner
/// itself receives source text and never reads environment variables or files.
@_spi(ExperimentalTools) public enum NativeSimulation {
    public static func run(source: String, name: String, outputDirectory: String, imagePrefix: String? = nil,
                           progress: ((String) -> Void)? = nil) throws -> String {
        let trace = try SimTrace(source)
        func runner(_ pass: Int) throws -> SimRunner {
            try SimRunner(trace, progress: { step, line, beginning in
                progress?("\(name) pass \(pass) step \(step) line \(line) \(beginning ? "begin" : "end")")
            })
        }
        progress?("\(name) pass 1 setup")
        let first = try runner(1).run()
        progress?("\(name) pass 2 setup")
        let second = try runner(2).run()
        var violation = first.violation, step = first.violationStep
        if violation.isEmpty {
            if !second.violation.isEmpty { violation = "nondeterministic: second run failed: " + second.violation; step = second.violationStep }
            else if first.transcript != second.transcript { violation = "nondeterministic: transcripts differ" }
            else if first.operationsHash != second.operationsHash { violation = "nondeterministic: byte operations differ" }
            else if first.images != second.images { violation = "nondeterministic: stored images differ" }
        }
        let directory = URL(fileURLWithPath: outputDirectory, isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        try Data(first.transcript.utf8).write(to: directory.appendingPathComponent(name + ".swift-native.jsonl"))
        if let imagePrefix {
            for file in first.images.keys.sorted(by: SimText.less) {
                let destination = URL(fileURLWithPath: imagePrefix + "." + file)
                try FileManager.default.createDirectory(at: destination.deletingLastPathComponent(), withIntermediateDirectories: true)
                try Data(first.images[file]!).write(to: destination)
            }
        }
        return "{\"trace\":\(SimText.quote(name)),\"core\":\"swift-native\",\"status\":\(SimText.quote(violation.isEmpty ? "pass" : "fail")),\"steps\":\(trace.steps.count),\"violation\":\(SimText.quote(violation)),\"violation_step\":\(step),\"transcript_hash\":\(SimText.quote(SimText.hex(first.transcriptHash))),\"ops_hash\":\(SimText.quote(SimText.hex(first.operationsHash)))}"
    }
}
