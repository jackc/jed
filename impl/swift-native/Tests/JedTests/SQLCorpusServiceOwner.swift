import Foundation
@testable import Jed

/// The unchanged owner-idle process protocol used by the supported service
/// corpus runners. It transports owner control only; SQL executes in this test
/// process through the normal Database/Session and remote page-client paths.
final class SQLCorpusServiceOwner {
    #if os(Linux)
    private let transport: ProcessPageTransport
    #endif
    private var buffered: [UInt8] = []
    private var ownedPath: String?
    private var closed = false
    private(set) var clientRecords = 0

    static func configured(environment: [String: String] = ProcessInfo.processInfo.environment) throws -> SQLCorpusServiceOwner? {
        guard let encoded = environment["JED_SWIFT_SERVICE_OWNER_COMMAND"] else { return nil }
        guard let bytes = encoded.data(using: .utf8),
              let command = try JSONSerialization.jsonObject(with: bytes) as? [String],
              !command.isEmpty, command.allSatisfy({ !$0.isEmpty && !$0.contains("\0") }),
              let root = environment["JED_SWIFT_SERVICE_ROOT"], !root.isEmpty else {
            throw SQLCorpusError("service corpus needs a nonempty JSON owner command and repository root")
        }
        guard environment["JED_CONFORMANCE_ENCRYPTED"] != "1" else {
            throw SQLCorpusError("the shared owner-idle protocol has no encrypted-open credential command")
        }
        return try SQLCorpusServiceOwner(command: command + ["owner-idle", root])
    }

    private init(command: [String]) throws {
        #if os(Linux)
        transport = try ProcessPageTransport(command: command)
        do {
            guard try readLine(deadline: .now.advanced(by: .seconds(15))) == "READY" else {
                throw SQLCorpusError("service owner did not return READY")
            }
        } catch { transport.close(); throw error }
        #else
        throw SQLCorpusError("native service corpus host is not verified on this platform")
        #endif
    }

    #if os(Linux)
    private func readLine(deadline: ContinuousClock.Instant) throws -> String {
        while true {
            if let end = buffered.firstIndex(of: 10) {
                let bytes = Array(buffered[..<end]); buffered.removeFirst(end + 1)
                guard let line = String(bytes: bytes, encoding: .utf8) else { throw SQLCorpusError("owner response is not UTF-8") }
                return line
            }
            guard buffered.count < 65_536 else { throw SQLCorpusError("owner response exceeds 64 KiB") }
            let bytes = try transport.read(maxBytes: min(4096, 65_536 - buffered.count), deadline: deadline)
            guard !bytes.isEmpty else { throw SQLCorpusError("owner exited during command: \(transport.diagnostic)") }
            buffered.append(contentsOf: bytes)
        }
    }
    #endif

    private func command(_ name: String, argument: String? = nil) throws {
        guard !closed else { throw SQLCorpusError("service owner is closed") }
        #if os(Linux)
        let deadline = ContinuousClock.now.advanced(by: .seconds(15))
        let line = name + (argument.map { "\t" + $0 } ?? "") + "\n"
        try transport.write(Array(line.utf8), deadline: deadline)
        let response = try readLine(deadline: deadline)
        guard response == "OK" || response.hasPrefix("OK\t") else {
            throw SQLCorpusError("service owner \(name) failed: \(response); \(transport.diagnostic)")
        }
        #else
        throw SQLCorpusError("native service corpus host is unavailable")
        #endif
    }

    func open(path: String) throws {
        if ownedPath == path { return }
        guard ownedPath == nil else { throw SQLCorpusError("previous service corpus file is still owned") }
        let digits = Array("0123456789abcdef".utf8)
        let encoded = path.utf8.flatMap { [digits[Int($0 >> 4)], digits[Int($0 & 15)]] }
        try command("OWNER_OPEN", argument: String(decoding: encoded, as: UTF8.self))
        ownedPath = path
    }

    func assertClient(_ database: Database, countRecord: Bool = true) throws {
        #if os(Linux)
        guard ownedPath != nil, (database.host as? ServiceDatabaseHost)?.toolingRole == "client" else {
            throw SQLCorpusError("service corpus record did not use an external owner's page client")
        }
        if countRecord { clientRecords += 1 }
        #else
        throw SQLCorpusError("native service corpus host is unavailable")
        #endif
    }

    func release() throws {
        guard ownedPath != nil else { return }
        try command("OWNER_CLOSE"); ownedPath = nil
    }

    func close() throws {
        guard !closed else { return }
        defer {
            closed = true; ownedPath = nil; buffered = []
            #if os(Linux)
            transport.close()
            #endif
        }
        var failure: Error?
        do { try release() } catch { failure = error }
        do { try command("CLOSE") } catch { if failure == nil { failure = error } }
        if let failure { throw failure }
    }
    deinit { try? close() }
}
