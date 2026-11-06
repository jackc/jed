/// One originating statement owns this action closure. Generated mutations use
/// the ordinary row pipeline, while parent-side checks wait for every sibling
/// action to finish. No generated row contributes to the caller's command tag.
internal enum ForeignKeyActions {
    struct Transition {
        let old: ForeignKeyValidation.Candidate
        let new: ForeignKeyValidation.Candidate?
    }
    final class State {
        var depth = 0
        let deferUntilStatementEnd: Bool
        private var checks: [Check] = []
        private let journal: Journal?
        private let domain: SQLText
        init(deferUntilStatementEnd: Bool = false, journal: Journal? = nil, domain: SQLText = DomainNames.main) {
            self.deferUntilStatementEnd = deferUntilStatementEnd; self.journal = journal; self.domain = domain
        }
        func enqueue(_ check: Check) {
            if let journal { journal.append(check, domain: domain) }
            else { checks.append(check) }
        }
        /// Capture child tuples when visited, preserving their place beside
        /// parent checks. Even MATCH FULL's partly-NULL rejection waits until
        /// the outer statement finishes; an immediate later error can win.
        func validateChildren(_ candidates: [ForeignKeyValidation.Candidate], child: CatalogTable,
                              snapshot: DomainSnapshot, assigned: Set<Int>? = nil,
                              vacated: Set<[UInt8]> = [], context: ExpressionContext) throws {
            if !deferUntilStatementEnd {
                try ForeignKeyValidation.validateChildren(candidates, child: child, snapshot: snapshot,
                    assigned: assigned, vacated: vacated, context: context)
                return
            }
            let plans = try child.foreignKeys.sorted { $0.name < $1.name }.filter {
                assigned == nil || !Set($0.columns).isDisjoint(with: assigned!)
            }.map { try ForeignKeyValidation.Plan(child: child, foreign: $0, snapshot: snapshot) }
            for candidate in candidates {
                for plan in plans {
                    enqueue(.child(plan, try plan.foreign.columns.map(candidate.value)))
                }
            }
        }
        func settle(snapshot: DomainSnapshot, settings: SessionSettings, context: ExpressionContext) throws {
            defer { checks.removeAll() }
            for check in checks { try ForeignKeyActions.settle(check, snapshot: snapshot, settings: settings, context: context) }
        }
    }
    /// A writable WITH shares one lexical check order across all of its target
    /// domains. Each entry retains its originating tuple and owner, while its
    /// final probes use that owner's final working roots.
    final class Journal {
        private var checks: [(SQLText, Check)] = []
        fileprivate func append(_ check: Check, domain: SQLText) { checks.append((domain, check)) }
        func settle(domains: QueryDomains, settings: SessionSettings, context: ExpressionContext) throws {
            defer { checks.removeAll() }
            for (domain, check) in checks {
                var scoped = settings; scoped.targetDomain = domain; scoped.domains = domains
                try ForeignKeyActions.settle(check, snapshot: domains.snapshot(domain), settings: scoped, context: context)
            }
        }
    }
    private static func settle(_ check: Check, snapshot: DomainSnapshot, settings: SessionSettings,
                               context: ExpressionContext) throws {
        switch check {
        case .parent(let plan, let key, let tuple, let update):
            if update, try plan.parentExists(key, snapshot: snapshot, context: context) { return }
            if try plan.hasChild(tuple, snapshot: snapshot, settings: settings, context: context) { throw plan.violation(parentSide: true) }
        case .child(let plan, let tuple):
            let positions = Dictionary(uniqueKeysWithValues: plan.foreign.columns.enumerated().map { ($0.element, $0.offset) })
            switch try plan.childProbe({ tuple[positions[$0]!] }, context: context) {
            case .exempt: break
            case .missing: throw plan.violation()
            case .key(let key):
                guard try plan.parentExists(key, snapshot: snapshot, context: context) else { throw plan.violation() }
            }
        }
    }
    enum Check {
        case parent(ForeignKeyValidation.Plan, key: [UInt8], tuple: [SQLValue], update: Bool)
        case child(ForeignKeyValidation.Plan, [SQLValue])
    }
    struct Target {
        let plan: ForeignKeyValidation.Plan
        let action: UInt8
        let deleting: Bool
        let tuples: [[SQLValue]]
        let replacements: [[SQLValue]]?
        let positions: [[UInt8]: Int]

