/// Multi-domain catalog orchestration keeps each actual catalog and page space
/// separate. All tentative snapshots remain private until every owner succeeds.
internal extension CatalogDDL {
    static func execute(_ statement: SQLStatement, source: SQLText, working: inout QueryDomains,
                        settings: SessionSettings, context: ExpressionContext, meter: CostMeter,
                        requireWritable: (SQLText) throws -> Void) throws -> CommandTag? {
        if case .drop(let definition) = statement,
           ["table", "view", "index", "sequence", "type"].contains(definition.object) {
            return try dropDomains(definition, source: source, working: &working, settings: settings,
                                   context: context, meter: meter, requireWritable: requireWritable)
        }
        switch statement {
        case .createType, .createTable, .createIndex, .createSequence, .createView, .alterView, .alterSequence, .alterTable: break
        default: return nil
        }
        let owner = settings.targetDomain
        try requireWritable(owner)
        var snapshot = try working.snapshot(owner), scoped = settings
        scoped.domains = working
        let oldPendingBase = meter.pendingDomainBase, oldPendingExcluded = meter.pendingDomainExcluded
        meter.pendingDomainExcluded = owner == DomainNames.temporary
        meter.pendingDomainBase = working.pendingBytes - (meter.pendingDomainExcluded ? 0 : snapshot.pendingBytes)
        defer { meter.pendingDomainBase = oldPendingBase; meter.pendingDomainExcluded = oldPendingExcluded }
        guard let tag = try execute(statement, source: source, snapshot: &snapshot, settings: scoped,
                                    context: context, meter: meter) else { return nil }
        var frame = working
        frame.snapshots[owner] = snapshot
        try CatalogExternalViews.apply(statement, before: working, after: &frame, settings: scoped, context: context)
        if let before = working.snapshots[DomainNames.temporary], let after = frame.snapshots[DomainNames.temporary],
           after.hasChanges(comparedTo: before) { try requireWritable(DomainNames.temporary) }
        working = frame
        return tag
    }

    private struct DropKey: Hashable { let owner: SQLText; let name: SQLText }

