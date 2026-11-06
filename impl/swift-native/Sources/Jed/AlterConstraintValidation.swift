/// One validation pass can consume either original records or the final rows of
/// a rewrite. Rewrites therefore do not pay a second table scan for constraints.
internal final class AlterConstraintValidation {
    private let table: CatalogTable
    private let snapshot: DomainSnapshot
    private let settings: SessionSettings
    private let context: ExpressionContext
    private let checks: [(SQLText, ResolvedExpression)]
    private let indexes: [StoredIndexPlan]
    private let foreign: [ForeignKeyValidation.Plan]
    private let exclusions: [ExclusionValidation]
    private var entries: [[StoredIndexPlan.Entry]]
    private var unique: [Set<[UInt8]>]
    private var rows: [ForeignKeyValidation.Candidate] = []

    init(table: inout CatalogTable, added: Set<SQLText>, rebuilt: Set<SQLText> = [], snapshot: DomainSnapshot,
         settings: SessionSettings, context: ExpressionContext) throws {
        self.snapshot = snapshot; self.settings = settings; self.context = context
        var checks: [(SQLText, ResolvedExpression)] = []
        for ordinal in table.checks.indices.sorted(by: { table.checks[$0].name < table.checks[$1].name }) where added.contains(table.checks[ordinal].name) {
            let check = table.checks[ordinal]
            guard let syntax = check.expression.parsed else { throw JedError("XX003", "stored check expression cannot be parsed") }
            let resolver = try CatalogDDL.resolver(snapshot: snapshot, settings: settings, context: context, table: table)
            let expression = try resolver.resolve(syntax, expected: TypeDescriptor(.scalar(.boolean)))
            table.checks[ordinal].hosts = try StoredExpressionDependencies.hosts([expression], environment: settings.environment)
            checks.append((check.name, expression))
        }
        self.checks = checks; self.table = table
        indexes = try table.indexes.filter { added.contains($0.name) || rebuilt.contains($0.name) }.sorted { $0.name < $1.name }.map {
            try StoredIndexPlan.resolve(table: table, index: $0, snapshot: snapshot, settings: settings, context: context)
        }
        foreign = try table.foreignKeys.filter { added.contains($0.name) }.sorted { $0.name < $1.name }.map {
            try ForeignKeyValidation.Plan(child: table, foreign: $0, snapshot: snapshot,
                parent: $0.referencedTable == table.name ? table : nil)
        }
        exclusions = try table.exclusions.filter { added.contains($0.name) }.sorted { $0.name < $1.name }.map {
            try ExclusionValidation(table: table, definition: $0, snapshot: snapshot, settings: settings, context: context)
        }
        entries = Array(repeating: [], count: indexes.count)
        unique = Array(repeating: Set<[UInt8]>(), count: indexes.count)
    }
    func consume(key: [UInt8], value: @escaping (Int) throws -> SQLValue) throws {
        var execution = context; execution.column = value
        for (name, expression) in checks {
            if case .boolean(false) = try execution.evaluate(expression) { throw JedError("23514", "check constraint \(name) is violated by some row") }
        }
        for ordinal in indexes.indices {
            let indexed = try indexes[ordinal].row(value, storageKey: key)
            if indexes[ordinal].index.unique, let prefix = indexed.prefix, !prefix.hasNull && !unique[ordinal].insert(prefix.bytes).inserted {
                throw JedError("23505", "duplicate key value violates unique constraint \(indexes[ordinal].index.name)")
            }
            entries[ordinal] += indexed.entries
        }
        if !foreign.isEmpty || !exclusions.isEmpty { rows.append(.init(key: key, value: value)) }
    }
    func finish() throws -> [SQLText: TableStore] {
        for plan in foreign {
            for row in rows {
                switch try plan.childProbe(row.value, context: context) {
                case .exempt: continue
                case .missing: throw plan.violation()
                case .key(let key):
                    if plan.parent.name == table.name {
                        var found = false
                        for candidate in rows {
                            context.meter.charge(1); try context.meter.step()
                            if try plan.parentProbe(candidate.value, context: context) == key { found = true; break }
                        }
                        guard found else { throw plan.violation() }
                    } else if try !plan.parentExists(key, snapshot: snapshot, context: context) { throw plan.violation() }
                }
            }
        }
        for exclusion in exclusions {
            let members = try rows.compactMap { try exclusion.member($0.value) }
            for second in members.indices {
                for first in 0..<second {
                    context.meter.charge(1); try context.meter.step()
                    if let a = members[second].probe, let b = members[first].probe, try exclusion.conflicts(a, b) {
                        throw exclusion.violation(a, other: b.values, alter: true)
                    }
                }
            }
        }
        var built: [SQLText: TableStore] = [:]
        for ordinal in indexes.indices {
            let store = try indexes[ordinal].build(entries[ordinal], snapshot: snapshot,
                netStaging: settings.options.maxQueryMemoryBytes > 0)
            built[indexes[ordinal].index.name] = store
        }
        return built
    }
}
