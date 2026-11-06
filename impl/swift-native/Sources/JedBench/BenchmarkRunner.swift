import Foundation
import Dispatch
import Synchronization
import Jed

private func now() -> UInt64 { DispatchTime.now().uptimeNanoseconds }
private func elapsed(_ start: UInt64) -> Int64 { Int64(now() - start) }

private func benchmarkEnvironment() throws -> Environment {
    var builder = Environment.builder()
    let mix: @Sendable (SQLValue) throws -> SQLValue = { value in
        guard case .integer(let number) = value else { throw BenchmarkError("host benchmark expected integer") }
        return .integer(number * 2_654_435_761 % 1_000_003)
    }
    try builder.register(HostFunction(name: SQLText("bench_mix_row"), arguments: [.i32], result: .i64,
        volatility: .immutable, kernel: { try mix($0[0]) }))
    try builder.register(HostFunction(name: SQLText("bench_mix_batch"), arguments: [.i32], result: .i64,
        volatility: .immutable, batchKernel: { columns, output in
            for value in columns[0] { output.append(try mix(value)) }
        }))
    return builder.build()
}

private final class BenchmarkEngine {
    let database: Database
    let session: Session
    private let scratch: URL?
    private var closed = false

    init(data: URL, benchmark: BenchmarkCase) throws {
        let environment = try benchmarkEnvironment()
        let scratch = benchmark.dataset == "scratch" ? data.appendingPathComponent("swift-scratch-\(UUID().uuidString)") : nil
        if let scratch { try FileManager.default.createDirectory(at: scratch, withIntermediateDirectories: false) }
        do {
            let database: Database
            if let scratch {
                database = try Database.create(options: CreateOptions(environment: environment,
                    path: scratch.appendingPathComponent("scratch.jed").path, synchronous: !benchmark.relaxed))
            } else {
                database = try Database.open(data.appendingPathComponent(benchmark.dataset + ".jed").path,
                    options: OpenOptions(environment: environment))
            }
            do { session = try database.session() }
            catch { try? database.close(); throw error }
            self.database = database; self.scratch = scratch
        } catch { if let scratch { try? FileManager.default.removeItem(at: scratch) }; throw error }
    }
    func close() throws {
        guard !closed else { return }; closed = true
        session.close()
        defer { if let scratch { try? FileManager.default.removeItem(at: scratch) } }
        try database.close()
    }
    deinit { try? close() }
    func integer(_ sql: String) throws -> Int64 {
        let rows = try session.query(sql); defer { rows.close() }
        guard let row = try rows.next(), row.count == 1, case .integer(let number) = row[0], try rows.next() == nil else {
            throw BenchmarkError("expected exactly one integer: \(sql)")
        }
        return number
    }
}

private func drain(_ rows: Rows, checksum: inout BenchmarkChecksum, measured: Bool) throws -> Int {
    defer { rows.close() }
    var count = 0
    while let row = try rows.next() { count += 1; if measured { try checksum.row(row) } }
    if let error = rows.error { throw error }
    return count
}

struct BenchmarkResult {
    let benchmark: BenchmarkCase
    let fingerprint: String
    let startedAt: String
    var samples: [Int64] = []
    var total: Int64 = 0
    var rows: Int64 = 0
    var checksum = BenchmarkChecksum()