        func match(_ row: (Int) throws -> SQLValue, context: ExpressionContext) throws -> Int? {
            guard case .key(let key) = try plan.childProbe(row, context: context, enforceMatch: false) else { return nil }
            return positions[key]
        }
        func mapped(_ position: Int, column: Int, context: ExpressionContext) throws -> SQLValue? {
            guard let replacements, let slot = plan.foreign.columns.firstIndex(of: column) else { return nil }
            let value = replacements[position][slot]
            let source = plan.parentTypes[plan.foreign.referencedColumns[slot]]
            let target = plan.childTypes[column]
            // FK assignment has precisely the admitted pair's assignment cast,
            // including decimal rounding and f64 narrowing. The ordinary store
            // path subsequently checks every destination modifier.
            if source.type == target.type { return value }
            // Decimal-to-integer and f64-to-f32 are explicit-only in ordinary
            // jed assignments. The FK contract expressly admits these two
            // conversions for CASCADE, before the strict ordinary store check.
            return try ContainerCasts.cast(value, from: source, to: TypeDescriptor(target.type),
                mode: .explicit, context: context.scalar, growth: false)
        }

        /// Compute the complete gather block before its admission guard. The
        /// records retained here are the ordinary prepared-mutation list; they
        /// carry packed cells, not an eagerly decoded result collector.
        func gather(snapshot: DomainSnapshot, settings: SessionSettings, context: ExpressionContext,
                    touched: [Int]) throws -> Gather {
            guard let store = snapshot.tables[plan.child.name] else { throw JedError.corruption("foreign-key child store is missing") }
            guard let path = plan.reversePath(snapshot: snapshot, settings: settings) else {
                try ScanCost.charge(store, touched: touched, meter: context.meter)
                return Gather(scan: ScanAccess.full.cursor(store))
            }
            let scratch = CostMeter(resources: context.meter.resources, cancellation: context.meter.cancellation)
            var records: [[UInt8]: PackedRecord] = [:]
            for tuple in tuples {
                try context.meter.pollStep()
                guard let prefix = try plan.reversePrefix(tuple, columns: path.columns, index: path.index, context: context) else { continue }
                let access = ScanAccess(lower: TreeBound(prefix),
                    upper: OrderedIndexPlan.successor(prefix).map { TreeBound($0, inclusive: false) }, isEmpty: false)
                if let index = path.index {
                    guard let tree = snapshot.indexes[index.name] else { throw JedError.corruption("foreign-key child index is missing") }
                    scratch.charge(access.pages(tree.tree))
                    var cursor = access.cursor(tree); defer { cursor.close() }
                    while let entry = try cursor.next() {
                        try context.meter.pollStep()
                        var offset = prefix.count
                        for position in path.columns.count..<index.keys.count {
                            guard case .column(let column) = index.keys[position], let width = ValueCodec.fixedWidth(plan.childTypes[column].type),
                                  offset < entry.key.count else { throw JedError.corruption("invalid foreign-key child index suffix") }
                            let flags = index.keyOptions.isEmpty ? 0 : index.keyOptions[position]
                            let descending = flags & 1 != 0, nullsFirst = flags & 2 != 0
                            let absent = KeyEncoding.slot(body: nil, descending: descending, nullsFirst: nullsFirst)[0]
                            let present = KeyEncoding.slot(body: [], descending: descending, nullsFirst: nullsFirst)[0]
                            let tag = entry.key[offset]; offset += 1
                            if tag == present { offset += width }
                            else if tag != absent { throw JedError.corruption("invalid foreign-key child index presence tag") }
                        }
                        guard offset < entry.key.count else { throw JedError.corruption("missing foreign-key child storage key") }
                        let key = Array(entry.key.dropFirst(offset))
                        let point = ScanAccess(lower: TreeBound(key), upper: TreeBound(key), isEmpty: false)
                        try ScanCost.charge(store, touched: touched, meter: scratch, access: point)
                        guard let record = try store.get(key) else { throw JedError.corruption("foreign-key child index points to a missing row") }
                        records[key] = record
                    }
                } else {
                    try ScanCost.charge(store, touched: touched, meter: scratch, access: access)
                    var cursor = access.cursor(store); defer { cursor.close() }
                    while let record = try cursor.next() {
                        try context.meter.pollStep(); records[record.key] = record
                    }
                }
            }
            context.meter.charge(scratch.accrued); try context.meter.guardCost()
            return Gather(records: records.keys.sorted { $0.lexicographicallyPrecedes($1) }.map { records[$0]! })
        }
    }
    struct Gather {
        var scan: ScanCursor?
        var records: [PackedRecord] = []
        var position = 0
        mutating func next() throws -> PackedRecord? {
            if scan != nil { return try scan!.next() }
            guard position < records.count else { return nil }
            defer { position += 1 }; return records[position]
        }
        mutating func close() { scan?.close(); scan = nil; records.removeAll() }
    }

