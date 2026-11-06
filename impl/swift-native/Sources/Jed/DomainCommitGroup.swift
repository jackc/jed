/// One root handle owns writer acquisition and its atomic multi-domain reader
/// publication boundary. This coordinator supplies the storage phase ordering
/// inside that boundary; it never opens files or substitutes domain snapshots.
internal struct DomainCommitParticipant {
    let name: SQLText
    let lease: WriterLease
    let working: DomainSnapshot
    var policy = DomainCommitPolicy()
    var entropy: KeyEntropy?
    var synchronous = true
}

internal enum DomainCommitGroup {
    static func prepare(_ participants: [DomainCommitParticipant]) throws -> PreparedDomainCommitGroup {
        var names: Set<SQLText> = [], identities: Set<ObjectIdentifier> = []
        for item in participants {
            guard names.insert(item.name).inserted, identities.insert(item.lease.domainIdentity).inserted else {
                throw JedError("55000", "duplicate storage domain in commit group")
            }
            try item.lease.checkRole()
        }
        // Admission at each statement also tracks attempted durable writes.
        // This defensive check covers actual changed storage at final commit.
        let changed = participants.filter { $0.working.hasChanges(comparedTo: $0.lease.snapshot) }
        guard changed.filter({ $0.lease.domainKind == .file }).count <= 1 else {
            throw JedError("0A000", "a transaction may modify at most one durable database")
        }
        let main = SQLText(validatedUTF8: Array("main".utf8))
        let ordered = changed.sorted { left, right in
            if left.name == main { return false }
            if right.name == main { return true }
            return left.name < right.name
        }
        // Every domain's storage budget and metadata layout are admitted before
        // any domain draws encryption entropy or writes staged bytes.
        for item in ordered {
            var policy = item.policy; policy.domainName = item.name
            try item.lease.precheck(item.working, policy: policy)
        }
        var plans: [(DomainCommitParticipant, PreparedDomainCommit)] = []
        for item in ordered {
            var policy = item.policy; policy.domainName = item.name
            if let plan = try item.lease.prepare(item.working, policy: policy, entropy: item.entropy) {
                plans.append((item, plan))
            }
        }
        return PreparedDomainCommitGroup(plans)
    }
}

internal final class PreparedDomainCommitGroup {
    private let plans: [(DomainCommitParticipant, PreparedDomainCommit)]
    private var attempted = false
    fileprivate init(_ plans: [(DomainCommitParticipant, PreparedDomainCommit)]) { self.plans = plans }
    var changedNames: [SQLText] { plans.map { $0.0.name } }

    /// Caller must hold the root handle's publication boundary from before this
    /// call until installing its root bundle. Retain every participant's writer
    /// lease until that installation completes. A failed attempt is discarded;
    /// the next transaction prepares again over the still-published roots.
    func commit() throws -> [SQLText: DomainSnapshot] {
        guard !attempted else { throw JedError("55000", "commit group was already attempted") }
        attempted = true
        let memory = plans.filter { $0.0.lease.domainKind == .memory }
        let file = plans.first { $0.0.lease.domainKind == .file }
        var staged: [(MemoryCommitGuard, SQLText, AdmittedMemoryDomainCommit)] = []
        var published: [SQLText: DomainSnapshot] = [:]
        func enter(_ position: Int) throws {
            if position < memory.count {
                let (item, plan) = memory[position]
                try item.lease.withMemoryCommitGuard { guardValue in
                    let admitted = try guardValue.stage(plan)
                    staged.append((guardValue, item.name, admitted))
                    defer { staged.removeLast() }
                    try enter(position + 1)
                }
                return
            }
            // All memory packing, token checks and adoption construction have
            // succeeded. A known file success is followed only by nonthrowing
            // memory adoption and resident root publication under held guards.
            if let (item, plan) = file {
                published[item.name] = try item.lease.persist(plan, synchronous: item.synchronous)
            }
            for (guardValue, name, admitted) in staged { published[name] = guardValue.publish(admitted) }
        }
        try enter(0)
        return published
    }
}