    var json: String {
        get throws {
            guard samples.count == benchmark.iterations, !samples.isEmpty else { throw BenchmarkError("incomplete benchmark samples") }
            let sorted = samples.sorted()
            let fields: [(String, Any)] = [
                ("schema", 2), ("bench", benchmark.name), ("dataset", benchmark.dataset),
                ("engine", "jed"), ("lang", "swift"), ("variant", "experimental-native"),
                ("iterations", benchmark.iterations), ("warmup", benchmark.warmup),
                ("readers", benchmark.kind == "concurrent_read" ? benchmark.readers : 0),
                ("total_ns", total), ("ns_per_op", total / Int64(benchmark.iterations)),
                ("min_ns", sorted[0]), ("p50_ns", sorted[(sorted.count - 1) * 50 / 100]),
                ("p90_ns", sorted[(sorted.count - 1) * 90 / 100]), ("p99_ns", sorted[(sorted.count - 1) * 99 / 100]),
                ("rows_total", rows), ("checksum", checksum.hex), ("fingerprint", fingerprint), ("started_at", startedAt),
            ]
            return "{" + (try fields.map { key, value in
                let encoded = try JSONSerialization.data(withJSONObject: value, options: [.fragmentsAllowed])
                return "\"\(key)\":" + String(decoding: encoded, as: UTF8.self)
            }).joined(separator: ",") + "}"
        }
    }
    var sampleJSON: Data {
        get throws { try JSONSerialization.data(withJSONObject: ["bench": benchmark.name, "samples_ns": samples]) }
    }
}

struct BenchmarkRunner {
    let input: BenchmarkInput
    let data: URL

    func run(_ benchmark: BenchmarkCase) throws -> BenchmarkResult {
        try benchmark.validate()
        let started = ISO8601DateFormatter().string(from: Date())
        var result = BenchmarkResult(benchmark: benchmark, fingerprint: input.fingerprint, startedAt: started)
        let engine = try BenchmarkEngine(data: data, benchmark: benchmark)
        defer { try? engine.close() }
        if benchmark.dataset != "scratch" {
            let sidecar = data.appendingPathComponent(benchmark.dataset + ".jed.fingerprint")
            let fingerprint = try String(contentsOf: sidecar, encoding: .utf8).trimmingCharacters(in: .whitespacesAndNewlines)
            guard fingerprint == input.fingerprint else { throw BenchmarkError("stale benchmark dataset \(benchmark.dataset)") }
        }
        if benchmark.kind == "cold_open" {
            try engine.close()
            return try cold(benchmark, result: result)
        }
        for sql in benchmark.setupSQL { try engine.session.execute(sql) }
        if benchmark.kind == "concurrent_read" {
            result = try concurrent(benchmark, database: engine.database, result: result)
            try engine.close(); return result
        }
        let initialRows: Int64
        if benchmark.kind != "query", benchmark.dataset == "scratch" {
            initialRows = try engine.integer("SELECT count(*) FROM \(benchmark.writeTable)")
        } else { initialRows = 0 }
        let prepared = try engine.session.prepare(benchmark.sql)
        var parameters = BenchmarkParameters(benchmark)
        result.samples.reserveCapacity(benchmark.iterations)
        for ordinal in 0..<(benchmark.warmup + benchmark.iterations) {
            let measured = ordinal >= benchmark.warmup
            let start = now()
            switch benchmark.kind {
            case "query":
                let rows = try engine.session.queryPrepared(prepared, parameters: parameters.next())
                let count = try drain(rows, checksum: &result.checksum, measured: measured)
                let duration = elapsed(start)
                if measured {
                    result.samples.append(duration); result.rows += Int64(count)
                    try benchmark.checkRows(count)
                }
            case "write_rollback":
                try engine.session.execute("BEGIN")
                for _ in 0..<benchmark.batch { try engine.session.executePrepared(prepared, parameters: parameters.next()) }
                try engine.session.execute("ROLLBACK")
                if measured { result.samples.append(elapsed(start)) }
            case "write_durable":
                try engine.session.executePrepared(prepared, parameters: parameters.next())
                if measured { result.samples.append(elapsed(start)) }
            default: throw BenchmarkError("unexpected workload kind \(benchmark.kind)")
            }
        }
        if benchmark.kind != "query" {
            let table = try benchmark.writeTable, count = try engine.integer("SELECT count(*) FROM \(table)")
            var expected = initialRows
            if benchmark.kind == "write_rollback", benchmark.dataset != "scratch" {
                guard let rows = input.tableRows[benchmark.dataset]?[table] else { throw BenchmarkError("unknown benchmark table") }
                expected = rows
            } else if benchmark.kind == "write_durable", benchmark.sql.split(whereSeparator: { $0.isWhitespace }).first?.uppercased() == "INSERT" {
                expected += Int64(benchmark.warmup + benchmark.iterations)
            }
            guard count == expected else { throw BenchmarkError("\(benchmark.name): post-run count \(count), expected \(expected)") }
            result.checksum.integer(count); result.checksum.byte(0x1e)
        }
        result.total = result.samples.reduce(0, +)
        try engine.close(); return result
    }

