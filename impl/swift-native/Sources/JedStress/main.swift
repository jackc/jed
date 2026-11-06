import Foundation
import Synchronization
import Jed

// Thin adapter for the frozen language-neutral Layer 3 workload. Sessions and
// cursors are created, used and closed on their worker's real OS thread.
struct StressFailure: Error, CustomStringConvertible { let description: String }
struct StressWorker: Decodable, Sendable {
    let kind: String
    let count: Int
    let iterations: Int
    let op: String
    let invariantQuery: String
    let invariantExpect: String
}
struct StressProfile: Decodable, Sendable {
    let name: String
    let parallel: String
    let seed: UInt64
    let setup: [String]
    let workers: [StressWorker]
    let finalQuery: String?
    let finalExpect: [[Int64]]
    let crossCoreChecksum: Bool
}
struct StressInput: Decodable { let schema: Int; let profiles: [StressProfile] }
struct StressResult: Encodable {
    let schema = 1
    let name: String
    let lang = "swift"
    let mode = "threaded"
    let storage: String
    var status = "fail"
    var invariant_checks = 0
    let writers: Int
    let writer_iters: Int
    var final_ok = false
    var checksum = ""
    let cross_core_checksum: Bool
    var duration_ms: UInt64 = 0
    var error = ""
}

struct StressChecksum {
    var value: UInt64 = 0xcbf29ce484222325
    mutating func byte(_ byte: UInt8) { value = (value ^ UInt64(byte)) &* 0x100000001b3 }
    mutating func cell(_ text: String) { for item in text.utf8 { byte(item) }; byte(0x1f) }
    var hex: String { String(format: "%016llx", value) }
}

func scalar(_ session: Session, _ query: String) throws -> String {
    let rows = try session.query(query); defer { rows.close() }
    guard rows.columns.count == 1, let type = rows.columns[0].type,
          let row = try rows.next(), row.count == 1 else {
        throw StressFailure(description: "invariant must return one typed scalar")
    }
    let value = try session.render(row[0], type: type).description
    guard try rows.next() == nil else { throw StressFailure(description: "invariant returned multiple rows") }
    return value
}

func writerStatements(_ op: String) -> [String] {
    splitStatements(op).map(\.text).filter { !["BEGIN", "COMMIT", "ROLLBACK"].contains($0.uppercased()) }
}

func worker(_ worker: StressWorker, database: Database) throws -> Int {
    switch worker.kind {
    case "writer":
        let statements = writerStatements(worker.op)
        for _ in 0..<worker.iterations {
            let session = try database.writeSession(); defer { session.close() }
            do {
                for statement in statements { try session.execute(statement) }
                try session.commit()
            } catch { try? session.rollback(); throw error }
        }
        return 0
    case "reader":
        for _ in 0..<worker.iterations {
            let session = try database.readSession(); defer { session.close() }
            let observed = try scalar(session, worker.invariantQuery)
            guard observed == worker.invariantExpect else {
                throw StressFailure(description: "invariant \(worker.invariantQuery): got \(observed), expected \(worker.invariantExpect)")
            }
        }
        return worker.iterations
    default: throw StressFailure(description: "unknown worker kind \(worker.kind)")
    }
}

func threaded(_ database: Database, profile: StressProfile) throws -> Int {
    let workers = profile.workers.flatMap { Array(repeating: $0, count: $0.count) }
    let results = Mutex<[Result<Int, StressFailure>?]>(Array(repeating: nil, count: workers.count))
    let group = DispatchGroup(), start = DispatchSemaphore(value: 0)
    for (index, specification) in workers.enumerated() {
        group.enter()
        Thread.detachNewThread {
            defer { group.leave() }
            start.wait()
            do {
                let checks = try worker(specification, database: database)
                results.withLock { $0[index] = .success(checks) }
            } catch { results.withLock { $0[index] = .failure(StressFailure(description: String(describing: error))) } }
        }
    }
    for _ in workers { start.signal() }
    guard group.wait(timeout: .now() + .seconds(60)) == .success else {
        // The actor's caller exits immediately after writing this failure.
        // Closing a database here could itself block behind a wedged writer.
        throw StressFailure(description: "deadlock: workers did not finish within 60 seconds")
    }
    return try results.withLock { results in
        try results.reduce(0) { sum, result in
            guard let result else { throw StressFailure(description: "worker produced no result") }
            return sum + (try result.get())
        }
    }
}

