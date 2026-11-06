import Foundation

extension SimRunner {
    #if os(Linux)
    func platform(_ process: Int) -> SimServicePlatform {
        if let platform = platforms[process] { return platform }
        let platform = SimServicePlatform(world: serviceWorld, process: process,
            scratchSeed: SimDecision.value(seed: seed, lane: 11, step: openings, offset: UInt64(process)))
        platforms[process] = platform; return platform
    }
    #endif
    func settleNetwork() throws {
        #if os(Linux)
        if service { try serviceWorld.network.settle() }
        #endif
    }
    func killNetwork(_ process: Int) throws {
        #if os(Linux)
        if service { serviceWorld.network.kill(process); try settleNetwork() }
        #endif
    }
    func releaseProcessHost(_ process: Int) throws {
        #if os(Linux)
        if service { serviceWorld.kill(process); try settleNetwork() }
        platforms.removeValue(forKey: process)
        #endif
    }
    func beginNetworkStep(disconnectAt: Int) {
        #if os(Linux)
        if service { serviceWorld.network.beginStep(disconnectAt: disconnectAt) }
        #endif
    }
    var networkDisconnected: Bool {
        #if os(Linux)
        return service && serviceWorld.network.disconnected
        #else
        return false
        #endif
    }
    func handle(_ domain: Domain, process: Int) throws -> Database {
        guard let database = processes[process] else { throw SimViolation("process p\(process) is absent") }
        return domain.name == "main" ? database : try database.domains.handle(SQLText(domain.name))
    }
    func ownerProcess(_ domain: Domain) -> Int? {
        guard service else { return processes[currentProcess] != nil ? currentProcess : processes.keys.min() }
        #if os(Linux)
        return processes.keys.sorted().first { process in
            guard let handle = try? handle(domain, process: process) else { return false }
            return (handle.host as? ServiceDatabaseHost)?.toolingRole == "owner"
        }
        #else
        return nil
        #endif
    }
    func enginePager(_ domain: Domain) -> Pager? {
        guard let owner = ownerProcess(domain) else { return nil }
        return try? handle(domain, process: owner).core.storage.pager
    }
    func roleName(_ domain: Domain) -> String {
        guard service, let handle = try? handle(domain) else { return "owner" }
        #if os(Linux)
        return (handle.host as? ServiceDatabaseHost)?.toolingRole ?? "owner"
        #else
        return "owner"
        #endif
    }
    func checkLive(_ domain: Domain) throws {
        let opened = try handle(domain)
        if service, roleName(domain) != "owner" { return }
        try filesystem.withOracle { try SimIntegrity.live(opened.core.storage) }
    }
    func stepProcess(_ step: SimTrace.Step) -> Int? {
        switch step.kind {
        case "open", "spawn", "exit", "kill": return step.process
        case "backup" where step.process >= 0: return step.process
        case "on", "commit", "rollback", "close": return sessions[step.session]?.process
        default: return processes.keys.min()
        }
    }
    func disconnectProcess(_ process: Int) throws {
        #if os(Linux)
        let field = ",\"pid\":\(process)"
        guard service, let database = processes[process],
              (database.host as? ServiceDatabaseHost)?.toolingRole == "client",
              let endpoint = serviceWorld.network.owner(domains["main"]!.file)?.endpoint else {
            emit("disconnect", field + ",\"skip\":true"); return
        }
        serviceWorld.network.disconnect(process, endpoint: endpoint)
        try settleNetwork(); emit("disconnect", field)
        #else
        throw SimViolation("page-service simulation is unavailable on this platform")
        #endif
    }
    func spawn(_ process: Int) throws {
        let field = ",\"pid\":\(process)"
        guard processes[process] == nil else { emit("spawn", field + ",\"skip\":true"); return }
        do { try openProcess(process) }
        catch { try reportError("spawn", fields: field, error); return }
        var versions: [String: UInt64] = [:]
        for domain in orderedDomains where (try? handle(domain)) != nil {
            let actual = try committedDigest(domain), version = try handle(domain).version
            try observe(domain, version: version, digest: actual, who: "spawned p\(process)")
            versions[domain.name] = version
        }
        emit("spawn", field + ",\"version\":\(versions["main"]!)" + versionsField(versions))
    }
    func observe(_ domain: Domain, version: UInt64, digest: String, who: String) throws {
        guard version >= domain.version || domain.ownerLost && version >= domain.floor else {
            throw SimViolation("\(who) sees \(domain.name) version \(version) after \(domain.version) was published")
        }
        guard version <= domain.attempted, domain.history[version]?.contains(digest) == true else {
            throw SimViolation("\(who) sees \(domain.name) contents never produced at version \(version)")
        }
        domain.version = version; domain.digest = digest; domain.ownerLost = false
    }
    func attachEverywhere(_ domain: Domain) throws {
        let first: Int
        if let owner = ownerProcess(domains["main"]!) { first = owner }
        else {
            first = processes.keys.min()!; currentProcess = first
            let main = domains["main"]!, actual = try committedDigest(main)
            try observe(main, version: database.version, digest: actual, who: "attaching p\(first)")
            try settleNetwork()
        }
        defer { currentProcess = first }
        let order = [first] + processes.keys.sorted().filter { $0 != first }
        for process in order {
            currentProcess = process
            if (try? handle(domain)) != nil { continue }
            let opened = database
            try opened.domains.attach(SQLText(domain.name), readOnly: false, main: opened) {
                try openFile(domain, environment: opened.environment)
            }
            try settleNetwork()
        }
    }
    func isStaleKey(_ error: Error) -> Bool {
        guard service, encrypted, let error = error as? JedError else { return false }
        return error.code == "28P04" || error.code == "28P02"
    }
    func reopenStale(_ error: Error) throws {
        guard isStaleKey(error) else { throw error }
        let process = currentProcess
        try closeProcess(process, crash: false); try openProcess(process)
        emit("reopened", ",\"pid\":\(process)")
    }
    func finishFault(_ fault: SimFault) throws {
        let fired = filesystem.fired.isEmpty ? "end" : filesystem.fired
        #if os(Linux)
        if service, fault.kind != .powerLoss, processes.count > 1 {
            var owners = Set(orderedDomains.compactMap(ownerProcess) + startingOwners.values.filter { processes[$0] != nil }).sorted()
            // Owner failure may already have revoked its serving role. The
            // endpoint record identifies the process whose I/O failed; it does
            // not establish ownership or permit engine access.
            if owners.isEmpty, filesystem.isDead {
                owners = Set(orderedDomains.compactMap { domain -> Int? in
                    guard let record = serviceWorld.network.owner(domain.file), processes[Int(record.pid)] != nil else { return nil }
                    return Int(record.pid)
                }).sorted()
            }
            if owners.isEmpty { owners = [currentProcess] }
            if owners.count < processes.count {
                for domain in orderedDomains {
                    if startingOwners[domain.name].map(owners.contains) == true ||
                       serviceWorld.network.owner(domain.file).map({ owners.contains(Int($0.pid)) }) == true { domain.ownerLost = true }
                }
                for owner in owners { try closeProcess(owner, crash: true) }
                filesystem.processDeath()
                for owner in owners { emit("killed", ",\"pid\":\(owner),\"fired\":" + SimText.quote(fired)) }
                for domain in orderedDomains where domain.keyChange != nil {
                    if let bytes = filesystem.facts(domain.file)?.cache { try settleKey(domain, bytes: bytes) }
                }
                return
            }
        }
        #endif
        try recover(power: fault.kind == .powerLoss, survive: fault.survive, tear: fault.tear, fired: fired)
    }
    /// The commit-role oracle reads each meta generation's actual persisted
    /// free-list bytes. It never asks a client to synthesize allocation state.
    func freeList(_ domain: Domain, version: UInt64) -> Set<UInt32>? {
        guard let bytes = filesystem.facts(domain.file)?.cache,
              let pager = try? Pager.open(store: SimText.store(bytes), credential: Self.credential(domain.key), writable: false) else { return nil }
        for slot in 0...1 where bytes.count >= (slot + 1) * pageSize {
            guard let meta = try? MetaPage.decode(Array(bytes[(slot * pageSize)..<((slot + 1) * pageSize)])), meta.txid == version else { continue }
            if let free = try? FreeList.load(meta: meta, protected: [], read: { try pager.readBody($0) }) { return Set(free.entries) }
        }
        return nil
    }
    func serviceCommitted(_ domain: Domain, base: UInt64, now: UInt64, digest: String, client: Bool) throws {
        guard service, client else { return }
        if let before = freeList(domain, version: base), let after = freeList(domain, version: now), !after.isSubset(of: before) {
            throw SimViolation("commit-role rule: client rebuilt \(domain.name)'s free list")
        }
        if filesystem.isDead {
            domain.remember(now + 1, digest); return
        }
        guard let owner = ownerProcess(domain) else { return }
        let version = try handle(domain, process: owner).version
        guard version > now else { return }
        for next in (now + 1)...version { domain.remember(next, digest) }
        domain.version = version
        if !relaxed { domain.floor = version }
        emit("maintenance", (domain.name == "main" ? "" : ",\"db\":" + SimText.quote(domain.name)) + ",\"version\":\(version)")
    }
}
