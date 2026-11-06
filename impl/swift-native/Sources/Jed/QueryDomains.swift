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
    private(set) var pins: [SQLText: SnapshotPin]
    var domains: QueryDomains { QueryDomains(snapshots: pins.mapValues(\.snapshot)) }
    init(pins: [SQLText: SnapshotPin]) { self.pins = pins }
    func install(_ pin: SnapshotPin, for name: SQLText) {
        pins.updateValue(pin, forKey: name)?.close()
    }
    func checkRole() throws { for name in pins.keys.sorted() { try pins[name]!.checkRole() } }
    func retain() throws -> DomainReadBundle {
        var retained: [SQLText: SnapshotPin] = [:]
        do {
            for name in pins.keys.sorted() { retained[name] = try pins[name]!.retain() }
            return DomainReadBundle(pins: retained)
        } catch { for pin in retained.values { pin.close() }; throw error }
    }
    func close() { for name in pins.keys.sorted() { pins[name]!.close() } }
    deinit { close() }
}