func final(_ database: Database, profile: StressProfile) throws -> (String, Bool) {
    guard let query = profile.finalQuery else { return ("", true) }
    let session = try database.readSession(); defer { session.close() }
    let rows = try session.query(query); defer { rows.close() }
    var checksum = StressChecksum(), values: [[Int64]] = []
    while let row = try rows.next() {
        var integers: [Int64] = []
        for value in row {
            switch value {
            case .integer(let number): integers.append(number); checksum.cell(String(number))
            case .null: checksum.cell("NULL")
            default: throw StressFailure(description: "stress final query requires integer columns")
            }
        }
        checksum.byte(0x1e); values.append(integers)
    }
    return (checksum.hex, profile.finalExpect.isEmpty || values == profile.finalExpect)
}

func run(_ profile: StressProfile, storage: String, path: String?) -> StressResult {
    let writers = profile.workers.filter { $0.kind == "writer" }
    var result = StressResult(name: profile.name, storage: storage,
        writers: writers.reduce(0) { $0 + $1.count },
        writer_iters: writers.reduce(0) { $0 + $1.count * $1.iterations },
        cross_core_checksum: profile.crossCoreChecksum)
    let started = DispatchTime.now().uptimeNanoseconds
    do {
        guard ["optional", "required"].contains(profile.parallel), !profile.workers.isEmpty,
              profile.workers.allSatisfy({ $0.count > 0 && $0.iterations > 0 && ["writer", "reader"].contains($0.kind) }) else {
            throw StressFailure(description: "invalid frozen stress profile")
        }
        let database = try Database.create(options: CreateOptions(path: path, cacheBytes: storage == "disk" ? 8192 : 256 * 1024 * 1024,
            locking: .exclusive))
        // No unconditional close-on-error: timeout must escape a wedged gate.
        let setup = try database.writeSession()
        do { for sql in profile.setup { try setup.execute(sql) }; try setup.commit(); setup.close() }
        catch { try? setup.rollback(); setup.close(); try? database.close(); throw error }
        result.invariant_checks = try threaded(database, profile: profile)
        let expected = profile.workers.filter { $0.kind == "reader" }.reduce(0) { $0 + $1.count * $1.iterations }
        guard result.invariant_checks == expected else { throw StressFailure(description: "missing reader invariant checks") }
        (result.checksum, result.final_ok) = try final(database, profile: profile)
        guard result.final_ok else { throw StressFailure(description: "final state differs from frozen expectation") }
        try database.close()
        if let path {
            let reopened = try Database.open(path, options: OpenOptions(readOnly: true, locking: .exclusive))
            let (checksum, okay) = try final(reopened, profile: profile)
            try reopened.close()
            guard okay && checksum == result.checksum else { throw StressFailure(description: "durable reopen changed final state") }
        }
        result.status = "pass"
    } catch { result.error = String(describing: error) }
    result.duration_ms = (DispatchTime.now().uptimeNanoseconds - started) / 1_000_000
    return result
}

do {
    let arguments = Array(CommandLine.arguments.dropFirst())
    guard arguments.count == 4, ["memory", "disk"].contains(arguments[2]) else {
        throw StressFailure(description: "usage: jed-stress INPUT.json OUTPUT.jsonl memory|disk SCRATCH-DIRECTORY")
    }
    let input = try JSONDecoder().decode(StressInput.self, from: Data(contentsOf: URL(fileURLWithPath: arguments[0])))
    guard input.schema == 1, !input.profiles.isEmpty, Set(input.profiles.map(\.name)).count == input.profiles.count else {
        throw StressFailure(description: "invalid stress inventory")
    }
    try FileManager.default.createDirectory(atPath: arguments[3], withIntermediateDirectories: true)
    let encoder = JSONEncoder(); encoder.outputFormatting = [.sortedKeys]
    var output = Data()
    for (index, profile) in input.profiles.enumerated() {
        let path = arguments[2] == "disk" ? "\(arguments[3])/stress-\(index).jed" : nil
        let result = run(profile, storage: arguments[2], path: path)
        output.append(try encoder.encode(result)); output.append(0x0a)
        try output.write(to: URL(fileURLWithPath: arguments[1]))
        if result.status != "pass" { exit(1) }
    }
} catch {
    FileHandle.standardError.write(Data("\(error)\n".utf8)); exit(1)
}
