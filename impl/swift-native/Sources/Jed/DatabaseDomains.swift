import Foundation

public enum AttachSource: Sendable {
    case memory
    case file(String, OpenOptions)
}

internal struct AttachedDomain: Sendable {
    let database: Database
    let readOnly: Bool
}

/// Handle-wide attachment identity and publication. The uncontended resident
/// lock does no file coordination or metadata read. Every attached file retains
/// its ordinary independent owner/client core and host lifetime.
internal final class DatabaseDomains: @unchecked Sendable {
    private let boundary = NSLock()
    private let writer = WriterGate()
    private var attachments: [SQLText: AttachedDomain] = [:]
    private var closed = false

    func withPublication<T>(_ body: () throws -> T) rethrows -> T {
        boundary.lock(); defer { boundary.unlock() }
        return try body()
    }
    private func checkOpen() throws {
        guard !closed else { throw JedError("55000", "database is closed") }
    }
    func handle(_ name: SQLText) throws -> Database {
        try withPublication {
            try checkOpen()
            guard let value = attachments[name] else { throw JedError("42704", "database \"\(name)\" is not attached") }
            return value.database
        }
    }
    private func maintenanceTargetLocked(_ name: SQLText, main: Database) throws -> Database {
        try checkOpen()
        if name == DomainNames.main {
            guard !main.isReadOnly else { throw JedError("25006", "database is read-only") }
            return main
        }
        guard let value = attachments[name] else { throw JedError("42704", "database \"\(name)\" is not attached") }
        guard !value.readOnly else { throw JedError("25006", "database is read-only") }
        return value.database
    }
    /// Preflight/KDF retains the handle without holding either writer gate.
    func maintenanceTarget(_ name: SQLText, main: Database) throws -> Database {
        try withPublication { try maintenanceTargetLocked(name, main: main) }
    }
    /// Recheck the selected identity after preflight, then exclude root writers
    /// and attach/detach while the target's own gate/watermark admits maintenance.
    func withMaintenance<T>(_ name: SQLText, main: Database, expected: Database,
                            _ body: () throws -> T) throws -> T {
        let token = try writer.acquire(wait: false)
        defer { writer.release(token) }
        return try withPublication {
            let current = try maintenanceTargetLocked(name, main: main)
            guard current === expected else { throw JedError("42704", "maintenance target is no longer attached") }
            try current.checkOpen()
            return try body()
        }
    }
    /// Opening is outside the publication lock. Duplicate/refusal cleanup owns
    /// the newly opened handle and cannot disturb an existing attachment.
    func attach(_ name: SQLText, source: AttachSource, readOnly: Bool, main: Database) throws {
        try attach(name, readOnly: readOnly, main: main) {
            switch source {
            case .memory: return try Database(environment: main.environment, pageSize: main.pageSize)
            case .file(let path, var options):
                options.environment = main.environment; options.readOnly = readOnly
                options.synchronous = main.synchronous; options.flushIntervalMilliseconds = main.flushIntervalMilliseconds
                return try Database.open(path, options: options)
            }
        }
    }
    /// First-party host factories supply only the opened handle. Identity,
    /// writer admission, scratch policy and failure cleanup stay shared.
    func attach(_ name: SQLText, readOnly: Bool, main: Database, open: () throws -> Database) throws {
        guard !name.utf8.isEmpty else { throw JedError("42710", "attachment name must not be empty") }
        let token = try writer.acquire(wait: false)
        defer { writer.release(token) }
        try withPublication {
            try checkOpen()
            guard DomainNames.isAttachment(name), attachments[name] == nil else {
                throw JedError("42710", "database \"\(name)\" is already attached or reserved")
            }
        }
        let database = try open()
        do {
            if database.core.storage.pager.codec.reservedBytes != 0,
               let scratch = main.scratchFileHost, !scratch.sealed {
                throw JedError("55000", "encrypted attachments require encrypted scratch storage")
            }
            try withPublication {
                try checkOpen()
                attachments[name] = AttachedDomain(database: database, readOnly: readOnly)
            }
        } catch { try? database.close(); throw error }
    }
    func detach(_ name: SQLText) throws {
        let token = try writer.acquire(wait: false)
        defer { writer.release(token) }
        try withPublication {
            try checkOpen()
            guard let attachment = attachments[name] else {
                throw JedError("42704", "database \"\(name)\" is not attached")
            }
            guard attachment.database.core.livePinCount == 0 else {
                throw JedError("55006", "database \"\(name)\" is in use")
            }
            do { try attachment.database.close() }
            catch {
                if attachment.database.isClosed { attachments.removeValue(forKey: name) }
                throw error
            }
            attachments.removeValue(forKey: name)
        }
    }
    func read(main: Database) throws -> DomainReadBundle {
        try withPublication {
            try checkOpen()
            var pins: [(name: SQLText, pin: SnapshotPin)] = []
            pins.reserveCapacity(attachments.count + 2)
            do {
                // Main is pinned first; the bundle wants name order.
                pins.append((DomainNames.main, try main.core.beginRead()))
                if !attachments.isEmpty {
                    for name in attachments.keys.sorted() { pins.append((name, try attachments[name]!.database.core.beginRead())) }
                    pins.sort { $0.name < $1.name }
                }
                return DomainReadBundle(ordered: pins, database: main)
            } catch { for entry in pins { entry.pin.close() }; throw error }
        }
    }
    /// Export captures one committed target while attachment identity remains
    /// fixed. Its pin survives publication and prevents detach/replacement;
    /// other domains need no read admission or coordination RPC.
    func pinCommitted(_ name: SQLText, main: Database) throws -> SnapshotPin {
        try withPublication {
            try checkOpen()
            let target: Database
            if name == DomainNames.main { target = main }
            else {
                guard let attachment = attachments[name] else {
                    throw JedError("42704", "database \"\(name)\" is not attached")
                }
                target = attachment.database
            }
            try target.checkOpen()
            return try target.core.beginRead()
        }
    }
    func integrityPin(_ name: SQLText, main: Database) throws -> (Database, SnapshotPin) {
        try withPublication {
            try checkOpen()
            let target: Database
            if name == DomainNames.main { target = main }
            else if let attachment = attachments[name] { target = attachment.database }
            else { throw JedError("42704", "database is not attached") }
            try target.checkOpen()
            return (target, try target.core.beginRead())
        }
    }

