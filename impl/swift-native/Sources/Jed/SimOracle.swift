import Foundation

extension SimRunner {
    func digest(_ session: Session, _ domain: Domain) throws -> String {
        try filesystem.withOracle {
            let limit = session.maxQueryMemoryBytes; session.maxQueryMemoryBytes = 0
            defer { session.maxQueryMemoryBytes = limit }
            let qualifier = domain.name == "main" ? "" : domain.name + "."
            var text = "", tables: [String] = []
            func fold(_ sql: String, catalog: Bool = false) throws {
                let result = try SimQueryResult.run(session, sql)
                if catalog { tables = result.firstColumn }
                text += sql + "\n" + result.rows.joined(separator: "\n") + "\n\n"
            }
            for name in ["jed_tables", "jed_columns", "jed_indexes", "jed_constraints", "jed_sequences"] {
                try fold("SELECT * FROM " + qualifier + name, catalog: name == "jed_tables")
            }
            for name in tables.sorted(by: SimText.less) {
                try fold("SELECT * FROM " + qualifier + "\"" + name.replacingOccurrences(of: "\"", with: "\"\"") + "\"")
            }
            return SimText.hex(SimText.hash(text.utf8))
        }
    }
    func emptyDigest(_ domain: Domain) -> String {
        let prefix = domain.name == "main" ? "" : domain.name + "."
        let text = ["jed_tables", "jed_columns", "jed_indexes", "jed_constraints", "jed_sequences"].map {
            "SELECT * FROM " + prefix + $0 + "\n\n\n"
        }.joined()
        return SimText.hex(SimText.hash(text.utf8))
    }
    func committedDigest(_ domain: Domain) throws -> String {
        func read() throws -> String {
            let session = try database.readSession(); defer { session.close() }; return try digest(session, domain)
        }
        do { return try read() }
        catch { try reopenStale(error); return try read() }
    }
    func image(_ domain: Domain, bytes original: [UInt8]? = nil) throws -> (bytes: [UInt8], facts: SimIntegrity.Image) {
        guard let bytes = original ?? filesystem.facts(domain.file)?.cache else { throw SimViolation("database file is missing: \(domain.file)") }
        let facts = try SimIntegrity.image(bytes: bytes, credential: Self.credential(domain.key), environment: database.environment)
        return (bytes, facts)
    }
    func imageFields(_ domain: Domain, versionKey: String, bytes: [UInt8]? = nil) throws -> String {
        let image = try image(domain, bytes: bytes)
        guard image.facts.version == domain.version else { throw SimViolation("integrity image selected an unexpected version of \(domain.name)") }
        let length = Int(image.facts.pageCount) * pageSize
        guard length <= image.bytes.count else { throw SimViolation("selected high-water exceeds image length") }
        return "\"\(versionKey)\":\(domain.version),\"digest\":\(SimText.quote(domain.digest)),\"live\":\(image.facts.livePages),\"pages\":\(image.facts.pageCount),\"free\":\(image.facts.freePages.count),\"image\":\(SimText.quote(SimText.hex(SimText.hash(image.bytes.prefix(length)))))"
    }
    func check() throws {
        var main = "", attachments: [String] = []
        for domain in orderedDomains {
            if service, (try? handle(domain)) == nil { continue }
            let actual = try committedDigest(domain), version = try handle(domain).version
            if service { try observe(domain, version: version, digest: actual, who: "check") }
            else if version != domain.version || actual != domain.digest { throw SimViolation("committed view of \(domain.name) changed without publication") }
            let fields = try imageFields(domain, versionKey: "version")
            if domain.name == "main" { main = fields }
            else { attachments.append(SimText.quote(domain.name) + ":{" + fields + "}") }
            try checkLive(domain)
        }
        emit("check", "," + main + databaseFields(attachments))
    }
    func databaseFields(_ entries: [String]) -> String { entries.isEmpty ? "" : ",\"dbs\":{" + entries.joined(separator: ",") + "}" }
    func commit(_ name: String) throws {
        let sid = ",\"sid\":" + SimText.quote(name)
        guard let active = sessions.removeValue(forKey: name) else { emit("commit", sid + ",\"skip\":true"); return }
        defer { active.session.close() }; currentProcess = active.process
        var before: [String: String] = [:]
        for domain in orderedDomains where active.pins[domain.name] != nil { before[domain.name] = try? digest(active.session, domain) }
        do { try active.session.commit() }
        catch {
            try reportError("commit", fields: sid, error)
            for domain in orderedDomains where active.pins[domain.name] != nil {
                guard try handle(domain).version == domain.version else { throw SimViolation("failed commit published \(domain.name)") }
                if filesystem.facts(domain.file)?.writes ?? 0 > 0, let digest = before[domain.name] { domain.remember(domain.version + 1, digest) }
            }
            return
        }
        var versions: [String: UInt64] = [:]
        for domain in orderedDomains { versions[domain.name] = (try? handle(domain).version) ?? 0 }
        let roles = Dictionary(uniqueKeysWithValues: orderedDomains.map { ($0.name, roleName($0)) })
        emit("commit", sid + ",\"version\":\(versions["main"]!)" + versionsField(versions) +
            (service ? ",\"role\":" + SimText.quote(roles["main"]!) : ""))
        let quiet = sessions.isEmpty && !service && currentFault == nil
        for domain in orderedDomains where active.pins[domain.name] != nil {
            let base = domain.version, now = versions[domain.name]!
            if now == domain.version {
                if let value = before[domain.name], value != domain.digest { throw SimViolation("commit omitted changes to \(domain.name)") }
                continue
            }
            guard now == domain.version + 1, let expected = before[domain.name] else { throw SimViolation("invalid committed generation for \(domain.name)") }
            let published: String
            do { published = try committedDigest(domain) }
            catch {
                if relaxed && !filesystem.fired.isEmpty || service && filesystem.isDead { published = expected }
                else if service, (error as? JedError)?.code == "08006" { published = try committedDigest(domain) }
                else { throw error }
            }
            guard published == expected else { throw SimViolation("committed \(domain.name) differs from its working snapshot") }
            domain.version = now; domain.digest = expected; domain.remember(now, expected)
            if !relaxed { domain.floor = now }
            if service, roles[domain.name] == "client", domain.due {
                domain.due = false; domain.batchOpen = true; domain.batchPending = true
            }
            if !service, filesystem.fired.isEmpty { try checkGrowth(domain, quiet: quiet) }
            try serviceCommitted(domain, base: base, now: now, digest: expected, client: roles[domain.name] == "client")
        }
        if filesystem.fired.isEmpty && !filesystem.isDead {
            for domain in orderedDomains where (try? handle(domain)) != nil { try checkLive(domain) }
        }
    }
    func endSession(_ name: String, operation: String) throws {
        let sid = ",\"sid\":" + SimText.quote(name)
        guard let active = sessions.removeValue(forKey: name) else { emit(operation, sid + ",\"skip\":true"); return }
        defer { active.session.close() }; currentProcess = active.process
        if operation == "rollback" {
            do { try active.session.rollback() }
            catch { try reportError(operation, fields: sid, error); return }
        } else if !active.write {
            for domain in orderedDomains {
                guard let pin = active.pins[domain.name] else { continue }
                do {
                    let actual = try digest(active.session, domain)
                    guard domain.history[pin]?.contains(actual) == true else { throw SimViolation("reader \(name) lost pinned version \(pin) of \(domain.name)") }
                } catch {
                    if service, (error as? JedError)?.code == "08006" {
                        emit(operation, sid + ",\"lost\":true"); return
                    }
                    throw error
                }
            }
        }
        emit(operation, sid)
    }
    func verifyRecovered(_ domain: Domain) throws {
        let now = try handle(domain).version, actual = try committedDigest(domain)
        guard now >= domain.floor else { throw SimViolation("durability: \(domain.name) recovered \(now), below durable floor \(domain.floor)") }
        guard now <= domain.attempted else { throw SimViolation("\(domain.name) recovered a generation never attempted") }
        guard domain.history[now]?.contains(actual) == true else { throw SimViolation("\(domain.name) recovered contents no transaction produced at version \(now)") }
        domain.version = now; domain.digest = actual; domain.ownerLost = false
    }
    func settleKey(_ domain: Domain, bytes: [UInt8]) throws {
        guard encrypted else { return }
        func accepts(_ key: Int) -> Bool {
            (try? Pager.open(store: SimText.store(bytes), credential: Self.credential(key), writable: false)) != nil
        }
        if let change = domain.keyChange {
            let old = accepts(change.old), next = accepts(change.next), acknowledged = domain.key == change.next
            if change.old != change.next {
                if old && !next && !acknowledged { domain.key = change.old }
                else if !old && next { domain.key = change.next }
                else if !(change.entryPending && !old && !next) { throw SimViolation("credential change did not recover exactly one permitted key") }
                else { try followKey(domain, accepts: accepts) }
            } else if !next { throw SimViolation("rotated data key cannot reopen \(domain.name)") }
            emit("unlocked", (domain.name == "main" ? "" : ",\"db\":" + SimText.quote(domain.name)) + ",\"key\":\(domain.key)")
        } else { try followKey(domain, accepts: accepts) }
        domain.keyChange = nil
    }
    private func followKey(_ domain: Domain, accepts: (Int) -> Bool) throws {
        if accepts(domain.key) { return }
        for key in -1..<4 where key != domain.key {
            if accepts(key) {
                domain.key = key
                emit("unlocked", (domain.name == "main" ? "" : ",\"db\":" + SimText.quote(domain.name)) + ",\"key\":\(key)"); return
            }
        }
        throw SimViolation("no simulation credential opens \(domain.name)")
    }
    func recover(power: Bool, survive: Int, tear: Int, fired: String) throws {
        for process in processes.keys.sorted() { try closeProcess(process, crash: true) }
        if power { filesystem.powerLoss(step: UInt64(stepIndex), survive: survive, tear: tear) }
        filesystem.processDeath()
        for domain in orderedDomains {
            guard let bytes = filesystem.facts(domain.file)?.cache else {
                guard domain.name != "main", domain.floor == 0 else { throw SimViolation("durable database \(domain.name) disappeared") }
                domains.removeValue(forKey: domain.name); continue
            }
            images["step\(stepIndex)" + (domain.name == "main" ? "" : "." + domain.name) + ".img"] = bytes
            try settleKey(domain, bytes: bytes)
        }
        try openProcess(0)
        var main = "", attachments: [String] = []
        for domain in orderedDomains {
            try verifyRecovered(domain); try observeGrowth(domain)
            // Open may repair divergent credential blocks. The crash transcript
            // and independent integrity check describe the saved crash image,
            // exactly the bytes emitted before opening the replacement process.
            let key = "step\(stepIndex)" + (domain.name == "main" ? "" : "." + domain.name) + ".img"
            guard let bytes = images[key] else { throw SimViolation("recovered crash image is missing") }
            let fields = try imageFields(domain, versionKey: "recovered", bytes: bytes)
            if domain.name == "main" { main = fields }
            else { attachments.append(SimText.quote(domain.name) + ":{" + fields + "}") }
        }
        emit("crash", ",\"fired\":" + SimText.quote(fired) + "," + main + databaseFields(attachments))
    }
    func reopen() throws {
        for process in processes.keys.sorted() { try closeProcess(process, crash: false) }
        for domain in orderedDomains {
            if let facts = filesystem.facts(domain.file), facts.pendingOffsets.isEmpty, filesystem.entryDurable(domain.file), !domain.ownerLost { domain.floor = max(domain.floor, domain.version) }
        }
        filesystem.processDeath(); try openProcess(0)
        var main = "", attachments: [String] = []
        for domain in orderedDomains {
            if service, domain.name != "main", domain.floor == 0, filesystem.facts(domain.file) == nil {
                domains.removeValue(forKey: domain.name); continue
            }
            let actual = try committedDigest(domain), version = try handle(domain).version
            if service { try observe(domain, version: version, digest: actual, who: "reopened p0") }
            else if version != domain.version || actual != domain.digest { throw SimViolation("clean reopen changed \(domain.name)") }
            try observeGrowth(domain)
            let fields = "\"version\":\(domain.version),\"digest\":" + SimText.quote(domain.digest)
            if domain.name == "main" { main = fields }
            else { attachments.append(SimText.quote(domain.name) + ":{" + fields + "}") }
        }
        emit("reopen", "," + main + databaseFields(attachments))
    }
    func observeGrowth(_ domain: Domain) throws {
        guard let pager = enginePager(domain) else { return }
        let state = try pager.allocationSnapshot(reuseAllowed: true)
        domain.high = state.highWater; domain.free = UInt32(state.persistedFree.count); domain.trail = state.highWater
        for page in state.persistedFree.reversed() {
            guard page + 1 == domain.trail else { break }; domain.trail = page
        }
        domain.liveBound = domain.high - domain.free; domain.reclaimed = false
    }
    func checkGrowth(_ domain: Domain, quiet: Bool) throws {
        let storage = try handle(domain).core.storage, meta = storage.pager.selected.meta
        let allocation = try storage.pager.allocationSnapshot(reuseAllowed: true)
        let pages = meta.pageCount, free = UInt32(allocation.persistedFree.count), live = meta.livePages
        let writtenPages = filesystem.facts(domain.file)!.pages.filter { $0 >= 2 }
        let written = UInt32(writtenPages.count), top = UInt32((writtenPages.max() ?? -1) + 1)
        if quiet {
            let expected = max(domain.batchPending ? domain.high : domain.trail, top)
            guard pages == expected else { throw SimViolation("trailing truncation: \(domain.name) has \(pages) pages, expected \(expected)") }
            if pages < domain.high {
                if relaxed { domain.shrinkDue = true }
                else { try checkLength(domain, pages: pages) }
            }
        }
        var trail = pages
        for page in allocation.persistedFree.reversed() { guard page + 1 == trail else { break }; trail = page }
        defer { domain.high = pages; domain.free = free; domain.trail = trail; domain.liveBound = max(domain.liveBound, live + written) }
        if domain.batchOpen { domain.reclaimed = false; return }
        if quiet && domain.reclaimed && pages > domain.previousHigh {
            guard pages - domain.previousHigh <= written - min(written, domain.previousFree) else { throw SimViolation("bounded growth failed to reuse the prior free list") }
        }
        let threshold = max(UInt64(GeneratedSpec.compactMinimumPages), UInt64(GeneratedSpec.compactGrowth) * UInt64(domain.liveBound))
        domain.reclaimed = quiet && UInt64(domain.high - domain.free) > threshold
        if domain.reclaimed {
            guard UInt64(pages) >= 2 + UInt64(live) + UInt64(free), UInt64(pages) - 2 - UInt64(live) - UInt64(free) <= UInt64(written) else {
                throw SimViolation("bounded growth left unreachable pages outside the free list")
            }
        }
        domain.previousHigh = pages; domain.previousFree = free
    }
    func checkLength(_ domain: Domain, pages: UInt32) throws {
        let limit = (UInt64(pages) + UInt64(Pager.growthStep(pages: pages, pageSize: pageSize))) * UInt64(pageSize)
        guard UInt64(filesystem.facts(domain.file)!.cache.count) <= limit else { throw SimViolation("trailing truncation left more than one growth quantum") }
    }
    func updateFloors() throws {
        for domain in orderedDomains {
            guard let facts = filesystem.facts(domain.file) else { continue }
            if domain.due, domain.version > startingVersions[domain.name, default: domain.version], filesystem.fired.isEmpty,
               !facts.barrierComplete { throw SimViolation("time-based flush: commit passed its batch deadline without a barrier") }
            if facts.barrierComplete, !domain.batchOpen { domain.floor = max(domain.floor, startingVersions[domain.name] ?? domain.version) }
            let pager = enginePager(domain)
            if relaxed, !filesystem.isDead, filesystem.fired.isEmpty, pager?.isDurable == true, filesystem.entryDurable(domain.file) {
                guard !facts.pendingOffsets.contains(where: { $0 < pageSize * 2 }) else { throw SimViolation("engine claimed durability with pending meta bytes") }
                domain.floor = max(domain.floor, domain.version)
            }
            // A pending batch must never truncate the durable fallback, even
            // when its own selected generation has a lower live high-water.
            for slot in 0...1 where facts.cache.count >= (slot + 1) * pageSize {
                if let meta = try? MetaPage.decode(Array(facts.cache[(slot * pageSize)..<((slot + 1) * pageSize)])), meta.txid == domain.floor,
                   UInt64(meta.pageCount) * UInt64(pageSize) > UInt64(facts.cache.count) {
                    throw SimViolation("trailing truncation shortened \(domain.name) below durable generation \(domain.floor)")
                }
            }
            if relaxed, flushInterval > 0 {
                if pager?.pendingEntries.isEmpty != false { domain.deadline = nil }
                else if !domain.batchPending || domain.deadline == nil {
                    let interval = flushInterval > UInt64(Int64.max / 1000) ? Int64.max : Int64(flushInterval) * 1000
                    let (deadline, overflow) = clock.microseconds().addingReportingOverflow(interval)
                    domain.deadline = overflow ? Int64.max : deadline
                }
            }
            if domain.shrinkDue, !filesystem.isDead, filesystem.fired.isEmpty, let pager, pager.isDurable {
                try checkLength(domain, pages: pager.selected.meta.pageCount); domain.shrinkDue = false
            }
        }
    }
}
