import Foundation

extension SimRunner {
    func attach(_ name: String) throws {
        if let writer = sessions.keys.sorted(by: SimText.less).first(where: { sessions[$0]!.write }) {
            throw SimViolation("attach while \(writer) holds the writer gate")
        }
        if let domain = domains[name], processes.keys.allSatisfy({ process in (try? handle(domain, process: process)) != nil }) {
            emit("attach", ",\"db\":\(SimText.quote(name)),\"version\":\(domain.version)"); return
        }
        let domain = domains[name] ?? Domain(name, key: encrypted ? 0 : -1); domains[name] = domain
        do {
            if filesystem.facts(domain.file) == nil {
                domain.history = [:]; domain.version = 1; domain.floor = 0; domain.attempted = 1
                domain.digest = emptyDigest(domain); domain.remember(1, domain.digest)
                try create(domain); domain.floor = 1
            }
            if service { try attachEverywhere(domain) }
            else {
                try database.domains.attach(SQLText(name), readOnly: false, main: database) {
                    try openFile(domain, environment: database.environment)
                }
            }
            try verifyRecovered(domain); try observeGrowth(domain)
            emit("attach", ",\"db\":\(SimText.quote(name)),\"version\":\(domain.version)")
        } catch { try reportError("attach", fields: ",\"db\":" + SimText.quote(name), error) }
    }
    func maintenance(_ step: SimTrace.Step) throws {
        let name = step.session, target = step.mode == "plain" ? -1 : Int(step.mode) ?? 0
        let field = name == "main" ? "" : ",\"db\":" + SimText.quote(name)
        guard let domain = domains[name], (try? handle(domain)) != nil else {
            emit(step.kind, ",\"db\":\(SimText.quote(name)),\"skip\":true"); return
        }
        if step.kind != "compact", !encrypted { emit(step.kind, ",\"skip\":true"); return }
        // Clients may retain an older resident view until their next begin.
        // Credential changes preserve that local view; failed replacement may
        // lag the observed owner generation but must never advance past it.
        let base = step.kind == "change_key" ? try handle(domain).version : domain.version
        if step.kind != "change_key" { domain.remember(base + 1, domain.digest) }
        if step.kind != "compact" { domain.keyChange = (domain.key, target, !filesystem.entryDurable(domain.file)) }
        do {
            switch step.kind {
            case "compact": try database.compact(name)
            case "change_key": try database.changeCredential(name, credential: Self.credential(target)!)
            case "rekey": try database.rekey(name, credential: Self.credential(target))
            default: throw SimViolation("unknown maintenance operation")
            }
        } catch {
            try reportError(step.kind, fields: field, error)
            let now = try handle(domain).version
            guard step.kind == "change_key" ? now == base : (now == base || service && now < base) else {
                throw SimViolation("failed maintenance published a generation")
            }
            return
        }
        if step.kind != "compact" { domain.key = target }
        let now = try handle(domain).version
        if step.kind == "change_key" {
            guard now == base else { throw SimViolation("credential change altered the generation") }
            emit(step.kind, field + ",\"key\":\(target)"); return
        }
        guard now == base + 1, try committedDigest(domain) == domain.digest else { throw SimViolation("maintenance changed logical contents or generation incorrectly") }
        domain.version = now; domain.floor = now; try observeGrowth(domain)
        try checkLive(domain)
        emit(step.kind, field + ",\"version\":\(now)")
    }
    func backup(_ step: SimTrace.Step) throws {
        backupNumber += 1
        let main = domains["main"]!, destination = main.file + ".backup\(backupNumber)"
        let field = ",\"form\":" + SimText.quote(step.mode)
        do { try database.backup(destination, form: step.mode == "page_identical" ? .pageIdentical : .compacted) }
        catch {
            try reportError("backup", fields: field, error)
            guard filesystem.facts(destination) == nil else { throw SimViolation("failed backup left its destination") }; return
        }
        guard let bytes = filesystem.facts(destination)?.cache else { throw SimViolation("successful backup left no destination") }
        let facts = try SimIntegrity.image(bytes: bytes, credential: Self.credential(main.key), environment: database.environment)
        let copy = try inspectionDatabase(bytes, credential: Self.credential(main.key), environment: database.environment)
        let session = try copy.readSession(); defer { session.close(); try? copy.close() }
        let actual = try digest(session, main), snapshot = facts.version - (step.mode == "compacted" ? 1 : 0)
        guard snapshot >= main.version || main.ownerLost && snapshot >= main.floor, main.history[snapshot]?.contains(actual) == true else {
            throw SimViolation("backup differs from the newest committed snapshot")
        }
        if main.ownerLost { main.version = snapshot; main.digest = actual; main.ownerLost = false }
        backups.append(Backup(number: backupNumber, file: destination, version: facts.version, digest: actual, key: main.key))
        images["step\(stepIndex).backup.img"] = bytes
        emit("backup", field + ",\"version\":\(facts.version),\"digest\":\(SimText.quote(actual)),\"live\":\(facts.livePages),\"pages\":\(facts.pageCount),\"free\":\(facts.freePages.count),\"image\":\(SimText.quote(SimText.hex(SimText.hash(bytes.prefix(Int(facts.pageCount) * pageSize)))))")
    }
    func inspectionDatabase(_ bytes: [UInt8], credential: Credential?, environment: Environment) throws -> Database {
        let storage = try StorageDomain.open(store: SimText.store(bytes), catalogID: Database.allocateCatalogID(),
            credential: credential, readOnly: true, collatedKey: { try environment.unicode.key($0, collation: $1) },
            collationVerdict: { try CatalogEnvironment.verdict($0, environment: environment) },
            validateCatalog: { try CatalogEnvironment.validate($0, environment: environment) })
        return Database(core: DatabaseCore(storage: storage), environment: environment)
    }
    func closeAllClean() throws {
        for process in processes.keys.sorted() { try closeProcess(process, crash: false) }
        for domain in orderedDomains {
            if let facts = filesystem.facts(domain.file), facts.pendingOffsets.isEmpty, filesystem.entryDurable(domain.file), !domain.ownerLost {
                domain.floor = max(domain.floor, domain.version)
            }
        }
        filesystem.processDeath()
    }
    func openRepaired() throws {
        try openProcess(0)
        for domain in orderedDomains {
            guard try handle(domain).version == domain.version, try committedDigest(domain) == domain.digest else {
                throw SimViolation("repair did not restore the original generation and contents")
            }
            try observeGrowth(domain)
        }
    }
    func corrupt(_ step: SimTrace.Step) throws {
        filesystem.beginStep(UInt64(stepIndex)); try closeAllClean()
        let main = domains["main"]!, disk = try filesystem.open(main.file)
        let original = disk.facts.cache
        let environment = inspectionEnvironment()
        let facts = try SimIntegrity.image(bytes: original, credential: Self.credential(main.key), environment: environment)
        let reseal = step.parameters["reseal"] == "1"
        let changed = try disk.corrupt(step: UInt64(stepIndex), flips: Int(step.parameters["flips"] ?? "0") ?? 0,
            pageCount: facts.pageCount, reseal: reseal)
        var opened = "ok", read = "-"
        func outcome(_ error: Error) throws -> String {
            let result = try code(error)
            guard reseal || result == "XX001" else { throw SimViolation("corruption surfaced as \(result), expected XX001") }; return result
        }
        do { try openProcess(0) }
        catch { opened = try outcome(error) }
        if !processes.isEmpty {
            read = "ok"
            do {
                let version = database.version, actual = try committedDigest(main)
                if !reseal {
                    guard main.history[version]?.contains(actual) == true,
                          version == main.version || version + 1 == main.version else { throw SimViolation("corrupt image returned unproduced contents") }
                }
            } catch { read = try outcome(error) }
            try closeProcess(0, crash: true)
        }
        disk.restorePages(changed); disk.close(); filesystem.processDeath(); try openRepaired()
        emit("corrupt", ",\"flipped\":\(changed.count),\"open\":\(SimText.quote(opened)),\"read\":\(SimText.quote(read))")
    }
    /// Oracle-only environment keeps entropy/opening ordinals untouched.
    func inspectionEnvironment() -> Environment {
        var builder = Environment.builder(); builder.setRandomSource(random); builder.setClockSource(clock); return builder.build()
    }
    func replay(_ kind: String) throws {
        filesystem.beginStep(UInt64(stepIndex)); try closeAllClean()
        let main = domains["main"]!, disk = try filesystem.open(main.file)
        let prefix = ",\"kind\":" + SimText.quote(kind)
        if main.key < 0 { try openRepaired(); emit("replay", prefix + ",\"skip\":true"); return }
        let original = disk.facts.cache, environment = inspectionEnvironment()
        let facts = try SimIntegrity.image(bytes: original, credential: Self.credential(main.key), environment: environment)
        let candidates = (facts.pointerKinds[kind] ?? []).sorted().compactMap { page -> (UInt32, [[UInt8]])? in
            let versions = disk.replayVersions(page); return versions.isEmpty ? nil : (page, versions)
        }
        guard !candidates.isEmpty else { try openRepaired(); emit("replay", prefix + ",\"skip\":true"); return }
        let candidate = candidates[Int(SimDecision.value(seed: seed, lane: 15, step: UInt64(stepIndex)) % UInt64(candidates.count))]
        let older = candidate.1[Int(SimDecision.value(seed: seed, lane: 16, step: UInt64(stepIndex)) % UInt64(candidate.1.count))]
        let other = (0...1).compactMap { slot -> UInt64? in
            guard let meta = try? MetaPage.decode(Array(original[(slot * pageSize)..<((slot + 1) * pageSize)])),
                  meta.txid < main.version else { return nil }; return meta.txid
        }.max()
        let changed = try disk.replay(candidate.0, bytes: older)
        var result = "", recovered: UInt64 = 0
        do {
            try openProcess(0); recovered = database.version
            let actual = try committedDigest(main)
            if recovered == main.version && actual == main.digest && !facts.protectedPages.contains(candidate.0) {
                _ = try SimIntegrity.image(bytes: disk.facts.cache, credential: Self.credential(main.key), environment: environment)
                throw SimViolation("bound-pointer replay went undetected")
            }
            guard recovered == other, recovered < main.version, facts.protectedPages.contains(candidate.0), main.history[recovered]?.contains(actual) == true else {
                throw SimViolation("replay returned an invalid generation or contents")
            }
            _ = try SimIntegrity.image(bytes: disk.facts.cache, credential: Self.credential(main.key), environment: environment)
            result = "fallback"
        } catch {
            guard try code(error) == "XX001" else { throw error }; result = "XX001"
        }
        for process in processes.keys.sorted() { try closeProcess(process, crash: true) }
        filesystem.processDeath(); disk.restorePages([changed]); disk.close(); try openRepaired()
        emit("replay", prefix + ",\"page\":\(candidate.0),\"outcome\":\(SimText.quote(result))" + (result == "fallback" ? ",\"version\":\(recovered)" : ""))
    }
    func restore(_ ordinal: Int) throws {
        filesystem.beginStep(UInt64(stepIndex)); try closeAllClean()
        guard !backups.isEmpty else { try openRepaired(); emit("restore", ",\"skip\":true"); return }
        let backup = backups[max(0, backups.count - ordinal)]
        guard filesystem.facts(backup.file) != nil else { try openRepaired(); emit("restore", ",\"skip\":true"); return }
        let main = domains["main"]!
        try filesystem.restore(backup.file, to: main.file)
        main.history = main.history.filter { $0.key < backup.version }; main.remember(backup.version, backup.digest)
        main.version = backup.version; main.digest = backup.digest; main.floor = backup.version; main.attempted = backup.version
        main.key = backup.key; main.keyChange = nil; main.shrinkDue = false; main.ownerLost = false
        try openRepaired()
        emit("restore", ",\"backup\":\(backup.number),\"version\":\(backup.version),\"digest\":\(SimText.quote(backup.digest))")
    }
}