    /// Provisional catalog routing must not open a reader before waiting for
    /// the writer gate: such a pin can cancel the owner's post-ack maintenance.
    /// These snapshots are for resident metadata only, never row/page access.
    func routing(main: Database) throws -> QueryDomains {
        try withPublication {
            try checkOpen()
            var snapshots = [DomainNames.main: main.core.routingSnapshot]
            for name in attachments.keys.sorted() { snapshots[name] = attachments[name]!.database.core.routingSnapshot }
            return QueryDomains(snapshots: snapshots)
        }
    }
    func acquire(main: Database, cancellation: CancellationToken?, timeoutMilliseconds: UInt64) throws -> DomainWriteBundle {
        let clock = ContinuousClock(), started = clock.now
        let deadline = timeoutMilliseconds == 0 ? nil : started.advanced(by: .milliseconds(Int64(clamping: timeoutMilliseconds)))
        func remaining() throws -> UInt64 {
            guard let deadline else { return 0 }
            let duration = clock.now.duration(to: deadline)
            guard duration > .zero else { throw JedError("55P03", "canceling statement due to lock timeout") }
            let parts = duration.components
            return max(1, UInt64(clamping: parts.seconds) * 1000 + UInt64(clamping: parts.attoseconds / 1_000_000_000_000_000))
        }
        let token = try writer.acquire(cancellation: cancellation, timeoutMilliseconds: timeoutMilliseconds)
        var leases: [SQLText: WriterLease] = [:], reads: [SQLText: SnapshotPin] = [:]
        do {
            let handles = try withPublication {
                try checkOpen()
                var result = attachments
                result[DomainNames.main] = AttachedDomain(database: main, readOnly: main.isReadOnly)
                return result
            }
            guard !main.isReadOnly else { throw JedError("25006", "cannot set transaction read-write mode on a read-only database") }
            let order = handles.keys.sorted { lhs, rhs in
                switch (handles[lhs]!.database.path, handles[rhs]!.database.path) {
                case (.some(let a), .some(let b)): return a == b ? lhs < rhs : a.utf8.lexicographicallyPrecedes(b.utf8)
                case (.some, .none): return true
                case (.none, .some): return false
                case (.none, .none): return lhs < rhs
                }
            }
            for name in order where !handles[name]!.readOnly {
                leases[name] = try handles[name]!.database.core.acquireWriter(cancellation: cancellation,
                    relaxed: !main.synchronous, timeoutMilliseconds: remaining())
            }
            try withPublication {
                for name in handles.keys.sorted() where handles[name]!.readOnly {
                    reads[name] = try handles[name]!.database.core.beginRead()
                }
            }
            return DomainWriteBundle(handles: handles, leases: leases, reads: reads,
                release: { self.writer.release(token) })
        } catch {
            for lease in leases.values { lease.close() }
            for pin in reads.values { pin.close() }
            writer.release(token); throw error
        }
    }
    /// Public close already coordinates the primary host. Taking this token
    /// first makes close/attach/detach fail immediately for an open write block.
    func withClose(main: Database, _ operation: () throws -> Void) throws {
        let token = try writer.acquire(wait: false)
        defer { writer.release(token) }
        try withPublication {
            guard !main.core.hasLocalWriter, attachments.values.allSatisfy({ !$0.database.core.hasLocalWriter }) else {
                throw JedError("55006", "database has an active writer")
            }
            var failure: Error?
            do { try operation() } catch { failure = error }
            // An admission failure leaves every attachment intact. Once main's
            // close consumes its host, finish child cleanup even on I/O error.
            guard main.isClosed else {
                if let failure { throw failure }
                throw JedError("55000", "database close did not release the primary host")
            }
            closed = true
            for name in attachments.keys.sorted() {
                let database = attachments[name]!.database
                do { try database.close() } catch { if failure == nil { failure = error } }
                if database.isClosed { attachments.removeValue(forKey: name) }
            }
            if let failure { throw failure }
        }
    }
}

