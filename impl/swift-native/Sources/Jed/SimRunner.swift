import Foundation

internal struct SimViolation: Error, CustomStringConvertible {
    let description: String
    init(_ description: String) { self.description = description }
}

/// Trace state is host-owned test data. Every SQL, commit, recovery, replacement
/// and backup operation below calls the production implementation.
internal final class SimRunner {
    final class Domain {
        let name: String, file: String
        var key: Int
        var version: UInt64 = 1, floor: UInt64 = 1, attempted: UInt64 = 1
        var digest = "", history: [UInt64: Set<String>] = [:]
        var high: UInt32 = 0, free: UInt32 = 0, trail: UInt32 = 0, liveBound: UInt32 = 0
        var reclaimed = false, previousHigh: UInt32 = 0, previousFree: UInt32 = 0
        var batchOpen = false, batchPending = false, shrinkDue = false, due = false, ownerLost = false
        var deadline: Int64?
        var keyChange: (old: Int, next: Int, entryPending: Bool)?
        init(_ name: String, key: Int) { self.name = name; file = name == "main" ? "sim.jed" : name + ".jed"; self.key = key }
        func remember(_ version: UInt64, _ digest: String) { history[version, default: []].insert(digest); attempted = max(attempted, version) }
    }
    final class OpenSession {
        let session: Session, write: Bool, process: Int, pins: [String: UInt64]
        let random: SeededRandomSource
        init(_ session: Session, write: Bool, process: Int, pins: [String: UInt64], seed: UInt64) {
            self.session = session; self.write = write; self.process = process; self.pins = pins
            random = SeededRandomSource(seed: seed)
        }
    }
    struct Backup { let number: Int, file: String, version: UInt64, digest: String, key: Int }
    struct Result {
        let transcript: String, operationsHash: UInt64, violation: String, violationStep: Int
        let images: [String: [UInt8]]
        var transcriptHash: UInt64 { SimText.hash(transcript.utf8) }
    }
    let progress: ((Int, Int, Bool) -> Void)?
    let trace: SimTrace, filesystem: SimFilesystem, clock: ManualClock, random = SimRandomSource()
    let seed: UInt64, pageSize: Int, cacheBytes: Int, encrypted: Bool, relaxed: Bool, service: Bool
    let storageLimit: Int64, flushInterval: UInt64
    let sessionOptions: SessionOptions
    var domains: [String: Domain] = [:], sessions: [String: OpenSession] = [:]
    var processes: [Int: Database] = [:], currentProcess = 0
    var currentFault: SimFault?, startingVersions: [String: UInt64] = [:], startingOwners: [String: Int] = [:]
    var openings: UInt64 = 0, armed: SimFault?, transcript = "", backups: [Backup] = []
    var backupNumber = 0
    var stepIndex = -1, violation = "", violationStep = -1
    var images: [String: [UInt8]] = [:]
    #if os(Linux)
    lazy var serviceWorld = SimServiceWorld(filesystem: filesystem, clock: clock)
    var platforms: [Int: SimServicePlatform] = [:]
    #endif
    var database: Database { processes[currentProcess]! }
    var orderedDomains: [Domain] { domains.keys.sorted { $0 == "main" || $1 != "main" && SimText.less($0, $1) }.map { domains[$0]! } }