    static func apply(_ transitions: [Transition], parent: CatalogTable, deleting: Bool,
                      snapshot: inout DomainSnapshot, settings: SessionSettings,
                      context: ExpressionContext, state: State) throws {
        guard !transitions.isEmpty else { return }
        let plans = try snapshot.catalog.tables.sorted { $0.name < $1.name }.flatMap { child in
            try child.foreignKeys.sorted { $0.name < $1.name }.filter { $0.referencedTable == parent.name }.map {
                try ForeignKeyValidation.Plan(child: child, foreign: $0, snapshot: snapshot)
            }
        }
        for plan in plans {
            let action = deleting ? plan.foreign.onDelete : plan.foreign.onUpdate
            guard action > 1 else { continue }
            var tuples: [[SQLValue]] = [], replacements: [[SQLValue]] = [], positions: [[UInt8]: Int] = [:]
            for transition in transitions {
                guard let key = try plan.parentProbe(transition.old.value, context: context) else { continue }
                if let new = transition.new, try plan.parentProbe(new.value, context: context) == key { continue }
                guard positions[key] == nil else { continue }
                positions[key] = tuples.count
                tuples.append(try plan.foreign.referencedColumns.map(transition.old.value))
                if !deleting, action == 2 {
                    guard let new = transition.new else { throw JedError("XX000", "foreign-key update has no new row") }
                    replacements.append(try plan.foreign.referencedColumns.map(new.value))
                }
            }
            guard !tuples.isEmpty else { continue }
            let target = Target(plan: plan, action: action, deleting: deleting && action == 2,
                tuples: tuples, replacements: !deleting && action == 2 ? replacements : nil, positions: positions)
            try context.meter.checkpoint()
            guard state.depth < 256 else { throw JedError("54001", "referential action depth exceeds the maximum of 256") }
            state.depth += 1
            do {
                defer { state.depth -= 1 }
                try MutationExecutor.executeAction(target, snapshot: &snapshot, settings: settings, context: context, state: state)
            }
        }
        for plan in plans {
            let action = deleting ? plan.foreign.onDelete : plan.foreign.onUpdate
            guard action <= 1 else { continue }
            for transition in transitions {
                guard let key = try plan.parentProbe(transition.old.value, context: context) else { continue }
                if let new = transition.new, try plan.parentProbe(new.value, context: context) == key { continue }
                state.enqueue(.parent(plan, key: key, tuple: try plan.foreign.referencedColumns.map(transition.old.value), update: !deleting))
            }
        }
    }
}