internal final class DomainWriteBundle {
    let handles: [SQLText: AttachedDomain]
    let leases: [SQLText: WriterLease]
    let reads: [SQLText: SnapshotPin]
    private let release: () -> Void
    private var closed = false
    var initial: QueryDomains {
        QueryDomains(snapshots: reads.mapValues(\.snapshot).merging(leases.mapValues(\.snapshot)) { _, new in new })
    }
    init(handles: [SQLText: AttachedDomain], leases: [SQLText: WriterLease], reads: [SQLText: SnapshotPin], release: @escaping () -> Void) {
        self.handles = handles; self.leases = leases; self.reads = reads; self.release = release
    }
    func checkRole() throws {
        for name in leases.keys.sorted() { try leases[name]!.checkRole() }
        for name in reads.keys.sorted() { try reads[name]!.checkRole() }
    }
    func pin(_ working: QueryDomains, borrowingWriter: Bool = false) throws -> DomainReadBundle {
        var pins: [SQLText: SnapshotPin] = [:]
        do {
            for name in leases.keys.sorted() { pins[name] = try leases[name]!.pin(working.snapshot(name), borrowingWriter: borrowingWriter) }
            for name in reads.keys.sorted() { pins[name] = try reads[name]!.retain() }
            return DomainReadBundle(pins: pins, database: handles[DomainNames.main]?.database)
        } catch { for pin in pins.values { pin.close() }; throw error }
    }
    func requireWritable(_ name: SQLText) throws {
        guard handles[name] != nil else { throw JedError("42P01", "database \"\(name)\" is not attached") }
        guard leases[name] != nil else { throw JedError("25006", "database \"\(name)\" is read-only") }
    }
    func validateDurableChanges(_ working: QueryDomains) throws {
        let changed = leases.keys.filter { name in
            handles[name]!.database.core.storage.kind == .file
                && working.snapshots[name]!.hasChanges(comparedTo: leases[name]!.snapshot)
        }
        guard changed.count <= 1 else { throw JedError("0A000", "a transaction may modify at most one durable database") }
    }
    func commit(_ working: QueryDomains, through domains: DatabaseDomains, synchronous: Bool,
                additional: [DomainCommitParticipant] = []) throws {
        try validateDurableChanges(working)
        let participants = leases.keys.sorted().map { name -> DomainCommitParticipant in
            let database = handles[name]!.database
            var policy = DomainCommitPolicy(); policy.storageLimit = database.maxStorageBytes; policy.domainName = name
            #if os(Linux) || canImport(Darwin)
            // Direct file attachments use the frozen common-reader gate.
            // Service attachments retain their own owner/client watermark.
            if DomainNames.isAttachment(name), database.host is FileDatabaseHost,
               handles[DomainNames.main]!.database.core.livePinCount > 0 {
                policy.oldestPinnedVersion = 0
            }
            #endif
            return DomainCommitParticipant(name: name, lease: leases[name]!, working: working.snapshots[name]!,
                policy: policy, entropy: database.keyEntropy, synchronous: synchronous)
        }
        let group = try DomainCommitGroup.prepare(participants + additional)
        try domains.withPublication { _ = try group.commit() }
    }
    func close() {
        guard !closed else { return }; closed = true
        for name in reads.keys.sorted() { reads[name]!.close() }
        for name in leases.keys.sorted().reversed() { leases[name]!.close() }
        release()
    }
    deinit { close() }
}
