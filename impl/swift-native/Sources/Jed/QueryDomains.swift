internal enum DomainNames {
    static let main = SQLText(validatedUTF8: Array("main".utf8))
    static let temporary = SQLText(validatedUTF8: Array("temp".utf8))
    static func isAttachment(_ name: SQLText) -> Bool { name != main && name != temporary }
}

/// A statement's named read roots. Each value keeps its own catalog identities,
/// packed stores and page space; resolution never manufactures a merged catalog.
internal struct QueryDomains {
    var snapshots: [SQLText: DomainSnapshot]
    var implicitDomain: SQLText = DomainNames.main

    func snapshot(_ name: SQLText) throws -> DomainSnapshot {
        guard let result = snapshots[name] else {
            throw JedError("42P01", "database \"\(name)\" is not attached")
        }
        return result
    }
    func relation(_ name: SQLName) throws -> (name: SQLText, snapshot: DomainSnapshot) {
        if let domain = name.database { return (domain, try snapshot(domain)) }
        if let temp = snapshots[DomainNames.temporary], Self.hasRelation(name.name, in: temp) {
            return (DomainNames.temporary, temp)
        }
        return (implicitDomain, try snapshot(implicitDomain))
    }
    static func statement(snapshot: DomainSnapshot, settings: SessionSettings) -> QueryDomains {
        guard var domains = settings.domains else {
            return QueryDomains(snapshots: [settings.targetDomain: snapshot], implicitDomain: settings.targetDomain)
        }
        // Catalog revalidation passes a prospective owner snapshot while the
        // other domains retain their original roots. A source-owned helper can
        // also pass an attachment snapshot under the outer statement settings.
        // Its storage identity, rather than the outer target, selects the entry.
        let owner = domains.snapshots.keys.sorted().first { domains.snapshots[$0]!.identity === snapshot.identity }
            ?? settings.targetDomain
        domains.snapshots[owner] = snapshot
        return domains
    }
    /// Persisted main bodies never capture a session's temporary names. A
    /// temporary view may read both roots. Attached view metadata is planned
    /// privately in its owning catalog without exposing attached view reads.
    func viewScope(owner: SQLText) throws -> QueryDomains {
        if owner == DomainNames.temporary {
            var values = [owner: try snapshot(owner)]
            if let main = snapshots[DomainNames.main] { values[DomainNames.main] = main }
            return QueryDomains(snapshots: values)
        }
        return QueryDomains(snapshots: [owner: try snapshot(owner)], implicitDomain: owner)
    }
    /// Indexes and sequences participate in the same flat relation namespace.
    static func hasRelation(_ name: SQLText, in snapshot: DomainSnapshot) -> Bool {
        snapshot.catalog.tables.contains { $0.name == name }
            || snapshot.catalog.views.contains { $0.name == name }
            || snapshot.catalog.sequences.contains { $0.name == name }
            || snapshot.catalog.tables.contains { $0.indexes.contains { $0.name == name } }
    }
    var pendingBytes: Int64 {
        snapshots.reduce(0) { $1.key == DomainNames.temporary ? $0 : saturatingAdd($0, $1.value.pendingBytes) }
    }
    mutating func forgetPendingKeys() {
        for name in snapshots.keys { snapshots[name]!.forgetPendingKeys() }
    }
}

/// Independent retention is essential: closing a read session must not release
/// a cursor's roots, and closing one cursor must not release a sibling's roots.
internal final class DomainReadBundle {
    /// Pins in name order: role checks and release visit them deterministically.
    private var orderedPins: [(name: SQLText, pin: SnapshotPin)]
    private(set) var domains: QueryDomains
    // A core pin protects page addresses, while the database owns the file or
    // service host. An escaped cursor must keep both until its final close.
    private var database: Database?
    convenience init(pins: [SQLText: SnapshotPin], database: Database? = nil) {
        self.init(ordered: pins.keys.sorted().map { (name: $0, pin: pins[$0]!) }, database: database)
    }
    /// `ordered` is already in name order.
    init(ordered: [(name: SQLText, pin: SnapshotPin)], database: Database? = nil) {
        orderedPins = ordered; self.database = database
        var snapshots: [SQLText: DomainSnapshot] = [:]
        snapshots.reserveCapacity(ordered.count + 1)
        for entry in ordered { snapshots[entry.name] = entry.pin.snapshot }
        domains = QueryDomains(snapshots: snapshots)
    }
    func install(_ pin: SnapshotPin, for name: SQLText) {
        domains.snapshots[name] = pin.snapshot
        // Keep name order by placing the one changed entry.
        if let index = orderedPins.firstIndex(where: { $0.name == name }) {
            let previous = orderedPins[index].pin
            orderedPins[index] = (name, pin)
            previous.close()
        } else {
            orderedPins.insert((name, pin), at: orderedPins.firstIndex(where: { name < $0.name }) ?? orderedPins.count)
        }
    }
    func checkRole() throws { for entry in orderedPins { try entry.pin.checkRole() } }
    func retain() throws -> DomainReadBundle {
        var retained: [(name: SQLText, pin: SnapshotPin)] = []
        retained.reserveCapacity(orderedPins.count + 1)
        do {
            for entry in orderedPins { retained.append((entry.name, try entry.pin.retain())) }
            return DomainReadBundle(ordered: retained, database: database)
        } catch { for entry in retained { entry.pin.close() }; throw error }
    }
    func close() {
        for entry in orderedPins { entry.pin.close() }
        database = nil
    }
    deinit { close() }
}
