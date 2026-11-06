/// Static ownership classification performs no expression evaluation or host
/// work. Full view planning still owns all body errors and their ordering.
internal enum ViewDomain {
    static func created(_ value: SQLCreateView, domains: QueryDomains) throws -> SQLText {
        if value.temporary, let qualifier = value.name.database, qualifier != DomainNames.temporary {
            throw JedError("42601", "a temporary view cannot be created in database \"\(qualifier)\"")
        }
        let written = value.name.database ?? (value.temporary ? DomainNames.temporary : DomainNames.main)
        _ = try domains.snapshot(written)
        if DomainNames.isAttachment(written) { return written }
        if written == DomainNames.temporary { return written }
        if value.replace, domains.snapshots[DomainNames.temporary]?.catalog.views.contains(where: { $0.name == value.name.name }) == true {
            return DomainNames.temporary
        }
        var temporary = false
        // An invalid body is still rejected by ordinary CREATE VIEW planning.
        // Classification only needs known owner facts to acquire writer leases.
        try? ViewSyntax.walk(value.query, expression: { expression in
            guard case .call(let call) = expression.kind,
                  ["nextval", "currval", "setval"].contains(call.name.description),
                  let first = call.arguments.first, case .string(let literal) = first.value.kind,
                  let name = try? SequenceName.parse(literal) else { return }
            if domains.snapshots[DomainNames.temporary]?.catalog.sequences.contains(where: { $0.name == name }) == true {
                temporary = true
            }
        }, from: { from, locals in
            guard case .table(let name) = from.source,
                  name.database != nil || !locals.contains(name.name),
                  let owner = try? domains.relation(name), owner.name == DomainNames.temporary else { return }
            if owner.snapshot.catalog.tables.contains(where: { $0.name == name.name })
                || owner.snapshot.catalog.views.contains(where: { $0.name == name.name }) { temporary = true }
        })
        return temporary ? DomainNames.temporary : written
    }
}