    init(_ trace: SimTrace, progress: ((Int, Int, Bool) -> Void)? = nil) throws {
        self.trace = trace; self.progress = progress
        func int(_ name: String, _ fallback: Int = 0) -> Int { trace.header[name].flatMap(Int.init) ?? fallback }
        seed = trace.header["seed"].flatMap(UInt64.init) ?? 0
        pageSize = int("page_size", 256); cacheBytes = int("cache_bytes", 256 * 1024 * 1024)
        encrypted = trace.header["encrypted"] == "1"; relaxed = trace.header["synchronous"] == "off"
        service = trace.header["processes"] == "service"
        storageLimit = Int64(int("max_storage_bytes")); flushInterval = trace.header["flush_interval_ms"].flatMap(UInt64.init) ?? 0
        clock = ManualClock(microseconds: trace.header["clock"].flatMap(Int64.init) ?? 0)
        filesystem = try SimFilesystem(seed: seed, sector: int("sector", 64), pageSize: pageSize, archiving: encrypted)
        var options = SessionOptions()
        if int("work_mem") > 0 { options.workMemoryBytes = Int64(int("work_mem")) }
        if int("temp_buffers") > 0 { options.temporaryBufferBytes = Int64(int("temp_buffers")) }
        options.maxQueryMemoryBytes = Int64(int("max_query_memory_bytes"))
        sessionOptions = options
    }
    static func credential(_ key: Int) -> Credential? {
        key < 0 ? nil : try! Credential.hostKey(Array(repeating: UInt8(0x4a + key), count: 32))
    }
    func environment(_ process: Int) -> Environment {
        var builder = Environment.builder(); builder.setRandomSource(random); builder.setClockSource(clock)
        if encrypted { builder.useInsecureKeyEntropy(seed: SimDecision.value(seed: seed, lane: 14, step: openings, offset: UInt64(process))) }
        openings &+= 1; return builder.build()
    }
    func scratch(_ environment: Environment) -> any ScratchFileHost {
        // Private sealed scratch has a separate stream from persisted keys.
        SimScratchFileHost(filesystem: filesystem, sealed: encrypted,
            random: SeededRandomSource(seed: SimDecision.value(seed: seed, lane: 11, step: openings)))
    }
    func create(_ domain: Domain) throws {
        let environment = environment(currentProcess), key = environment.newKeySource()
        let options = CreateOptions(environment: environment, path: domain.file, pageSize: pageSize,
            cacheBytes: cacheBytes, credential: Self.credential(domain.key), locking: .none, synchronous: !relaxed, flushIntervalMilliseconds: flushInterval,
            scratchEncryption: encrypted ? .on : .automatic)
        let created = try FileDatabaseHost.createResolved(path: domain.file, options: options,
            catalogID: Database.allocateCatalogID(), random: key, operations: filesystem, scratch: scratch(environment))
        try created.close()
    }
    func openFile(_ domain: Domain, environment: Environment) throws -> Database {
        let options = OpenOptions(environment: environment, cacheBytes: cacheBytes, maxStorageBytes: storageLimit,
            credential: Self.credential(domain.key), locking: .none, synchronous: !relaxed, flushIntervalMilliseconds: flushInterval, scratchEncryption: encrypted ? .on : .automatic)
        let opened: Database
        if service {
            #if os(Linux)
            var options = options
            options.pageService.idleSessionMilliseconds = 0; options.pageService.idleWriteMilliseconds = 0
            options.pageService.shutdownGraceMilliseconds = 1000
            opened = try ServiceDatabaseHost.openResolved(path: domain.file, options: options,
                catalogID: Database.allocateCatalogID(), platform: platform(currentProcess))
            #else
            throw SimViolation("page-service simulation is unavailable on this platform")
            #endif
        } else {
            opened = try FileDatabaseHost.openResolved(path: domain.file, options: options,
                catalogID: Database.allocateCatalogID(), operations: filesystem, scratch: scratch(environment))
        }
        opened.synchronous = !relaxed
        return opened
    }
    func openProcess(_ process: Int) throws {
        guard processes[process] == nil else { throw SimViolation("process p\(process) already exists") }
        currentProcess = process
        let environment = environment(process), opened = try openFile(domains["main"]!, environment: environment)
        do {
            for domain in orderedDomains where domain.name != "main" && filesystem.facts(domain.file) != nil {
                try opened.domains.attach(SQLText(domain.name), readOnly: false, main: opened) {
                    try openFile(domain, environment: environment)
                }
            }
        } catch { try? opened.close(); throw error }
        processes[process] = opened; currentProcess = process
        try settleNetwork()
    }
    func handle(_ domain: Domain) throws -> Database {
        domain.name == "main" ? database : try database.domains.handle(SQLText(domain.name))
    }
    func closeProcess(_ process: Int, crash: Bool) throws {
        guard let database = processes[process] else { return }
        if crash {
            for domain in orderedDomains {
                if ownerProcess(domain) == process { domain.ownerLost = true }
                if let handle = try? handle(domain, process: process) { handle.core.storage.pager.revoke() }
            }
            // Model kernel EOF before releasing logical handles. Real protocol
            // readers release their pins and pending writer admissions first.
            try killNetwork(process)
        }
        for name in sessions.keys.sorted(by: SimText.less) where sessions[name]!.process == process {
            sessions.removeValue(forKey: name)!.session.close()
        }
        var failure: Error?
        do { try database.close() } catch { if !crash { failure = error } }
        processes.removeValue(forKey: process)
        try releaseProcessHost(process)
        if let next = processes.keys.min() { currentProcess = next }
        if let failure { throw failure }
    }
    func emit(_ operation: String, _ fields: String = "") {
        transcript += "{\"i\":\(stepIndex),\"op\":\(SimText.quote(operation))\(fields)}\n"
    }
    func code(_ error: Error) throws -> String {
        guard let error = error as? JedError else { throw error }; return error.code
    }
    func reportError(_ operation: String, fields: String = "", _ error: Error) throws {
        let code = try code(error)
        emit(operation, fields + ",\"error\":" + SimText.quote(code))
        if code == "XX001" { throw SimViolation("corruption: \(operation) reported XX001: \(error)") }
    }
    func versions(_ session: Session) throws -> [String: UInt64] {
        try session.withToolingDomains { bundle in
            Dictionary(uniqueKeysWithValues: bundle.snapshots.filter { $0.key != DomainNames.temporary }.map { ($0.key.description, $0.value.txid) })
        }
    }
    func versionsField(_ versions: [String: UInt64]) -> String {
        let names = domains.keys.filter { $0 != "main" }.sorted(by: SimText.less)
        guard !names.isEmpty else { return "" }
        return ",\"dbs\":{" + names.compactMap { name in versions[name].map { SimText.quote(name) + ":\($0)" } }.joined(separator: ",") + "}"
    }
    func run() -> Result {
        do {
            guard trace.header["processes"] == nil || service else { throw SimViolation("unsupported processes mode") }
            let main = Domain("main", key: encrypted ? 0 : -1); domains["main"] = main
            try create(main); try openProcess(0)
            main.version = database.version; main.floor = main.version
            main.digest = try committedDigest(main); main.remember(main.version, main.digest); try observeGrowth(main)
            for index in trace.steps.indices {
                stepIndex = index; progress?(index, trace.steps[index].line, true)
                try step(trace.steps[index]); progress?(index, trace.steps[index].line, false)
            }
        } catch {
            violationStep = stepIndex
            let prefix = stepIndex < 0 ? "setup: " : "step \(stepIndex) (line \(trace.steps[stepIndex].line)): "
            violation = prefix + String(describing: error)
        }
        for domain in orderedDomains {
            if let facts = filesystem.facts(domain.file) { images[domain.file] = facts.cache }
        }
        // Cleanup is outside the transcript/hash result and must not make a
        // failed trace durable. Revoke before releasing host ownership.
        let hash = filesystem.operationsHash
        for process in processes.keys.sorted() { try? closeProcess(process, crash: true) }
        return Result(transcript: transcript, operationsHash: hash, violation: violation, violationStep: violationStep, images: images)
    }
    func step(_ step: SimTrace.Step) throws {
        switch step.kind {
        case "fault": armed = try step.fault(); emit("fault", ",\"kind\":" + SimText.quote(step.mode)); return
        case "clock": try clock.advance(Int64(step.parameters["delta"]!)!); emit("clock"); return
        case "tick": emit("tick"); return
        case "crash":
            filesystem.beginStep(UInt64(stepIndex))
            try recover(power: step.mode == "power", survive: Int(step.parameters["survive"] ?? "0") ?? 0,
                        tear: Int(step.parameters["tear"] ?? "0") ?? 0, fired: "none"); return
        case "disconnect": try disconnectProcess(step.process); return
        case "exit", "kill":
            let field = ",\"pid\":\(step.process)"
            guard processes[step.process] != nil else { emit(step.kind, field + ",\"skip\":true"); return }
            try closeProcess(step.process, crash: step.kind == "kill")
            if step.kind == "kill", processes.isEmpty {
                filesystem.beginStep(UInt64(stepIndex)); try recover(power: false, survive: 0, tear: 0, fired: "none")
            } else { emit(step.kind, field) }
            return
        case "corrupt": try corrupt(step); return
        case "replay": try replay(step.mode); return
        case "restore": try restore(Int(step.parameters["j"]!)!); return
        default: break
        }
        if processes.isEmpty, step.kind != "spawn" {
            filesystem.beginStep(UInt64(stepIndex)); try spawn(0)
            guard !processes.isEmpty else { throw SimViolation("p0 could not restart") }
        }
        let fault = armed; armed = nil; currentFault = fault
        startingVersions = domains.mapValues(\.version)
        startingOwners = Dictionary(uniqueKeysWithValues: orderedDomains.compactMap { domain in
            ownerProcess(domain).map { (domain.name, $0) }
        })
        for domain in orderedDomains {
            let pager = enginePager(domain)
            let pending = relaxed && pager?.pendingEntries.isEmpty == false
            domain.due = pending && domain.deadline.map { clock.microseconds() >= $0 } == true
            domain.batchOpen = relaxed && pager?.isDurable != true && !domain.due
            domain.batchPending = pending && !domain.due
        }
        if let process = stepProcess(step) {
            if processes[process] != nil { currentProcess = process }
            else if step.kind == "open" { emit("open", ",\"sid\":\(SimText.quote(step.session)),\"skip\":true"); return }
            else if step.kind == "backup" { emit("backup", ",\"pid\":\(process),\"skip\":true"); return }
        }
        filesystem.beginStep(UInt64(stepIndex), fault: fault?.kind == .disconnect ? nil : fault)
        beginNetworkStep(disconnectAt: fault?.kind == .disconnect ? fault!.nth : 0)
        switch step.kind {
        case "spawn": try spawn(step.process)
        case "open": try openSession(step)
        case "on":
            guard let active = sessions[step.session] else { emit("on", ",\"sid\":\(SimText.quote(step.session)),\"skip\":true"); break }
            currentProcess = active.process; random.select(active.random)
            do {
                let result = try SimQueryResult.run(active.session, step.sql)
                emit("on", ",\"sid\":\(SimText.quote(step.session)),\"rows\":\(result.rows.count),\"affected\":\(result.affected),\"hash\":\(SimText.quote(result.hash)),\"cost\":\(result.cost)")
            } catch { try reportError("on", fields: ",\"sid\":" + SimText.quote(step.session), error) }
        case "commit": try commit(step.session)
        case "rollback", "close": try endSession(step.session, operation: step.kind)
        case "check": try check()
        case "reopen": try reopen()
        case "attach": try attach(step.session)
        case "compact", "rekey", "change_key": try maintenance(step)
        case "backup": try backup(step)
        default: throw SimViolation("unimplemented simulation operation: \(step.kind)")
        }
        try settleNetwork(); try updateFloors()
        if let fault {
            if fault.kind == .disconnect {
                emit("disconnected", ",\"fired\":" + SimText.quote(networkDisconnected ? "frame" : "end"))
            } else { try finishFault(fault) }
        }
        for domain in orderedDomains { domain.keyChange = nil }
    }
    func openSession(_ step: SimTrace.Step) throws {
        guard sessions[step.session] == nil else { throw SimViolation("session \(step.session) already exists") }
        guard processes[step.process] != nil else {
            emit("open", ",\"sid\":\(SimText.quote(step.session)),\"skip\":true"); return
        }
        currentProcess = step.process
        if step.mode == "write", let other = sessions.keys.sorted(by: SimText.less).first(where: { sessions[$0]!.write }) {
            throw SimViolation("open writer while \(other) holds the writer gate")
        }
        var session: Session?
        do {
            let opened = try step.mode == "write" ? database.writeSession(options: sessionOptions) : database.readSession(options: sessionOptions)
            session = opened
            let pins = try versions(opened)
            if service {
                for domain in orderedDomains {
                    if let version = pins[domain.name] { try observe(domain, version: version, digest: digest(opened, domain), who: "session " + step.session) }
                }
            }
            sessions[step.session] = OpenSession(opened, write: step.mode == "write", process: step.process, pins: pins,
                seed: SimDecision.value(seed: seed, lane: 8, step: UInt64(stepIndex)))
            emit("open", ",\"sid\":\(SimText.quote(step.session)),\"version\":\(pins["main"]!)" + versionsField(pins))
        } catch {
            session?.close()
            if isStaleKey(error) {
                try reportError("open", fields: ",\"sid\":" + SimText.quote(step.session), error)
                try reopenStale(error)
            } else { throw error }
        }
    }
}
