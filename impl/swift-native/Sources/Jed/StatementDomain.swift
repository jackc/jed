/// Select real owning snapshots before entering catalog or mutation kernels.
/// Reads keep their separate named-domain map; this list never merges catalogs.
internal enum StatementDomain {
    /// Unique writable owners in statement order. A query without a writable
    /// WITH contributes no owner, so merely reading main does not acquire its
    /// writer when the statement only changes the private temporary database.
    static func targets(_ statement: SQLStatement, in domains: QueryDomains) throws -> [SQLText] {
        var result: [SQLText] = []
        func append(_ owner: SQLText) { if !result.contains(owner) { result.append(owner) } }
        func existing(_ name: SQLName) throws { append(try domains.relation(name).name) }
        func created(_ name: SQLName, temporary: Bool = false) throws {
            if temporary, let explicit = name.database, explicit != DomainNames.temporary {
                throw JedError("42601", "temporary table cannot specify another database")
            }
            let owner = name.database ?? (temporary ? DomainNames.temporary : DomainNames.main)
            _ = try domains.snapshot(owner)
            append(owner)
        }
        func nested(_ value: SQLStatement) throws {
            for owner in try targets(value, in: domains) { append(owner) }
        }
        switch statement {
        case .insert(let value): try existing(value.table)
        case .update(let value): try existing(value.table)
        case .delete(let value): try existing(value.table)
        case .merge(let value): try existing(value.table)
        case .createTable(let value):
            try created(value.name, temporary: value.temporary)
            if value.withData, let query = value.query { try nested(.query(query)) }
        case .createView(let value): append(try ViewDomain.created(value, domains: domains))
        case .createType(let value): try created(value.name)
        case .createSequence(let value): try created(value.name)
        case .createIndex(let value): try existing(value.table)
        case .alterTable(let value): try existing(value.name)
        case .alterView(let value): try existing(value.name)
        case .alterSequence(let value): try existing(value.name)
        case .analyze(let name, _): try existing(name)
        case .reindex(let value):
            if let name = value.name { try existing(name) } else { append(DomainNames.main) }
        case .drop(let value) where value.object == "type": append(DomainNames.main)
        case .drop(let value): for name in value.names { try existing(name) }
        case .with, .query:
            if let part = WritableCTEPlan.extract(statement) {
                for binding in part.clause.bindings { try nested(binding.body) }
                try nested(part.body)
            }
        case .explain(let options, let body): if options.analyze { try nested(body) }
        default: break
        }
        return result
    }

    /// Compatibility entry point for a single target kernel. A grouped caller
    /// admits every entry of targets before it invokes kernels for each owner.
    static func target(_ statement: SQLStatement, in domains: QueryDomains) throws -> SQLText {
        if case .explain(_, let body) = statement { return try target(body, in: domains) }
        return try targets(statement, in: domains).first ?? DomainNames.main
    }
}