    private static func dropDomains(_ definition: SQLDrop, source: SQLText, working: inout QueryDomains,
                                    settings: SessionSettings, context: ExpressionContext, meter: CostMeter,
                                    requireWritable: (SQLText) throws -> Void) throws -> CommandTag {
        let statement = SQLStatement.drop(definition)
        let owners = try StatementDomain.targets(statement, in: working)
        // Frozen admission uses the temp DDL gate for a mixed DROP containing a
        // temporary target, as all three supported cores do. The ordinary SQL
        // privilege preflight has the same rule; do not re-gate its main part.
        var admission = settings
        admission.targetDomain = owners.contains(DomainNames.temporary) ? DomainNames.temporary : DomainNames.main
        try CatalogDomain.requireDDL(admission)
        for owner in owners { try requireWritable(owner) }
        var groups: [SQLText: [SQLName]] = [:], targets: Set<DropKey> = []
        for input in definition.names {
            let owner = definition.object == "type" ? DomainNames.main : try working.relation(input).name
            let snapshot = try working.snapshot(owner), image = snapshot.catalog
            var scoped = settings; scoped.targetDomain = owner
            let name = try CatalogDomain.name(input, settings: scoped)
            if definition.object == "table" || definition.object == "view" { try SystemRelationPlan.rejectTarget(name, dropping: true) }
            let exists: Bool
            switch definition.object {
            case "table": exists = image.tables.contains { $0.name == name }
            case "view": exists = image.views.contains { $0.name == name }
            case "sequence": exists = image.sequences.contains { $0.name == name }
            case "type": exists = image.composites.contains { $0.name == name }
            default:
                let table = image.tables.first { $0.indexes.contains { $0.name == name } }
                let index = table?.indexes.first { $0.name == name }
                if let index, index.isConstraint || table!.exclusions.contains(where: { $0.index == name }) {
                    throw JedError("2BP01", "cannot drop index \(name) because a constraint requires it").hinted(DependencyDiagnostics.ownedIndex(name, table: table!))
                }
                exists = index != nil
            }
            if !exists {
                if ["table", "view", "index"].contains(definition.object), CatalogSQL.relationNames(image).contains(name) {
                    throw JedError("42809", "\(name) is not a \(definition.object)")
                }
                if !definition.ifExists {
                    throw JedError(definition.object == "type" || definition.object == "index" ? "42704" : "42P01",
                                   "\(definition.object) does not exist: \(name)")
                }
                continue
            }
            let key = DropKey(owner: owner, name: name)
            if targets.insert(key).inserted {
                groups[owner, default: []].append(SQLName(database: owner, name: name, span: input.span))
            }
        }
        let tag = CommandTag(command: "DROP " + definition.object.uppercased(), rowsAffected: 0, cost: meter.accrued)
        guard !targets.isEmpty else { return tag }
        var frame = working
        // The name-only catalog dependency contract is scoped: main never reads
        // temp, a private view can read either, and an attachment is isolated.
        func visible(_ referenced: SQLText, from owner: SQLText) -> Bool {
            owner == referenced || owner == DomainNames.temporary && referenced == DomainNames.main
        }
        var removedViews: Set<DropKey> = definition.object == "view" ? targets : []
        var droppedSequences: Set<DropKey> = definition.object == "sequence" ? targets : []
        if definition.object == "table" {
            for target in targets {
                for sequence in try frame.snapshot(target.owner).catalog.sequences where sequence.owner?.table == target.name {
                    droppedSequences.insert(DropKey(owner: target.owner, name: sequence.name))
                }
            }
        }
        // A main sequence may appear in a private table/view default too.
        if definition.object == "sequence" {
            for target in targets.sorted(by: { $0.name < $1.name }) {
                for owner in frame.snapshots.keys.sorted() where visible(target.owner, from: owner) {
                    let image = frame.snapshots[owner]!.catalog
                    for table in image.tables.sorted(by: { $0.name < $1.name }) {
                        for column in table.columns {
                            if case .expression(let expression) = column.defaultValue,
                               SequenceReferences.contains(target.name, in: expression.text) {
                                throw JedError(definition.cascade ? "0A000" : "2BP01", "default value depends on sequence \(target.name)")
                            }
                        }
                    }
                    for view in image.views.sorted(by: { $0.name < $1.name }) {
                        for column in view.columns {
                            if let text = column.defaultText, SequenceReferences.contains(target.name, in: text) {
                                throw JedError(definition.cascade ? "0A000" : "2BP01", "view default depends on sequence \(target.name)")
                            }
                        }
                    }
                }
            }
        }
        var changed = true
        while changed {
            changed = false
            var views: [(SQLText, CatalogView)] = []
            for (owner, snapshot) in frame.snapshots {
                for view in snapshot.catalog.views { views.append((owner, view)) }
            }
            views.sort { left, right in
                if left.1.name == right.1.name { return left.0 < right.0 }
                return left.1.name < right.1.name
            }
            for (owner, view) in views {
                let key = DropKey(owner: owner, name: view.name)
                if removedViews.contains(key) { continue }
                let depends = view.dependencies.contains { dependency in
                    if dependency.kind == 1, removedViews.contains(where: { $0.name == dependency.name && visible($0.owner, from: owner) }) { return true }
                    if dependency.kind == 2, droppedSequences.contains(where: { $0.name == dependency.name && visible($0.owner, from: owner) }) { return true }
                    let kind: UInt8? = definition.object == "table" ? 0 : definition.object == "type" ? 3 : nil
                    return dependency.kind == kind && targets.contains { $0.name == dependency.name && visible($0.owner, from: owner) }
                }
                if depends {
                    guard definition.cascade else { throw JedError("2BP01", "cannot drop \(definition.object) because view \(view.name) depends on it").hinted(["table", "view"].contains(definition.object) ? DependencyDiagnostics.cascade : nil) }
                    removedViews.insert(key); changed = true
                }
            }
        }
        // Remove the complete view closure before any local kernel validates its
        // prospective catalog. This includes same-statement explicitly dropped
        // views and private dependents of a persistent target.
        for owner in frame.snapshots.keys.sorted() {
            let names = Set(removedViews.filter { $0.owner == owner }.map(\.name))
            guard !names.isEmpty else { continue }
            try requireWritable(owner)
            var snapshot = frame.snapshots[owner]!, image = snapshot.catalog
            image.views.removeAll { names.contains($0.name) }
            try snapshot.updateCatalog(image, schemaChanged: true)
            frame.snapshots[owner] = snapshot
        }
        if definition.object != "view" {
            // FK dependency admission is main first, then temp, then attachments.
            let ordered = groups.keys.sorted { left, right in
                func rank(_ value: SQLText) -> Int { value == DomainNames.main ? 0 : value == DomainNames.temporary ? 1 : 2 }
                return rank(left) == rank(right) ? left < right : rank(left) < rank(right)
            }
            let oldPendingBase = meter.pendingDomainBase, oldPendingExcluded = meter.pendingDomainExcluded
            defer { meter.pendingDomainBase = oldPendingBase; meter.pendingDomainExcluded = oldPendingExcluded }
            for owner in ordered {
                var snapshot = frame.snapshots[owner]!, scoped = settings
                scoped.targetDomain = owner; scoped.domains = frame
                // The grouped admission above already applied the frozen gate.
                scoped.options.allowDDL = true; scoped.options.allowTemporaryDDL = true
                meter.pendingDomainExcluded = owner == DomainNames.temporary
                meter.pendingDomainBase = frame.pendingBytes - (meter.pendingDomainExcluded ? 0 : snapshot.pendingBytes)
                let local = SQLDrop(object: definition.object, names: groups[owner]!, ifExists: false, cascade: definition.cascade)
                _ = try execute(.drop(local), source: source, snapshot: &snapshot, settings: scoped, context: context, meter: meter)
                frame.snapshots[owner] = snapshot
            }
        }
        working = frame
        return tag
    }
}
