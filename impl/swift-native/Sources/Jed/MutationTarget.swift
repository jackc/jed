/// Shared target/view preparation for execution, RETURNING schemas, and EXPLAIN.
/// The snapshot is the real writable base domain; the named view can belong to
/// a different domain and is looked up only in the separate statement read map.
internal enum MutationTarget {
    struct Prepared {
        let relation: MutationExecutor.Relation
        let settings: SessionSettings
    }
    static func resolve(_ name: SQLName, operation: String, snapshot: DomainSnapshot,
                        settings: SessionSettings, context: ExpressionContext) throws -> Prepared {
        let domains = QueryDomains.statement(snapshot: snapshot, settings: settings)
        let selected = try domains.relation(name)
        let privilege: PrivilegeSet = operation == "insert into" ? .insert : operation == "update" ? .update : .delete
        let relation: MutationExecutor.Relation
        var inherited = settings
        if let definition = selected.snapshot.catalog.views.first(where: { $0.name == name.name }) {
            guard !DomainNames.isAttachment(selected.name) else { throw JedError("0A000", "writes through attached views are not supported") }
            if selected.name != DomainNames.temporary { try settings.privileges.require(privilege, on: definition.name) }
            var scoped = settings; scoped.targetDomain = selected.name; scoped.domains = domains
            let view = try UpdatableView.resolve(definition, operation: operation, snapshot: selected.snapshot,
                                                settings: scoped, context: context)
            guard view.tableDomain == settings.targetDomain else { throw JedError("XX000", "view write owner was not admitted") }
            inherited = view.layers.last!.settings
            inherited.targetDomain = view.tableDomain; inherited.domains = domains
            relation = try MutationExecutor.Relation(SQLName(database: view.tableDomain, name: view.table.name, span: name.span),
                snapshot: snapshot, view: view, domain: view.tableDomain)
        } else {
            relation = try MutationExecutor.Relation(name, snapshot: snapshot, domain: settings.targetDomain)
        }
        if inherited.targetDomain != DomainNames.temporary { try inherited.privileges.require(privilege, on: relation.table.name) }
        return Prepared(relation: relation, settings: inherited)
    }
}
