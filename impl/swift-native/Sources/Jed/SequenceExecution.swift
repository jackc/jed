/// Session history follows statement publication and transaction/savepoint
/// rollback. The counter itself remains part of the catalog snapshot.
internal struct SequenceHistory {
    var values: [SQLText: Int64] = [:]
    var lastName: SQLText?
}

/// An expression may advance sequences while DML mutates its private snapshot.
/// Keeping this small pending set separate avoids aliasing the inout snapshot;
/// successful statement publication applies both sets of edits together.
internal final class SequenceExecution {
    private struct Key: Hashable { let domain: SQLText; let name: SQLText }
    private var definitions: [Key: CatalogSequence]
    private var pending: [Key: CatalogSequence] = [:]
    private let owners: [ObjectIdentifier: SQLText]
    private let targetDomain: SQLText
    private let writableDomains: Set<SQLText>?
    private(set) var history: SequenceHistory
    private let meter: CostMeter
    private let writable: Bool

    convenience init(snapshot: DomainSnapshot, history: SequenceHistory, meter: CostMeter, writable: Bool) {
        self.init(domains: QueryDomains(snapshots: [DomainNames.main: snapshot]), history: history, meter: meter, writable: writable)
    }
    init(domains: QueryDomains, targetDomain: SQLText = DomainNames.main, history: SequenceHistory,
         meter: CostMeter, writable: Bool, writableDomains: Set<SQLText>? = nil) {
        var values: [Key: CatalogSequence] = [:], owners: [ObjectIdentifier: SQLText] = [:]
        for (domain, snapshot) in domains.snapshots {
            owners[ObjectIdentifier(snapshot.identity)] = domain
            for sequence in snapshot.catalog.sequences { values[Key(domain: domain, name: sequence.name)] = sequence }
        }
        definitions = values; self.owners = owners; self.targetDomain = targetDomain
        self.history = history; self.meter = meter; self.writable = writable; self.writableDomains = writableDomains
    }

    func install(into context: inout ExpressionContext) {
        context.sequences = self
        let fallback = context.functions.stateCall
        context.functions.stateCall = { name, values in
            if ["nextval", "setval", "currval", "lastval"].contains(name) { return try self.call(name, values) }
            guard let fallback else { throw JedError("55000", "function requires an execution-owned service: \(name)") }
            return try fallback(name, values)
        }
    }

    /// Apply pending counters before editing definitions. Refreshing the action
    /// namespace removes stale objects, including a dropped and reused name.
    func synchronizeDefinitions(from image: CatalogImage, domain: SQLText = DomainNames.main) {
        definitions = definitions.filter { $0.key.domain != domain }
        for sequence in image.sequences { definitions[Key(domain: domain, name: sequence.name)] = sequence }
        pending = pending.filter { $0.key.domain != domain }
    }
    func apply(to domains: inout QueryDomains) throws {
        for domain in Set(pending.keys.map(\.domain)).sorted() {
            guard var snapshot = domains.snapshots[domain] else { throw JedError("42P01", "sequence database is not attached") }
            try apply(to: &snapshot)
            domains.snapshots[domain] = snapshot
        }
    }

    func apply(to snapshot: inout DomainSnapshot) throws {
        guard !pending.isEmpty else { return }
        let domain = owners[ObjectIdentifier(snapshot.identity)] ?? targetDomain
        var image = snapshot.catalog, changed = false
        for index in image.sequences.indices {
            if let sequence = pending[Key(domain: domain, name: image.sequences[index].name)] { image.sequences[index] = sequence; changed = true }
        }
        if changed { try snapshot.updateCatalog(image, schemaChanged: false) }
    }

    private func call(_ function: String, _ values: [SQLValue]) throws -> SQLValue {
        if values.contains(where: \.isNull) { return .null }
        if function == "lastval" {
            guard let name = history.lastName, let value = history.values[name] else {
                throw JedError("55000", "lastval is not yet defined in this session")
            }
            return .integer(value)
        }
        if function == "nextval" || function == "setval" {
            guard writable else { throw JedError("25006", "cannot advance a sequence in a read-only transaction") }
            meter.charge(1)
        }
        guard let first = values.first, case .text(let text) = first else { throw JedError("42804", "sequence name must be text") }
        let name = try SequenceName.parse(text)
        let domains = [DomainNames.temporary, DomainNames.main]
        guard let key = domains.map({ Key(domain: $0, name: name) }).first(where: { pending[$0] != nil || definitions[$0] != nil }),
              var sequence = pending[key] ?? definitions[key] else { throw JedError("42P01", "relation does not exist: \(name)") }
        if function == "nextval" || function == "setval", let writableDomains, !writableDomains.contains(key.domain) {
            throw JedError("25006", "cannot advance a sequence in a read-only database")
        }
        if function == "currval" {
            guard let value = history.values[name] else { throw JedError("55000", "currval of sequence \(name) is not yet defined in this session") }
            return .integer(value)
        }
        if function == "nextval" {
            if sequence.called {
                let (next, overflow) = sequence.last.addingReportingOverflow(sequence.increment)
                let crosses = overflow || (sequence.increment > 0 ? next > sequence.maximum : next < sequence.minimum)
                if crosses {
                    guard sequence.cycle else { throw JedError("2200H", "nextval reached the bound of sequence \(name)") }
                    sequence.last = sequence.increment > 0 ? sequence.minimum : sequence.maximum
                } else { sequence.last = next }
            }
            sequence.called = true
            history.values[name] = sequence.last; history.lastName = name
        } else {
            guard values.count == 2 || values.count == 3, case .integer(let value) = values[1] else {
                throw JedError("42804", "invalid setval arguments")
            }
            guard value >= sequence.minimum && value <= sequence.maximum else {
                throw JedError("22003", "setval value is outside sequence bounds")
            }
            var called = true
            if values.count == 3 {
                guard case .boolean(let flag) = values[2] else { throw JedError("42804", "invalid setval flag") }
                called = flag
            }
            sequence.last = value; sequence.called = called
            if called { history.values[name] = value }
        }
        pending[key] = sequence
        return .integer(sequence.last)
    }

    static func mutates(_ expressions: [ResolvedExpression]) -> Bool {
        var result = false
        StoredExpressionDependencies.walk(expressions, queryBodies: true) { expression in
            if case .call(let call) = expression.node, call.hostID == nil,
               call.name == "nextval" || call.name == "setval" { result = true }
        }
        return result
    }
}