    private func cold(_ benchmark: BenchmarkCase, result base: BenchmarkResult) throws -> BenchmarkResult {
        var result = base, parameters = BenchmarkParameters(benchmark)
        for ordinal in 0..<(benchmark.warmup + benchmark.iterations) {
            let measured = ordinal >= benchmark.warmup, arguments = try parameters.next()
            let start = now(), engine = try BenchmarkEngine(data: data, benchmark: benchmark)
            do {
                let prepared = try engine.session.prepare(benchmark.sql)
                let rows = try engine.session.queryPrepared(prepared, parameters: arguments)
                let count = try drain(rows, checksum: &result.checksum, measured: measured)
                let duration = elapsed(start)
                try engine.close()
                if measured { result.samples.append(duration); result.rows += Int64(count); try benchmark.checkRows(count) }
            } catch { try? engine.close(); throw error }
        }
        result.total = result.samples.reduce(0, +)
        return result
    }

    private struct ReaderOutcome: Sendable {
        var checksum = BenchmarkChecksum()
        var samples: [Int64] = []
        var rows: Int64 = 0
    }
    private func concurrent(_ benchmark: BenchmarkCase, database: Database, result base: BenchmarkResult) throws -> BenchmarkResult {
        var stream = BenchmarkParameters(benchmark)
        let warm = try (0..<benchmark.warmup).map { _ in try stream.next() }
        let measured = try (0..<benchmark.iterations).map { _ in try stream.next() }
        func pass(_ arguments: [[SQLValue]], measured: Bool) throws -> [ReaderOutcome] {
            let results = Mutex<[Result<ReaderOutcome, BenchmarkError>?]>(Array(repeating: nil, count: benchmark.readers))
            let group = DispatchGroup()
            var offset = 0
            for reader in 0..<benchmark.readers {
                let length = arguments.count / benchmark.readers + (reader < arguments.count % benchmark.readers ? 1 : 0)
                let block = arguments[offset..<(offset + length)]; offset += length
                group.enter()
                Thread.detachNewThread {
                    defer { group.leave() }
                    do {
                        // A session is created and closed on its worker thread.
                        // Like the Rust peer, wall time includes spawn/join and
                        // session construction; each query reparses its SQL.
                        let session = try database.readSession(); defer { session.close() }
                        var outcome = ReaderOutcome()
                        outcome.samples.reserveCapacity(block.count)
                        for parameters in block {
                            let start = measured ? now() : 0
                            let rows = try session.query(benchmark.sql, parameters: parameters)
                            let count = try drain(rows, checksum: &outcome.checksum, measured: measured)
                            if measured {
                                outcome.samples.append(elapsed(start)); outcome.rows += Int64(count)
                                try benchmark.checkRows(count)
                            }
                        }
                        results.withLock { $0[reader] = .success(outcome) }
                    } catch { results.withLock { $0[reader] = .failure(BenchmarkError(String(describing: error))) } }
                }
            }
            group.wait()
            return try results.withLock { values in
                try values.map { value in
                    guard let value else { throw BenchmarkError("benchmark reader did not complete") }
                    return try value.get()
                }
            }
        }
        _ = try pass(warm, measured: false)
        let start = now(), outcomes = try pass(measured, measured: true), duration = elapsed(start)
        var result = base
        result.total = duration
        for outcome in outcomes {
            result.checksum.text(outcome.checksum.hex.utf8)
            result.samples += outcome.samples; result.rows += outcome.rows
        }
        return result
    }
}
